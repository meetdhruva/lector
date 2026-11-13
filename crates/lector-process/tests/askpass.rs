//! Askpass server/client round trips in-process (the git-driven test is `askpass_git.rs`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lector_process::askpass::{
    run_askpass_client_with, AskpassClientError, AskpassServer, Endpoint, PromptKind, PromptReply,
    PromptRequest, Secret, ENV_TOKEN,
};
#[cfg(unix)]
use lector_process::askpass::{ENV_ENDPOINT, HELPER_FLAG, HELPER_NAME};

fn token_of(server: &AskpassServer) -> String {
    server
        .env(std::path::Path::new("/x"))
        .into_iter()
        .find(|(k, _)| k == ENV_TOKEN)
        .map(|(_, v)| v.to_string_lossy().into_owned())
        .unwrap()
}

fn answering(
    seen: Arc<Mutex<Vec<PromptRequest>>>,
) -> impl Fn(PromptRequest) -> PromptReply + Send + Sync + 'static {
    move |req: PromptRequest| {
        let reply = match req.kind {
            PromptKind::Username => PromptReply::Answer(Secret::new("ann")),
            PromptKind::Password => PromptReply::Answer(Secret::new("hunter2")),
            _ => PromptReply::Cancel,
        };
        seen.lock().unwrap().push(req);
        reply
    }
}

fn ask(
    server: &AskpassServer,
    token: &str,
    prompt: &str,
) -> Result<Option<String>, AskpassClientError> {
    let mut out = Vec::new();
    let answered = run_askpass_client_with(prompt, server.endpoint(), token, &mut out)?;
    Ok(answered.then(|| String::from_utf8(out).unwrap()))
}

fn round_trip(server: AskpassServer, seen: Arc<Mutex<Vec<PromptRequest>>>) {
    let token = token_of(&server);
    assert_eq!(
        ask(&server, &token, "Username for 'https://x': ")
            .unwrap()
            .as_deref(),
        Some("ann\n")
    );
    assert_eq!(
        ask(&server, &token, "Password for 'https://ann@x': ")
            .unwrap()
            .as_deref(),
        Some("hunter2\n")
    );
    assert_eq!(
        ask(&server, &token, "Something else? ").unwrap(),
        None,
        "cancel"
    );
    let kinds: Vec<PromptKind> = seen.lock().unwrap().iter().map(|r| r.kind).collect();
    assert_eq!(
        kinds,
        vec![
            PromptKind::Username,
            PromptKind::Password,
            PromptKind::Other
        ]
    );

    // A wrong token gets no answer and never reaches the handler.
    let err = ask(&server, "not-the-token", "Password for 'https://ann@x': ").unwrap_err();
    assert!(matches!(err, AskpassClientError::Io(_)), "{err}");
    assert_eq!(seen.lock().unwrap().len(), 3);
}

#[test]
fn tcp_round_trip_and_token_check() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let server = AskpassServer::start_tcp(answering(seen.clone())).unwrap();
    assert!(matches!(server.endpoint(), Endpoint::Tcp(a) if a.ip().is_loopback()));
    round_trip(server, seen);
}

#[test]
fn platform_server_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let server = AskpassServer::start(dir.path(), answering(seen.clone())).unwrap();
    round_trip(server, seen);
}

#[cfg(unix)]
#[test]
fn unix_socket_is_private_and_removed_on_drop() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let server = AskpassServer::start_unix(dir.path(), |_| PromptReply::Cancel).unwrap();
    let Endpoint::Unix(socket) = server.endpoint().clone() else {
        panic!("expected a unix socket");
    };
    let parent = socket.parent().unwrap().to_path_buf();
    assert!(parent.starts_with(dir.path()));
    let mode = std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode & 0o077, 0, "socket not accessible to others: {mode:o}");
    let env = server.env(std::path::Path::new("/usr/bin/lector"));
    let get = |k: &str| {
        env.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.to_string_lossy().into_owned())
    };
    assert_eq!(get("GIT_ASKPASS").as_deref(), Some("/usr/bin/lector"));
    assert_eq!(get("SSH_ASKPASS").as_deref(), Some("/usr/bin/lector"));
    assert_eq!(get("SSH_ASKPASS_REQUIRE").as_deref(), Some("force"));
    assert_eq!(get("GIT_TERMINAL_PROMPT").as_deref(), Some("0"));
    assert_eq!(
        get("LECTOR_ASKPASS"),
        None,
        "the environment never selects helper mode"
    );
    assert_eq!(
        get(ENV_ENDPOINT).and_then(|v| Endpoint::parse(&v)),
        Some(Endpoint::Unix(socket.clone()))
    );
    assert_eq!(get(ENV_TOKEN).map(|t| t.len()), Some(64));
    // The helper shim: private, executable only by the owner, passes the hidden flag.
    let helper = server
        .helper_program(std::path::Path::new("/opt/it's lector/lector"))
        .unwrap();
    assert_eq!(helper.parent(), Some(parent.as_path()));
    assert_eq!(helper.file_name().unwrap(), HELPER_NAME);
    let mode = std::fs::metadata(&helper).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    let script = std::fs::read_to_string(&helper).unwrap();
    assert!(script.starts_with("#!/bin/sh\n"), "{script}");
    assert!(
        script.contains(&format!(
            "exec '/opt/it'\\''s lector/lector' {HELPER_FLAG} \"$@\""
        )),
        "{script}"
    );
    // Idempotent.
    assert_eq!(
        server
            .helper_program(std::path::Path::new("/other"))
            .unwrap(),
        helper
    );
    drop(server);
    assert!(!parent.exists(), "socket directory removed");
}

