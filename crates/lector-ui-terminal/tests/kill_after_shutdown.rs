//! A terminal dropped after the scheduler stopped (a spawn that finished during quit) still
//! kills its process tree: the scheduler's task runner hands the kill escalation back and the
//! terminal runs it itself, instead of the cleanup being dropped.

#[cfg(unix)]
#[test]
fn a_terminal_dropped_after_scheduler_shutdown_still_kills_a_child_that_ignores_sighup() {
    use std::time::{Duration, Instant};

    use lector_app::scheduler::{JobKey, JobKind, Priority, Scheduler, Subject};
    use lector_terminal::{ShellProfile, SpawnOptions, Terminal};

    fn alive(pid: u32) -> bool {
        // `ps` prints nothing for a missing pid and `Z` for a zombie.
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .expect("ps");
        let stat = String::from_utf8_lossy(&out.stdout);
        let stat = stat.trim();
        !stat.is_empty() && !stat.starts_with('Z')
    }

    let scheduler = Scheduler::new(1);
    let kill = JobKey::new(JobKind::TerminalKill, Subject::App);
    let runner = scheduler.task_runner(kill, Priority::Background);
    let mut profile = ShellProfile::new("/bin/sh");
    profile.args = vec!["-c".into(), "trap '' HUP; while :; do sleep 1; done".into()];
    let mut options = SpawnOptions::new(profile);
    options.cwd = Some(std::env::temp_dir());
    options.kill_runner = Some(runner);
    let term = Terminal::spawn(options, None).expect("spawn");
    let pid = term.pid().expect("pid");
    std::thread::sleep(Duration::from_millis(200));
    assert!(alive(pid));
    // Quit: the scheduler no longer accepts jobs.
    assert!(scheduler.shutdown(Duration::from_secs(2)));
    drop(term);
    let end = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(
            Instant::now() < end,
            "the shell ignoring SIGHUP survived its terminal"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
