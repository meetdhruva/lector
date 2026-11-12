//! Tests for the health log: summary math, markers, stale and reused-pid sessions, concurrency
//! (threads and real processes) and the panic hook in a child process.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::process::ProcessIdentity;
use super::session::{trim_log_for_tests, write_record};
use super::*;

/// A pid no process has (above every platform's pid range).
const DEAD_PID: u32 = 4_000_000_000;

fn paths(dir: &Path) -> HealthPaths {
    HealthPaths::in_dir(dir)
}

fn dead_identity() -> ProcessIdentity {
    ProcessIdentity {
        pid: DEAD_PID,
        start_token: Some("1".into()),
        exe: "lector".into(),
    }
}

fn record(id: &str, state: SessionState, who: &ProcessIdentity, version: &str) -> SessionRecord {
    SessionRecord {
        schema: 1,
        id: id.into(),
        pid: who.pid,
        start_token: who.start_token.clone(),
        exe: who.exe.clone(),
        version: version.into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        started: 1_700_000_000,
        state,
        ended: (state == SessionState::Clean).then_some(1_700_000_100),
        panics: 0,
    }
}

fn put(p: &HealthPaths, rec: &SessionRecord) -> PathBuf {
    fs::create_dir_all(&p.sessions).expect("mkdir");
    let path = p.sessions.join(format!("{}.json", rec.id));
    write_record(&path, rec).expect("write");
    path
}

#[test]
fn timestamps() {
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
    assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
}

#[test]
fn an_empty_log_has_no_rate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let s = health_summary_in(&paths(dir.path()));
    assert_eq!((s.sessions, s.clean, s.abnormal, s.panics), (0, 0, 0, 0));
    assert_eq!(s.crash_free_rate, None);
    assert!(s.last_crashes.is_empty() && s.by_version.is_empty());
}

#[test]
fn summary_math() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = paths(dir.path());
    let me = ProcessIdentity::current();
    // 0.6.0: three clean, one abnormal (dead process). 0.7.0: one clean, one frozen abnormal
    // (marker), one still running (this process), with a few panics.
    for i in 0..3 {
        put(
            &p,
            &record(&format!("a{i}"), SessionState::Clean, &me, "0.6.0"),
        );
    }
    put(
        &p,
        &record("a3", SessionState::Running, &dead_identity(), "0.6.0"),
    );
    let mut clean = record("b0", SessionState::Clean, &me, "0.7.0");
    clean.panics = 2;
    put(&p, &clean);
    let frozen = put(&p, &record("b1", SessionState::Running, &me, "0.7.0"));
    fs::write(format!("{}.abnormal", frozen.display()), "").expect("marker");
    let mut running = record("b2", SessionState::Running, &me, "0.7.0");
    running.started += 50;
    running.panics = 1;
    put(&p, &running);
    // Unreadable records and stray files are ignored.
    fs::write(p.sessions.join("junk.json"), "{ nope").expect("write");
    fs::write(p.sessions.join("notes.txt"), "hello").expect("write");

    let s = health_summary_in(&p);
    assert_eq!(s.sessions, 7);
    assert_eq!((s.clean, s.abnormal, s.running), (4, 2, 1));
    assert_eq!(s.panics, 3);
    assert_eq!(s.crash_free_rate, Some(4.0 / 6.0));
    assert_eq!(s.by_version.len(), 2);
    // Newest first.
    assert_eq!(s.by_version[0].version, "0.7.0");
    assert_eq!(
        (
            s.by_version[0].sessions,
            s.by_version[0].clean,
            s.by_version[0].abnormal
        ),
        (3, 1, 1)
    );
    assert_eq!(s.by_version[0].panics, 3);
    assert_eq!(
        (
            s.by_version[1].sessions,
            s.by_version[1].clean,
            s.by_version[1].abnormal
        ),
        (4, 3, 1)
    );
}

#[test]
fn begin_and_end_clean_flip_the_marker() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = paths(dir.path());
    let session = SessionHealth::begin_in(p.clone(), ProcessIdentity::current(), "0.7.0");
    let s = health_summary_in(&p);
    assert_eq!((s.sessions, s.running, s.clean, s.abnormal), (1, 1, 0, 0));
    assert_eq!(
        s.crash_free_rate, None,
        "a running session is not a verdict"
    );

    session.end_clean();
    session.end_clean(); // idempotent
    let s = health_summary_in(&p);
    assert_eq!((s.sessions, s.running, s.clean, s.abnormal), (1, 0, 1, 0));
    assert_eq!(s.crash_free_rate, Some(1.0));
    let stored = session::read_records(&p.sessions);
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].0.id, session.id());
    assert!(stored[0].0.ended.is_some());
    // Nothing but the record is left in the directory.
    assert_eq!(fs::read_dir(&p.sessions).expect("dir").count(), 1);
}

