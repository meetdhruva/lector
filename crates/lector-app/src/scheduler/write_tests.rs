//! Writes the user asked for (ADR 0010 items 4 and 5), the serial lane, quit policy of undo,
//! lock discipline and the guaranteed lane's growth.

use super::*;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

const SECS5: Duration = Duration::from_secs(5);

fn key(kind: JobKind, n: u64) -> JobKey {
    JobKey::new(kind, Subject::View(n))
}

/// Occupies every general worker with a job that waits for its release; returns the senders.
fn block_general(s: &Scheduler) -> Vec<mpsc::Sender<()>> {
    let (started_tx, started_rx) = mpsc::channel();
    let mut senders = Vec::new();
    for i in 0..s.workers() {
        let (tx, rx) = mpsc::channel::<()>();
        let started = started_tx.clone();
        let _ = s.spawn(
            key(JobKind::DocumentHighlight, 1000 + i as u64),
            Priority::Idle,
            move |_| {
                let _ = started.send(());
                let _ = rx.recv_timeout(Duration::from_secs(30));
            },
        );
        senders.push(tx);
    }
    for _ in 0..s.workers() {
        started_rx
            .recv_timeout(SECS5)
            .expect("a general worker started");
    }
    senders
}

/// The audit's reproduction, kept: a write queued behind busy workers whose task was dropped
/// (its token cancelled, as the bridge did) still runs.
#[test]
fn a_queued_write_whose_task_was_dropped_still_runs() {
    let s = Scheduler::new(1);
    let release = block_general(&s);
    let wrote = Arc::new(AtomicBool::new(false));
    let w = wrote.clone();
    let write = s.spawn(
        JobKey::new(JobKind::GitWrite, Subject::repo("/r")),
        Priority::Visible,
        move |_| w.store(true, Ordering::SeqCst),
    );
    let read_ran = Arc::new(AtomicBool::new(false));
    let r = read_ran.clone();
    let read = s.spawn(key(JobKind::GitLog, 2), Priority::Visible, move |_| {
        r.store(true, Ordering::SeqCst)
    });
    // What dropping a `BackgroundTask` did before ADR 0010 item 4.
    write.cancel();
    read.cancel();
    for r in release {
        r.send(()).ok();
    }
    assert!(s.wait_idle(SECS5));
    assert!(wrote.load(Ordering::SeqCst), "the user's write never ran");
    assert!(
        !read_ran.load(Ordering::SeqCst),
        "a cancelled read is skipped"
    );
    assert_eq!(
        write.wait_outcome(),
        Err(JobError::Cancelled),
        "its result is dropped"
    );
}

