//! Who is running: a process identity that survives pid reuse, and a liveness check.
//!
//! No `unsafe` and no new dependencies: Linux reads `/proc/<pid>/stat` (its start time makes
//! the identity exact), other Unixes ask `ps`, Windows asks `tasklist`. Where the start time is
//! not available the executable's file name is compared instead, which is enough to tell
//! "Lector is still running" from "the pid now belongs to something else".

use std::path::Path;
use std::process::Command;

/// What a session marker records about its process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    /// Process id.
    pub pid: u32,
    /// Opaque start-time token (Linux: field 22 of `/proc/<pid>/stat`), when the platform has one.
    pub start_token: Option<String>,
    /// File name of the executable (`lector`, `lector.exe`).
    pub exe: String,
}

/// The answer of [`liveness`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The process exists and is the one that wrote the marker.
    Alive,
    /// No such process (or the pid now belongs to a different program).
    Dead,
    /// The check itself failed; callers treat this as alive so nothing is counted falsely.
    Unknown,
}

impl ProcessIdentity {
    /// The identity of the running process.
    pub fn current() -> ProcessIdentity {
        let pid = std::process::id();
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        ProcessIdentity {
            pid,
            start_token: start_token(pid),
            exe,
        }
    }
}

/// Whether the process described by (`pid`, `start_token`, `exe`) is still running.
pub fn liveness(pid: u32, start_token: Option<&str>, exe: &str) -> Liveness {
    // Without a start token (other platforms) our own pid is the best evidence there is.
    if start_token.is_none() && pid == std::process::id() {
        return Liveness::Alive;
    }
    platform::liveness(pid, start_token, exe)
}

fn start_token(pid: u32) -> Option<String> {
    platform::start_token(pid)
}