#[test]
fn prompts_are_served_concurrently() {
    // The first prompt blocks until the second one arrived: both must be in flight at once.
    let gate = Arc::new((Mutex::new(0usize), std::sync::Condvar::new()));
    let g = gate.clone();
    let server = AskpassServer::start_tcp(move |req| {
        let (lock, cv) = &*g;
        let mut n = lock.lock().unwrap();
        *n += 1;
        cv.notify_all();
        let (_n, timeout) = cv
            .wait_timeout_while(n, Duration::from_secs(5), |n| *n < 2)
            .unwrap();
        if timeout.timed_out() {
            return PromptReply::Cancel;
        }
        PromptReply::Answer(Secret::new(req.text))
    })
    .unwrap();
    let token = token_of(&server);
    let server = Arc::new(server);
    let threads: Vec<_> = ["one", "two"]
        .into_iter()
        .map(|p| {
            let server = server.clone();
            let token = token.clone();
            std::thread::spawn(move || ask(&server, &token, p).unwrap())
        })
        .collect();
    let answers: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(
        answers,
        vec![Some("one\n".to_owned()), Some("two\n".to_owned())]
    );
}

#[test]
fn a_panicking_handler_cancels() {
    let server = AskpassServer::start_tcp(|_| panic!("boom")).unwrap();
    let token = token_of(&server);
    assert_eq!(ask(&server, &token, "Password: ").unwrap(), None);
}

#[test]
fn client_without_a_server_fails_cleanly() {
    let server = AskpassServer::start_tcp(|_| PromptReply::Cancel).unwrap();
    let endpoint = server.endpoint().clone();
    drop(server);
    let err = run_askpass_client_with("Password: ", &endpoint, "t", &mut Vec::new()).unwrap_err();
    assert!(
        matches!(
            err,
            AskpassClientError::Connect(_) | AskpassClientError::Io(_)
        ),
        "{err}"
    );
}

#[test]
fn operation_ids_reach_the_handler() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let server = AskpassServer::start_tcp(answering(seen.clone())).unwrap();
    let token = token_of(&server);
    let mut out = Vec::new();
    let answered = lector_process::askpass::run_askpass_client_for_op(
        "Username for 'https://x': ",
        Some(42),
        server.endpoint(),
        &token,
        &mut out,
    )
    .unwrap();
    assert!(answered);
    ask(&server, &token, "Username for 'https://y': ").unwrap();
    let ops: Vec<Option<u64>> = seen.lock().unwrap().iter().map(|r| r.op).collect();
    assert_eq!(ops, vec![Some(42), None]);
}

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut f = (payload.len() as u32).to_be_bytes().to_vec();
    f.extend_from_slice(payload);
    f
}

#[test]
fn version_1_requests_are_still_answered() {
    use std::io::{Read as _, Write as _};
    let seen = Arc::new(Mutex::new(Vec::new()));
    let server = AskpassServer::start_tcp(answering(seen.clone())).unwrap();
    let token = token_of(&server);
    let Endpoint::Tcp(addr) = server.endpoint().clone() else {
        panic!("tcp");
    };
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    let mut req = frame(b"LECTOR-ASKPASS/1");
    req.extend(frame(token.as_bytes()));
    req.extend(frame(b"Username for 'https://x': "));
    s.write_all(&req).unwrap();
    let mut reply = Vec::new();
    s.read_to_end(&mut reply).unwrap();
    assert_eq!(reply, frame(b"Aann"));
    assert_eq!(seen.lock().unwrap()[0].op, None);
}

#[test]
fn idle_unauthenticated_connections_neither_take_prompt_slots_nor_linger() {
    use std::io::Read as _;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let server = AskpassServer::start_tcp(answering(seen.clone())).unwrap();
    let token = token_of(&server);
    let Endpoint::Tcp(addr) = server.endpoint().clone() else {
        panic!("tcp");
    };
    // More silent connections than there are prompt slots (8), fewer than handshake slots.
    let mut idle: Vec<std::net::TcpStream> = (0..12)
        .map(|_| std::net::TcpStream::connect(addr).unwrap())
        .collect();
    std::thread::sleep(Duration::from_millis(200));
    let started = std::time::Instant::now();
    assert_eq!(
        ask(&server, &token, "Username for 'https://x': ")
            .unwrap()
            .as_deref(),
        Some("ann\n")
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    // The handshake deadline closes them (EOF or reset), well before the request timeout.
    for s in &mut idle {
        s.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
        let mut buf = [0u8; 1];
        let closed = match s.read(&mut buf) {
            Ok(n) => n == 0,
            Err(e) => !matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
        };
        assert!(closed, "idle connection still open");
    }
    assert!(started.elapsed() < Duration::from_secs(8));
}
