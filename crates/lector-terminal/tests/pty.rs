//! PTY integration tests: real shells in real pseudo terminals.
//!
//! Timing-robust: every wait polls with a generous deadline, never a fixed sleep.

use std::time::{Duration, Instant};

use lector_terminal::{Emulator, ShellProfile, SpawnOptions, TermSize, Terminal, TerminalEvent};

const DEADLINE: Duration = Duration::from_secs(30);
/// How often the flood test re-sends its marker command (override to exercise the path).
const RESEND_MS: u64 = 2000;
/// The flood tests move megabytes through the PTY; slow CI runners need more time.
const FLOOD_DEADLINE: Duration = Duration::from_secs(180);

fn wait_until(term: &Terminal, what: &str, pred: impl FnMut(&mut Emulator) -> bool) {
    wait_until_for(term, what, DEADLINE, pred);
}

fn wait_until_for(
    term: &Terminal,
    what: &str,
    deadline: Duration,
    mut pred: impl FnMut(&mut Emulator) -> bool,
) {
    let start = Instant::now();
    loop {
        if pred(&mut term.lock()) {
            return;
        }
        if start.elapsed() > deadline {
            let screen = term.lock().screen_text();
            panic!("timed out waiting for {what}; screen:\n{screen}");
        }
        let _ = term.recv_event_timeout(Duration::from_millis(50));
        let _ = term.snapshot();
    }
}

fn wait_event(
    term: &Terminal,
    what: &str,
    mut pred: impl FnMut(&TerminalEvent) -> bool,
) -> TerminalEvent {
    let start = Instant::now();
    loop {
        if let Some(e) = term.recv_event_timeout(Duration::from_millis(50)) {
            if pred(&e) {
                return e;
            }
            if e == TerminalEvent::Wakeup {
                let _ = term.snapshot();
            }
        }
        if start.elapsed() > DEADLINE {
            let screen = term.lock().screen_text();
            panic!("timed out waiting for {what}; screen:\n{screen}");
        }
    }
}

/// Text of a line without leading `$ ` prompts (output of type-ahead can share the prompt
/// line, and type-ahead that was already echoed leaves several prompts in a row).
fn strip_prompt(line: &str) -> &str {
    let mut t = line.trim();
    while let Some(rest) = t.strip_prefix("$ ") {
        t = rest.trim_start();
    }
    t.trim()
}