#[test]
fn a_leftover_marker_is_frozen_as_abnormal_at_the_next_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = paths(dir.path());
    let stale = put(
        &p,
        &record("old", SessionState::Running, &dead_identity(), "0.6.0"),
    );
    assert!(!PathBuf::from(format!("{}.abnormal", stale.display())).exists());

    let _next = SessionHealth::begin_in(p.clone(), ProcessIdentity::current(), "0.7.0");
    assert!(PathBuf::from(format!("{}.abnormal", stale.display())).exists());
    let s = health_summary_in(&p);
    assert_eq!((s.sessions, s.abnormal, s.running, s.clean), (2, 1, 1, 0));

    // Even if the OS hands the dead session's pid to a live process later, the verdict stays.
    let me = ProcessIdentity::current();
    put(&p, &record("old", SessionState::Running, &me, "0.6.0"));
    assert_eq!(health_summary_in(&p).abnormal, 1);
}

#[test]
fn a_running_marker_of_a_live_process_is_left_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = paths(dir.path());
    let other_window = SessionHealth::begin_in(p.clone(), ProcessIdentity::current(), "0.7.0");
    let _second = SessionHealth::begin_in(p.clone(), ProcessIdentity::current(), "0.7.0");
    let s = health_summary_in(&p);
    assert_eq!((s.sessions, s.running, s.abnormal), (2, 2, 0));
    other_window.end_clean();
    let s = health_summary_in(&p);
    assert_eq!((s.running, s.clean, s.abnormal), (1, 1, 0));
}

#[test]
fn a_clean_record_wins_over_a_late_abnormal_marker() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = paths(dir.path());
    let path = put(
        &p,
        &record("c", SessionState::Clean, &dead_identity(), "0.7.0"),
    );
    // Another process judged it dead a moment before it finished writing "clean".
    fs::write(format!("{}.abnormal", path.display()), "").expect("marker");
    let s = health_summary_in(&p);
    assert_eq!((s.clean, s.abnormal), (1, 0));
}

#[cfg(target_os = "linux")]
#[test]
fn a_reused_pid_does_not_resurrect_a_dead_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = paths(dir.path());
    let mut reused = ProcessIdentity::current();
    reused.start_token = Some(format!("{}0", reused.start_token.unwrap_or_default()));
    put(&p, &record("r", SessionState::Running, &reused, "0.7.0"));
    let s = health_summary_in(&p);
    assert_eq!((s.running, s.abnormal), (0, 1));
}

#[test]
fn the_log_is_rolling_and_never_drops_live_sessions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = paths(dir.path());
    let me = ProcessIdentity::current();
    // The oldest record belongs to a live process; the others are finished.
    put(&p, &record("000", SessionState::Running, &me, "0.7.0"));
    for i in 1..12 {
        let path = put(
            &p,
            &record(&format!("{i:03}"), SessionState::Clean, &me, "0.7.0"),
        );
        if i % 4 == 0 {
            fs::write(format!("{}.abnormal", path.display()), "").expect("marker");
        }
    }
    trim_log_for_tests(&p.sessions, 5);
    let ids: Vec<String> = session::read_records(&p.sessions)
        .into_iter()
        .map(|(r, _)| r.id)
        .collect();
    // 12 records, keep 5: the 7 oldest are candidates, the live one (000) survives.
    assert_eq!(ids, ["000", "007", "008", "009", "010", "011"]);
    // Markers of deleted records are gone too.
    let leftovers = fs::read_dir(&p.sessions)
        .expect("dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".abnormal"))
        .count();
    assert_eq!(leftovers, 1, "only 008 keeps its marker");
}

#[test]
fn many_threads_beginning_and_ending_do_not_corrupt_the_log() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = paths(dir.path());
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let p = p.clone();
            scope.spawn(move || {
                for _ in 0..10 {
                    let s = SessionHealth::begin_in(p.clone(), ProcessIdentity::current(), "0.7.0");
                    s.end_clean();
                }
            });
        }
    });
    let s = health_summary_in(&p);
    assert_eq!(s.sessions, 80);
    assert_eq!((s.clean, s.abnormal, s.running), (80, 0, 0));
    assert_eq!(s.crash_free_rate, Some(1.0));
    // No temporary files and no markers.
    let names: Vec<String> = fs::read_dir(&p.sessions)
        .expect("dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 80, "{names:?}");
    assert!(names
        .iter()
        .all(|n| n.ends_with(".json") && !n.starts_with('.')));
}

// ---- real processes -------------------------------------------------------------------

