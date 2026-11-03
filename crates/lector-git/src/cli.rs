//! The few operations that shell out to the `git` CLI: blame, `log -L` and
//! `log --follow`. Everything else uses gix.
//!
//! All invocations are read-only and run with `GIT_OPTIONAL_LOCKS=0` so git
//! doesn't take the index lock to refresh stat data. The [`GitCli`] type is the
//! single place that spawns processes, so it can be swapped for a gix-native
//! implementation later.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Error, Result};
use crate::{BlameHunk, CommitInfo, Oid, Repo};

/// Spawns `git` in a repository with machine-friendly, read-only settings.
pub(crate) struct GitCli {
    program: PathBuf,
    workdir: PathBuf,
}

impl GitCli {
    pub(crate) fn new(workdir: &Path) -> Self {
        GitCli {
            program: PathBuf::from("git"),
            workdir: workdir.to_path_buf(),
        }
    }

    /// Run `git <args>` and return stdout. `what` names the feature for error messages.
    pub(crate) fn run<I, S>(&self, what: &'static str, args: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<_> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let output = Command::new(&self.program)
            .arg("-C")
            .arg(&self.workdir)
            .args([
                "--no-pager",
                "-c",
                "core.quotepath=off",
                "-c",
                "log.showSignature=false",
                // Never run a repository-configured fsmonitor hook.
                "-c",
                "core.fsmonitor=false",
            ])
            .args(&args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::null())
            .output()
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    Error::GitNotInstalled(what)
                } else {
                    Error::Io(err)
                }
            })?;
        if !output.status.success() {
            return Err(Error::GitCommand {
                command: args
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(" "),
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(output.stdout)
    }
}

/// Marker that prefixes commit ids in `git log` output we parse.
const COMMIT_MARKER: &str = "\x1elector:";

fn parse_marked_oids(stdout: &[u8]) -> Vec<Oid> {
    let text = String::from_utf8_lossy(stdout);
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(hex) = line.strip_prefix(COMMIT_MARKER) {
            if let Some(oid) = Oid::from_hex(hex) {
                if !out.contains(&oid) {
                    out.push(oid);
                }
            }
        }
    }
    out
}

impl Repo {
    fn cli(&self) -> Result<GitCli> {
        Ok(GitCli::new(self.require_workdir()?))
    }

    /// `git blame --porcelain` for `path` at `rev`, or including uncommitted
    /// working-tree changes when `rev` is `None` (those lines have a zero oid and
    /// `uncommitted: true`). Consecutive lines from the same commit form one hunk.
    pub fn blame(&self, path: &str, rev: Option<&str>) -> Result<Vec<BlameHunk>> {
        let cli = self.cli()?;
        let mut args: Vec<&str> = vec!["blame", "--porcelain"];
        if let Some(rev) = rev {
            if rev.starts_with('-') {
                // Would be parsed as an option (e.g. `--contents=<file>`).
                return Err(Error::InvalidRevision {
                    rev: rev.to_owned(),
                    message: "revisions cannot start with '-'".into(),
                });
            }
            args.push(rev);
        }
        args.push("--");
        args.push(path);
        let stdout = cli.run("blame", &args)?;
        parse_blame_porcelain(&stdout)
    }

    /// Commits that changed the given 0-based line range of `path` (as it is in
    /// `HEAD`), newest first, via `git log -L`. `limit == 0` means unlimited.
    pub fn line_history(
        &self,
        path: &str,
        lines: Range<u32>,
        limit: usize,
    ) -> Result<Vec<CommitInfo>> {
        let cli = self.cli()?;
        let start = lines.start.saturating_add(1);
        let end = lines.end.max(start);
        let range = format!("-L{start},{end}:{path}");
        let format = format!("--format={COMMIT_MARKER}%H");
        let mut args = vec![
            "log".to_owned(),
            "--no-color".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            format,
            range,
        ];
        if limit > 0 {
            args.push(format!("--max-count={limit}"));
        }
        let stdout = cli.run("line history", &args)?;
        self.commit_infos(&parse_marked_oids(&stdout))
    }

    /// Commits that changed `path`, following renames (`git log --follow`),
    /// newest first. `limit == 0` means unlimited.
    pub fn file_history(&self, path: &str, limit: usize) -> Result<Vec<CommitInfo>> {
        let cli = self.cli()?;
        let format = format!("--format={COMMIT_MARKER}%H");
        let mut args = vec![
            "log".to_owned(),
            "--follow".into(),
            "--no-color".into(),
            format,
        ];
        if limit > 0 {
            args.push(format!("--max-count={limit}"));
        }
        args.push("--".into());
        args.push(path.into());
        let stdout = cli.run("file history", &args)?;
        self.commit_infos(&parse_marked_oids(&stdout))
    }
}

#[derive(Default, Clone)]
struct BlameCommit {
    author: String,
    time: i64,
    summary: String,
    filename: String,
}