/// Some line of the buffer (screen or scrollback) equals `want` after trimming.
fn has_line(e: &Emulator, want: &str) -> bool {
    let top = -(e.history_size() as i32);
    (top..i32::from(e.size().rows)).any(|l| strip_prompt(&e.line_text(l)) == want)
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn sh_options() -> SpawnOptions {
        let mut profile = ShellProfile::new("/bin/sh");
        profile.env.push(("PS1".into(), "$ ".into()));
        profile.env.push(("ENV".into(), String::new()));
        let mut o = SpawnOptions::new(profile);
        o.size = TermSize::new(80, 24);
        o.cwd = Some(std::env::temp_dir());
        o
    }

    fn spawn_sh() -> Terminal {
        let term = Terminal::spawn(sh_options(), None).unwrap_or_else(|e| panic!("spawn: {e}"));
        wait_until(&term, "prompt", |e| e.screen_text().contains('$'));
        term
    }

    fn alive(pid: i32) -> bool {
        // `ps` prints nothing for a missing pid and `Z` for a zombie.
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output();
        match out {
            Ok(o) => {
                let s = String::from_utf8_lossy(&o.stdout);
                let s = s.trim();
                !s.is_empty() && !s.starts_with('Z')
            }
            Err(_) => false,
        }
    }

    fn pid_from_screen(e: &Emulator, tag: &str) -> Option<i32> {
        let top = -(e.history_size() as i32);
        (top..i32::from(e.size().rows))
            .filter_map(|l| {
                strip_prompt(&e.line_text(l))
                    .strip_prefix(tag)
                    .and_then(|s| s.trim().parse().ok())
            })
            .next()
    }

    #[test]
    fn echo_hello() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let w = wakes.clone();
        let term = Terminal::spawn(
            sh_options(),
            Some(Arc::new(move || {
                w.fetch_add(1, Ordering::Relaxed);
            })),
        )
        .unwrap_or_else(|e| panic!("spawn: {e}"));
        term.send_text("echo hel''lo\r");
        wait_until(&term, "hello", |e| has_line(e, "hello"));
        // The wake callback runs on the parser thread right after the content changed.
        let start = Instant::now();
        while wakes.load(Ordering::Relaxed) == 0 {
            assert!(start.elapsed() < DEADLINE, "wake callback never ran");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(term.is_running());
    }

    #[test]
    fn resize_is_seen_by_child() {
        let term = spawn_sh();
        term.resize(TermSize::new(101, 31))
            .unwrap_or_else(|e| panic!("resize: {e}"));
        term.send_text("stty size\r");
        wait_until(&term, "stty size", |e| has_line(e, "31 101"));
        assert_eq!(term.snapshot().size, TermSize::new(101, 31));
    }

    #[test]
    fn exit_status_is_reported() {
        let term = spawn_sh();
        term.send_text("exit 3\r");
        let e = wait_event(&term, "exit", |e| matches!(e, TerminalEvent::Exited(_)));
        let TerminalEvent::Exited(info) = e else {
            unreachable!()
        };
        assert_eq!(info.code, 3);
        assert!(!info.success);
        assert!(!term.is_running());
        assert_eq!(term.exit_info().map(|i| i.code), Some(3));
    }

    #[test]
    fn drop_kills_shell_and_children() {
        let term = spawn_sh();
        let shell = term.pid().unwrap_or_else(|| panic!("no pid")) as i32;
        // A background job (own process group when job control is on) and a foreground child.
        term.send_text("sleep 1000 & echo bg:$!\r");
        wait_until(&term, "bg pid", |e| pid_from_screen(e, "bg:").is_some());
        let bg = pid_from_screen(&term.lock(), "bg:").unwrap_or(0);
        term.send_text("sh -c 'echo fg:$$; exec sleep 1000'\r");
        wait_until(&term, "fg pid", |e| pid_from_screen(e, "fg:").is_some());
        let fg = pid_from_screen(&term.lock(), "fg:").unwrap_or(0);
        assert!(alive(shell) && alive(bg) && alive(fg));
        drop(term);
        let start = Instant::now();
        while alive(shell) || alive(bg) || alive(fg) {
            assert!(
                start.elapsed() < DEADLINE,
                "still alive: shell {} bg {} fg {}",
                alive(shell),
                alive(bg),
                alive(fg)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn explicit_kill() {
        let term = spawn_sh();
        term.kill();
        wait_event(&term, "exit after kill", |e| {
            matches!(e, TerminalEvent::Exited(_))
        });
        assert!(!term.is_running());
    }

    #[cfg(target_os = "linux")]
    fn rss_kib() -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
        line.split_whitespace().nth(1)?.parse().ok()
    }

    #[test]
    fn flood_is_bounded_and_input_gets_through() {
        // macOS PTYs move far fewer bytes per second than Linux ones and CI runners are
        // slow, so the flood is smaller there; the behaviour under test is the same.
        let (flood_bytes, junk_lines): (u64, usize) = if cfg!(target_os = "linux") {
            (20_000_000, 8192)
        } else {
            (4_000_000, 2048)
        };
        let mut o = sh_options();
        o.scrollback = 2000;
        let term = Terminal::spawn(o, None).unwrap_or_else(|e| panic!("spawn: {e}"));
        wait_until(&term, "prompt", |e| e.screen_text().contains('$'));
        #[cfg(target_os = "linux")]
        let before = rss_kib();
        let start = Instant::now();
        // The flood, then input queued behind it while the shell is not reading.
        term.send_text(&format!(
            "yes 'flood flood flood flood flood flood flood' | head -c {flood_bytes}; echo flood-done\r"
        ));
        let t = Instant::now();
        // Typed-ahead input is made of complete lines (shell comments): a canonical-mode
        // tty keeps only ~1 KiB (macOS) / 4 KiB (Linux) of an unfinished line and drops the
        // rest, including a later newline on macOS, so one huge line would make the test
        // depend on the kernel's line discipline instead of on Lector. Hundreds of KiB of
        // complete lines fill the PTY input queue and block the writer thread until the
        // shell reads again, which must never block the caller.
        let mut junk = Vec::with_capacity(junk_lines * 80);
        for _ in 0..junk_lines {
            junk.extend_from_slice(&[b'#'; 79]);
            junk.push(b'\r');
        }
        term.write(junk);
        term.send_text("echo after-flood\r");
        assert!(
            t.elapsed() < Duration::from_millis(500),
            "write must not block"
        );
        // A canonical-mode tty silently drops typed-ahead input once its small queue is
        // full, so the first marker command may be lost by the kernel while the shell is
        // busy. That is tty behaviour, not Lector's: keep asking until the shell answers.
        let mut last_ask = Instant::now();
        let flood_wait = Instant::now();
        loop {
            // The lock is released before input is sent (sending takes it too).
            let seen = has_line(&term.lock(), "after-flood");
            if seen {
                break;
            }
            if flood_wait.elapsed() > FLOOD_DEADLINE {
                let screen = term.lock().screen_text();
                panic!("timed out waiting for after-flood; screen:\n{screen}");
            }
            if last_ask.elapsed() > Duration::from_millis(RESEND_MS) {
                last_ask = Instant::now();
                term.send_text("\recho after-flood\r");
            }
            let _ = term.recv_event_timeout(Duration::from_millis(50));
            let _ = term.snapshot();
        }
        let elapsed = start.elapsed();
        let em = term.lock();
        assert!(em.history_size() <= 2000);
        drop(em);
        #[cfg(target_os = "linux")]
        if let (Some(a), Some(b)) = (before, rss_kib()) {
            // Scrollback of 2000 lines plus channels: far below the 20 MB that went through.
            assert!(b.saturating_sub(a) < 64 * 1024, "RSS grew by {} KiB", b - a);
        }
        eprintln!("flood of {flood_bytes} bytes through the PTY took {elapsed:?}");
    }

    #[test]
    fn osc7_from_shell() {
        let term = spawn_sh();
        term.send_text("printf '\\033]7;file://localhost/usr\\007'\r");
        wait_event(&term, "cwd", |e| matches!(e, TerminalEvent::CwdChanged(_)));
        assert_eq!(term.cwd(), std::path::PathBuf::from("/usr"));
    }

    #[test]
    fn cursor_position_report_reaches_the_program() {
        let term = spawn_sh();
        term.send_text("stty raw -echo; printf 'ab\\033[6n'; dd bs=1 count=6 2>/dev/null | od -An -c; stty sane\r");
        wait_until(&term, "CPR", |e| {
            let t = e.screen_text();
            t.contains("033") && t.contains('R')
        });
    }

    fn integration_test(shell: &str) {
        let Some(path) = lector_terminal::shell::find_on_path(shell) else {
            eprintln!("{shell} not installed; skipping");
            return;
        };
        let home = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let data = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let mut o = SpawnOptions::new(ShellProfile::new(path));
        o.env
            .push(("HOME".into(), home.path().to_string_lossy().into_owned()));
        o.env.push(("PS1".into(), "$ ".into()));
        o.cwd = Some(home.path().to_path_buf());
        o.shell_integration = Some(data.path().to_path_buf());
        let term = Terminal::spawn(o, None).unwrap_or_else(|e| panic!("spawn: {e}"));
        wait_event(&term, "first prompt", |e| {
            *e == TerminalEvent::PromptStarted
        });
        term.send_text("false\r");
        let e = wait_event(&term, "command end", |e| {
            matches!(e, TerminalEvent::CommandFinished { .. })
        });
        assert_eq!(e, TerminalEvent::CommandFinished { exit_code: Some(1) });
        term.send_text("cd /\r");
        wait_until(&term, "cwd /", |e| {
            e.cwd() == Some(std::path::Path::new("/"))
        });
        wait_until(&term, "blocks", |e| {
            e.command_blocks().iter().any(|b| b.failed())
        });
        let em = term.lock();
        let failed = em
            .command_blocks()
            .into_iter()
            .find(|b| b.failed())
            .unwrap_or_else(|| panic!("no block"));
        assert_eq!(em.command_text(&failed).as_deref(), Some("false"));
    }

    #[test]
    fn bash_integration() {
        integration_test("bash");
    }

    #[test]
    fn zsh_integration() {
        integration_test("zsh");
    }

    #[test]
    fn fish_integration() {
        integration_test("fish");
    }

    #[test]
    fn invalid_cwd_is_an_error() {
        let mut o = sh_options();
        o.cwd = Some("/definitely/not/here".into());
        assert!(matches!(
            Terminal::spawn(o, None),
            Err(lector_terminal::TerminalError::InvalidCwd(_))
        ));
    }

    #[test]
    fn lector_env_is_scrubbed() {
        let mut o = sh_options();
        // Pretend the parent had an internal variable by checking a profile-provided one survives
        // while scrubbed names never reach the shell.
        o.env_scrub.push("HOME".into());
        o.env.push(("KEEP_ME".into(), "1".into()));
        let term = Terminal::spawn(o, None).unwrap_or_else(|e| panic!("spawn: {e}"));
        term.send_text("echo home=${HOME:-none} keep=$KEEP_ME term=$TERM prog=$TERM_PROGRAM\r");
        wait_until(&term, "env", |e| {
            has_line(e, "home=none keep=1 term=xterm-256color prog=Lector")
        });
    }
}

#[cfg(windows)]
mod windows {
    use super::*;

    fn cmd_options() -> SpawnOptions {
        let mut o = SpawnOptions::new(ShellProfile::new("cmd.exe"));
        o.size = TermSize::new(80, 24);
        o
    }

    fn spawn_cmd() -> Terminal {
        Terminal::spawn(cmd_options(), None).unwrap_or_else(|e| panic!("spawn: {e}"))
    }

    #[test]
    fn echo_hello() {
        let term = spawn_cmd();
        term.send_text("echo hello\r");
        wait_until(&term, "hello", |e| has_line(e, "hello"));
    }

    #[test]
    fn resize_is_seen_by_child() {
        let term = spawn_cmd();
        term.resize(TermSize::new(101, 31))
            .unwrap_or_else(|e| panic!("resize: {e}"));
        term.send_text("mode con\r");
        wait_until(&term, "mode con", |e| {
            let t = e.screen_text();
            t.contains("101") && t.contains("31")
        });
    }

    #[test]
    fn exit_status_is_reported() {
        let term = spawn_cmd();
        term.send_text("exit 3\r");
        let e = wait_event(&term, "exit", |e| matches!(e, TerminalEvent::Exited(_)));
        let TerminalEvent::Exited(info) = e else {
            unreachable!()
        };
        assert_eq!(info.code, 3);
    }

    #[test]
    fn drop_kills_children() {
        let term = spawn_cmd();
        term.send_text("start /b ping -n 1000 127.0.0.1 > NUL\r");
        term.send_text("echo started\r");
        wait_until(&term, "started", |e| has_line(e, "started"));
        let pid = term.pid().unwrap_or(0);
        drop(term);
        let start = Instant::now();
        loop {
            let out = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            if !out.contains(&pid.to_string()) {
                break;
            }
            assert!(start.elapsed() < DEADLINE, "cmd.exe still alive");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn flood_is_bounded() {
        let mut o = cmd_options();
        o.scrollback = 2000;
        let term = Terminal::spawn(o, None).unwrap_or_else(|e| panic!("spawn: {e}"));
        term.send_text("for /L %i in (1,1,20000) do @echo flood line %i\r");
        term.send_text("echo after-flood\r");
        // A canonical-mode tty silently drops typed-ahead input once its small queue is
        // full, so the first marker command may be lost by the kernel while the shell is
        // busy. That is tty behaviour, not Lector's: keep asking until the shell answers.
        let mut last_ask = Instant::now();
        wait_until_for(&term, "after-flood", FLOOD_DEADLINE, |e| {
            if has_line(e, "after-flood") {
                return true;
            }
            if last_ask.elapsed() > Duration::from_secs(2) {
                last_ask = Instant::now();
                term.send_text("\recho after-flood\r");
            }
            false
        });
        assert!(term.lock().history_size() <= 2000);
    }
}
