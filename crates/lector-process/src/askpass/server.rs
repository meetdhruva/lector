use std::ffi::OsString;
use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use zeroize::Zeroize as _;

use super::protocol::{
    read_frame_until, same_bytes, write_frame, Conn, Endpoint, MAGIC, MAGIC_V1, MAX_FRAME,
    TAG_ANSWER, TAG_CANCEL,
};
use super::{
    askpass_helper_path, classify_prompt, parse_op, PromptReply, PromptRequest, Secret,
    ENV_ENDPOINT, ENV_TOKEN, HELPER_NAME,
};

/// Errors starting an [`AskpassServer`].
#[derive(Debug, thiserror::Error)]
pub enum AskpassError {
    /// Creating the socket, its directory or the listener failed.
    #[error("cannot start the askpass server: {0}")]
    Io(#[from] io::Error),
    /// The OS random generator failed (no token can be made).
    #[error("cannot create the askpass token: {0}")]
    Random(String),
}

type Handler = Arc<dyn Fn(PromptRequest) -> PromptReply + Send + Sync>;

/// How long a client may take to send the magic and the token, in total. The helper sends
/// its whole request in one write, so this only ever bites connections that are not ours.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long an authenticated client may take to send the rest of its request, in total.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long writing the reply may take.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// Authenticated prompts handled at the same time; further ones are closed at once.
const MAX_ACTIVE: usize = 8;
/// Connections still in the handshake at the same time; further ones are closed at once.
/// Counted separately so unauthenticated clients cannot take the prompt slots.
const MAX_HANDSHAKES: usize = 16;
/// Longest operation-id frame.
const MAX_OP_FRAME: usize = 32;
/// Accept-loop poll interval (the listener is non-blocking so the server can stop).
const ACCEPT_POLL: Duration = Duration::from_millis(25);

enum Listener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixListener),
}

impl Listener {
    fn set_nonblocking(&self) -> io::Result<()> {
        match self {
            Listener::Tcp(l) => l.set_nonblocking(true),
            #[cfg(unix)]
            Listener::Unix(l) => l.set_nonblocking(true),
        }
    }

    /// Accepts one connection as a blocking stream (accepted sockets inherit
    /// non-blocking mode on some systems).
    fn accept(&self) -> io::Result<Box<dyn Conn>> {
        match self {
            Listener::Tcp(l) => {
                let (s, peer) = l.accept()?;
                if !peer.ip().is_loopback() {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "non-loopback askpass client",
                    ));
                }
                s.set_nonblocking(false)?;
                Ok(Box::new(s))
            }
            #[cfg(unix)]
            Listener::Unix(l) => {
                let (s, _) = l.accept()?;
                s.set_nonblocking(false)?;
                Ok(Box::new(s))
            }
        }
    }
}

/// Answers askpass requests from git / ssh children with a handler (the UI's prompt
/// dialog). Stops and removes its socket when dropped.
pub struct AskpassServer {
    endpoint: Endpoint,
    token: Secret,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    dir: Option<PathBuf>,
}