const CHILD_ENV: &str = "LECTOR_HEALTH_TEST_CHILD";

/// Not a test of its own: the body of the child processes the tests below spawn (this test
/// binary re-run with `CHILD_ENV` set). Does nothing in a normal run.
#[test]
fn child_process_entry() {
    let Ok(mode) = std::env::var(CHILD_ENV) else {
        return;
    };
    install_panic_hook();
    let session = SessionHealth::begin();
    match mode.as_str() {
        "clean" => {
            std::thread::sleep(std::time::Duration::from_millis(50));
            session.end_clean();
        }
        // Killed: the process ends without end_clean.
        "killed" => std::process::exit(7),
        "panic" => panic!("child boom while reading /home/zed/secret/project.rs"),
        other => panic!("unknown child mode {other}"),
    }
}

fn spawn_child(mode: &str, data_dir: &Path) -> std::process::Child {
    let exe = std::env::current_exe().expect("current exe");
    Command::new(exe)
        .args([
            "--exact",
            "health::tests::child_process_entry",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, mode)
        .env("LECTOR_DATA_DIR", data_dir)
        .env("LECTOR_CONFIG_DIR", data_dir.join("config"))
        .env_remove("RUST_BACKTRACE")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child")
}

#[test]
fn concurrent_processes_are_counted_exactly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data = dunce::canonicalize(dir.path()).expect("canonical");
    let modes = ["clean", "clean", "clean", "killed", "killed", "panic"];
    let mut children: Vec<_> = modes.iter().map(|m| spawn_child(m, &data)).collect();
    for child in &mut children {
        child.wait().expect("wait");
    }

    let p = HealthPaths::in_dir(&data);
    // Before anyone looks, the summary already tells the dead from the living by itself.
    let before = health_summary_in(&p);
    assert_eq!(
        (
            before.sessions,
            before.clean,
            before.abnormal,
            before.running
        ),
        (6, 3, 3, 0)
    );

    // The next start freezes the verdicts; the numbers do not change, nor do they double count.
    let next = SessionHealth::begin_in(p.clone(), ProcessIdentity::current(), "0.7.0");
    let after = health_summary_in(&p);
    assert_eq!(
        (after.sessions, after.clean, after.abnormal, after.running),
        (7, 3, 3, 1)
    );
    assert_eq!(after.panics, 1, "the panicking child recorded its panic");
    assert_eq!(after.crash_free_rate, Some(0.5));
    next.end_clean();
    assert_eq!(health_summary_in(&p).crash_free_rate, Some(4.0 / 7.0));

    // The panic left one well-formed, redacted report.
    assert_eq!(after.crash_reports, 1);
    let crash = &after.last_crashes[0];
    // The harness runs the test on a thread named after it, or on `main` with one test thread.
    let thread = crash.thread.as_deref().unwrap_or_default();
    assert!(
        thread == "main" || thread.ends_with("child_process_entry"),
        "{thread}"
    );
    assert_eq!(crash.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    let message = crash.message.as_deref().unwrap_or_default();
    assert_eq!(message, "child boom while reading ~/secret/project.rs");
    let text = fs::read_to_string(&crash.path).expect("report");
    assert!(!text.contains("zed"), "{text}");
    assert!(text.contains("\nbacktrace:\n"));
    assert!(text.contains("\nsession: "));
}

#[test]
fn the_hook_survives_an_unwritable_data_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    // The data "directory" is a file: nothing under it can be created.
    let blocker = dir.path().join("data");
    fs::write(&blocker, "not a directory").expect("write");
    let status = spawn_child("panic", &blocker).wait().expect("wait");
    // The test harness reports its failed test normally (exit code 101); an abort or a crash
    // inside the hook would show another code or no code at all.
    assert_eq!(status.code(), Some(101));
}

#[test]
fn the_previous_session_is_abnormal_only_when_it_was_the_last_and_ended_badly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = paths(dir.path());
    let dead = dead_identity();
    put(&p, &record("001-a", SessionState::Clean, &dead, "0.7.0"));
    put(&p, &record("002-b", SessionState::Running, &dead, "0.7.0"));
    put(
        &p,
        &record("003-current", SessionState::Running, &dead, "0.7.0"),
    );
    // The latest earlier session crashed (its process is gone).
    let crashed = previous_session_abnormal(&p, "003-current").expect("abnormal");
    assert_eq!(crashed.id, "002-b");
    // A later clean session hides the older crash: only the previous run counts.
    put(&p, &record("002-b", SessionState::Clean, &dead, "0.7.0"));
    assert!(previous_session_abnormal(&p, "003-current").is_none());
    // Nothing earlier: no notice.
    assert!(previous_session_abnormal(&p, "000-first").is_none());
}
