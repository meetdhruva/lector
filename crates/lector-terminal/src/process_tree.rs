//! Finding and signalling every process that belongs to a terminal (Unix).
//!
//! An interactive shell with job control puts each job in its own process
//! group, so signalling the shell's group alone misses background jobs. The
//! processes of a terminal are those in the shell's session (the PTY spawn
//! calls `setsid`, so the session id is the shell's pid); programs that called
//! `setsid` themselves (daemons) deliberately left and are not touched.
//!
//! Linux reads `/proc`; other Unix systems ask `ps` for the descendants of the
//! shell and their process groups. The list must be collected while the shell
//! is still alive (children are re-parented when it exits).

use rustix::process::{kill_process, kill_process_group, test_kill_process, Pid, Signal};

/// Processes and process groups to signal.
#[derive(Debug, Default, Clone)]
pub(crate) struct Victims {
    pids: Vec<i32>,
    groups: Vec<i32>,
}

impl Victims {
    /// Every process of the session led by `shell`, plus the PTY's foreground group.
    pub(crate) fn collect(shell: i32, foreground_group: Option<i32>) -> Victims {
        let mut v = Victims {
            pids: vec![shell],
            groups: vec![shell],
        };
        if let Some(fg) = foreground_group.filter(|&g| g > 0) {
            v.groups.push(fg);
        }
        for (pid, pgid) in session_members(shell) {
            v.pids.push(pid);
            v.groups.push(pgid);
        }
        v.pids.sort_unstable();
        v.pids.dedup();
        v.groups.sort_unstable();
        v.groups.dedup();
        v.pids.retain(|&p| p > 1);
        v.groups.retain(|&g| g > 1);
        v
    }

    /// Sends `signal` to every process and group.
    pub(crate) fn signal(&self, signal: Signal) {
        for &g in &self.groups {
            if let Some(pg) = Pid::from_raw(g) {
                let _ = kill_process_group(pg, signal);
            }
        }
        for &p in &self.pids {
            if let Some(pid) = Pid::from_raw(p) {
                let _ = kill_process(pid, signal);
            }
        }
    }

    /// Whether any listed process is still running (zombies do not count).
    pub(crate) fn any_alive(&self) -> bool {
        self.pids.iter().any(|&p| is_alive(p))
    }

    /// Kills the listed processes that are still running.
    pub(crate) fn kill_survivors(&self) {
        for &p in &self.pids {
            if is_alive(p) {
                if let Some(pid) = Pid::from_raw(p) {
                    let _ = kill_process(pid, Signal::KILL);
                }
            }
        }
    }
}

fn is_alive(pid: i32) -> bool {
    let Some(p) = Pid::from_raw(pid) else {
        return false;
    };
    if test_kill_process(p).is_err() {
        return false;
    }
    #[cfg(target_os = "linux")]
    if let Some(stat) = read_stat(pid) {
        return stat.state != 'Z';
    }
    true
}

#[cfg(target_os = "linux")]
struct Stat {
    state: char,
    ppid: i32,
    pgid: i32,
    session: i32,
}

#[cfg(target_os = "linux")]
fn read_stat(pid: i32) -> Option<Stat> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ppid pgrp session ...`; comm may contain spaces and parens.
    let rest = &text[text.rfind(')')? + 1..];
    let mut f = rest.split_whitespace();
    let state = f.next()?.chars().next()?;
    let ppid = f.next()?.parse().ok()?;
    let pgid = f.next()?.parse().ok()?;
    let session = f.next()?.parse().ok()?;
    Some(Stat {
        state,
        ppid,
        pgid,
        session,
    })
}

/// `(pid, pgid)` of the processes in `shell`'s session or descending from it.
#[cfg(target_os = "linux")]
fn session_members(shell: i32) -> Vec<(i32, i32)> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut all: Vec<(i32, Stat)> = Vec::new();
    for entry in dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        if let Some(stat) = read_stat(pid) {
            all.push((pid, stat));
        }
    }
    let mut members: Vec<i32> = all
        .iter()
        .filter(|(_, s)| s.session == shell)
        .map(|(p, _)| *p)
        .collect();
    members.extend(descendants(shell, all.iter().map(|(p, s)| (*p, s.ppid))));
    members.sort_unstable();
    members.dedup();
    all.iter()
        .filter(|(p, _)| members.contains(p))
        .map(|(p, s)| (*p, s.pgid))
        .collect()
}

/// `(pid, pgid)` of the processes descending from `shell`.
#[cfg(not(target_os = "linux"))]
fn session_members(shell: i32) -> Vec<(i32, i32)> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "ppid=", "-o", "pgid="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let rows: Vec<(i32, i32, i32)> = text
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace().map(|x| x.parse::<i32>().ok());
            Some((f.next()??, f.next()??, f.next()??))
        })
        .collect();
    let members = descendants(shell, rows.iter().map(|&(p, pp, _)| (p, pp)));
    rows.iter()
        .filter(|(p, _, _)| members.contains(p))
        .map(|&(p, _, g)| (p, g))
        .collect()
}

/// Transitive children of `root` given `(pid, ppid)` pairs.
fn descendants(root: i32, pairs: impl Iterator<Item = (i32, i32)>) -> Vec<i32> {
    let pairs: Vec<(i32, i32)> = pairs.collect();
    let mut found = vec![root];
    let mut i = 0;
    while i < found.len() {
        let parent = found[i];
        for &(pid, ppid) in &pairs {
            if ppid == parent && !found.contains(&pid) {
                found.push(pid);
            }
        }
        i += 1;
    }
    found.remove(0);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descendants_walks_the_tree() {
        let pairs = vec![(10, 1), (11, 10), (12, 11), (13, 1), (14, 12)];
        let mut d = descendants(10, pairs.into_iter());
        d.sort_unstable();
        assert_eq!(d, vec![11, 12, 14]);
    }

    #[test]
    fn collects_own_process() {
        let me = std::process::id() as i32;
        let v = Victims::collect(me, None);
        assert!(v.pids.contains(&me));
        assert!(is_alive(me));
    }
}
