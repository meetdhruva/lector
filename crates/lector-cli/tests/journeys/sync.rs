//! Journey: sync with a remote. A bare remote and two clones; one commits and pushes, the
//! other fetches, pulls and sees the commit — through the registry commands the UI runs
//! (`git.stage`, `git.commit`, `git.upstream_status`, `git.push`, `git.fetch`, `git.pull`),
//! plus the rejected push and the rebase pull that stops with conflicts.

use std::path::PathBuf;

use serde_json::json;

use crate::fixture::{git_in, hermetic, norm, Session};

/// A bare remote and two clones of it, `alice` and `bob`, both on `main` with one commit.
pub struct SyncFixture {
    _tmp: tempfile::TempDir,
    /// The bare repository both clones use as `origin`.
    pub remote: PathBuf,
    /// First clone (the author in the journey).
    pub alice: PathBuf,
    /// Second clone (the one that pulls).
    pub bob: PathBuf,
}

impl SyncFixture {
    /// Builds the remote and both clones; `README.md` is committed and pushed from `alice`.
    pub fn new() -> SyncFixture {
        hermetic();
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = dunce::canonicalize(tmp.path()).expect("canonical tempdir");
        let remote = base.join("remote.git");
        std::fs::create_dir_all(&remote).expect("mkdir");
        git_in(&remote, &["init", "-q", "--bare", "-b", "main"]);
        let clone = |name: &str| {
            let dir = base.join(name);
            let from = remote.to_string_lossy().into_owned();
            let to = dir.to_string_lossy().into_owned();
            git_in(&base, &["clone", "-q", &from, &to]);
            for (key, value) in [
                ("user.name", name),
                ("user.email", "sync@example.com"),
                ("core.autocrlf", "false"),
            ] {
                git_in(&dir, &["config", key, value]);
            }
            dir
        };
        let alice = clone("alice");
        git_in(&alice, &["checkout", "-q", "-B", "main"]);
        std::fs::write(alice.join("README.md"), "# shared\n").expect("write");
        git_in(&alice, &["add", "-A"]);
        git_in(&alice, &["commit", "-q", "-m", "first"]);
        git_in(&alice, &["push", "-q", "-u", "origin", "main"]);
        let bob = clone("bob");
        SyncFixture {
            _tmp: tmp,
            remote,
            alice,
            bob,
        }
    }
}

/// The fixture itself works on this platform (plain git only): a commit pushed from one clone
/// reaches the other, and the bare remote has the branch.
#[test]
fn the_sync_fixture_is_ready() {
    let fx = SyncFixture::new();
    assert!(fx.remote.join("HEAD").is_file(), "a bare repository");
    assert_eq!(
        git_in(&fx.bob, &["log", "-1", "--format=%s"]).trim(),
        "first"
    );
    assert_eq!(
        norm(&std::fs::read_to_string(fx.bob.join("README.md")).expect("read")),
        "# shared\n"
    );
    std::fs::write(fx.alice.join("two.txt"), "two\n").expect("write");
    git_in(&fx.alice, &["add", "-A"]);
    git_in(&fx.alice, &["commit", "-q", "-m", "second"]);
    git_in(&fx.alice, &["push", "-q"]);
    git_in(&fx.bob, &["fetch", "-q"]);
    assert_eq!(
        git_in(&fx.bob, &["rev-list", "--count", "HEAD..origin/main"]).trim(),
        "1",
        "bob is one commit behind after a fetch"
    );
}

