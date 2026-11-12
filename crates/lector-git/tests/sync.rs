//! Network and branch operations against fixtures built with the git CLI: a bare "remote"
//! plus two clones (`a`, `b`). Hermetic: no user/system config, fixed identities.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use std::time::{Duration, Instant};

use lector_git::{
    CancelToken, Error, FetchOptions, PullMode, PullStatus, PushOptions, PushRefStatus, Repo,
    StashPushOptions, SyncError, SyncErrorKind, SyncEvent, SyncOptions,
};

fn hermetic() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        for (k, v) in [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_AUTHOR_NAME", "Ann"),
            ("GIT_AUTHOR_EMAIL", "ann@example.com"),
            ("GIT_COMMITTER_NAME", "Ann"),
            ("GIT_COMMITTER_EMAIL", "ann@example.com"),
            ("GIT_CONFIG_COUNT", "4"),
            ("GIT_CONFIG_KEY_0", "gc.auto"),
            ("GIT_CONFIG_VALUE_0", "0"),
            ("GIT_CONFIG_KEY_1", "commit.gpgsign"),
            ("GIT_CONFIG_VALUE_1", "false"),
            ("GIT_CONFIG_KEY_2", "maintenance.auto"),
            ("GIT_CONFIG_VALUE_2", "false"),
            ("GIT_CONFIG_KEY_3", "advice.detachedHead"),
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
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

struct Fx {
    _tmp: tempfile::TempDir,
    remote: PathBuf,
    a: PathBuf,
    b: PathBuf,
}

impl Fx {
    /// remote with one commit on `main`, cloned into `a` and `b`.
    fn new() -> Fx {
        hermetic();
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        let remote = root.join("remote.git");
        let a = root.join("a");
        let b = root.join("b");
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
        std::fs::write(a.join("f.txt"), "one\ntwo\nthree\n").unwrap();
        git_in(&a, &["add", "-A"]);
        git_in(&a, &["commit", "-q", "-m", "initial"]);
        git_in(&a, &["push", "-q", "-u", "origin", "main"]);
        git_in(
            &root,
            &["clone", "-q", remote.to_str().unwrap(), b.to_str().unwrap()],
        );
        Fx {
            _tmp: tmp,
            remote,
            a,
            b,
        }
    }

    fn repo(&self, dir: &Path) -> Repo {
        Repo::discover(dir).unwrap()
    }

    fn commit(&self, dir: &Path, file: &str, content: &str, msg: &str) -> String {
        std::fs::write(dir.join(file), content).unwrap();
        git_in(dir, &["add", "-A"]);
        git_in(dir, &["commit", "-q", "-m", msg]);
        git_in(dir, &["rev-parse", "HEAD"])
    }
}

fn no_events() -> impl FnMut(SyncEvent) {
    |_| {}
}

fn sync_err(err: Error) -> SyncError {
    match err {
        Error::Sync(e) => *e,
        other => panic!("expected a sync error, got {other:?}"),
    }
}

#[test]
fn fetch_reports_updated_refs_and_progress() {
    let fx = Fx::new();
    let new = fx.commit(&fx.b, "g.txt", "g\n", "from b");
    git_in(&fx.b, &["push", "-q"]);
    git_in(&fx.b, &["tag", "v1"]);
    git_in(&fx.b, &["push", "-q", "origin", "v1"]);
    let repo = fx.repo(&fx.a);
    let mut events = Vec::new();
    let out = repo
        .fetch(
            &FetchOptions::default(),
            &SyncOptions::default(),
            &mut |e| events.push(e),
        )
        .unwrap();
    let main = out
        .updated
        .iter()
        .find(|u| u.name == "refs/remotes/origin/main")
        .expect("main updated");
    assert_eq!(main.new.unwrap().to_string(), new);
    assert!(main.old.is_some());
    assert!(out
        .updated
        .iter()
        .any(|u| u.name == "refs/tags/v1" && u.old.is_none()));
    assert!(!events.is_empty(), "git reported progress or messages");

    // Nothing new: no updates. Prune removes a deleted remote branch.
    git_in(&fx.b, &["push", "-q", "origin", "HEAD:refs/heads/tmp"]);
    repo.fetch(
        &FetchOptions::default(),
        &SyncOptions::default(),
        &mut no_events(),
    )
    .unwrap();
    git_in(&fx.b, &["push", "-q", "origin", "--delete", "tmp"]);
    let out = repo
        .fetch(
            &FetchOptions {
                remote: Some("origin".into()),
                prune: true,
            },
            &SyncOptions::default(),
            &mut no_events(),
        )
        .unwrap();
    assert_eq!(out.updated.len(), 1);
    assert_eq!(out.updated[0].name, "refs/remotes/origin/tmp");
    assert_eq!(out.updated[0].new, None);

    let err = repo
        .fetch(
            &FetchOptions {
                remote: Some("nope".into()),
                prune: false,
            },
            &SyncOptions::default(),
            &mut no_events(),
        )
        .unwrap_err();
    assert_eq!(sync_err(err).kind, SyncErrorKind::NoRemote);
}

#[test]
fn pull_fast_forwards_and_reports_up_to_date() {
    let fx = Fx::new();
    let before = git_in(&fx.a, &["rev-parse", "HEAD"]);
    let new = fx.commit(&fx.b, "g.txt", "g\n", "from b");
    git_in(&fx.b, &["push", "-q"]);
    let repo = fx.repo(&fx.a);
    let out = repo
        .pull(
            PullMode::FastForwardOnly,
            &SyncOptions::default(),
            &mut no_events(),
        )
        .unwrap();
    assert_eq!(out.status, PullStatus::FastForwarded);
    assert_eq!(out.incoming, 1);
    assert_eq!(out.before.unwrap().to_string(), before);
    assert_eq!(out.after.unwrap().to_string(), new);
    assert_eq!(out.upstream, "origin/main");
    let again = repo
        .pull(
            PullMode::FastForwardOnly,
            &SyncOptions::default(),
            &mut no_events(),
        )
        .unwrap();
    assert_eq!(again.status, PullStatus::UpToDate);

    // Undo: the branch goes back exactly; a wrong expectation is refused.
    let err = repo
        .restore_ref("refs/heads/main", out.before, out.before)
        .unwrap_err();
    assert_eq!(sync_err(err).kind, SyncErrorKind::RefChanged);
    repo.restore_ref("refs/heads/main", out.after, out.before)
        .unwrap();
    assert_eq!(git_in(&fx.a, &["rev-parse", "HEAD"]), before);
    assert!(
        !fx.a.join("g.txt").exists(),
        "worktree follows (reset --keep)"
    );
}

#[test]
fn diverged_pull_needs_rebase_and_never_merges() {
    let fx = Fx::new();
    fx.commit(&fx.b, "g.txt", "g\n", "from b");
    git_in(&fx.b, &["push", "-q"]);
    fx.commit(&fx.a, "h.txt", "h\n", "local");
    let repo = fx.repo(&fx.a);
    let err = repo
        .pull(
            PullMode::FastForwardOnly,
            &SyncOptions::default(),
            &mut no_events(),
        )
        .unwrap_err();
    let err = sync_err(err);
    assert_eq!(err.kind, SyncErrorKind::Diverged, "{err}");
    assert!(!err.message.contains("hint:"));

    // ahead/behind after the fetch the failed pull did.
    let st = repo.upstream_status().unwrap();
    assert_eq!((st.ahead, st.behind), (1, 1));
    assert_eq!(st.upstream.as_deref(), Some("origin/main"));
    assert_eq!(st.remote.as_deref(), Some("origin"));
    assert_eq!(st.push_remote.as_deref(), Some("origin"));

    let out = repo
        .pull(PullMode::Rebase, &SyncOptions::default(), &mut no_events())
        .unwrap();
    assert_eq!(out.status, PullStatus::Rebased);
    assert_eq!((out.incoming, out.rebased), (1, 1));
    let parents = git_in(&fx.a, &["log", "--format=%P", "-1"]);
    assert_eq!(parents.split(' ').count(), 1, "no merge commit");
    assert!(fx.a.join("g.txt").exists() && fx.a.join("h.txt").exists());
}

#[test]
fn rebase_pull_with_conflicts_hands_over_to_the_conflict_flow() {
    let fx = Fx::new();
    fx.commit(&fx.b, "f.txt", "one\nTWO-b\nthree\n", "b edits");
    git_in(&fx.b, &["push", "-q"]);
    fx.commit(&fx.a, "f.txt", "one\nTWO-a\nthree\n", "a edits");
    let repo = fx.repo(&fx.a);
    let out = repo
        .pull(PullMode::Rebase, &SyncOptions::default(), &mut no_events())
        .unwrap();
    assert_eq!(out.status, PullStatus::Conflicts);
    assert_eq!(out.conflicts, vec!["f.txt".to_owned()]);
    let op = repo.operation().unwrap().expect("rebase in progress");
    assert_eq!(op.kind, lector_git::OperationKind::Rebase);
    repo.abort_operation(op.kind).unwrap();
    assert!(repo.operation().unwrap().is_none());
}

#[test]
fn pull_without_upstream_or_with_dirty_tree() {
    let fx = Fx::new();
    let repo = fx.repo(&fx.a);
    git_in(&fx.a, &["switch", "-q", "-c", "lonely"]);
    let err = sync_err(
        repo.pull(
            PullMode::FastForwardOnly,
            &SyncOptions::default(),
            &mut no_events(),
        )
        .unwrap_err(),
    );
    assert_eq!(err.kind, SyncErrorKind::NoUpstream);
    assert!(err.hint.is_some());
    git_in(&fx.a, &["switch", "-q", "main"]);
    fx.commit(&fx.b, "g.txt", "g\n", "from b");
    git_in(&fx.b, &["push", "-q"]);
    fx.commit(&fx.a, "h.txt", "h\n", "local");
    std::fs::write(fx.a.join("f.txt"), "dirty\n").unwrap();
    let err = sync_err(
        repo.pull(PullMode::Rebase, &SyncOptions::default(), &mut no_events())
            .unwrap_err(),
    );
    assert!(
        matches!(
            err.kind,
            SyncErrorKind::DirtyWorktree | SyncErrorKind::LocalChanges
        ),
        "{err}"
    );
}

#[test]
fn push_fast_forward_rejected_and_hint() {
    let fx = Fx::new();
    let repo_a = fx.repo(&fx.a);
    fx.commit(&fx.a, "h.txt", "h\n", "a");
    let st = repo_a.upstream_status().unwrap();
    assert_eq!((st.ahead, st.behind), (1, 0));
    let mut events = Vec::new();
    let out = repo_a
        .push(&PushOptions::default(), &SyncOptions::default(), &mut |e| {
            events.push(e)
        })
        .unwrap();
    assert_eq!(out.remote, "origin");
    assert_eq!(out.remote_branch, "main");
    assert!(!out.set_upstream);
    assert_eq!(out.refs.len(), 1);
    assert_eq!(out.refs[0].status, PushRefStatus::FastForward);
    assert!(
        events.iter().any(|e| matches!(e, SyncEvent::Progress(_))),
        "{events:?}"
    );
    let again = repo_a
        .push(
            &PushOptions::default(),
            &SyncOptions::default(),
            &mut no_events(),
        )
        .unwrap();
    assert!(again.up_to_date);

    // b is behind now: its push is rejected with "pull first".
    fx.commit(&fx.b, "g.txt", "g\n", "b");
    let err = sync_err(
        fx.repo(&fx.b)
            .push(
                &PushOptions::default(),
                &SyncOptions::default(),
                &mut no_events(),
            )
            .unwrap_err(),
    );
    assert_eq!(err.kind, SyncErrorKind::Rejected, "{err}");
    assert!(err.hint.as_deref().unwrap_or("").contains("Pull first"));
}

#[test]
fn push_sets_upstream_for_new_branches() {
    let fx = Fx::new();
    let repo = fx.repo(&fx.a);
    repo.create_branch("feature", None).unwrap();
    repo.switch_branch("feature", &SyncOptions::default())
        .unwrap();
    fx.commit(&fx.a, "feat.txt", "x\n", "feature");
    assert_eq!(repo.upstream_status().unwrap().upstream, None);
    let out = repo
        .push(
            &PushOptions::default(),
            &SyncOptions::default(),
            &mut no_events(),
        )
        .unwrap();
    assert!(out.set_upstream);
    assert_eq!(out.refs[0].status, PushRefStatus::Created);
    let st = repo.upstream_status().unwrap();
    assert_eq!(st.upstream.as_deref(), Some("origin/feature"));
    assert_eq!((st.ahead, st.behind), (0, 0));
    assert_eq!(
        git_in(&fx.remote, &["rev-parse", "feature"]),
        git_in(&fx.a, &["rev-parse", "HEAD"])
    );
}

#[test]
fn force_with_lease_only_when_the_remote_did_not_move() {
    let fx = Fx::new();
    let repo = fx.repo(&fx.a);
    fx.commit(&fx.a, "h.txt", "h\n", "a");
    repo.push(
        &PushOptions::default(),
        &SyncOptions::default(),
        &mut no_events(),
    )
    .unwrap();
    git_in(&fx.a, &["commit", "-q", "--amend", "-m", "a rewritten"]);
    let plain = sync_err(
        repo.push(
            &PushOptions::default(),
            &SyncOptions::default(),
            &mut no_events(),
        )
        .unwrap_err(),
    );
    assert_eq!(plain.kind, SyncErrorKind::Rejected);
    let lease = PushOptions {
        force_with_lease: true,
        ..PushOptions::default()
    };
    let out = repo
        .push(&lease, &SyncOptions::default(), &mut no_events())
        .unwrap();
    assert_eq!(out.refs[0].status, PushRefStatus::Forced);

    // Someone else pushes; our lease (last fetched value) is stale now.
    git_in(&fx.b, &["pull", "-q", "--rebase"]);
    git_in(&fx.b, &["reset", "-q", "--hard", "origin/main"]);
    fx.commit(&fx.b, "g.txt", "g\n", "b");
    git_in(&fx.b, &["push", "-q"]);
    git_in(
        &fx.a,
        &["commit", "-q", "--amend", "-m", "a rewritten again"],
    );
    let err = sync_err(
        repo.push(&lease, &SyncOptions::default(), &mut no_events())
            .unwrap_err(),
    );
    assert_eq!(err.kind, SyncErrorKind::StaleLease, "{err}");
}

#[test]
fn hooks_that_decline_report_their_output() {
    let fx = Fx::new();
    let repo = fx.repo(&fx.a);
    fx.commit(&fx.a, "h.txt", "h\n", "a");
    let hook = fx.remote.join("hooks/pre-receive");
    std::fs::write(&hook, "#!/bin/sh\necho server says no\nexit 1\n").unwrap();
    make_executable(&hook);
    let err = sync_err(
        repo.push(
            &PushOptions::default(),
            &SyncOptions::default(),
            &mut no_events(),
        )
        .unwrap_err(),
    );
    assert_eq!(err.kind, SyncErrorKind::HookDeclined, "{err}");
    assert!(
        err.hook_output
            .as_deref()
            .unwrap_or("")
            .contains("server says no"),
        "{err:?}"
    );
    std::fs::remove_file(&hook).unwrap();

    let local = fx.a.join(".git/hooks/pre-push");
    std::fs::write(&local, "#!/bin/sh\necho not today >&2\nexit 1\n").unwrap();
    make_executable(&local);
    let err = sync_err(
        repo.push(
            &PushOptions::default(),
            &SyncOptions::default(),
            &mut no_events(),
        )
        .unwrap_err(),
    );
    assert_eq!(err.kind, SyncErrorKind::HookDeclined, "{err}");
    assert!(
        err.hook_output
            .as_deref()
            .unwrap_or("")
            .contains("not today"),
        "{err:?}"
    );
}

#[cfg(unix)]
fn make_executable(p: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_: &Path) {}

/// Uses an ssh remote whose "ssh" is a shell snippet, injected through `SyncOptions::env`.
#[cfg(unix)]
fn with_fake_ssh(fx: &Fx, ssh: &str) -> (Repo, SyncOptions) {
    git_in(
        &fx.a,
        &[
            "remote",
            "add",
            "fake",
            "ssh://git@example.invalid/repo.git",
        ],
    );
    let opts = SyncOptions::default().with_env(vec![("GIT_SSH_COMMAND".into(), ssh.into())]);
    (fx.repo(&fx.a), opts)
}

#[cfg(unix)]
#[test]
fn ssh_failures_are_classified() {
    let fx = Fx::new();
    let fetch_fake = FetchOptions {
        remote: Some("fake".into()),
        prune: false,
    };
    let (repo, opts) = with_fake_ssh(
        &fx,
        "echo 'git@example.invalid: Permission denied (publickey).' >&2; exit 255; :",
    );
    let err = sync_err(
        repo.fetch(&fetch_fake, &opts, &mut no_events())
            .unwrap_err(),
    );
    assert_eq!(err.kind, SyncErrorKind::AuthenticationFailed, "{err}");
    let opts = SyncOptions::default().with_env(vec![(
        "GIT_SSH_COMMAND".into(),
        "echo 'Host key verification failed.' >&2; exit 255; :".into(),
    )]);
    let err = sync_err(
        repo.fetch(&fetch_fake, &opts, &mut no_events())
            .unwrap_err(),
    );
    assert_eq!(err.kind, SyncErrorKind::HostKeyUnknown, "{err}");
    let opts = SyncOptions::default().with_env(vec![(
        "GIT_SSH_COMMAND".into(),
        "echo 'ssh: Could not resolve hostname example.invalid: Name or service not known' >&2; exit 255; :".into(),
    )]);
    let err = sync_err(
        repo.fetch(&fetch_fake, &opts, &mut no_events())
            .unwrap_err(),
    );
    assert_eq!(err.kind, SyncErrorKind::NetworkUnreachable, "{err}");
}

#[cfg(unix)]
#[test]
fn cancel_mid_fetch_kills_git_and_ssh() {
    let fx = Fx::new();
    let pidfile = fx.a.join("ssh.pid");
    let ssh = format!("echo $$ > '{}'; exec sleep 1000; :", pidfile.display());
    let (repo, opts) = with_fake_ssh(&fx, &ssh);
    let token = CancelToken::new();
    let opts = opts.with_cancel(token.clone());
    let pf = pidfile.clone();
    let canceller = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !pf.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(50));
        token.cancel();
    });
    let started = Instant::now();
    let err = repo
        .fetch(
            &FetchOptions {
                remote: Some("fake".into()),
                prune: false,
            },
            &opts,
            &mut no_events(),
        )
        .unwrap_err();
    canceller.join().unwrap();
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(15));
    let pid = std::fs::read_to_string(&pidfile).unwrap().trim().to_owned();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if stat.is_empty() || stat.starts_with('Z') {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fake ssh {pid} survived the cancel"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn branches_create_switch_rename_delete_and_upstream() {
    let fx = Fx::new();
    let repo = fx.repo(&fx.a);
    let head = git_in(&fx.a, &["rev-parse", "HEAD"]);
    assert_eq!(repo.create_branch("topic", None).unwrap().to_string(), head);
    assert_eq!(
        sync_err(repo.create_branch("topic", None).unwrap_err()).kind,
        SyncErrorKind::AlreadyExists
    );
    assert_eq!(
        sync_err(repo.create_branch("bad..name", None).unwrap_err()).kind,
        SyncErrorKind::InvalidName
    );
    assert_eq!(
        sync_err(repo.create_branch("-x", None).unwrap_err()).kind,
        SyncErrorKind::InvalidName
    );
    let sw = repo
        .switch_branch("topic", &SyncOptions::default())
        .unwrap();
    assert_eq!(sw.from.as_deref(), Some("main"));
    assert_eq!(sw.to, "topic");
    let topic_tip = fx.commit(&fx.a, "f.txt", "topic\n", "topic edit");
    repo.switch_branch("main", &SyncOptions::default()).unwrap();

    // Dirty tree that the switch would overwrite: refused, files listed, stash offered as data.
    std::fs::write(fx.a.join("f.txt"), "local edit\n").unwrap();
    let err = sync_err(
        repo.switch_branch("topic", &SyncOptions::default())
            .unwrap_err(),
    );
    assert_eq!(err.kind, SyncErrorKind::LocalChanges, "{err}");
    assert_eq!(err.paths, vec!["f.txt".to_owned()]);
    assert_eq!(err.alternatives, vec!["stash_and_switch".to_owned()]);
    assert_eq!(
        std::fs::read_to_string(fx.a.join("f.txt")).unwrap(),
        "local edit\n"
    );

    // Unmerged delete is refused; force returns what restore needs.
    repo.rename_branch("topic", "topic2").unwrap();
    assert_eq!(
        sync_err(repo.delete_branch("topic2", false).unwrap_err()).kind,
        SyncErrorKind::NotMerged
    );
    repo.set_upstream("topic2", Some("origin/main")).unwrap();
    let gone = repo.delete_branch("topic2", true).unwrap();
    assert_eq!(gone.oid.to_string(), topic_tip);
    assert_eq!(gone.upstream.as_deref(), Some("origin/main"));
    assert!(repo.branch_tip("topic2").unwrap().is_none());
    repo.restore_ref("refs/heads/topic2", None, Some(gone.oid))
        .unwrap();
    assert_eq!(repo.branch_tip("topic2").unwrap(), Some(gone.oid));
    assert_eq!(
        repo.set_upstream("topic2", None).unwrap(),
        None,
        "restored without upstream"
    );
    assert_eq!(
        repo.set_upstream("topic2", Some("origin/main")).unwrap(),
        None
    );
    assert_eq!(
        repo.set_upstream("topic2", None).unwrap().as_deref(),
        Some("origin/main")
    );
    // Deleting through restore_ref (undo of a create) checks the expected value.
    assert_eq!(
        sync_err(
            repo.restore_ref("refs/heads/topic2", None, None)
                .unwrap_err()
        )
        .kind,
        SyncErrorKind::RefChanged
    );
    repo.restore_ref("refs/heads/topic2", Some(gone.oid), None)
        .unwrap();
    assert!(repo.branch_tip("topic2").unwrap().is_none());
}

