//! Supervision end to end: streaming, large output, cancellation and timeouts that kill
//! whole trees, handle drop, the registry. Unix tests use `sh`; Windows gets `cmd` basics.

#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use lector_process::{CancelToken, LineEnd, ProcessKind, StdinMode, Stream};
use lector_process::{ProcessCommand, ProcessError, RunOptions, Supervisor};

#[cfg(unix)]
fn sh(script: &str) -> ProcessCommand {
    ProcessCommand::new("sh").arg("-c").arg(script)
}

/// True once `pid` no longer exists (or is a zombie waiting for its new parent to reap it).
#[cfg(unix)]
fn gone(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    let stat = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    stat.is_empty() || stat.starts_with('Z')
}

#[cfg(unix)]
fn wait_gone(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if gone(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[cfg(unix)]
fn kill_pid(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
}

#[test]
fn missing_program_is_not_found() {
    let sup = Supervisor::new();
    let err = sup
        .run(
            &ProcessCommand::new("definitely-not-a-program-lector"),
            &RunOptions::default(),
        )
        .unwrap_err();
    assert!(matches!(err, ProcessError::NotFound { .. }), "{err}");
}

#[cfg(windows)]
#[test]
fn windows_runs_cmd_and_captures() {
    let sup = Supervisor::new();
    let out = sup
        .run(
            &ProcessCommand::new("cmd").args(["/C", "echo out& echo err 1>&2& exit 3"]),
            &RunOptions::default(),
        )
        .unwrap();
    assert_eq!(out.code(), Some(3));
    assert!(out.stdout_text().contains("out"));
    assert!(out.stderr_text().contains("err"));
    assert!(sup.list().is_empty());
}

#[cfg(unix)]
#[test]
fn captures_both_streams_and_exit_code_with_env_and_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let sup = Supervisor::new();
    let cmd = sh("echo \"$LECTOR_T\"; pwd; echo \"[$HOME]\"; echo err >&2; exit 3")
        .env("LECTOR_T", "hello")
        .env_remove("HOME")
        .cwd(dir.path());
    let out = sup.run(&cmd, &RunOptions::default()).unwrap();
    assert_eq!(out.code(), Some(3));
    let text = out.stdout_text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "hello");
    assert_eq!(
        std::fs::canonicalize(lines[1]).unwrap(),
        std::fs::canonicalize(dir.path()).unwrap()
    );
    assert_eq!(lines[2], "[]");
    assert_eq!(out.stderr_text(), "err\n");
    assert!(sup.list().is_empty(), "finished runs leave the registry");
}

#[cfg(unix)]
#[test]
fn streams_lines_in_arrival_order_with_progress_updates() {
    let sup = Supervisor::new();
    let mut seen = Vec::new();
    let out = sup
        .run_streaming(
            &sh("echo a; sleep 0.1; echo b >&2; sleep 0.1; printf 'p 10%%\\rp 100%%\\r\\n'; sleep 0.1; printf 'tail'"),
            &RunOptions::default(),
            &mut |line| seen.push((line.stream, line.text().into_owned(), line.end)),
        )
        .unwrap();
    assert!(out.success());
    assert_eq!(
        seen,
        vec![
            (Stream::Stdout, "a".to_owned(), LineEnd::Newline),
            (Stream::Stderr, "b".to_owned(), LineEnd::Newline),
            (Stream::Stdout, "p 10%".to_owned(), LineEnd::CarriageReturn),
            (Stream::Stdout, "p 100%".to_owned(), LineEnd::Newline),
            (Stream::Stdout, "tail".to_owned(), LineEnd::None),
        ]
    );
    // Captures are byte-exact.
    assert_eq!(out.stdout, b"a\np 10%\rp 100%\r\ntail");
}

#[cfg(unix)]
#[test]
fn large_output_on_both_pipes_does_not_deadlock() {
    let sup = Supervisor::new();
    // stderr first: a reader that drained stdout to EOF before stderr would deadlock.
    let started = Instant::now();
    let out = sup
        .run(
            &sh("head -c 3000000 /dev/zero >&2; head -c 3000000 /dev/zero; cat"),
            &RunOptions::default().with_timeout(Duration::from_secs(60)),
        )
        .unwrap();
    assert!(out.success());
    assert_eq!(out.stderr.len(), 3_000_000);
    assert_eq!(out.stdout.len(), 3_000_000);
    assert!(started.elapsed() < Duration::from_secs(60));

    // Large stdin written concurrently with large output.
    let input: Vec<u8> = (0..2_000_000u32).map(|i| (i % 251) as u8).collect();
    let out = sup
        .run(
            &ProcessCommand::new("cat").stdin(StdinMode::Bytes(input.clone())),
            &RunOptions::default(),
        )
        .unwrap();
    assert_eq!(out.stdout, input);
}

#[cfg(unix)]
#[test]
fn capture_is_bounded_but_streaming_sees_everything() {
    let sup = Supervisor::new();
    let opts = RunOptions {
        capture_limit: 10,
        ..RunOptions::default()
    };
    let mut lines = 0;
    let out = sup
        .run_streaming(&sh("seq 1 1000"), &opts, &mut |_| lines += 1)
        .unwrap();
    assert_eq!(lines, 1000);
    assert_eq!(out.stdout.len(), 10);
    assert!(out.stdout_truncated);
    assert!(!out.stderr_truncated);
}