/// commit -> push -> second clone pulls -> sees the commit, all through registry commands.
#[test]
fn commit_push_second_clone_pulls() {
    let fx = SyncFixture::new();
    let data = tempfile::tempdir().expect("tempdir");
    let alice = Session::open_at(&fx.alice, &data.path().join("alice"));
    let bob = Session::open_at(&fx.bob, &data.path().join("bob"));
    let upstream = |s: &Session| s.ok("git.upstream_status", json!({})).data;

    // alice: commit a new file.
    std::fs::write(fx.alice.join("feature.txt"), "feature\n").expect("write");
    alice.ok("git.stage", json!({ "paths": ["feature.txt"] }));
    alice.ok("git.commit", json!({ "message": "Add feature" }));
    let st = upstream(&alice);
    assert_eq!(st["upstream"], "origin/main");
    assert_eq!(st["ahead"], 1);

    // alice: push. Journaled, not undoable; nothing left to push.
    let pushed = alice.ok("git.push", json!({}));
    assert_eq!(pushed.data["branch"], "main");
    assert_eq!(pushed.data["remote"], "origin");
    assert_eq!(pushed.data["remote_branch"], "main");
    assert_eq!(pushed.data["up_to_date"], false);
    assert!(pushed.undo.is_none(), "a push cannot be undone");
    assert_eq!(alice.journal.list(1)[0].command, "git.push");
    assert_eq!(upstream(&alice)["ahead"], 0);

    // bob: nothing behind until a fetch, then one commit behind.
    assert_eq!(upstream(&bob)["behind"], 0);
    let fetched = bob.ok("git.fetch", json!({}));
    assert!(
        fetched.data["updated"]
            .as_array()
            .is_some_and(|u| !u.is_empty()),
        "{:?}",
        fetched.data
    );
    assert_eq!(upstream(&bob)["behind"], 1);

    // bob: pull (fast-forward): the file arrives, history shows the commit, undoable.
    let pulled = bob.ok("git.pull", json!({}));
    assert_eq!(pulled.data["status"], "fast_forwarded");
    assert_eq!(pulled.data["incoming"], 1);
    assert!(pulled.undo.is_some(), "pull is undoable (branch back)");
    assert_eq!(
        norm(&std::fs::read_to_string(fx.bob.join("feature.txt")).expect("read")),
        "feature\n"
    );
    let history = bob.ok("git.history", json!({ "limit": 1 }));
    assert_eq!(history.data["commits"][0]["summary"], "Add feature");
    let st = upstream(&bob);
    assert_eq!(
        (st["ahead"].clone(), st["behind"].clone()),
        (json!(0), json!(0))
    );
    let snap = bob.status();
    assert!(snap.status.is_empty(), "clean after the pull");

    // A push rejected because the remote moved reports a typed error and changes nothing.
    std::fs::write(fx.bob.join("feature.txt"), "feature by bob\n").expect("write");
    bob.ok("git.stage", json!({ "paths": ["feature.txt"] }));
    bob.ok("git.commit", json!({ "message": "Bob edits feature" }));
    bob.ok("git.push", json!({}));
    std::fs::write(fx.alice.join("feature.txt"), "feature by alice\n").expect("write");
    alice.ok("git.stage", json!({ "paths": ["feature.txt"] }));
    alice.ok("git.commit", json!({ "message": "Alice edits feature" }));
    let head_before = git_in(&fx.alice, &["rev-parse", "HEAD"]);
    let remote_before = git_in(&fx.remote, &["rev-parse", "main"]);
    let err = alice.run("git.push", json!({})).expect_err("rejected");
    let kind = match &err {
        lector_app::commands::CommandError::Failed(e) => {
            match e.downcast_ref::<lector_git::Error>() {
                Some(lector_git::Error::Sync(s)) => Some(s.kind),
                _ => None,
            }
        }
        _ => None,
    };
    assert_eq!(kind, Some(lector_git::SyncErrorKind::Rejected), "{err}");
    assert_eq!(git_in(&fx.alice, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git_in(&fx.remote, &["rev-parse", "main"]), remote_before);

    // A rebase pull whose changes conflict stops with the rebase in progress, listed by status.
    let pulled = alice.ok("git.pull", json!({ "mode": "rebase" }));
    assert_eq!(pulled.data["status"], "conflicts");
    assert_eq!(pulled.data["conflicts"], json!(["feature.txt"]));
    let changes = alice.ok("git.list_changes", json!({}));
    assert!(
        changes.data["operation"]
            .as_str()
            .is_some_and(|o| o.contains("Rebase")),
        "{:?}",
        changes.data
    );
    assert_eq!(changes.data["conflicts"], 1);
    alice.ok("git.abort_operation", json!({ "kind": "rebase" }));
    assert_eq!(git_in(&fx.alice, &["rev-parse", "HEAD"]), head_before);
}
