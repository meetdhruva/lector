//! Fixture repositories built with the `git` CLI.
#![allow(dead_code)]

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::Command;

use lector_git::Repo;

/// Base timestamp for fixture commits (2023-11-14).
pub const BASE_TIME: i64 = 1_700_000_000;

pub struct Fixture {
    pub dir: tempfile::TempDir,
    tick: Cell<i64>,
}

impl Fixture {
    /// An empty repository with `main` as the initial branch.
    pub fn new() -> Fixture {
        let fixture = Fixture {
            dir: tempfile::tempdir().expect("tempdir"),
            tick: Cell::new(0),
        };
        fixture.git(&["init", "-q", "-b", "main"]);
        fixture
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn repo(&self) -> Repo {
        Repo::discover(self.path()).expect("discover")
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let time = BASE_TIME + self.tick.get() * 60;
        self.tick.set(self.tick.get() + 1);
        let date = format!("@{time} +0100");
        let mut cmd = Command::new("git");
        cmd.current_dir(self.path())
            .args([
                "-c",
                "core.autocrlf=false",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Alice Author")
            .env("GIT_AUTHOR_EMAIL", "alice@example.com")
            .env("GIT_COMMITTER_NAME", "Carl Committer")
            .env("GIT_COMMITTER_EMAIL", "carl@example.com")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
        cmd
    }

    /// Run git; panic on failure; return stdout.
    pub fn git(&self, args: &[&str]) -> String {
        let out = self.command(args).output().expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Run git and return (success, stdout).
    pub fn git_allow_fail(&self, args: &[&str]) -> bool {
        self.command(args)
            .output()
            .expect("spawn git")
            .status
            .success()
    }

    pub fn git_as(&self, author: &str, email: &str, args: &[&str]) -> String {
        let out = self
            .command(args)
            .env("GIT_AUTHOR_NAME", author)
            .env("GIT_AUTHOR_EMAIL", email)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    pub fn write(&self, path: &str, content: &str) {
        let full = self.path().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(full, content).expect("write");
    }

    pub fn remove(&self, path: &str) {
        std::fs::remove_file(self.path().join(path)).expect("remove");
    }

    /// `git add -A && git commit -m msg`; returns the new HEAD sha.
    pub fn commit_all(&self, msg: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", msg]);
        self.rev("HEAD")
    }

    pub fn rev(&self, rev: &str) -> String {
        self.git(&["rev-parse", rev]).trim().to_owned()
    }

    pub fn file(&self, path: &str) -> PathBuf {
        self.path().join(path)
    }
}
