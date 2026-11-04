//! Repository writes (v0.2): staging, unstaging, discarding, applying patches and
//! committing. Everything here goes through the `git` CLI so hooks, index locking,
//! filters (autocrlf, LFS, `working-tree-encoding`) and commit signing behave
//! exactly as in a terminal.
//!
//! Rules:
//! * Only called as the direct result of an explicit user action.
//! * `git -C <workdir> …` with arguments as separate argv entries; never a shell.
//! * Paths are passed after `--` with `--literal-pathspecs`, so names containing
//!   `*`, `[` or a leading `:` are never treated as globs or pathspec magic.
//! * Patches and commit messages go through stdin.
//! * Failures surface git's own output verbatim in [`Error::GitCommand`]
//!   (`stderr`, or `stdout` when git reports there, e.g. "nothing to commit").
//! * Never `--no-verify`: hooks always run.

use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Component, Path};
use std::process::{Command, Stdio};

use gix::bstr::ByteSlice;

use crate::error::{Error, Result, ResultExt};
use crate::{Oid, Repo};

/// Where [`Repo::apply_patch`] applies a patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatchTarget {
    /// The index only (`git apply --cached`): stage or unstage a hunk or lines.
    Index,
    /// The working tree only (`git apply`): discard a hunk or lines.
    Worktree,
}

/// Options for [`Repo::commit`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitOptions {
    /// The full commit message (subject, blank line, body). Passed on stdin.
    pub message: String,
    /// Replace the tip commit (`--amend`).
    pub amend: bool,
    /// Add a `Signed-off-by` trailer (`--signoff`).
    pub sign_off: bool,
}

/// Maximum number of paths per git invocation (keeps argv well below OS limits).
const PATH_CHUNK: usize = 500;

/// Spawns `git` for a write, terminal-like: the user's config, hooks and locale apply.
struct WriteCli<'a> {
    workdir: &'a Path,
}

impl WriteCli<'_> {
    fn command(&self, literal_paths: bool) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(self.workdir);
        if literal_paths {
            cmd.arg("--literal-pathspecs");
        }
        cmd.args(["--no-pager", "-c", "core.quotepath=off"])
            .env("GIT_TERMINAL_PROMPT", "0")
            // Never open an interactive editor; every command here is non-interactive.
            .env("GIT_EDITOR", "true")
            .env("GIT_SEQUENCE_EDITOR", "true")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OPTIONAL_LOCKS");
        cmd
    }

    /// Runs `git <args>` (paths are literal), feeding `stdin` if given; returns stdout.
    fn run(&self, what: &'static str, args: &[OsString], stdin: Option<&[u8]>) -> Result<Vec<u8>> {
        self.run_with(what, true, args, stdin)
    }

    fn run_with(
        &self,
        what: &'static str,
        literal_paths: bool,
        args: &[OsString],
        stdin: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        let mut cmd = self.command(literal_paths);
        cmd.args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                Error::GitNotInstalled(what)
            } else {
                Error::Io(err)
            }
        })?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            // Write on a helper thread so a large patch can't deadlock against a full
            // stdout/stderr pipe. A broken pipe means git exited early; its exit status
            // and stderr explain why, so the write error itself is ignored.
            let input = input.to_vec();
            let writer = std::thread::spawn(move || {
                let _ = pipe.write_all(&input);
            });
            let output = child.wait_with_output()?;
            let _ = writer.join();
            return finish(args, output);
        }
        let output = child.wait_with_output()?;
        finish(args, output)
    }
}

fn finish(args: &[OsString], output: std::process::Output) -> Result<Vec<u8>> {
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    // `git commit` reports "nothing to commit" and failing-hook context on stdout.
    let message = match (stderr.is_empty(), stdout.is_empty()) {
        (false, true) => stderr,
        (true, false) => stdout,
        (false, false) => format!("{stderr}\n{stdout}"),
        (true, true) => String::new(),
    };
    Err(Error::GitCommand {
        command: args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" "),
        status: output.status.to_string(),
        stderr: message,
    })
}

