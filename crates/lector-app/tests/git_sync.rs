//! Git sync commands end to end through the registry and journal: a bare "remote" plus
//! two clones built with the git CLI; undo of pull / branches / stash; typed errors;
//! progress, cancellation bookkeeping and credential env injection through `SyncHost`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, Once};

use lector_app::commands::git_sync::SyncHost;
use lector_app::commands::{CommandContext, CommandError, Source};
use lector_app::journal::{Journal, UndoMode};
use lector_app::{AppCore, CoreOptions, Outcome};
use lector_config::Settings;
use lector_git::{SyncErrorKind, SyncEvent};
use lector_workspace::Project;
use serde_json::{json, Value};

fn hermetic() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        for (k, v) in [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_SYSTEM", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_AUTHOR_NAME", "Ann"),
            ("GIT_AUTHOR_EMAIL", "ann@example.com"),
            ("GIT_COMMITTER_NAME", "Ann"),
            ("GIT_COMMITTER_EMAIL", "ann@example.com"),
            ("GIT_CONFIG_COUNT", "4"),
            ("GIT_CONFIG_KEY_0", "gc.auto"),
            ("GIT_CONFIG_VALUE_0", "0"),
            ("GIT_CONFIG_KEY_1", "maintenance.auto"),
            ("GIT_CONFIG_VALUE_1", "false"),
            ("GIT_CONFIG_KEY_2", "commit.gpgsign"),
            ("GIT_CONFIG_VALUE_2", "false"),
            ("GIT_CONFIG_KEY_3", "gc.autoDetach"),
            ("GIT_CONFIG_VALUE_3", "false"),
        ] {
            std::env::set_var(k, v);
        }
    });
}

fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

struct Fx {
    _tmp: tempfile::TempDir,
    a: PathBuf,
    b: PathBuf,
    cx: CommandContext,
    journal: Journal,
}

impl Fx {
    fn new() -> Fx {
        hermetic();
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        let remote = root.join("remote.git");
        let (a, b) = (root.join("a"), root.join("b"));
        git_in(
            &root,
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                remote.to_str().unwrap(),
            ],
        );
        git_in(
            &root,
            &["clone", "-q", remote.to_str().unwrap(), a.to_str().unwrap()],
        );
        std::fs::write(a.join("f.txt"), "one\n").unwrap();
        git_in(&a, &["add", "-A"]);
        git_in(&a, &["commit", "-q", "-m", "initial"]);
        git_in(&a, &["push", "-q", "-u", "origin", "main"]);
        git_in(
            &root,
            &["clone", "-q", remote.to_str().unwrap(), b.to_str().unwrap()],
        );
        let core = AppCore::open(CoreOptions::for_tests());
        let journal = Journal::install_in(&core, root.join("data"));
        let mut settings = Settings::default();
        settings.lsp.enabled = false;
        let project = Project::open(&a, settings).unwrap();
        let cx = CommandContext::new(core, Some(project), Source::Internal);
        Fx {
            _tmp: tmp,
            a,
            b,
            cx,
            journal,
        }
    }

    fn run(&self, id: &str, args: Value) -> Result<Outcome, CommandError> {
        self.cx.core.registry().invoke(id, args, &self.cx)
    }

    fn ok(&self, id: &str, args: Value) -> Outcome {
        self.run(id, args).unwrap_or_else(|e| panic!("{id}: {e}"))
    }

    fn undo(&self, outcome: &Outcome) {
        self.journal
            .undo(
                outcome.journal_id.expect("journaled"),
                &self.cx,
                UndoMode::Check,
            )
            .unwrap_or_else(|e| panic!("undo: {e}"));
    }

    fn commit(&self, dir: &Path, file: &str, content: &str) -> String {
        std::fs::write(dir.join(file), content).unwrap();
        git_in(dir, &["add", "-A"]);
        git_in(dir, &["commit", "-q", "-m", file]);
        git_in(dir, &["rev-parse", "HEAD"])
    }

    fn head(&self) -> String {
        git_in(&self.a, &["rev-parse", "HEAD"])
    }
}

fn sync_kind(err: &CommandError) -> Option<SyncErrorKind> {
    match err {
        CommandError::Failed(e) => match e.downcast_ref::<lector_git::Error>() {
            Some(lector_git::Error::Sync(s)) => Some(s.kind),
            _ => None,
        },
        _ => None,
    }
}