#[allow(dead_code)] // used by the non-Linux backends and the tests
/// Whether two executable names are the same program (case-insensitive on Windows).
fn same_exe(a: &str, b: &str) -> bool {
    let strip = |s: &str| {
        Path::new(s)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let (a, b) = (strip(a), strip(b));
    if cfg!(windows) {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

#[allow(dead_code)] // used by the non-Linux backends
/// Runs a helper program and returns its stdout when it exited successfully.
fn run_helper(program: &str, args: &[&str]) -> Option<Result<String, ()>> {
    let out = Command::new(program).args(args).output().ok()?;
    if out.status.success() {
        Some(Ok(String::from_utf8_lossy(&out.stdout).into_owned()))
    } else {
        Some(Err(()))
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::Liveness;

    /// `(state, start time)` from the text of `/proc/<pid>/stat`: the command name (field 2) is
    /// in parentheses and may contain spaces and parentheses itself, so fields are counted from
    /// the last `)`.
    pub(super) fn parse_stat(stat: &str) -> Option<(char, String)> {
        let rest = stat.get(stat.rfind(')')? + 1..)?;
        let mut fields = rest.split_whitespace();
        let state = fields.next()?.chars().next()?;
        // Field 3 is the state; the start time is field 22, i.e. the 19th after it.
        let start = fields.nth(18)?;
        Some((state, start.to_string()))
    }

    fn read_stat(pid: u32) -> std::io::Result<String> {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
    }

    pub(super) fn start_token(pid: u32) -> Option<String> {
        parse_stat(&read_stat(pid).ok()?).map(|(_, start)| start)
    }

    pub(super) fn liveness(pid: u32, start_token: Option<&str>, exe: &str) -> Liveness {
        match read_stat(pid) {
            Ok(stat) => match parse_stat(&stat) {
                // Zombies and dead tasks are finished programs whose exit has not been collected.
                Some(('Z' | 'X' | 'x', _)) => Liveness::Dead,
                Some((_, start)) => match start_token {
                    Some(token) if token != start => Liveness::Dead,
                    Some(_) => Liveness::Alive,
                    None => comm_matches(&stat, exe),
                },
                None => Liveness::Unknown,
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Liveness::Dead,
            Err(_) => Liveness::Unknown,
        }
    }

    /// Without a start token: compare the (15-character) command name with the executable.
    fn comm_matches(stat: &str, exe: &str) -> Liveness {
        let comm = stat
            .find('(')
            .zip(stat.rfind(')'))
            .and_then(|(a, b)| stat.get(a + 1..b));
        match comm {
            Some(comm) if exe.starts_with(comm) || comm.is_empty() => Liveness::Alive,
            Some(_) => Liveness::Dead,
            None => Liveness::Unknown,
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
mod platform {
    use super::{run_helper, same_exe, Liveness};

    pub(super) fn start_token(_pid: u32) -> Option<String> {
        None
    }

    pub(super) fn liveness(pid: u32, _start_token: Option<&str>, exe: &str) -> Liveness {
        let pid = pid.to_string();
        match run_helper("ps", &["-o", "stat=", "-o", "comm=", "-p", &pid]) {
            None => Liveness::Unknown,
            // `ps` exits with 1 when there is no such process.
            Some(Err(())) => Liveness::Dead,
            Some(Ok(text)) => {
                let text = text.trim();
                let Some((stat, comm)) = text.split_once(char::is_whitespace) else {
                    return Liveness::Dead;
                };
                if stat.starts_with('Z') || !same_exe(comm.trim(), exe) {
                    Liveness::Dead
                } else {
                    Liveness::Alive
                }
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{run_helper, same_exe, Liveness};

    pub(super) fn start_token(_pid: u32) -> Option<String> {
        None
    }

    /// `tasklist /FO CSV /NH` prints `"image.exe","1234","Console","1","12,345 K"`, or an
    /// informational sentence when nothing matches.
    pub(super) fn liveness(pid: u32, _start_token: Option<&str>, exe: &str) -> Liveness {
        let filter = format!("PID eq {pid}");
        match run_helper("tasklist", &["/FI", &filter, "/FO", "CSV", "/NH"]) {
            None => Liveness::Unknown,
            Some(Err(())) => Liveness::Unknown,
            Some(Ok(text)) => {
                for line in text.lines() {
                    let mut cells = line.trim().trim_matches('"').split("\",\"");
                    if let (Some(image), Some(found)) = (cells.next(), cells.next()) {
                        if found == pid.to_string() {
                            return if same_exe(image, exe) {
                                Liveness::Alive
                            } else {
                                Liveness::Dead
                            };
                        }
                    }
                }
                Liveness::Dead
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::Liveness;

    pub(super) fn start_token(_pid: u32) -> Option<String> {
        None
    }

    pub(super) fn liveness(_pid: u32, _start_token: Option<&str>, _exe: &str) -> Liveness {
        Liveness::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_alive() {
        let me = ProcessIdentity::current();
        assert_eq!(me.pid, std::process::id());
        assert!(!me.exe.is_empty());
        assert_eq!(
            liveness(me.pid, me.start_token.as_deref(), &me.exe),
            Liveness::Alive
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_identity_has_a_start_token_and_detects_pid_reuse() {
        let me = ProcessIdentity::current();
        let token = me.start_token.clone().expect("start token on Linux");
        // Another process (our parent) with the right token is alive; with a wrong one it is a
        // different process that reuses the pid.
        let ppid = std::os::unix::process::parent_id();
        let parent_token = start_token(ppid).expect("parent start token");
        assert_eq!(
            liveness(ppid, Some(&parent_token), "whatever"),
            Liveness::Alive
        );
        assert_eq!(
            liveness(ppid, Some(&format!("{parent_token}0")), "whatever"),
            Liveness::Dead
        );
        assert_ne!(token, "");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stat_parsing_survives_odd_command_names() {
        let stat = "1234 (a (weird) name) S 1 1234 1234 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 100 18446744073709551615";
        assert_eq!(
            platform::parse_stat(stat),
            Some(('S', "987654".to_string()))
        );
        assert_eq!(platform::parse_stat("garbage"), None);
        assert_eq!(platform::parse_stat("1 (x) S 1 2"), None);
    }

    #[test]
    fn a_finished_child_is_dead() {
        let exe = std::env::current_exe().expect("current exe");
        let mut child = Command::new(&exe)
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn");
        let pid = child.id();
        let token = start_token(pid);
        child.wait().expect("wait");
        let name = exe
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        assert_eq!(liveness(pid, token.as_deref(), &name), Liveness::Dead);
    }

    #[test]
    fn exe_names_compare_by_file_name() {
        assert!(same_exe("/usr/bin/lector", "lector"));
        assert!(same_exe("lector", "lector"));
        assert!(!same_exe("lector", "sh"));
    }
}
