use super::*;
use crate::scheduler::{JobKey, Priority, Subject};
use lector_process::{ProcessCommand, RunOptions, Supervisor};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Instant;

const SECS5: Duration = Duration::from_secs(5);

fn key(kind: JobKind, n: u64) -> JobKey {
    JobKey::new(kind, Subject::View(n))
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn bounds(drain: Duration, grace: Duration) -> QuitBounds {
    QuitBounds {
        drain,
        network_grace: grace,
        process_grace: Duration::from_millis(300),
        scheduler: Duration::from_millis(300),
    }
}

#[cfg(unix)]
#[test]
fn quit_completes_a_queued_save_and_a_running_git_write() {
    let repo = tempfile::tempdir().expect("tempdir");
    let dir = repo.path().to_path_buf();
    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.name", "Ada"]);
    git(&dir, &["config", "user.email", "ada@example.com"]);
    std::fs::write(dir.join("a.txt"), "one\n").expect("write");
    git(&dir, &["add", "a.txt"]);
    // A slow commit: the write is still running when the app quits.
    let hook = dir.join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nsleep 1\n").expect("hook");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let supervisor = Arc::new(Supervisor::new());
    let scheduler = Scheduler::with_limits(1, 4);
    let (started_tx, started_rx) = mpsc::channel();
    let (sup, wd) = (supervisor.clone(), dir.clone());
    let commit = scheduler.spawn(key(JobKind::GitWrite, 1), Priority::Visible, move |_| {
        started_tx.send(()).ok();
        let cmd = ProcessCommand::new("git")
            .args(["commit", "-q", "-m", "slow commit"])
            .cwd(&wd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        sup.run(&cmd, &RunOptions::default()).map(|o| o.success())
    });
    started_rx.recv_timeout(SECS5).expect("commit started");
    // Queued behind the commit on the only general worker.
    let target = dir.join("saved.txt");
    let file = target.clone();
    let save = scheduler.spawn(
        key(JobKind::DocumentSave, 2),
        Priority::Visible,
        move |_| {
            let tmp = file.with_extension("tmp");
            std::fs::write(&tmp, "saved\n").and_then(|_| std::fs::rename(&tmp, &file))
        },
    );
    let read = scheduler.spawn(key(JobKind::GitLog, 3), Priority::Visible, |_| 1);

    let report = shutdown_with(&supervisor, &scheduler, bounds(SECS5 * 2, SECS5), None);

    assert!(report.abandoned.is_empty(), "{:?}", report.abandoned);
    assert!(
        matches!(commit.wait(), Some(Ok(true))),
        "the commit finished"
    );
    assert!(save.wait().is_some_and(|r| r.is_ok()), "the save ran");
    assert_eq!(read.wait(), None, "reads are cancelled");
    assert_eq!(std::fs::read_to_string(&target).expect("saved"), "saved\n");
    assert!(git(&dir, &["log", "--oneline"]).contains("slow commit"));
    assert!(!dir.join(".git/index.lock").exists(), "no torn git write");
}

#[test]
fn quit_gives_up_on_a_hung_network_write_within_its_grace_and_reports_it() {
    let crashes = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new();
    let scheduler = Scheduler::with_limits(1, 4);
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let hung = scheduler.spawn(key(JobKind::ForgeWrite, 1), Priority::Visible, move |_| {
        started_tx.send(()).ok();
        // Ignores its token, like a request stuck on the network.
        let _ = release_rx.recv_timeout(Duration::from_secs(30));
    });
    started_rx.recv_timeout(SECS5).expect("started");
    let started = Instant::now();
    let report = shutdown_with(
        &supervisor,
        &scheduler,
        bounds(Duration::from_secs(10), Duration::from_millis(300)),
        Some(crashes.path()),
    );
    let took = started.elapsed();
    assert!(took < Duration::from_secs(2), "{took:?}");
    assert_eq!(report.abandoned, vec![JobKind::ForgeWrite]);
    let reports = crate::health::report_files(crashes.path());
    assert_eq!(reports.len(), 1);
    let text = std::fs::read_to_string(&reports[0]).expect("report");
    assert!(text.contains("ForgeWrite"), "{text}");
    release_tx.send(()).ok();
    assert_eq!(
        hung.wait_outcome(),
        Err(crate::scheduler::JobError::Cancelled)
    );
}

#[test]
fn reads_are_cancelled_at_once_while_writes_drain() {
    let supervisor = Supervisor::new();
    let scheduler = Scheduler::with_limits(2, 4);
    let (tx, rx) = mpsc::channel();
    let read_cancelled_at = Arc::new(parking_lot::Mutex::new(None::<Instant>));
    let seen = read_cancelled_at.clone();
    let tx2 = tx.clone();
    let read = scheduler.spawn(
        key(JobKind::ProjectSearch, 1),
        Priority::Visible,
        move |tok| {
            tx2.send(()).ok();
            while !tok.is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            *seen.lock() = Some(Instant::now());
        },
    );
    let wrote = Arc::new(AtomicBool::new(false));
    let w = wrote.clone();
    let write = scheduler.spawn(key(JobKind::GitWrite, 2), Priority::Visible, move |tok| {
        tx.send(()).ok();
        std::thread::sleep(Duration::from_millis(500));
        // A write does not stop for its token at quit.
        let _ = tok;
        w.store(true, Ordering::SeqCst);
    });
    rx.recv_timeout(SECS5).expect("one started");
    rx.recv_timeout(SECS5).expect("both started");
    let started = Instant::now();
    let report = shutdown_with(&supervisor, &scheduler, bounds(SECS5, SECS5), None);
    let cancelled_after = read_cancelled_at
        .lock()
        .expect("the read saw its cancellation")
        .duration_since(started);
    assert!(
        cancelled_after < Duration::from_millis(100),
        "{cancelled_after:?}"
    );
    assert!(
        wrote.load(Ordering::SeqCst),
        "the write finished before quit returned"
    );
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert!(report.abandoned.is_empty());
    assert!(report.cancelled >= 1);
    assert_eq!(read.wait(), None);
    assert_eq!(write.wait(), Some(()));
}

#[test]
fn quits_promptly_with_reads_queued_and_running() {
    let supervisor = Supervisor::new();
    let scheduler = Scheduler::with_limits(1, 2);
    let (tx, rx) = mpsc::channel();
    let running = scheduler.spawn(
        key(JobKind::GitLog, 1),
        Priority::Background,
        move |cancel| {
            tx.send(()).ok();
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
        },
    );
    rx.recv_timeout(SECS5).ok();
    let queued: Vec<_> = (0..50)
        .map(|i| {
            scheduler.spawn(
                JobKey::new(JobKind::DocumentHighlight, Subject::Document(i)),
                Priority::Idle,
                |_| (),
            )
        })
        .collect();
    let started = Instant::now();
    let report = shutdown_with(&supervisor, &scheduler, bounds(SECS5, SECS5), None);
    assert!(report.scheduler_stopped);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(running.wait().is_none());
    assert!(queued.into_iter().all(|h| h.wait().is_none()));
    assert!(supervisor.is_shutting_down());
    // After quit began, a read is refused at once.
    assert_eq!(
        scheduler
            .spawn(key(JobKind::GitLog, 9), Priority::Visible, |_| 1)
            .wait(),
        None
    );
}