#[test]
fn specs_mark_writes_reads_and_undo() {
    let fx = Fx::new();
    let reg = fx.cx.core.registry();
    for id in ["git.upstream_status", "git.list_remotes", "git.stash_list"] {
        assert!(!reg.get(id).unwrap().spec().writes, "{id}");
    }
    for id in [
        "git.fetch",
        "git.pull",
        "git.push",
        "git.create_branch",
        "git.switch_branch",
        "git.rename_branch",
        "git.delete_branch",
        "git.set_upstream",
        "git.stash_push",
        "git.stash_apply",
        "git.stash_pop",
        "git.stash_drop",
        "git.stash_store",
        "git.restore_ref",
    ] {
        let spec = reg
            .get(id)
            .unwrap_or_else(|| panic!("{id} registered"))
            .spec()
            .clone();
        assert!(spec.writes, "{id} is a write (MCP needs --allow-writes)");
        let undoable = !matches!(id, "git.fetch" | "git.push");
        assert_eq!(spec.undoable, undoable, "{id}");
    }
    assert!(reg.get("git.delete_branch").unwrap().spec().destructive);
    assert!(reg.get("git.stash_drop").unwrap().spec().destructive);
}

#[test]
fn fetch_then_pull_and_undo_the_pull() {
    let fx = Fx::new();
    let before = fx.head();
    let new = fx.commit(&fx.b, "g.txt", "g\n");
    git_in(&fx.b, &["push", "-q"]);

    let host = SyncHost::of(&fx.cx.core);
    let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::default();
    let ev = events.clone();
    let sub = host.subscribe(move |p| ev.lock().unwrap().push(p.event.clone()));

    let fetched = fx.ok("git.fetch", json!({}));
    assert!(
        fetched.summary.contains("1 updated") || fetched.summary.contains("ref"),
        "{}",
        fetched.summary
    );
    assert!(fetched.undo.is_none());
    assert!(fetched.journal_id.is_some(), "fetch is journaled");
    let status = fx.ok("git.upstream_status", json!({}));
    assert_eq!(status.data["behind"], 1);
    assert_eq!(status.data["upstream"], "origin/main");

    let pulled = fx.ok("git.pull", json!({}));
    assert_eq!(pulled.data["status"], "fast_forwarded");
    assert_eq!(pulled.data["incoming"], 1);
    assert_eq!(fx.head(), new);
    assert!(host.running().is_empty());
    drop(sub);
    assert!(!events.lock().unwrap().is_empty(), "progress was published");

    fx.undo(&pulled);
    assert_eq!(fx.head(), before);
    assert!(!fx.a.join("g.txt").exists());
}

#[test]
fn diverged_pull_is_typed_then_rebase_and_push() {
    let fx = Fx::new();
    fx.commit(&fx.b, "g.txt", "g\n");
    git_in(&fx.b, &["push", "-q"]);
    fx.commit(&fx.a, "h.txt", "h\n");
    let err = fx.run("git.pull", json!({"mode": "ff_only"})).unwrap_err();
    assert_eq!(sync_kind(&err), Some(SyncErrorKind::Diverged), "{err}");
    let err = fx.run("git.push", json!({})).unwrap_err();
    assert_eq!(sync_kind(&err), Some(SyncErrorKind::Rejected), "{err}");

    let rebased = fx.ok("git.pull", json!({"mode": "rebase"}));
    assert_eq!(rebased.data["status"], "rebased");
    assert!(rebased.undo.is_some());
    let pushed = fx.ok("git.push", json!({}));
    assert!(pushed.undo.is_none(), "push is not undoable");
    assert!(pushed.journal_id.is_some());
    assert!(pushed.summary.contains("cannot be undone"));
    assert_eq!(pushed.data["refs"][0]["status"], "fast_forward");
}

