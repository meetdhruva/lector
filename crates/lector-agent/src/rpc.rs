//! Minimal JSON-RPC 2.0 over a byte stream (newline-delimited JSON, as ACP uses on stdio).
//!
//! Runtime-agnostic: one reader thread per connection; requests block the calling thread
//! (with an optional timeout); incoming requests are answered from their own thread so a
//! slow one (a permission prompt) never stalls the stream; notifications are delivered in
//! order on the reader thread. Malformed lines are logged and skipped.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{json, Value};

/// JSON-RPC "method not found".
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC "invalid params".
pub const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC "internal error".
pub const INTERNAL_ERROR: i64 = -32603;

/// Failure of a request.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RpcError {
    /// The peer answered with an error object.
    #[error("{message} (code {code})")]
    Remote {
        /// Error code.
        code: i64,
        /// Message.
        message: String,
        /// Extra data.
        data: Option<Value>,
    },
    /// No answer within the timeout.
    #[error("`{0}` timed out")]
    Timeout(String),
    /// The connection is closed (the process exited).
    #[error("connection closed: {0}")]
    Closed(String),
}

impl RpcError {
    /// An error answer for an incoming request.
    pub fn remote(code: i64, message: impl Into<String>) -> RpcError {
        RpcError::Remote {
            code,
            message: message.into(),
            data: None,
        }
    }

    fn to_json(&self) -> Value {
        match self {
            RpcError::Remote {
                code,
                message,
                data,
            } => {
                let mut v = json!({ "code": code, "message": message });
                if let Some(d) = data {
                    v["data"] = d.clone();
                }
                v
            }
            other => json!({ "code": INTERNAL_ERROR, "message": other.to_string() }),
        }
    }
}

/// Receives what the peer sends.
pub trait Handler: Send + Sync {
    /// An incoming request; runs on its own thread, may block.
    fn on_request(&self, method: &str, params: Value) -> Result<Value, RpcError>;
    /// An incoming notification; runs on the reader thread, in order.
    fn on_notification(&self, method: &str, params: Value);
    /// The stream ended (EOF or read error). Called once.
    fn on_closed(&self, reason: String);
}

type Reply = mpsc::Sender<Result<Value, RpcError>>;

struct Inner {
    writer: Mutex<Box<dyn Write + Send>>,
    pending: Mutex<HashMap<u64, Reply>>,
    next_id: AtomicU64,
    closed: Mutex<Option<String>>,
    malformed: AtomicU64,
}

/// An outstanding request (see [`Connection::start_request`]). Dropping it forgets the answer.
pub struct PendingReply {
    rx: mpsc::Receiver<Result<Value, RpcError>>,
    id: u64,
    conn: Connection,
}

impl PendingReply {
    /// Waits up to `timeout`; `None` if no answer arrived yet.
    pub fn wait(&self, timeout: Duration) -> Option<Result<Value, RpcError>> {
        match self.rx.recv_timeout(timeout) {
            Ok(result) => Some(result),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Some(Err(RpcError::Closed(self.conn.close_reason())))
            }
        }
    }
}

impl Drop for PendingReply {
    fn drop(&mut self) {
        self.conn.inner.pending.lock().remove(&self.id);
    }
}

/// One JSON-RPC connection. Cheap to clone.
#[derive(Clone)]
pub struct Connection {
    inner: Arc<Inner>,
}

impl Connection {
    /// Starts the reader thread. `handler` is held weakly (the owner keeps it alive).
    pub fn new(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        handler: Weak<dyn Handler>,
    ) -> Connection {
        let inner = Arc::new(Inner {
            writer: Mutex::new(Box::new(writer)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            closed: Mutex::new(None),
            malformed: AtomicU64::new(0),
        });
        let conn = Connection { inner };
        let reader_conn = conn.clone();
        let spawned = std::thread::Builder::new()
            .name("acp-reader".into())
            .spawn(move || reader_conn.read_loop(reader, handler));
        if let Err(err) = spawned {
            conn.mark_closed(format!("cannot start the reader thread: {err}"));
        }
        conn
    }

    /// Sends a request and waits for its answer (`None` = no timeout).
    pub fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, RpcError> {
        let pending = self.start_request(method, params)?;
        match timeout {
            Some(t) => pending
                .wait(t)
                .unwrap_or_else(|| Err(RpcError::Timeout(method.to_string()))),
            None => loop {
                if let Some(result) = pending.wait(Duration::from_secs(3600)) {
                    break result;
                }
            },
        }
    }

    /// Sends a request; wait for the answer with [`PendingReply::wait`] (lets the caller
    /// interleave other checks, e.g. cancellation).
    pub fn start_request(&self, method: &str, params: Value) -> Result<PendingReply, RpcError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        {
            // Register under the `closed` lock so `mark_closed` cannot miss it.
            let closed = self.inner.closed.lock();
            if let Some(reason) = closed.as_ref() {
                return Err(RpcError::Closed(reason.clone()));
            }
            self.inner.pending.lock().insert(id, tx);
        }
        let pending = PendingReply {
            rx,
            id,
            conn: self.clone(),
        };
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send(&msg)?;
        Ok(pending)
    }

    /// Sends a notification.
    pub fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// Whether the stream ended.
    pub fn is_closed(&self) -> bool {
        self.inner.closed.lock().is_some()
    }

    /// Lines that could not be parsed so far.
    pub fn malformed_count(&self) -> u64 {
        self.inner.malformed.load(Ordering::Relaxed)
    }

