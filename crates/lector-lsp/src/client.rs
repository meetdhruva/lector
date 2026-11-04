//! JSON-RPC transport over a child process's stdio.

use std::collections::HashMap;
use std::future::Future;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use futures::channel::oneshot;
use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::error::{LspError, Result};

struct Pending {
    tx: oneshot::Sender<Result<Value>>,
    method: String,
    deadline: Instant,
}

/// Receives server → client traffic the transport does not answer itself.
pub(crate) trait ServerHandler: Send + Sync + 'static {
    /// A notification (after the built-in logging).
    fn notification(&self, method: &str, params: Option<&Value>);
    /// A request. Take the responder to answer (possibly later, from any
    /// thread) and return `None`; return it back for the built-in answer.
    fn request(
        &self,
        method: &str,
        params: Option<&Value>,
        responder: Responder,
    ) -> Option<Responder>;
}

/// Answers one server → client request. Dropping it unanswered replies with
/// the result given to [`Responder::with_default`] (or an error).
pub(crate) struct Responder {
    shared: Weak<Shared>,
    id: Value,
    default: Option<Value>,
    done: bool,
}

impl Responder {
    /// Result sent if the responder is dropped without an answer.
    pub(crate) fn with_default(mut self, result: Value) -> Self {
        self.default = Some(result);
        self
    }

    /// Marks the request as answered elsewhere (no reply on drop).
    fn disarm(&mut self) {
        self.done = true;
    }

    /// Sends `result`.
    pub(crate) fn respond(mut self, result: Value) {
        self.send(Some(result));
    }

    fn send(&mut self, result: Option<Value>) {
        if std::mem::replace(&mut self.done, true) {
            return;
        }
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        let msg = match result {
            Some(r) => json!({"jsonrpc": "2.0", "id": self.id, "result": r}),
            None => json!({"jsonrpc": "2.0", "id": self.id,
                "error": {"code": -32603, "message": "request dropped by the client"}}),
        };
        shared.send(&msg);
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        let default = self.default.take();
        self.send(default);
    }
}

struct Shared {
    tx: Sender<Vec<u8>>,
    pending: Mutex<HashMap<i64, Pending>>,
    closed: AtomicBool,
    root_uri: String,
    /// `workspace/configuration` section name and value (read-only settings).
    settings: Option<(String, Value)>,
    handler: Option<Arc<dyn ServerHandler>>,
}

impl Shared {
    fn send(&self, msg: &Value) -> bool {
        let body = match serde_json::to_vec(msg) {
            Ok(b) => b,
            Err(_) => return false,
        };
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(&body);
        self.tx.send(frame).is_ok()
    }

    fn fail_all(&self, make: impl Fn(&Pending) -> LspError) {
        let drained: Vec<_> = self.pending.lock().drain().collect();
        for (_, p) in drained {
            let err = make(&p);
            let _ = p.tx.send(Err(err));
        }
    }
}

/// A running, connected language server process.
pub(crate) struct Client {
    shared: Arc<Shared>,
    next_id: AtomicI64,
    child: Mutex<Child>,
    timeout: Duration,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.kill();
    }
}