fn args<I, S>(items: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    items.into_iter().map(Into::into).collect()
}

/// Rejects absolute paths and `..` components: writes stay inside the work tree.
fn check_paths(paths: &[&str]) -> Result<()> {
    for p in paths {
        let path = Path::new(p);
        let bad = p.is_empty()
            || path.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            });
        if bad {
            return Err(Error::Parse(format!(
                "refusing to write outside the working tree: {p:?}"
            )));
        }
    }
    Ok(())
}

impl Repo {
    fn write_cli(&self) -> Result<WriteCli<'_>> {
        Ok(WriteCli {
            workdir: self.require_workdir()?,
        })
    }

    /// Runs `git <prefix> -- <paths>` in chunks.
    fn run_paths(&self, what: &'static str, prefix: &[&str], paths: &[&str]) -> Result<()> {
        check_paths(paths)?;
        let cli = self.write_cli()?;
        for chunk in paths.chunks(PATH_CHUNK) {
            let mut a = args(prefix.iter().copied());
            a.push("--".into());
            a.extend(chunk.iter().map(OsString::from));
            cli.run(what, &a, None)?;
        }
        Ok(())
    }

    /// `true` when `HEAD` points at an unborn branch (no commits yet).
    fn head_unborn(&self) -> Result<bool> {
        Ok(self.head()?.oid.is_none())
    }

    /// Stages the given paths exactly as they are in the working tree, including
    /// deletions and untracked files (`git add -A -- <paths>`).
    pub fn stage_paths(&self, paths: &[&str]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        self.run_paths("staging", &["add", "-A"], paths)
    }

    /// Resets the index entries of `paths` to `HEAD` (`git restore --staged`), or
    /// removes them from the index on an unborn branch (`git rm --cached`). For a
    /// staged rename pass both the old and the new path.
    pub fn unstage_paths(&self, paths: &[&str]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        if self.head_unborn()? {
            self.run_paths(
                "unstaging",
                &["rm", "--cached", "-r", "-q", "--ignore-unmatch"],
                paths,
            )
        } else {
            self.run_paths("unstaging", &["restore", "--staged"], paths)
        }
    }

    /// Stages every change in the working tree (`git add -A`).
    pub fn stage_all(&self) -> Result<()> {
        self.write_cli()?
            .run_with("staging", false, &args(["add", "-A"]), None)
            .map(drop)
    }

    /// Unstages everything (`git reset -q`; on an unborn branch `git rm --cached -r .`).
    pub fn unstage_all(&self) -> Result<()> {
        let cli = self.write_cli()?;
        let a = if self.head_unborn()? {
            args(["rm", "--cached", "-r", "-q", "--ignore-unmatch", "--", "."])
        } else {
            args(["reset", "-q"])
        };
        cli.run("unstaging", &a, None).map(drop)
    }

    /// Discards working-tree changes of `paths`: tracked files are restored from the
    /// index (`git restore --worktree`, which also brings back deleted files);
    /// untracked files are **deleted** (`git clean -f`, which never touches tracked
    /// or ignored files).
    pub fn discard_paths(&self, paths: &[&str]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        check_paths(paths)?;
        let tracked = self.tracked_subset(paths)?;
        let (known, untracked): (Vec<&str>, Vec<&str>) =
            paths.iter().partition(|p| tracked.contains(**p));
        if !known.is_empty() {
            self.run_paths("discarding", &["restore", "--worktree"], &known)?;
        }
        if !untracked.is_empty() {
            self.run_paths("discarding", &["clean", "-f", "-d", "-q"], &untracked)?;
        }
        Ok(())
    }

    /// Which of `paths` are in the index (`git ls-files -z`).
    fn tracked_subset(&self, paths: &[&str]) -> Result<std::collections::HashSet<String>> {
        let cli = self.write_cli()?;
        let mut out = std::collections::HashSet::new();
        for chunk in paths.chunks(PATH_CHUNK) {
            let mut a = args(["ls-files", "-z", "--"]);
            a.extend(chunk.iter().map(OsString::from));
            let stdout = cli.run("discarding", &a, None)?;
            for name in stdout.split(|b| *b == 0).filter(|n| !n.is_empty()) {
                out.insert(String::from_utf8_lossy(name).into_owned());
            }
        }
        Ok(out)
    }

    /// Applies a unified-diff `patch` (as produced by `lector_diff::patch`) to the
    /// index (`git apply --cached`) or the working tree (`git apply`), reversed with
    /// `-R` when `reverse` is set. The patch is passed on stdin; `git apply` checks
    /// that every context line matches, so a stale patch fails instead of corrupting.
    pub fn apply_patch(
        &self,
        patch: impl AsRef<[u8]>,
        target: PatchTarget,
        reverse: bool,
    ) -> Result<()> {
        let cli = self.write_cli()?;
        let mut a = args(["apply"]);
        if target == PatchTarget::Index {
            a.push("--cached".into());
        }
        if reverse {
            a.push("-R".into());
        }
        a.push("-".into());
        cli.run("applying a patch", &a, Some(patch.as_ref()))
            .map(drop)
    }

    /// Creates a commit from the index (`git commit -F -`, message on stdin), or
    /// amends the tip with `amend`. (Named `create_commit` because [`Repo::commit`]
    /// reads a commit.) Hooks run; empty commits are refused by git.
    /// Returns the new `HEAD`.
    pub fn create_commit(&self, opts: &CommitOptions) -> Result<Oid> {
        let cli = self.write_cli()?;
        let mut a = args(["commit", "-q", "-F", "-"]);
        if opts.amend {
            a.push("--amend".into());
        }
        if opts.sign_off {
            a.push("--signoff".into());
        }
        cli.run_with("committing", false, &a, Some(opts.message.as_bytes()))?;
        self.resolve("HEAD")
    }

    /// The full message of the `HEAD` commit (for amending); `None` on an unborn branch.
    pub fn last_commit_message(&self) -> Result<Option<String>> {
        let repo = self.local();
        let Ok(commit) = repo.head_commit() else {
            return Ok(None);
        };
        let message = commit.message_raw().git()?.to_str_lossy().into_owned();
        Ok(Some(message))
    }

    /// The author identity git will use for a commit (`git var GIT_AUTHOR_IDENT`,
    /// honoring config, `GIT_AUTHOR_*` and `user.useConfigOnly`). `None` when git
    /// cannot determine one, i.e. `git commit` would fail with "Author identity unknown".
    pub fn identity(&self) -> Result<Option<(String, String)>> {
        let cli = self.write_cli()?;
        let out = match cli.run_with(
            "reading the identity",
            false,
            &args(["var", "GIT_AUTHOR_IDENT"]),
            None,
        ) {
            Ok(out) => out,
            Err(Error::GitCommand { .. }) => return Ok(None),
            Err(err) => return Err(err),
        };
        Ok(parse_ident(&String::from_utf8_lossy(&out)))
    }
}

/// Parses `Name <email> 1700000000 +0100` into `(name, email)`.
fn parse_ident(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    let open = line.find('<')?;
    let close = open + line[open..].find('>')?;
    let name = line[..open].trim().to_owned();
    let email = line[open + 1..close].trim().to_owned();
    if name.is_empty() && email.is_empty() {
        return None;
    }
    Some((name, email))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_identities() {
        assert_eq!(
            parse_ident("Ann Example <ann@x.org> 1700000000 +0100\n"),
            Some(("Ann Example".into(), "ann@x.org".into()))
        );
        assert_eq!(parse_ident("garbage"), None);
    }

    #[test]
    fn rejects_escaping_paths() {
        assert!(check_paths(&["a/b.txt", "c d", "*.rs"]).is_ok());
        assert!(check_paths(&["../x"]).is_err());
        assert!(check_paths(&["/etc/passwd"]).is_err());
        assert!(check_paths(&[""]).is_err());
    }
}