#[test]
fn branches_and_their_undo() {
    let fx = Fx::new();
    let created = fx.ok(
        "git.create_branch",
        json!({"name": "topic", "switch": true}),
    );
    assert_eq!(git_in(&fx.a, &["branch", "--show-current"]), "topic");
    fx.undo(&created);
    assert_eq!(git_in(&fx.a, &["branch", "--show-current"]), "main");
    assert!(git_in(&fx.a, &["branch", "--list", "topic"]).is_empty());

    fx.ok("git.create_branch", json!({"name": "topic"}));
    fx.ok("git.switch_branch", json!({"name": "topic"}));
    let tip = fx.commit(&fx.a, "f.txt", "topic\n");
    fx.ok("git.switch_branch", json!({"name": "main"}));

    // Dirty tree: refused with the files and the stash alternative...
    std::fs::write(fx.a.join("f.txt"), "local\n").unwrap();
    let err = fx
        .run("git.switch_branch", json!({"name": "topic"}))
        .unwrap_err();
    let CommandError::Failed(e) = &err else {
        panic!("{err}")
    };
    let Some(lector_git::Error::Sync(s)) = e.downcast_ref::<lector_git::Error>() else {
        panic!("{err}")
    };
    assert_eq!(s.kind, SyncErrorKind::LocalChanges);
    assert_eq!(s.paths, vec!["f.txt".to_owned()]);
    assert_eq!(s.alternatives, vec!["stash_and_switch".to_owned()]);
    // ...and the explicit "stash and switch" choice; undo switches back and pops.
    let switched = fx.ok(
        "git.switch_branch",
        json!({"name": "topic", "stash_changes": true}),
    );
    assert_eq!(
        std::fs::read_to_string(fx.a.join("f.txt")).unwrap(),
        "topic\n"
    );
    fx.undo(&switched);
    assert_eq!(git_in(&fx.a, &["branch", "--show-current"]), "main");
    assert_eq!(
        std::fs::read_to_string(fx.a.join("f.txt")).unwrap(),
        "local\n"
    );
    git_in(&fx.a, &["checkout", "-q", "--", "f.txt"]);

    let renamed = fx.ok(
        "git.rename_branch",
        json!({"name": "topic", "new_name": "topic2"}),
    );
    fx.undo(&renamed);
    let err = fx
        .run("git.delete_branch", json!({"name": "topic"}))
        .unwrap_err();
    assert_eq!(sync_kind(&err), Some(SyncErrorKind::NotMerged));
    fx.ok(
        "git.set_upstream",
        json!({"branch": "topic", "upstream": "origin/main"}),
    );
    let deleted = fx.ok("git.delete_branch", json!({"name": "topic", "force": true}));
    fx.undo(&deleted);
    assert_eq!(git_in(&fx.a, &["rev-parse", "topic"]), tip);
    assert_eq!(
        git_in(&fx.a, &["rev-parse", "--abbrev-ref", "topic@{upstream}"]),
        "origin/main",
        "the upstream comes back too"
    );
    let unset = fx.ok(
        "git.set_upstream",
        json!({"branch": "topic", "upstream": null}),
    );
    fx.undo(&unset);
    assert_eq!(
        git_in(&fx.a, &["rev-parse", "--abbrev-ref", "topic@{upstream}"]),
        "origin/main"
    );
}

#[test]
fn stash_round_trip_with_undo() {
    let fx = Fx::new();
    let none = fx.ok("git.stash_push", json!({}));
    assert!(none.undo.is_none());
    std::fs::write(fx.a.join("f.txt"), "work\n").unwrap();
    std::fs::write(fx.a.join("u.txt"), "untracked\n").unwrap();
    let pushed = fx.ok(
        "git.stash_push",
        json!({"message": "wip", "include_untracked": true}),
    );
    assert!(!fx.a.join("u.txt").exists());
    let listed = fx.ok("git.stash_list", json!({}));
    assert_eq!(listed.data["stashes"].as_array().unwrap().len(), 1);
    let oid = pushed.data["stash"]["oid"].as_str().unwrap().to_owned();

    let dropped = fx.ok("git.stash_drop", json!({"index": 0}));
    assert!(fx.ok("git.stash_list", json!({})).data["stashes"]
        .as_array()
        .unwrap()
        .is_empty());
    fx.undo(&dropped);
    assert_eq!(
        fx.ok("git.stash_list", json!({})).data["stashes"][0]["oid"],
        oid.as_str()
    );

    // Pop, then undo: files back as before the pop, and the stash entry is back.
    let popped = fx.ok("git.stash_pop", json!({"oid": oid}));
    assert_eq!(
        std::fs::read_to_string(fx.a.join("f.txt")).unwrap(),
        "work\n"
    );
    assert!(fx.ok("git.stash_list", json!({})).data["stashes"]
        .as_array()
        .unwrap()
        .is_empty());
    fx.undo(&popped);
    assert_eq!(
        std::fs::read_to_string(fx.a.join("f.txt")).unwrap(),
        "one\n"
    );
    assert!(!fx.a.join("u.txt").exists());
    assert_eq!(
        fx.ok("git.stash_list", json!({})).data["stashes"][0]["oid"],
        oid.as_str()
    );

    // Undo of the push pops it back.
    fx.undo(&pushed);
    assert_eq!(
        std::fs::read_to_string(fx.a.join("f.txt")).unwrap(),
        "work\n"
    );
    assert_eq!(
        std::fs::read_to_string(fx.a.join("u.txt")).unwrap(),
        "untracked\n"
    );
}

#[cfg(unix)]
#[test]
fn credential_env_from_the_host_reaches_git() {
    let fx = Fx::new();
    git_in(
        &fx.a,
        &["remote", "add", "fake", "ssh://git@example.invalid/x.git"],
    );
    let host = SyncHost::of(&fx.cx.core);
    host.set_askpass_env(vec![(
        "GIT_SSH_COMMAND".into(),
        "echo 'Permission denied (publickey).' >&2; exit 255; :".into(),
    )]);
    let err = fx.run("git.fetch", json!({"remote": "fake"})).unwrap_err();
    assert_eq!(
        sync_kind(&err),
        Some(SyncErrorKind::AuthenticationFailed),
        "{err}"
    );
    assert!(host.running().is_empty());
    assert!(!host.cancel(12345));
}