impl std::fmt::Debug for AskpassServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskpassServer")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl AskpassServer {
    /// Starts the platform's server: on Unix a socket in a fresh `0700` directory under
    /// `runtime_dir` (e.g. Lector's data dir; falls back to the temp dir when the socket
    /// path would be too long), on Windows a loopback TCP port. `handler` runs on a
    /// connection thread, may block while the user answers, and may be called for several
    /// prompts at once.
    pub fn start(
        runtime_dir: &Path,
        handler: impl Fn(PromptRequest) -> PromptReply + Send + Sync + 'static,
    ) -> Result<AskpassServer, AskpassError> {
        #[cfg(unix)]
        {
            Self::start_unix(runtime_dir, handler)
        }
        #[cfg(not(unix))]
        {
            let mut server = Self::start_tcp(handler)?;
            // A private directory for the helper program (`helper_program`).
            let dir = private_dir(runtime_dir)?;
            if let Err(err) = write_pid_file(&dir) {
                tracing::debug!(
                    "askpass: cannot record the owner of {}: {err}",
                    dir.display()
                );
            }
            sweep_stale_dirs(runtime_dir, &dir);
            server.dir = Some(dir);
            Ok(server)
        }
    }

    /// Starts a server on a loopback TCP port (the Windows transport; the token is what
    /// keeps other local users out).
    pub fn start_tcp(
        handler: impl Fn(PromptRequest) -> PromptReply + Send + Sync + 'static,
    ) -> Result<AskpassServer, AskpassError> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let endpoint = Endpoint::Tcp(listener.local_addr()?);
        Self::serve(Listener::Tcp(listener), endpoint, None, Arc::new(handler))
    }

    /// Starts a server on a Unix domain socket in a new private directory under `runtime_dir`.
    #[cfg(unix)]
    pub fn start_unix(
        runtime_dir: &Path,
        handler: impl Fn(PromptRequest) -> PromptReply + Send + Sync + 'static,
    ) -> Result<AskpassServer, AskpassError> {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        // sun_path holds 104 (macOS) / 108 (Linux) bytes including the NUL.
        const MAX_SOCKET_PATH: usize = 100;
        let mut last_err = io::Error::other("no usable directory for the askpass socket");
        for base in [runtime_dir.to_path_buf(), std::env::temp_dir()] {
            if let Err(err) = std::fs::create_dir_all(&base) {
                last_err = err;
                continue;
            }
            for _ in 0..4 {
                let name = format!("{DIR_PREFIX}{}", hex(&random_bytes::<6>()?));
                let dir = base.join(name);
                let socket = dir.join("s");
                if socket.as_os_str().len() > MAX_SOCKET_PATH {
                    break;
                }
                // Fails if it exists: never reuse a directory someone else prepared.
                match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(err) => {
                        last_err = err;
                        break;
                    }
                }
                let bound = std::os::unix::net::UnixListener::bind(&socket).and_then(|l| {
                    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
                    Ok(l)
                });
                match bound {
                    Ok(listener) => {
                        if let Err(err) = write_pid_file(&dir) {
                            tracing::debug!(
                                "askpass: cannot record the owner of {}: {err}",
                                dir.display()
                            );
                        }
                        sweep_stale_dirs(&base, &dir);
                        return Self::serve(
                            Listener::Unix(listener),
                            Endpoint::Unix(socket),
                            Some(dir),
                            Arc::new(handler),
                        );
                    }
                    Err(err) => {
                        let _ = std::fs::remove_dir_all(&dir);
                        last_err = err;
                        break;
                    }
                }
            }
        }
        Err(AskpassError::Io(last_err))
    }

    fn serve(
        listener: Listener,
        endpoint: Endpoint,
        dir: Option<PathBuf>,
        handler: Handler,
    ) -> Result<AskpassServer, AskpassError> {
        listener.set_nonblocking()?;
        let token = Secret::new(hex(&random_bytes::<32>()?));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            let token = token.clone();
            std::thread::Builder::new()
                .name("lector-askpass".into())
                .spawn(move || accept_loop(listener, token, handler, stop))?
        };
        tracing::debug!("askpass server listening on {}", endpoint.to_env_value());
        Ok(AskpassServer {
            endpoint,
            token,
            stop,
            thread: Some(thread),
            dir,
        })
    }

    /// Where it listens.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The private directory holding the socket and the helper program (`None` for a
    /// plain [`start_tcp`](Self::start_tcp) server).
    pub fn private_dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Creates (once) the helper program git and ssh run, in this server's private
    /// directory, for the running executable `exe` (`std::env::current_exe()`):
    /// * Unix: a shell script `lector-askpass` (mode `0700`) running `exe` with the hidden
    ///   [`HELPER_FLAG`](super::HELPER_FLAG) in front of the prompt;
    /// * Windows: a hard link (or a copy) of the console binary
    ///   ([`askpass_helper_path`]) named `lector-askpass.exe`, selected by its file name.
    ///
    /// Put the result into [`env`](Self::env). Removed with the directory when the server
    /// stops.
    pub fn helper_program(&self, exe: &Path) -> io::Result<PathBuf> {
        let dir = match &self.dir {
            Some(d) => d.clone(),
            None => {
                return Err(io::Error::other(
                    "this askpass server has no private directory for its helper",
                ))
            }
        };
        write_helper(&dir, exe)
    }

    /// Environment for children that should prompt through this server: `GIT_ASKPASS` and
    /// `SSH_ASKPASS` = `helper` (from [`helper_program`](Self::helper_program)),
    /// `SSH_ASKPASS_REQUIRE=force`, `GIT_TERMINAL_PROMPT=0`, plus the endpoint and token.
    /// The variables alone do not make any program act as the helper.
    /// `credential.helper` is untouched: helpers answer first, askpass only when they don't.
    pub fn env(&self, helper: &Path) -> Vec<(OsString, OsString)> {
        vec![
            ("GIT_ASKPASS".into(), helper.into()),
            ("SSH_ASKPASS".into(), helper.into()),
            ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            (ENV_ENDPOINT.into(), self.endpoint.to_env_value().into()),
            (ENV_TOKEN.into(), self.token.expose().into()),
        ]
    }

    /// Stops listening and removes the socket (same as dropping it).
    pub fn shutdown(self) {}
}

