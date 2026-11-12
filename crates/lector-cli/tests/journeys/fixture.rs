//! Fixtures shared by the journeys: hermetic git, a repository in a temp folder, and a headless
//! application core opened on it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

use lector_app::commands::{CommandContext, CommandError, Source};
use lector_app::journal::Journal;
use lector_app::{AppCore, CoreOptions, Outcome};
use lector_config::Settings;
use lector_workspace::Project;
use serde_json::Value;

/// Makes git in this process ignore global and system configuration and background
/// maintenance. Child `git` processes inherit it too.
pub fn hermetic() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("GIT_CONFIG_COUNT", "4");
        std::env::set_var("GIT_CONFIG_KEY_0", "gc.auto");
        std::env::set_var("GIT_CONFIG_VALUE_0", "0");
        std::env::set_var("GIT_CONFIG_KEY_1", "maintenance.auto");
        std::env::set_var("GIT_CONFIG_VALUE_1", "false");
        std::env::set_var("GIT_CONFIG_KEY_2", "commit.gpgsign");
        std::env::set_var("GIT_CONFIG_VALUE_2", "false");
        std::env::set_var("GIT_CONFIG_KEY_3", "gc.autoDetach");
        std::env::set_var("GIT_CONFIG_VALUE_3", "false");
    });
}

/// Runs `git` in `dir` (the test fails with git's message when it does) and returns stdout.
pub fn git_in(dir: &Path, args: &[&str]) -> String {
    hermetic();
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Journey")
        .env("GIT_AUTHOR_EMAIL", "journey@example.com")
        .env("GIT_COMMITTER_NAME", "Journey")
        .env("GIT_COMMITTER_EMAIL", "journey@example.com")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Line endings normalized to `\n`: git on Windows runners may check files out with CRLF.
pub fn norm(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// Whether two paths name the same file (canonical, `\\?\`-free spelling).
pub fn same_path(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// A repository in a temp folder, with folders for Lector's own state next to it.
pub struct Repo {
    _tmp: tempfile::TempDir,
    /// Work directory (canonical).
    pub root: PathBuf,
    /// Lector's data directory (journal backups) for this fixture.
    pub data: PathBuf,
    /// Lector's config directory for this fixture.
    pub config: PathBuf,
}

impl Repo {
    /// `git init` on branch `main` in `<tmp>/<name>`, identity set, `core.autocrlf` off.
    pub fn init(name: &str) -> Repo {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = dunce::canonicalize(tmp.path()).expect("canonical tempdir");
        let root = base.join(name);
        fs::create_dir_all(&root).expect("mkdir");
        git_in(&root, &["init", "-q", "-b", "main"]);
        for (key, value) in [
            ("user.name", "Journey"),
            ("user.email", "journey@example.com"),
            ("core.autocrlf", "false"),
            ("core.safecrlf", "false"),
        ] {
            git_in(&root, &["config", key, value]);
        }
        Repo {
            data: base.join("data"),
            config: base.join("config"),
            root,
            _tmp: tmp,
        }
    }

    /// Writes `rel` (with parent folders) under the work directory.
    pub fn write(&self, rel: &str, content: &str) {
        let path = self.root.join(rel);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).expect("mkdir");
        }
        fs::write(path, content).expect("write");
    }

    /// The file's text, line endings normalized.
    pub fn read(&self, rel: &str) -> String {
        norm(&fs::read_to_string(self.root.join(rel)).expect("read"))
    }

    /// `git <args>` in the work directory.
    pub fn git(&self, args: &[&str]) -> String {
        git_in(&self.root, args)
    }

    /// `git add -A && git commit`.
    pub fn commit_all(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }
}

/// A headless application core opened on a [`Repo`], the way `lector run` opens one.
pub struct Session {
    /// The core (registry, journal, database, git service, document store).
    pub core: AppCore,
    /// The command context (CLI origin).
    pub cx: CommandContext,
    /// The activity journal.
    pub journal: Journal,
    /// Work directory.
    pub root: PathBuf,
}

impl Session {
    /// Opens the project at the repository root.
    pub fn open(repo: &Repo) -> Session {
        Session::open_at(&repo.root, &repo.data)
    }

    /// Opens the project at an existing work directory `root`, with Lector's data in `data`.
    pub fn open_at(root: &Path, data: &Path) -> Session {
        hermetic();
        let core = AppCore::open(CoreOptions {
            in_memory_db: true,
            workers: Some(2),
            data_dir: Some(data.to_path_buf()),
            ..Default::default()
        });
        let journal = Journal::of(&core).expect("the journal is installed by AppCore::open");
        let mut settings = Settings::default();
        settings.lsp.enabled = false;
        let project = Project::open(root, settings).expect("project opens");
        let cx = CommandContext::new(core.clone(), Some(project), Source::Cli);
        Session {
            core,
            cx,
            journal,
            root: root.to_path_buf(),
        }
    }

    /// Invokes a registry command, as `lector run` does.
    pub fn run(&self, id: &str, args: Value) -> Result<Outcome, CommandError> {
        self.core.registry().invoke(id, args, &self.cx)
    }

    /// Like [`Session::run`], failing the test with the command's error.
    pub fn ok(&self, id: &str, args: Value) -> Outcome {
        self.run(id, args)
            .unwrap_or_else(|e| panic!("{id} failed: {e}"))
    }

    /// The project (for APIs that take one).
    pub fn project(&self) -> &Project {
        self.cx.project.as_ref().expect("project")
    }

    /// A fresh status snapshot of the repository.
    pub fn status(&self) -> std::sync::Arc<lector_app::GitSnapshot> {
        let state = self
            .core
            .git()
            .repo(&self.root)
            .expect("the folder is a git repository");
        state.refresh_blocking()
    }
}

/// `(staged, unstaged, untracked)` path counts of a snapshot.
pub fn counts(snap: &lector_app::GitSnapshot) -> (usize, usize, usize) {
    let staged = snap.status.iter().filter(|e| e.staged.is_some()).count();
    let unstaged = snap.status.iter().filter(|e| e.unstaged.is_some()).count();
    let untracked = snap.status.iter().filter(|e| e.untracked).count();
    (staged, unstaged, untracked)
}

/// 30 one-line functions: edits far apart form separate hunks.
pub fn thirty_functions() -> String {
    (1..=30)
        .map(|i| format!("pub fn f{i}() -> u32 {{ {i} }}\n"))
        .collect()
}