impl Client {
    /// Spawn the process and start the I/O threads. `on_exit` runs on the
    /// reader thread once the server's stdout closes.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
        command: &Path,
        args: &[String],
        root: &Path,
        root_uri: String,
        timeout: Duration,
        label: &str,
        handler: Option<Arc<dyn ServerHandler>>,
        on_exit: impl FnOnce() + Send + 'static,
    ) -> Result<Client> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if root.is_dir() {
            cmd.current_dir(root);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| LspError::Io(format!("failed to start {}: {e}", command.display())))?;
        let (stdin, stdout, stderr) =
            match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
                (Some(i), Some(o), Some(e)) => (i, o, e),
                _ => {
                    let _ = child.kill();
                    return Err(LspError::Io("missing child pipes".into()));
                }
            };
        let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let shared = Arc::new(Shared {
            tx,
            pending: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            root_uri,
            settings: crate::config::read_only_settings(label).map(|v| (label.to_string(), v)),
            handler,
        });

        let name = label.to_string();
        std::thread::Builder::new()
            .name(format!("lsp-write-{name}"))
            .spawn(move || {
                let mut stdin = stdin;
                while let Ok(frame) = rx.recv() {
                    if stdin.write_all(&frame).and_then(|_| stdin.flush()).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| LspError::Io(e.to_string()))?;

        let name = label.to_string();
        std::thread::Builder::new()
            .name(format!("lsp-err-{name}"))
            .spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    match line {
                        Ok(l) => tracing::debug!(target: "lector_lsp::stderr", "[{name}] {l}"),
                        Err(_) => break,
                    }
                }
            })
            .map_err(|e| LspError::Io(e.to_string()))?;

        let reader_shared = shared.clone();
        let name = label.to_string();
        std::thread::Builder::new()
            .name(format!("lsp-read-{name}"))
            .spawn(move || {
                read_loop(BufReader::new(stdout), &reader_shared, &name);
                reader_shared.closed.store(true, Ordering::SeqCst);
                reader_shared.fail_all(|_| LspError::ServerExited);
                on_exit();
            })
            .map_err(|e| LspError::Io(e.to_string()))?;

        let weak: Weak<Shared> = Arc::downgrade(&shared);
        std::thread::Builder::new()
            .name(format!("lsp-timeout-{label}"))
            .spawn(move || watchdog(weak))
            .map_err(|e| LspError::Io(e.to_string()))?;

        Ok(Client {
            shared,
            next_id: AtomicI64::new(1),
            child: Mutex::new(child),
            timeout,
        })
    }

    /// Send a request; the future resolves with the raw `result` value.
    pub(crate) fn request(
        &self,
        method: &str,
        params: Value,
    ) -> impl std::future::Future<Output = Result<Value>> + Send + 'static {
        self.request_with_timeout(method, params, self.timeout)
    }

    pub(crate) fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> impl std::future::Future<Output = Result<Value>> + Send + 'static {
        let (tx, rx) = oneshot::channel();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let immediate = if self.shared.closed.load(Ordering::SeqCst) {
            Some(LspError::ServerExited)
        } else {
            self.shared.pending.lock().insert(
                id,
                Pending {
                    tx,
                    method: method.to_string(),
                    deadline: Instant::now() + timeout,
                },
            );
            let ok = self
                .shared
                .send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
            if ok {
                None
            } else {
                self.shared.pending.lock().remove(&id);
                Some(LspError::ServerExited)
            }
        };
        async move {
            if let Some(e) = immediate {
                return Err(e);
            }
            rx.await.unwrap_or(Err(LspError::ServerExited))
        }
    }

    /// Send a notification (fire and forget).
    pub(crate) fn notify(&self, method: &str, params: Value) {
        self.shared
            .send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    /// Polite shutdown: `shutdown` request, `exit` notification, then kill
    /// after `grace`. Blocks the calling thread.
    pub(crate) fn shutdown(&self, grace: Duration) {
        if !self.is_closed() {
            let mut fut = Box::pin(self.request_with_timeout("shutdown", Value::Null, grace));
            let deadline = Instant::now() + grace;
            let waker = futures::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            while Instant::now() < deadline {
                if fut.as_mut().poll(&mut cx).is_ready() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            self.notify("exit", Value::Null);
            while Instant::now() < deadline {
                if matches!(self.child.lock().try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        self.kill();
    }

    pub(crate) fn kill(&self) {
        let mut c = self.child.lock();
        let _ = c.kill();
        let _ = c.wait();
    }
}

fn watchdog(weak: Weak<Shared>) {
    loop {
        std::thread::sleep(Duration::from_millis(25));
        let Some(shared) = weak.upgrade() else { return };
        if shared.closed.load(Ordering::SeqCst) {
            return;
        }
        let now = Instant::now();
        let expired: Vec<i64> = shared
            .pending
            .lock()
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            let p = shared.pending.lock().remove(&id);
            if let Some(p) = p {
                shared.send(
                    &json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": id}}),
                );
                let _ = p.tx.send(Err(LspError::Timeout(p.method)));
            }
        }
    }
}

fn read_message<R: BufRead>(r: &mut R) -> Option<Value> {
    loop {
        let mut len: Option<usize> = None;
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    len = v.trim().parse().ok();
                }
            }
        }
        let Some(len) = len else { continue };
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf).ok()?;
        match serde_json::from_slice(&buf) {
            Ok(v) => return Some(v),
            Err(e) => tracing::warn!("invalid JSON from language server: {e}"),
        }
    }
}

