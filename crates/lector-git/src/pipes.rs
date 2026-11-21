//! Child-process pipe helpers for the `git` invocations that bypass `lector-process`
//! (`write` and the shadow repository): feeding stdin without deadlocking against full output
//! pipes, and draining stderr. Bounded helpers use `std::thread::scope` inside one call; the
//! stderr drain that outlives the call is a named pipe-reader thread (ADR 0003).

use std::io::{self, Read, Write};
use std::process::{Child, ChildStderr, Output};
use std::thread::JoinHandle;

use lector_base::thread::{spawn_named, ThreadKind};

/// Waits for `child` and collects its output while `input` (if any) is written to its stdin
/// on a scoped helper thread, so a large input cannot deadlock against a full stdout/stderr
/// pipe. A broken pipe means the child exited early; its exit status and stderr explain why,
/// so the write error itself is ignored.
pub(crate) fn output_with_input(mut child: Child, input: Option<&[u8]>) -> io::Result<Output> {
    match (input, child.stdin.take()) {
        (Some(input), Some(mut pipe)) => std::thread::scope(|s| {
            let writer = s.spawn(move || {
                let _ = pipe.write_all(input);
            });
            let output = child.wait_with_output();
            let _ = writer.join();
            output
        }),
        _ => child.wait_with_output(),
    }
}

/// Writes `input` to `pipe` on a scoped helper thread while `read` runs on the calling thread
/// (both inside this call), then closes the pipe.
pub(crate) fn feed_while<W, R>(pipe: Option<W>, input: &[u8], read: impl FnOnce() -> R) -> R
where
    W: Write + Send,
{
    std::thread::scope(|s| {
        let writer = s.spawn(move || {
            if let Some(mut pipe) = pipe {
                let _ = pipe.write_all(input);
            }
        });
        let out = read();
        let _ = writer.join();
        out
    })
}

/// A child's stderr collected on a pipe-reader thread, see [`drain`].
pub(crate) struct Drain(Option<JoinHandle<String>>);

impl Drain {
    /// What the child wrote to stderr (empty when no reader thread could start).
    pub(crate) fn join(self) -> String {
        self.0.and_then(|h| h.join().ok()).unwrap_or_default()
    }
}

/// Collects a child's stderr on a pipe-reader thread so a chatty child can't block on a full
/// pipe. The thread ends when the child closes its stderr.
pub(crate) fn drain(pipe: Option<ChildStderr>) -> Drain {
    let spawned = spawn_named(ThreadKind::PipeReader, "git stderr", move || {
        let mut text = String::new();
        if let Some(mut pipe) = pipe {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            text = String::from_utf8_lossy(&bytes).into_owned();
        }
        text
    });
    // Without a reader (the OS refused a thread) the pipe is closed: git then fails to write
    // its stderr instead of blocking on it.
    Drain(spawned.ok())
}

// Both tests drive Unix tools (`cat`, `sh`).
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn large_input_and_output_do_not_deadlock() {
        let input: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("cat");
        let out = output_with_input(child, Some(&input)).expect("output");
        assert_eq!(out.stdout, input);
    }

    #[test]
    fn drain_collects_stderr() {
        let mut child = Command::new("sh")
            .args(["-c", "echo oops >&2"])
            .stderr(Stdio::piped())
            .spawn()
            .expect("sh");
        let d = drain(child.stderr.take());
        let _ = child.wait();
        assert_eq!(d.join().trim(), "oops");
    }
}