#[cfg(unix)]
#[test]
fn cancel_kills_grandchildren() {
    let sup = Supervisor::new();
    let token = CancelToken::new();
    let mut grandchild = None;
    let started = Instant::now();
    let err = sup
        .run_streaming(
            &sh("sleep 1000 & echo $!; wait"),
            &RunOptions::default().with_cancel(token.clone()),
            &mut |line| {
                if let Ok(pid) = line.text().trim().parse::<u32>() {
                    grandchild = Some(pid);
                    token.cancel();
                }
            },
        )
        .unwrap_err();
    assert!(matches!(err, ProcessError::Cancelled), "{err}");
    assert!(started.elapsed() < Duration::from_secs(10));
    let pid = grandchild.expect("grandchild pid");
    assert!(wait_gone(pid), "grandchild {pid} survived the cancel");
    assert!(sup.list().is_empty());
}

#[cfg(unix)]
#[test]
fn a_cancelled_token_does_not_start_anything() {
    let token = CancelToken::new();
    token.cancel();
    let err = Supervisor::new()
        .run(&sh("echo never"), &RunOptions::default().with_cancel(token))
        .unwrap_err();
    assert!(matches!(err, ProcessError::Cancelled));
}

#[cfg(unix)]
#[test]
fn timeout_kills_the_tree() {
    let sup = Supervisor::new();
    let mut grandchild = None;
    let started = Instant::now();
    let err = sup
        .run_streaming(
            &sh("sleep 1000 & echo $!; wait"),
            &RunOptions::default().with_timeout(Duration::from_millis(300)),
            &mut |line| grandchild = line.text().trim().parse::<u32>().ok(),
        )
        .unwrap_err();
    assert!(matches!(err, ProcessError::TimedOut { .. }), "{err}");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(wait_gone(grandchild.expect("pid")));
}

#[cfg(unix)]
#[test]
fn terminate_is_graceful_first() {
    let sup = Supervisor::new();
    let mut child = sup
        .spawn(&sh(
            "trap 'echo bye; exit 7' TERM; echo ready; while :; do sleep 0.05; done",
        ))
        .unwrap();
    let lines = child.lines(16);
    let first = lines.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(first.text(), "ready");
    let status = child.terminate(Duration::from_secs(5)).expect("reaped");
    assert_eq!(status.code(), Some(7), "the TERM trap ran");
    // (The shell may also report the killed `sleep` on stderr.)
    let rest: Vec<String> = lines
        .iter()
        .filter(|l| l.stream == Stream::Stdout)
        .map(|l| l.text().into_owned())
        .collect();
    assert_eq!(rest, vec!["bye"]);
}

#[cfg(unix)]
#[test]
fn dropping_a_handle_kills_the_tree() {
    let sup = Supervisor::new();
    let mut child = sup
        .spawn(
            &sh("sleep 1000 & echo $!; wait")
                .label("sleeper")
                .kind(ProcessKind::LanguageServer),
        )
        .unwrap();
    let pid = child
        .lines(4)
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .text()
        .trim()
        .parse::<u32>()
        .unwrap();
    let main_pid = child.pid();
    let info = sup.list();
    assert_eq!(info.len(), 1);
    assert_eq!(info[0].label, "sleeper");
    assert_eq!(info[0].kind, ProcessKind::LanguageServer);
    assert!(info[0].running);
    drop(child);
    assert!(wait_gone(pid), "grandchild survived the drop");
    assert!(wait_gone(main_pid));
    assert!(sup.list().is_empty());
}

#[cfg(unix)]
#[test]
fn leftovers_are_killed_on_exit_unless_asked_to_survive() {
    let sup = Supervisor::new();
    let script = "sleep 1000 >/dev/null 2>&1 & echo $!";
    let out = sup.run(&sh(script), &RunOptions::default()).unwrap();
    let pid: u32 = out.stdout_text().trim().parse().unwrap();
    assert!(
        wait_gone(pid),
        "default: the tree dies with its main process"
    );

    let out = sup
        .run(&sh(script).kill_tree_on_exit(false), &RunOptions::default())
        .unwrap();
    let pid: u32 = out.stdout_text().trim().parse().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(!gone(pid), "an intentional daemon survives");
    kill_pid(pid);
    assert!(wait_gone(pid));
}

#[cfg(unix)]
#[test]
fn shutdown_kills_everything_and_refuses_new_work() {
    let sup = Supervisor::new();
    let a = sup.spawn(&sh("sleep 1000").label("a")).unwrap();
    let b = sup
        .spawn(&sh("trap '' TERM; sleep 1000").label("b"))
        .unwrap();
    let pids = [a.pid(), b.pid()];
    let mut labels: Vec<String> = sup.list().into_iter().map(|i| i.label).collect();
    labels.sort();
    assert_eq!(labels, vec!["a", "b"]);
    let started = Instant::now();
    sup.shutdown(Duration::from_millis(300));
    assert!(started.elapsed() < Duration::from_secs(5));
    for pid in pids {
        assert!(wait_gone(pid), "{pid} survived shutdown");
    }
    assert!(matches!(
        sup.spawn(&sh("true")),
        Err(ProcessError::ShuttingDown)
    ));
    drop((a, b));
    assert!(sup.list().is_empty());
}

#[cfg(unix)]
#[test]
fn killer_stops_a_process_from_another_thread() {
    let sup = Supervisor::new();
    let mut child = sup.spawn(&sh("sleep 1000")).unwrap();
    let killer = child.killer();
    let t = std::thread::spawn(move || killer.kill());
    assert!(t.join().unwrap());
    let status = child.wait_timeout(Duration::from_secs(5)).unwrap();
    assert!(status.is_some_and(|s| !s.success()));
    let id = child.id();
    assert!(!sup.kill(id + 1000));
}