/// Parse `git blame --porcelain` output into merged hunks.
pub(crate) fn parse_blame_porcelain(stdout: &[u8]) -> Result<Vec<BlameHunk>> {
    let text = String::from_utf8_lossy(stdout);
    let mut commits: HashMap<String, BlameCommit> = HashMap::new();
    // (sha, orig_line, final_line) per blamed line, 1-based from git.
    let mut lines: Vec<(String, u32, u32)> = Vec::new();
    let mut current: Option<String> = None;
    for line in text.split('\n') {
        if line.is_empty() {
            continue;
        }
        if line.starts_with('\t') {
            // Content line; ends the current header group.
            current = None;
            continue;
        }
        if current.is_none() {
            let mut parts = line.split(' ');
            let sha = parts.next().unwrap_or_default();
            let orig = parts.next().and_then(|p| p.parse::<u32>().ok());
            let fin = parts.next().and_then(|p| p.parse::<u32>().ok());
            let (Some(orig), Some(fin)) = (orig, fin) else {
                return Err(Error::Parse(format!("unexpected blame header: {line}")));
            };
            if Oid::from_hex(sha).is_none() {
                return Err(Error::Parse(format!("unexpected blame header: {line}")));
            }
            lines.push((sha.to_owned(), orig, fin));
            commits.entry(sha.to_owned()).or_default();
            current = Some(sha.to_owned());
            continue;
        }
        let Some(sha) = current.as_ref() else {
            continue;
        };
        let commit = commits.entry(sha.clone()).or_default();
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "author" => commit.author = value.to_owned(),
            "author-time" => commit.time = value.parse().unwrap_or_default(),
            "summary" => commit.summary = value.to_owned(),
            "filename" => commit.filename = value.to_owned(),
            _ => {}
        }
    }

    let mut hunks: Vec<BlameHunk> = Vec::new();
    for (sha, orig, fin) in lines {
        let Some(oid) = Oid::from_hex(&sha) else {
            continue;
        };
        let commit = commits.get(&sha).cloned().unwrap_or_default();
        let row = fin.saturating_sub(1);
        let orig_row = orig.saturating_sub(1);
        if let Some(last) = hunks.last_mut() {
            if last.oid == oid
                && last.lines.end == row
                && last
                    .orig_start
                    .checked_add(last.lines.end - last.lines.start)
                    == Some(orig_row)
                && last.orig_path == commit.filename
            {
                last.lines.end = row + 1;
                continue;
            }
        }
        hunks.push(BlameHunk {
            oid,
            lines: row..row + 1,
            orig_start: orig_row,
            orig_path: commit.filename,
            author: commit.author,
            time: commit.time,
            summary: commit.summary,
            uncommitted: oid.is_zero(),
        });
    }
    Ok(hunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_and_merges_hunks() {
        let a = "1111111111111111111111111111111111111111";
        let z = "0000000000000000000000000000000000000000";
        let out = format!(
            "{a} 1 1 2\nauthor Ann\nauthor-mail <a@x>\nauthor-time 100\nauthor-tz +0000\nsummary first\nfilename old.txt\n\tline1\n\
             {a} 2 2\n\tline2\n\
             {z} 3 3 1\nauthor Not Committed Yet\nauthor-time 200\nsummary Version of new.txt from new.txt\nfilename new.txt\n\tline3\n\
             {a} 3 4 1\nfilename old.txt\n\tline4\n"
        );
        let hunks = parse_blame_porcelain(out.as_bytes()).unwrap();
        assert_eq!(hunks.len(), 3);
        assert_eq!(hunks[0].lines, 0..2);
        assert_eq!(hunks[0].author, "Ann");
        assert_eq!(hunks[0].orig_path, "old.txt");
        assert_eq!(hunks[0].time, 100);
        assert!(!hunks[0].uncommitted);
        assert_eq!(hunks[1].lines, 2..3);
        assert!(hunks[1].uncommitted);
        assert_eq!(hunks[2].lines, 3..4);
        assert_eq!(hunks[2].orig_start, 2);
        assert_eq!(hunks[2].summary, "first");
    }

    #[test]
    fn missing_git_is_a_clear_error() {
        let cli = GitCli {
            program: PathBuf::from("definitely-not-git-lector-test"),
            workdir: std::env::temp_dir(),
        };
        let err = cli.run("blame", ["--version"]).unwrap_err();
        assert!(matches!(err, Error::GitNotInstalled("blame")), "{err}");
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn failing_command_reports_stderr() {
        let cli = GitCli::new(&std::env::temp_dir());
        match cli.run("test", ["definitely-not-a-subcommand"]) {
            Err(Error::GitCommand { stderr, .. }) => assert!(!stderr.is_empty()),
            Err(Error::GitNotInstalled(_)) => {}
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_blame_porcelain(b"not blame output\n").is_err());
    }
}