    /// Fails every pending request and refuses new ones.
    pub fn mark_closed(&self, reason: String) -> bool {
        let pending: Vec<Reply> = {
            let mut closed = self.inner.closed.lock();
            if closed.is_some() {
                return false;
            }
            *closed = Some(reason.clone());
            self.inner
                .pending
                .lock()
                .drain()
                .map(|(_, tx)| tx)
                .collect()
        };
        for tx in pending {
            let _ = tx.send(Err(RpcError::Closed(reason.clone())));
        }
        true
    }

    fn close_reason(&self) -> String {
        self.inner
            .closed
            .lock()
            .clone()
            .unwrap_or_else(|| "closed".into())
    }

    fn send(&self, msg: &Value) -> Result<(), RpcError> {
        let mut line = serde_json::to_vec(msg).map_err(|e| RpcError::Closed(e.to_string()))?;
        line.push(b'\n');
        let mut w = self.inner.writer.lock();
        w.write_all(&line)
            .and_then(|_| w.flush())
            .map_err(|e| RpcError::Closed(format!("write failed: {e}")))
    }

    fn respond(&self, id: Value, result: Result<Value, RpcError>) {
        let msg = match result {
            Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
            Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e.to_json() }),
        };
        if let Err(err) = self.send(&msg) {
            tracing::debug!("cannot answer an agent request: {err}");
        }
    }

    fn read_loop(self, reader: impl Read, handler: Weak<dyn Handler>) {
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        let reason = loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break "the agent closed its output".to_string(),
                Ok(_) => {}
                Err(err) => break format!("read failed: {err}"),
            }
            let text = String::from_utf8_lossy(&line);
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            let msg: Value = match serde_json::from_str(text) {
                Ok(v @ Value::Object(_)) => v,
                _ => {
                    self.inner.malformed.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!("malformed message from agent: {}", truncate(text, 200));
                    continue;
                }
            };
            self.dispatch(msg, &handler);
        };
        self.mark_closed(reason.clone());
        if let Some(h) = handler.upgrade() {
            h.on_closed(reason);
        }
    }

    fn dispatch(&self, mut msg: Value, handler: &Weak<dyn Handler>) {
        let method = msg.get("method").and_then(Value::as_str).map(str::to_owned);
        let id = msg.get("id").cloned().filter(|v| !v.is_null());
        let params = msg
            .get_mut("params")
            .map(Value::take)
            .unwrap_or(Value::Null);
        match (method, id) {
            (Some(method), Some(id)) => {
                let Some(h) = handler.upgrade() else {
                    self.respond(id, Err(RpcError::remote(INTERNAL_ERROR, "session ended")));
                    return;
                };
                let conn = self.clone();
                let spawned = std::thread::Builder::new()
                    .name("acp-request".into())
                    .spawn(move || {
                        let result = h.on_request(&method, params);
                        conn.respond(id, result);
                    });
                if let Err(err) = spawned {
                    tracing::warn!("cannot handle an agent request: {err}");
                }
            }
            (Some(method), None) => {
                if let Some(h) = handler.upgrade() {
                    h.on_notification(&method, params);
                }
            }
            (None, Some(id)) => {
                let Some(id) = id.as_u64() else {
                    self.inner.malformed.fetch_add(1, Ordering::Relaxed);
                    return;
                };
                let Some(tx) = self.inner.pending.lock().remove(&id) else {
                    tracing::debug!("answer to an unknown request {id}");
                    return;
                };
                let result = if let Some(err) = msg.get("error") {
                    Err(RpcError::Remote {
                        code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                        message: err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("error")
                            .to_string(),
                        data: err.get("data").cloned(),
                    })
                } else {
                    Ok(msg
                        .get_mut("result")
                        .map(Value::take)
                        .unwrap_or(Value::Null))
                };
                let _ = tx.send(result);
            }
            (None, None) => {
                self.inner.malformed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// At most `max` bytes of `s` (on a char boundary).
pub(crate) fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Recorder(Mutex<Vec<String>>);
    impl Handler for Recorder {
        fn on_request(&self, method: &str, _: Value) -> Result<Value, RpcError> {
            Err(RpcError::remote(METHOD_NOT_FOUND, method))
        }
        fn on_notification(&self, method: &str, _: Value) {
            self.0.lock().push(method.to_string());
        }
        fn on_closed(&self, reason: String) {
            self.0.lock().push(format!("closed: {reason}"));
        }
    }

    #[test]
    fn malformed_lines_are_skipped_and_close_fails_pending() {
        let input = b"not json\n{\"jsonrpc\":\"2.0\",\"method\":\"a\"}\n[1,2]\n\n{\"jsonrpc\":\"2.0\",\"method\":\"b\",\"params\":{}}\n".to_vec();
        let rec = Arc::new(Recorder(Mutex::new(Vec::new())));
        let weak: Weak<dyn Handler> = Arc::downgrade(&(rec.clone() as Arc<dyn Handler>));
        let conn = Connection::new(Cursor::new(input), std::io::sink(), weak);
        for _ in 0..200 {
            if conn.is_closed() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(conn.is_closed());
        assert_eq!(conn.malformed_count(), 2);
        let seen = rec.0.lock().clone();
        assert_eq!(seen[..2], ["a".to_string(), "b".to_string()]);
        assert!(seen[2].starts_with("closed"));
        let err = conn.request("x", Value::Null, Some(Duration::from_secs(1)));
        assert!(matches!(err, Err(RpcError::Closed(_))));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("héllo", 2), "h");
        assert_eq!(truncate("abc", 10), "abc");
    }
}
