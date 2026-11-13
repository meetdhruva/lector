use std::io::{self, Write};
use std::time::Duration;

use zeroize::Zeroize as _;

use super::protocol::{read_frame, write_frame, Endpoint, MAGIC, TAG_ANSWER, TAG_CANCEL};
use super::{ENV_ENDPOINT, ENV_OP, ENV_TOKEN};

/// Why the askpass helper could not get an answer.
#[derive(Debug, thiserror::Error)]
pub enum AskpassClientError {
    /// `LECTOR_ASKPASS_ENDPOINT` / `LECTOR_ASKPASS_TOKEN` are missing or malformed.
    #[error("askpass is not configured (missing or invalid {ENV_ENDPOINT} / {ENV_TOKEN})")]
    NotConfigured,
    /// The server is not reachable (Lector quit?).
    #[error("cannot reach Lector's askpass server: {0}")]
    Connect(#[source] io::Error),
    /// The exchange failed.
    #[error("askpass exchange failed: {0}")]
    Io(#[from] io::Error),
    /// The server sent something unexpected.
    #[error("unexpected askpass reply")]
    Protocol,
}

/// The helper-mode entry: reads the endpoint and token from the environment, forwards
/// `prompt`, prints the answer (plus `\n`) on stdout. Returns the process exit code:
/// `0` answered, `1` cancelled by the user, `2` error (reported on stderr, never the answer).
pub fn run_askpass_client(prompt: &str) -> i32 {
    let endpoint = std::env::var(ENV_ENDPOINT)
        .ok()
        .and_then(|v| Endpoint::parse(&v));
    let token = std::env::var(ENV_TOKEN)
        .ok()
        .filter(|t| super::is_valid_token(t));
    let result = match (endpoint, token) {
        (Some(endpoint), Some(mut token)) => {
            let op = std::env::var(ENV_OP).ok().and_then(|v| super::parse_op(&v));
            let mut out = io::stdout().lock();
            let r = send_request(prompt, op, &endpoint, &token, &mut out);
            token.zeroize();
            r
        }
        _ => Err(AskpassClientError::NotConfigured),
    };
    match result {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(err) => {
            eprintln!("lector askpass: {err}");
            2
        }
    }
}

/// Asks the server at `endpoint` and writes the answer plus `\n` to `out`.
/// `Ok(false)` = the user cancelled (nothing written). Sends no operation id.
pub fn run_askpass_client_with(
    prompt: &str,
    endpoint: &Endpoint,
    token: &str,
    out: &mut dyn Write,
) -> Result<bool, AskpassClientError> {
    send_request(prompt, None, endpoint, token, out)
}

/// [`run_askpass_client_with`] tagged with operation id `op` (see [`super::ENV_OP`]).
pub fn run_askpass_client_for_op(
    prompt: &str,
    op: Option<u64>,
    endpoint: &Endpoint,
    token: &str,
    out: &mut dyn Write,
) -> Result<bool, AskpassClientError> {
    send_request(prompt, op, endpoint, token, out)
}

fn send_request(
    prompt: &str,
    op: Option<u64>,
    endpoint: &Endpoint,
    token: &str,
    out: &mut dyn Write,
) -> Result<bool, AskpassClientError> {
    let mut conn = endpoint
        .connect(Duration::from_secs(5))
        .map_err(AskpassClientError::Connect)?;
    conn.set_write_timeout(Some(Duration::from_secs(10)))?;
    let op = op.map(|o| o.to_string()).unwrap_or_default();
    // One write for the whole request: the server's handshake deadline is short.
    let mut request = Vec::new();
    write_frame(&mut request, MAGIC)?;
    write_frame(&mut request, token.as_bytes())?;
    write_frame(&mut request, op.as_bytes())?;
    write_frame(&mut request, prompt.as_bytes())?;
    let sent = conn.write_all(&request).and_then(|()| conn.flush());
    request.zeroize();
    sent?;
    // The user may take their time; Lector closing the connection ends the wait.
    conn.set_read_timeout(None)?;
    let mut reply = read_frame(&mut conn)?;
    let result = match reply.split_first() {
        Some((&TAG_ANSWER, answer)) => {
            let r = out
                .write_all(answer)
                .and_then(|()| out.write_all(b"\n"))
                .and_then(|()| out.flush());
            r.map(|()| true).map_err(AskpassClientError::Io)
        }
        Some((&TAG_CANCEL, _)) => Ok(false),
        _ => Err(AskpassClientError::Protocol),
    };
    reply.zeroize();
    result
}