impl Drop for AskpassServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(dir) = self.dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn accept_loop(listener: Listener, token: Secret, handler: Handler, stop: Arc<AtomicBool>) {
    let slots = Arc::new(Slots::default());
    while !stop.load(Ordering::SeqCst) {
        let conn = match listener.accept() {
            Ok(conn) => conn,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL);
                continue;
            }
            Err(err) => {
                tracing::debug!("askpass accept: {err}");
                std::thread::sleep(ACCEPT_POLL);
                continue;
            }
        };
        let Some(handshake) = Slot::take(&slots, |s| &s.handshakes, MAX_HANDSHAKES) else {
            tracing::warn!("askpass: too many connections at once; refusing one");
            continue;
        };
        let token = token.clone();
        let handler = handler.clone();
        let slots2 = slots.clone();
        let spawned = std::thread::Builder::new()
            .name("lector-askpass-conn".into())
            .spawn(move || {
                if let Err(err) = handle(conn, &token, &handler, &slots2, handshake) {
                    tracing::debug!("askpass connection: {err}");
                }
            });
        if let Err(err) = spawned {
            tracing::warn!("askpass: cannot start a connection thread: {err}");
        }
    }
}

/// Counters of connections in the handshake and of authenticated prompts.
#[derive(Default)]
struct Slots {
    handshakes: AtomicUsize,
    active: AtomicUsize,
}

/// One taken slot; released when dropped (also when a thread could not be started).
struct Slot {
    slots: Arc<Slots>,
    which: fn(&Slots) -> &AtomicUsize,
}

impl Slot {
    fn take(slots: &Arc<Slots>, which: fn(&Slots) -> &AtomicUsize, max: usize) -> Option<Slot> {
        let counter = which(slots);
        if counter.fetch_add(1, Ordering::SeqCst) >= max {
            counter.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Slot {
            slots: slots.clone(),
            which,
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        (self.which)(&self.slots).fetch_sub(1, Ordering::SeqCst);
    }
}

fn handle(
    mut conn: Box<dyn Conn>,
    token: &Secret,
    handler: &Handler,
    slots: &Arc<Slots>,
    handshake: Slot,
) -> io::Result<()> {
    conn.set_write_timeout(Some(REPLY_TIMEOUT))?;
    // Magic and token: a short overall deadline, before any prompt slot is used.
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let magic = read_frame_until(&mut *conn, MAGIC.len(), deadline)?;
    let version = if magic == MAGIC {
        2
    } else if magic == MAGIC_V1 {
        1
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad askpass magic",
        ));
    };
    let mut given = read_frame_until(&mut *conn, 256, deadline)?;
    let ok = same_bytes(&given, token.expose().as_bytes());
    given.zeroize();
    drop(handshake);
    if !ok {
        tracing::warn!("askpass: rejected a connection without the session token");
        return Ok(());
    }
    let Some(_active) = Slot::take(slots, |s| &s.active, MAX_ACTIVE) else {
        tracing::warn!("askpass: too many prompts at once; refusing one");
        return Ok(());
    };
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let op = if version >= 2 {
        let raw = read_frame_until(&mut *conn, MAX_OP_FRAME, deadline)?;
        std::str::from_utf8(&raw).ok().and_then(parse_op)
    } else {
        None
    };
    let prompt = read_frame_until(&mut *conn, MAX_FRAME, deadline)?;
    let text = String::from_utf8_lossy(&prompt).into_owned();
    let request = PromptRequest {
        kind: classify_prompt(&text),
        text,
        op,
    };
    tracing::debug!(kind = ?request.kind, op = ?request.op, "askpass prompt");
    let reply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(request)))
        .unwrap_or(PromptReply::Cancel);
    let mut payload = match &reply {
        PromptReply::Answer(answer) => {
            let mut p = Vec::with_capacity(1 + answer.expose().len());
            p.push(TAG_ANSWER);
            p.extend_from_slice(answer.expose().as_bytes());
            p
        }
        PromptReply::Cancel => vec![TAG_CANCEL],
    };
    let res = write_frame(&mut conn, &payload);
    payload.zeroize();
    res
}

