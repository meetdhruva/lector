//! Fixture shared by the folder tests (`folder.rs`, `folder_scale.rs`): hermetic git, the fake
//! agent (Python 3) and a headless core opened on a project folder.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use std::time::Duration;

use lector_agent::*;
use lector_app::journal::{ActivityEntry, Journal};
use lector_app::Project;
use lector_app::{AppCore, CoreOptions};

pub const WAIT: Duration = Duration::from_secs(60);

pub fn hermetic_git() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("GIT_CONFIG_COUNT", "2");
        std::env::set_var("GIT_CONFIG_KEY_0", "gc.auto");
        std::env::set_var("GIT_CONFIG_VALUE_0", "0");
        std::env::set_var("GIT_CONFIG_KEY_1", "commit.gpgsign");
        std::env::set_var("GIT_CONFIG_VALUE_1", "false");
    });
}

pub fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|p| {
        Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

pub fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

pub struct Fx {
    pub work: tempfile::TempDir,
    pub data: PathBuf,
    pub root: PathBuf,
    pub core: AppCore,
    pub project: Project,
    pub py: &'static str,
}

impl Fx {
    /// `<tmp>/proj` with `a.txt` and `src/lib.rs`, not a git repository. `None` without Python.
    pub fn folder() -> Option<Fx> {
        Fx::build(|_| {}, "proj")
    }

    /// Like [`Fx::folder`]; `prepare(<tmp>)` runs first and the project opens at `<tmp>/<at>`.
    pub fn build(prepare: impl FnOnce(&Path), at: &str) -> Option<Fx> {
        hermetic_git();
        let py = python()?;
        let work = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(work.path()).unwrap();
        let root = base.join(at);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        prepare(&base);
        let data = base.join("data");
        let core = AppCore::open(CoreOptions {
            data_dir: Some(data.clone()),
            ..CoreOptions::for_tests()
        });
        let mut settings = lector_config::Settings::default();
        settings.lsp.enabled = false;
        let project = Project::open(&root, settings).unwrap();
        let root = project.root().to_path_buf();
        Some(Fx {
            work,
            data,
            root,
            core,
            project,
            py,
        })
    }

    pub fn config(&self) -> AgentConfig {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_acp_agent.py");
        AgentConfig::new("Fake", self.py, &[script.to_str().unwrap()])
    }

    pub fn start(&self) -> AgentSession {
        let options = SessionOptions {
            request_timeout: Duration::from_secs(20),
            cancel_grace: Duration::from_secs(5),
            ..Default::default()
        };
        AgentManager::of(&self.core)
            .start_with(&self.core, &self.project, &self.config(), options)
            .unwrap()
    }

    pub fn run(&self, s: &AgentSession, text: &str) -> Turn {
        let id = s.prompt(text.into(), vec![]).unwrap();
        s.wait_turn(id, WAIT).expect("turn finished")
    }

    pub fn read(&self, rel: &str) -> Option<String> {
        fs::read_to_string(self.root.join(rel)).ok()
    }

    pub fn journal(&self) -> Journal {
        Journal::of(&self.core).unwrap()
    }

    /// Journal entries of `command`, newest first.
    pub fn entries(&self, command: &str) -> Vec<ActivityEntry> {
        self.journal()
            .list(100)
            .into_iter()
            .filter(|e| e.command == command)
            .collect()
    }

    /// Where the shadow repository of `dir` would live.
    pub fn shadow_path(&self, dir: &Path) -> PathBuf {
        lector_git::ShadowRepo::path_for(&self.data, dir)
    }
}

#[macro_export]
macro_rules! need {
    ($fx:expr) => {
        match $fx {
            Some(f) => f,
            None => {
                eprintln!("python3 not found: skipping");
                return;
            }
        }
    };
}