fn read_loop<R: BufRead>(mut r: R, shared: &Arc<Shared>, name: &str) {
    while let Some(msg) = read_message(&mut r) {
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id");
        match (method, id) {
            (Some(method), Some(id)) => {
                let mut responder = Responder {
                    shared: Arc::downgrade(shared),
                    id: id.clone(),
                    default: None,
                    done: false,
                };
                if let Some(handler) = &shared.handler {
                    match handler.request(method, msg.get("params"), responder) {
                        Some(back) => responder = back,
                        None => continue,
                    }
                }
                responder.disarm();
                let result = handle_server_request(
                    method,
                    msg.get("params"),
                    &shared.root_uri,
                    shared.settings.as_ref(),
                );
                let reply = match result {
                    Some(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
                    None => json!({"jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": format!("method not supported: {method}")}}),
                };
                shared.send(&reply);
            }
            (Some(method), None) => {
                handle_notification(method, msg.get("params"), name);
                if let Some(handler) = &shared.handler {
                    handler.notification(method, msg.get("params"));
                }
            }
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else { continue };
                let p = shared.pending.lock().remove(&id);
                if let Some(p) = p {
                    let out = if let Some(err) = msg.get("error") {
                        Err(LspError::Server {
                            code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                            message: err
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        })
                    } else {
                        Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                    };
                    let _ = p.tx.send(out);
                }
            }
            (None, None) => {}
        }
    }
}

/// Minimal answers to server → client requests; `None` = unsupported.
fn handle_server_request(
    method: &str,
    params: Option<&Value>,
    root_uri: &str,
    settings: Option<&(String, Value)>,
) -> Option<Value> {
    match method {
        "workspace/configuration" => {
            let items = params
                .and_then(|p| p.get("items"))
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            Some(Value::Array(
                items
                    .iter()
                    .map(|item| match settings {
                        Some((section, value))
                            if item.get("section").and_then(Value::as_str)
                                == Some(section.as_str()) =>
                        {
                            value.clone()
                        }
                        _ => Value::Null,
                    })
                    .collect(),
            ))
        }
        "workspace/workspaceFolders" => Some(json!([{"uri": root_uri, "name": "root"}])),
        "client/registerCapability"
        | "client/unregisterCapability"
        | "window/workDoneProgress/create"
        | "window/showMessageRequest"
        | "window/showDocument" => Some(if method == "window/showDocument" {
            json!({"success": false})
        } else {
            Value::Null
        }),
        _ => None,
    }
}

fn handle_notification(method: &str, params: Option<&Value>, name: &str) {
    match method {
        "window/logMessage" | "window/showMessage" => {
            let msg = params
                .and_then(|p| p.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let level = params
                .and_then(|p| p.get("type"))
                .and_then(Value::as_i64)
                .unwrap_or(4);
            match level {
                1 => tracing::warn!(target: "lector_lsp::server", "[{name}] {msg}"),
                2 => tracing::info!(target: "lector_lsp::server", "[{name}] {msg}"),
                _ => tracing::debug!(target: "lector_lsp::server", "[{name}] {msg}"),
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_configuration_answers_own_section() {
        let settings = ("rust-analyzer".to_string(), json!({"checkOnSave": false}));
        let params = json!({"items": [{"section": "rust-analyzer"}, {"section": "other"}, {}]});
        let reply = handle_server_request(
            "workspace/configuration",
            Some(&params),
            "file:///r",
            Some(&settings),
        )
        .expect("reply");
        assert_eq!(reply, json!([{"checkOnSave": false}, null, null]));
        let reply =
            handle_server_request("workspace/configuration", Some(&params), "file:///r", None)
                .expect("reply");
        assert_eq!(reply, json!([null, null, null]));
        // Edits are never applied.
        assert!(handle_server_request("workspace/applyEdit", None, "file:///r", None).is_none());
    }
}