/// Creates a fresh private directory `lector-askpass-<random>` under `base`.
#[cfg(not(unix))]
fn private_dir(base: &Path) -> Result<PathBuf, AskpassError> {
    std::fs::create_dir_all(base)?;
    for _ in 0..4 {
        let dir = base.join(format!("{DIR_PREFIX}{}", hex(&random_bytes::<6>()?)));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    }
    Err(AskpassError::Io(io::Error::other(
        "no fresh directory for the askpass helper",
    )))
}

/// Name prefix of the private directories (followed by 12 lowercase hex digits).
const DIR_PREFIX: &str = "lector-askpass-";
/// File in a private directory holding the owning process id, so directories left behind
/// by a crash or `SIGTERM` (no `Drop`) can be removed by the next server.
const PID_FILE: &str = "pid";

/// Records this process as the owner of `dir`.
fn write_pid_file(dir: &Path) -> io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(dir.join(PID_FILE))?;
    file.write_all(std::process::id().to_string().as_bytes())
}

/// Whether `name` is one of our private directory names.
fn is_private_dir_name(name: &str) -> bool {
    name.strip_prefix(DIR_PREFIX).is_some_and(|rest| {
        rest.len() == 12 && rest.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Removes private directories under `base` (other than `ours`) whose owner process is
/// gone. Only directories that look exactly like ours are touched: our name pattern, a
/// real directory (not a symlink), on Unix owned by us with no access for others, and a
/// readable pid file naming a process that no longer exists. When in doubt (no pid file,
/// the check fails, the pid is alive, possibly reused) the directory stays.
fn sweep_stale_dirs(base: &Path, ours: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    #[cfg(unix)]
    let our_uid = {
        use std::os::unix::fs::MetadataExt as _;
        match std::fs::symlink_metadata(ours) {
            Ok(m) => m.uid(),
            Err(_) => return,
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == ours || !is_private_dir_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_dir() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            if meta.uid() != our_uid || meta.permissions().mode() & 0o077 != 0 {
                continue;
            }
        }
        let Some(pid) = std::fs::read_to_string(path.join(PID_FILE))
            .ok()
            .and_then(|t| t.trim().parse::<u32>().ok())
        else {
            continue;
        };
        if pid_alive(pid) == Some(false) {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => tracing::debug!("askpass: removed stale {}", path.display()),
                Err(err) => tracing::debug!("askpass: cannot remove {}: {err}", path.display()),
            }
        }
    }
}

/// Whether process `pid` exists; `None` when that cannot be told.
fn pid_alive(pid: u32) -> Option<bool> {
    if pid == std::process::id() {
        return Some(true);
    }
    #[cfg(target_os = "linux")]
    {
        Some(Path::new("/proc").join(pid.to_string()).exists())
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // `ps -p` exits non-zero when there is no such process.
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "pid="])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()
            .map(|s| s.success())
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        Some(text.contains(&format!("\",\"{pid}\",\"")))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// Single-quotes `s` for `sh` (`'` becomes `'\''`).
#[cfg(unix)]
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(unix)]
fn write_helper(dir: &Path, exe: &Path) -> io::Result<PathBuf> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let path = dir.join(HELPER_NAME);
    if path.is_file() {
        return Ok(path);
    }
    let Some(exe) = askpass_helper_path(exe).to_str().map(str::to_owned) else {
        return Err(io::Error::other(
            "the executable path is not valid UTF-8; cannot write the askpass helper",
        ));
    };
    let flag = super::HELPER_FLAG;
    let script = format!(
        "#!/bin/sh\n# Lector askpass helper (generated; removed when Lector quits).\nexec {} {flag} \"$@\"\n",
        sh_quote(&exe)
    );
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&path)?;
    file.write_all(script.as_bytes())?;
    file.sync_all()?;
    Ok(path)
}