#[test]
fn stash_round_trip_and_conflicts() {
    let fx = Fx::new();
    let repo = fx.repo(&fx.a);
    assert_eq!(repo.stash_push(&StashPushOptions::default()).unwrap(), None);
    std::fs::write(fx.a.join("f.txt"), "changed\n").unwrap();
    std::fs::write(fx.a.join("new.txt"), "untracked\n").unwrap();
    let st = repo
        .stash_push(&StashPushOptions {
            message: Some("my work".into()),
            include_untracked: true,
        })
        .unwrap()
        .expect("stashed");
    assert!(st.message.contains("my work"));
    assert!(!fx.a.join("new.txt").exists());
    let mut paths = repo.stash_paths(0).unwrap();
    paths.sort();
    assert_eq!(paths, vec!["f.txt", "new.txt"]);

    let applied = repo.stash_apply(0, false).unwrap();
    assert!(applied.conflicts.is_empty() && !applied.dropped);
    assert_eq!(
        std::fs::read_to_string(fx.a.join("new.txt")).unwrap(),
        "untracked\n"
    );
    git_in(&fx.a, &["checkout", "-q", "--", "f.txt"]);
    std::fs::remove_file(fx.a.join("new.txt")).unwrap();

    let dropped = repo.stash_drop(0).unwrap();
    assert!(repo.stashes().unwrap().is_empty());
    repo.stash_store(dropped.oid, &dropped.message).unwrap();
    assert_eq!(repo.stash_index_of(dropped.oid).unwrap(), Some(0));

    // Pop onto a conflicting commit: conflicts reported, stash kept (as git does).
    fx.commit(&fx.a, "f.txt", "committed differently\n", "conflict");
    let popped = repo.stash_apply(0, true).unwrap();
    assert_eq!(popped.conflicts, vec!["f.txt".to_owned()]);
    assert!(!popped.dropped);
    assert_eq!(repo.stash_index_of(dropped.oid).unwrap(), Some(0));
}
