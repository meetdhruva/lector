use std::ffi::OsString;
use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use zeroize::Zeroize as _;

use super::protocol::{
    read_frame, same_bytes, write_frame, Conn, Endpoint, MAGIC, TAG_ANSWER, TAG_CANCEL,
};
use super::{
    askpass_helper_path, classify_prompt, PromptReply, PromptRequest, Secret, ENV_ENDPOINT,
    ENV_TOKEN, HELPER_NAME,
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

/// How long a client may take to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Prompts handled at the same time; further connections are closed at once.
const MAX_ACTIVE: usize = 8;
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
            server.dir = Some(private_dir(runtime_dir)?);
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
                let name = format!("lector-askpass-{}", hex(&random_bytes::<6>()?));
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
    let active = Arc::new(AtomicUsize::new(0));
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
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_ACTIVE {
            active.fetch_sub(1, Ordering::SeqCst);
            tracing::warn!("askpass: too many prompts at once; refusing one");
            continue;
        }
        let token = token.clone();
        let handler = handler.clone();
        let active2 = active.clone();
        let spawned = std::thread::Builder::new()
            .name("lector-askpass-conn".into())
            .spawn(move || {
                if let Err(err) = handle(conn, &token, &handler) {
                    tracing::debug!("askpass connection: {err}");
                }
                active2.fetch_sub(1, Ordering::SeqCst);
            });
        if let Err(err) = spawned {
            active.fetch_sub(1, Ordering::SeqCst);
            tracing::warn!("askpass: cannot start a connection thread: {err}");
        }
    }
}

fn handle(mut conn: Box<dyn Conn>, token: &Secret, handler: &Handler) -> io::Result<()> {
    conn.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    conn.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    let magic = read_frame(&mut conn)?;
    if magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad askpass magic",
        ));
    }
    let mut given = read_frame(&mut conn)?;
    let ok = same_bytes(&given, token.expose().as_bytes());
    given.zeroize();
    if !ok {
        tracing::warn!("askpass: rejected a connection without the session token");
        return Ok(());
    }
    let prompt = read_frame(&mut conn)?;
    let text = String::from_utf8_lossy(&prompt).into_owned();
    let request = PromptRequest {
        kind: classify_prompt(&text),
        text,
    };
    tracing::debug!(kind = ?request.kind, "askpass prompt");
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
        let dir = base.join(format!("lector-askpass-{}", hex(&random_bytes::<6>()?)));
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