#[cfg(not(unix))]
fn write_helper(dir: &Path, exe: &Path) -> io::Result<PathBuf> {
    let path = dir.join(format!("{HELPER_NAME}.exe"));
    if path.is_file() {
        return Ok(path);
    }
    let source = askpass_helper_path(exe);
    if std::fs::hard_link(&source, &path).is_err() {
        std::fs::copy(&source, &path)?;
    }
    Ok(path)
}

fn random_bytes<const N: usize>() -> Result<[u8; N], AskpassError> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).map_err(|e| AskpassError::Random(e.to_string()))?;
    Ok(buf)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_dir(base: &Path, name: &str, pid: Option<u32>) -> PathBuf {
        let dir = base.join(name);
        std::fs::create_dir(&dir).unwrap_or_else(|e| panic!("{e}"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                .unwrap_or_else(|e| panic!("{e}"));
        }
        if let Some(pid) = pid {
            std::fs::write(dir.join(PID_FILE), pid.to_string()).unwrap_or_else(|e| panic!("{e}"));
        }
        dir
    }

    /// A pid that belonged to a process that has exited and been reaped.
    fn dead_pid() -> u32 {
        let exe = std::env::current_exe().unwrap_or_else(|e| panic!("{e}"));
        let mut child = std::process::Command::new(exe)
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("{e}"));
        let pid = child.id();
        let _ = child.wait();
        pid
    }

    #[test]
    fn private_dir_names_are_recognized() {
        assert!(is_private_dir_name("lector-askpass-0123456789ab"));
        assert!(!is_private_dir_name("lector-askpass-0123456789AB"));
        assert!(!is_private_dir_name("lector-askpass-0123"));
        assert!(!is_private_dir_name("other-0123456789ab"));
    }

    #[test]
    fn stale_private_dirs_are_swept_at_start() {
        let base = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dead = fake_dir(base.path(), "lector-askpass-00000000000d", Some(dead_pid()));
        let live = fake_dir(
            base.path(),
            "lector-askpass-00000000000a",
            Some(std::process::id()),
        );
        let unknown = fake_dir(base.path(), "lector-askpass-00000000000u", None);
        let unknown2 = fake_dir(base.path(), "lector-askpass-000000000000", None);
        let foreign = fake_dir(base.path(), "something-else", Some(dead_pid()));
        let server = AskpassServer::start(base.path(), |_| PromptReply::Cancel)
            .unwrap_or_else(|e| panic!("{e}"));
        let ours = server.private_dir().map(Path::to_path_buf);
        assert!(!dead.exists(), "a dead owner's directory is removed");
        assert!(live.exists() && foreign.exists() && unknown2.exists());
        // Not our name pattern (`u` is not hex): never touched.
        assert!(unknown.exists());
        let ours = ours.unwrap_or_else(|| panic!("private dir"));
        assert!(ours.starts_with(base.path()) || ours.starts_with(std::env::temp_dir()));
        assert_eq!(
            std::fs::read_to_string(ours.join(PID_FILE)).ok().as_deref(),
            Some(std::process::id().to_string().as_str())
        );
        drop(server);
        assert!(!ours.exists());
    }

    #[test]
    fn this_process_is_alive_and_a_reaped_child_is_not() {
        assert_eq!(pid_alive(std::process::id()), Some(true));
        assert_ne!(pid_alive(dead_pid()), Some(true));
    }
}
