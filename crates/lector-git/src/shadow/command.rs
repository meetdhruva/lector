//! Running git for the shadow repository: hermetic commands against the shadow and read-only
//! commands in the user's work tree, the repository lock, and process execution.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use std::{fs, io};

use crate::error::{Error, Result};
use crate::pipes::output_with_input;

use super::ShadowRepo;

impl ShadowRepo {
    /// `git` with the shadow as `GIT_DIR`, hermetic config, no editor, no prompts.
    pub(super) fn base_command(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(self.cwd());
        cmd.args([
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
            "-c",
            "core.fsmonitor=false",
            // Always keep a fast-import pack: loosening would go through
            // unpack-objects, which freshens (touches) objects in the alternates.
            "-c",
            "fastimport.unpackLimit=0",
            "-c",
            "core.quotepath=off",
        ])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("LC_ALL", "C");
        if self.folder {
            // Belt and braces: every command names the shadow (`--git-dir`), which turns
            // discovery off; should one ever not, it may not climb above the folder either.
            if let Some(parent) = self.workdir.parent() {
                cmd.env("GIT_CEILING_DIRECTORIES", parent);
            }
        }
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_NAMESPACE",
            "GIT_OPTIONAL_LOCKS",
        ] {
            cmd.env_remove(var);
        }
        cmd
    }

    fn cwd(&self) -> &Path {
        if self.workdir.is_dir() {
            &self.workdir
        } else {
            self.git_dir.parent().unwrap_or(Path::new("."))
        }
    }

    /// Refuses operations that run git in the user's work tree (whole-tree listings, the
    /// index) for the shadow of a folder without git.
    pub(super) fn needs_repository(&self) -> Result<()> {
        if self.folder {
            return Err(Error::NotARepository(self.workdir.clone()));
        }
        Ok(())
    }

    pub(super) fn shadow_git(&self) -> Command {
        let mut cmd = self.base_command();
        cmd.arg("--git-dir").arg(&self.git_dir);
        cmd
    }

    /// Read-only `git` in the *user's* work tree: user config, `GIT_OPTIONAL_LOCKS=0`,
    /// literal pathspecs. Never used for the shadow of a folder without git (callers check
    /// [`ShadowRepo::needs_repository`] first).
    pub(super) fn user_git(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.workdir)
            .args([
                "--literal-pathspecs",
                "--no-pager",
                "-c",
                "core.quotepath=off",
                "-c",
                "core.fsmonitor=false",
            ])
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::null());
        cmd
    }

    pub(super) fn lock(&self) -> Result<LockGuard> {
        let path = self.git_dir.join("lector.lock");
        let started = std::time::Instant::now();
        loop {
            match lector_base::private::create_new(&path) {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(LockGuard(path));
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > Duration::from_secs(120));
                    if stale {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if started.elapsed() > Duration::from_secs(20) {
                        return Err(Error::Git(
                            "the shadow repository is locked by another operation".into(),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(err) => return Err(err.into()),
            }
        }
    }
}

pub(super) struct LockGuard(PathBuf);

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

pub(super) fn spawn_error(err: io::Error) -> Error {
    if err.kind() == io::ErrorKind::NotFound {
        Error::GitNotInstalled("the shadow repository")
    } else {
        Error::Io(err)
    }
}

/// Runs `cmd` to completion, feeding `stdin`; non-zero exit is [`Error::GitCommand`].
pub(super) fn exec(mut cmd: Command, what: &str, stdin: Option<&[u8]>) -> Result<Vec<u8>> {
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let child: Child = cmd.spawn().map_err(spawn_error)?;
    let output = output_with_input(child, stdin)?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Err(Error::GitCommand {
        command: what.to_owned(),
        status: output.status.to_string(),
        stderr: if stderr.is_empty() { stdout } else { stderr },
    })
}