/// `cancel_key` is the one way to cancel a write, and it only works before the write starts:
/// a running write runs to completion and reports its result.
#[test]
fn cancel_key_stops_a_queued_write_but_not_a_running_one() {
    let s = Scheduler::new(1);
    let k = JobKey::new(JobKind::DocumentSave, Subject::Document(1));
    let (started_tx, started_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let running = s.spawn(k.clone(), Priority::Visible, move |_| {
        started_tx.send(()).ok();
        go_rx.recv_timeout(SECS5).ok();
        "saved"
    });
    started_rx.recv_timeout(SECS5).expect("started");
    let ran = Arc::new(AtomicBool::new(false));
    let r = ran.clone();
    let queued = s.spawn(k.clone(), Priority::Visible, move |_| {
        r.store(true, Ordering::SeqCst)
    });
    s.cancel_key(&k);
    go_tx.send(()).ok();
    assert_eq!(running.wait_outcome(), Ok("saved"));
    assert_eq!(queued.wait_outcome(), Err(JobError::Cancelled));
    assert!(!ran.load(Ordering::SeqCst));
}

/// "Is a write" is one predicate; every policy that depends on it agrees with it.
#[test]
fn the_five_write_policies_agree_for_every_kind() {
    for &kind in JobKind::ALL {
        let write = kind.is_write();
        assert!(!(write && kind.replaces()), "{kind:?}: a write replaces");
        assert_eq!(kind.cancelled_with_subject(), !write, "{kind:?}: subject");
        assert_eq!(kind.detaches_on_drop(), write, "{kind:?}: drop");
        assert_eq!(kind.at_quit() != AtQuit::Cancel, write, "{kind:?}: quit");
        assert_eq!(
            kind.runs_once_queued(),
            write,
            "{kind:?}: skip if cancelled"
        );
    }
}

#[cfg(unix)]
fn git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository with an identity and one commit, and `hook` as its executable `name` hook.
#[cfg(unix)]
fn repo_with_hook(name: &str, hook: &str) -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Ada"]);
    git(root, &["config", "user.email", "ada@example.com"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    git(root, &["commit", "-q", "--allow-empty", "-m", "initial"]);
    let path = root.join(".git/hooks").join(name);
    std::fs::write(&path, hook).expect("hook");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    dir
}

#[cfg(unix)]
fn commit(dir: &std::path::Path, message: &str) -> bool {
    std::process::Command::new("git")
        .args(["commit", "-q", "--allow-empty", "-m", message])
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .is_ok_and(|s| s.success())
}

/// Two repositories committing through a slow pre-commit hook hold no compute worker: an
/// interactive job and a document open on a two-worker pool run at once.
#[cfg(unix)]
#[test]
fn slow_commit_hooks_in_two_repositories_do_not_delay_compute_work() {
    let s = Scheduler::with_limits(2, BLOCKING_MAX);
    let marker = tempfile::tempdir().expect("tempdir");
    let hook = format!(
        "#!/bin/sh\ntouch \"{}/started-$$\"\nsleep 2\n",
        marker.path().display()
    );
    let repos = [
        repo_with_hook("pre-commit", &hook),
        repo_with_hook("pre-commit", &hook),
    ];
    let commits: Vec<_> = repos
        .iter()
        .map(|r| {
            let dir = r.path().to_path_buf();
            s.spawn(
                JobKey::new(JobKind::GitWrite, Subject::repo(&dir)),
                Priority::Visible,
                move |_| commit(&dir, "slow"),
            )
        })
        .collect();
    let end = Instant::now() + SECS5;
    while std::fs::read_dir(marker.path()).map_or(0, |d| d.count()) < 2 {
        assert!(Instant::now() < end, "both hooks started");
        thread::sleep(Duration::from_millis(5));
    }
    let started = Instant::now();
    let open = s.spawn(
        JobKey::new(JobKind::DocumentOpen, Subject::Document(1)),
        Priority::Visible,
        |_| "opened",
    );
    let rank = s.spawn(key(JobKind::PickerRank, 1), Priority::Interactive, |_| {
        "ranked"
    });
    assert_eq!(open.wait(), Some("opened"));
    assert_eq!(rank.wait(), Some("ranked"));
    let took = started.elapsed();
    assert!(
        took < Duration::from_millis(500),
        "compute work waited {took:?}"
    );
    for c in commits {
        assert_eq!(c.wait(), Some(true), "the commit succeeded");
    }
}

/// Writes to one repository run one at a time in submission order: the hook detects an
/// overlap with a lock file and records the order through the commit message.
#[cfg(unix)]
#[test]
fn writes_to_one_repository_run_in_order_and_never_overlap() {
    let s = Scheduler::with_limits(4, BLOCKING_MAX);
    let hook = "#!/bin/sh\n\
        d=\"$(git rev-parse --git-dir)\"\n\
        if [ -e \"$d/hook.lock\" ]; then echo overlap >> \"$d/hook.log\"; fi\n\
        touch \"$d/hook.lock\"\n\
        sleep 0.2\n\
        head -n1 \"$1\" >> \"$d/hook.log\"\n\
        rm -f \"$d/hook.lock\"\n";
    let repo = repo_with_hook("commit-msg", hook);
    let dir = repo.path().to_path_buf();
    let writes: Vec<_> = (0..4)
        .map(|i| {
            let dir = dir.clone();
            s.spawn(
                JobKey::new(JobKind::GitWrite, Subject::repo(&dir)),
                Priority::Visible,
                move |_| commit(&dir, &format!("write {i}")),
            )
        })
        .collect();
    for w in writes {
        assert_eq!(w.wait(), Some(true), "every commit succeeded");
    }
    let log = std::fs::read_to_string(dir.join(".git/hook.log")).expect("hook log");
    assert_eq!(log, "write 0\nwrite 1\nwrite 2\nwrite 3\n");
}

/// Git writes are not bounded by the blocking lane: a full blocking lane does not delay them,
/// and they do not take its slots.
#[test]
fn git_writes_neither_wait_for_nor_take_blocking_slots() {
    let s = Scheduler::with_limits(1, 2);
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = Arc::new(parking_lot::Mutex::new(go_rx));
    let mut blocked = Vec::new();
    for i in 0..2 {
        let rx = go_rx.clone();
        blocked.push(
            s.spawn(key(JobKind::ForgeRead, i), Priority::Visible, move |_| {
                let _ = rx.lock().recv_timeout(SECS5);
            }),
        );
    }
    let _general = block_general(&s);
    let w = s.spawn(
        JobKey::new(JobKind::GitWrite, Subject::repo("/a")),
        Priority::Visible,
        |_| thread::current().name().map(str::to_owned),
    );
    let name = w.wait().flatten().expect("ran on a named thread");
    assert!(name.contains("serial"), "{name}");
    for _ in 0..2 {
        go_tx.send(()).ok();
    }
}

/// Undoing an agent turn restores files: it is drained fully at quit, while cancelling a turn
/// keeps its brief grace.
#[test]
fn undoing_a_turn_drains_fully_at_quit_and_a_turn_cancel_only_briefly() {
    let s = Scheduler::new(1);
    let (started_tx, started_rx) = mpsc::channel();
    let restored = Arc::new(AtomicBool::new(false));
    let r = restored.clone();
    let st = started_tx.clone();
    let undo = s.spawn(key(JobKind::AgentUndo, 1), Priority::Visible, move |_| {
        st.send(()).ok();
        thread::sleep(Duration::from_millis(600));
        r.store(true, Ordering::SeqCst);
    });
    let control = s.spawn(
        key(JobKind::AgentControl, 1),
        Priority::Visible,
        move |_| {
            started_tx.send(()).ok();
            thread::sleep(Duration::from_millis(1500));
        },
    );
    started_rx.recv_timeout(SECS5).expect("started");
    started_rx.recv_timeout(SECS5).expect("started");
    s.begin_quit();
    let report = s.drain(SECS5, Duration::from_millis(100));
    assert!(restored.load(Ordering::SeqCst), "the restore completed");
    let kinds: Vec<JobKind> = report.abandoned.iter().map(|a| a.kind).collect();
    assert_eq!(kinds, vec![JobKind::AgentControl]);
    assert_eq!(undo.wait(), Some(()));
    assert_eq!(control.wait(), None);
}

/// The abandoned-write report names what the write was and where.
#[test]
fn an_abandoned_write_is_reported_with_its_description_and_repository() {
    let s = Scheduler::new(1);
    let (started_tx, started_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let _w = s.spawn_described(
        JobKey::new(JobKind::GitWrite, Subject::repo("/work/repo")),
        Priority::Visible,
        "git.commit",
        move |_| {
            started_tx.send(()).ok();
            go_rx.recv_timeout(SECS5).ok();
        },
    );
    started_rx.recv_timeout(SECS5).expect("started");
    s.begin_quit();
    let report = s.drain(Duration::from_millis(100), Duration::from_millis(50));
    go_tx.send(()).ok();
    assert_eq!(report.abandoned.len(), 1);
    let a = &report.abandoned[0];
    assert_eq!(a.kind, JobKind::GitWrite);
    assert_eq!(a.what.as_deref(), Some("git.commit"));
    assert!(a.running);
    let text = a.to_string();
    assert!(
        text.contains("git.commit") && text.contains("/work/repo"),
        "{text}"
    );
}

/// A cancel hook that submits a job runs outside the scheduler lock (it used to run under it
/// when a newer job replaced an older one, and deadlocked).
#[test]
fn a_cancel_hook_that_submits_a_job_does_not_deadlock() {
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        let s = Scheduler::new(1);
        let release = block_general(&s);
        let k = JobKey::new(JobKind::LspHover, Subject::Document(1));
        let old = s.spawn(k.clone(), Priority::Visible, |_| ());
        let s2 = s.clone();
        let hook_job = Arc::new(parking_lot::Mutex::new(None));
        let slot = hook_job.clone();
        old.token().on_cancel(move || {
            *slot.lock() = Some(s2.spawn(key(JobKind::GitLog, 9), Priority::Visible, |_| 7));
        });
        // Replaces `old`: its hook runs.
        let new = s.spawn(k, Priority::Visible, |_| ());
        for r in release {
            r.send(()).ok();
        }
        let from_hook = hook_job.lock().take().and_then(|h| h.wait());
        done_tx.send((new.wait(), from_hook)).ok();
    });
    let got = done_rx.recv_timeout(SECS5).expect("no deadlock");
    assert_eq!(got, (Some(()), Some(7)));
}

/// 200 guaranteed-lane jobs that each wait until all have started: all start, and their
/// threads exit once idle.
#[test]
fn two_hundred_guaranteed_jobs_all_start_and_their_threads_are_reaped() {
    let s = Scheduler::with_idle_timeout(1, BLOCKING_MAX, Duration::from_millis(200));
    let base = s.inner().threads_alive();
    let n = 200;
    let started = Arc::new(AtomicUsize::new(0));
    let handles: Vec<_> = (0..n)
        .map(|i| {
            let started = started.clone();
            s.spawn(
                key(JobKind::AgentRequest, i),
                Priority::Visible,
                move |_| {
                    started.fetch_add(1, Ordering::SeqCst);
                    let end = Instant::now() + SECS5;
                    while started.load(Ordering::SeqCst) < n as usize && Instant::now() < end {
                        thread::sleep(Duration::from_millis(1));
                    }
                    started.load(Ordering::SeqCst)
                },
            )
        })
        .collect();
    for h in handles {
        assert_eq!(
            h.wait(),
            Some(n as usize),
            "every job saw all the others start"
        );
    }
    let end = Instant::now() + SECS5;
    while s.inner().threads_alive() > base {
        assert!(
            Instant::now() < end,
            "{} threads left",
            s.inner().threads_alive()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// The debug check of the lock discipline: entering user code with the scheduler lock held
/// panics (instead of deadlocking later), and the count recovers after the unwind.
#[cfg(debug_assertions)]
#[test]
fn entering_user_code_with_the_scheduler_lock_held_is_caught() {
    let s = Scheduler::new(1);
    let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _locked = s.inner().lock();
        pool::assert_unlocked("running a test hook");
    }));
    assert!(caught.is_err());
    pool::assert_unlocked("after the unwind");
    // A cancel hook that would run under the lock is caught the same way.
    let ticket = ticket::Ticket::new(false);
    let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _locked = s.inner().lock();
        ticket.revoke();
    }));
    assert!(caught.is_err());
}
