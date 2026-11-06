//! Model Context Protocol (MCP) server for Lector.
//!
//! Every command of the [`lector_app::CommandRegistry`] becomes an MCP **tool**
//! (`git.list_changes` → `git_list_changes`, the command's JSON schema as `inputSchema`),
//! and open documents / changesets are **resources** (`lector://document/<path>`,
//! `lector://changeset/<key>`). The transport is the stdio flavour of MCP: UTF-8 JSON-RPC 2.0
//! messages, one per line. JSON-RPC is handled directly with `serde_json`; there is no async
//! runtime. Nothing but protocol messages is ever written to the output (logs go to stderr).
//!
//! Policy:
//!
//! * Read commands are always listed and callable.
//! * Commands that write (`CommandSpec::writes`) are **neither listed nor callable** unless the
//!   server was built with [`ServerOptions::allow_writes`]. A call to one anyway (a client with a
//!   stale tool list) is answered with a tool error (`isError: true`) that says how to enable it.
//! * Every write goes through [`CommandRegistry::invoke`] with [`Source::Mcp`], so the activity
//!   journal records it with origin `mcp` (and the journal id is returned in `_meta.lector`).
//! * Tool calls run one at a time, in arrival order, on a worker thread, so a slow search never
//!   blocks `ping` and two writes can never race. `notifications/cancelled` skips a queued call
//!   or drops the response of a running one.
//!
//! The tool list is built from the registry on every `tools/list`, so commands registered later
//! (by other crates) appear without changes here.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};

use lector_app::commands::{CommandError, Source};
use lector_app::documents::DocumentStore;
use lector_app::{AppCore, Command, CommandContext};
use lector_workspace::Project;
use serde_json::{json, Map, Value};

/// Newest protocol revision this server implements.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

/// Protocol revisions this server can speak, newest first. A client asking for one of them
/// gets it; any other request is answered with [`LATEST_PROTOCOL_VERSION`] and the client
/// decides whether it can live with that.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// URI scheme of Lector resources.
pub const URI_SCHEME: &str = "lector";

// JSON-RPC 2.0 error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
/// MCP: the requested resource does not exist.
const RESOURCE_NOT_FOUND: i64 = -32002;

/// How the server behaves.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Expose (and allow calling) commands that write to files or the repository.
    pub allow_writes: bool,
    /// `serverInfo.name`.
    pub name: String,
    /// `serverInfo.version`.
    pub version: String,
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            allow_writes: false,
            name: "lector".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        }
    }
}

/// An MCP server over one [`AppCore`] and (optionally) one project.
pub struct McpServer {
    core: AppCore,
    project: Option<Project>,
    options: ServerOptions,
    /// Serializes tool execution (reads included: simple and predictable).
    exec: Mutex<()>,
}

/// A JSON-RPC error: code and message.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    /// JSON-RPC error code.
    pub code: i64,
    /// Human-readable message.
    pub message: String,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
        }
    }
}

type RpcResult = Result<Value, RpcError>;

/// The MCP tool name of a command id: `git.list_changes` → `git_list_changes`. Clients
/// restrict tool names to `[A-Za-z0-9_-]`, so every other character becomes `_`.
pub fn tool_name(command_id: &str) -> String {
    command_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl McpServer {
    /// A server for `core`; `project` is the folder the commands run against (`None`: no
    /// folder, most commands report that).
    pub fn new(core: AppCore, project: Option<Project>, options: ServerOptions) -> Self {
        McpServer {
            core,
            project,
            options,
            exec: Mutex::new(()),
        }
    }

    /// The options in effect.
    pub fn options(&self) -> &ServerOptions {
        &self.options
    }

    fn context(&self) -> CommandContext {
        CommandContext::new(self.core.clone(), self.project.clone(), Source::Mcp)
    }

    fn exec_guard(&self) -> MutexGuard<'_, ()> {
        // A panic inside a command is caught; a poisoned lock carries no broken state.
        self.exec.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ------------------------------------------------------------------ transport

    /// Serves JSON-RPC messages (one per line) from `input` until it ends, writing
    /// responses to `output`. Returns when the input is exhausted and all queued tool
    /// calls have been answered.
    pub fn serve<R: BufRead, W: Write + Send>(&self, input: R, output: W) -> io::Result<()> {
        let out = Mutex::new(output);
        let state = Shared::default();
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for job in rx {
                    if state.take_cancelled(&job.key) {
                        continue;
                    }
                    state.set_running(Some(job.key.clone()));
                    let response = self.finish_request(&job.id, self.call_tool(&job.params));
                    let dropped = state.take_cancelled(&job.key);
                    state.set_running(None);
                    if !dropped {
                        write_message(&out, &response, &state);
                    }
                }
            });
            let mut result = Ok(());
            for line in input.lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(err) => {
                        result = Err(err);
                        break;
                    }
                };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(line) {
                    Ok(Value::Array(batch)) => {
                        // Batches (2025-03-26 and earlier) are answered together, in order.
                        if batch.is_empty() {
                            write_message(
                                &out,
                                &error_response(&Value::Null, INVALID_REQUEST, "empty batch"),
                                &state,
                            );
                            continue;
                        }
                        let responses: Vec<Value> = batch
                            .iter()
                            .filter_map(|m| self.handle_message(m, Some(&state)))
                            .collect();
                        if !responses.is_empty() {
                            write_message(&out, &Value::Array(responses), &state);
                        }
                    }
                    Ok(message) => {
                        if let Some(job) = self.queue_if_tool_call(&message) {
                            state.register_pending(&job.key);
                            if tx.send(job).is_err() {
                                break;
                            }
                        } else if let Some(response) = self.handle_message(&message, Some(&state)) {
                            write_message(&out, &response, &state);
                        }
                    }
                    Err(err) => write_message(
                        &out,
                        &error_response(&Value::Null, PARSE_ERROR, &format!("parse error: {err}")),
                        &state,
                    ),
                }
                if state.output_closed() {
                    break;
                }
            }
            drop(tx); // the worker drains the queue and ends
            result
        })
    }

    /// A `tools/call` request that should run on the worker (single, non-batch messages).
    fn queue_if_tool_call(&self, message: &Value) -> Option<Job> {
        let obj = message.as_object()?;
        if obj.get("method")?.as_str()? != "tools/call" {
            return None;
        }
        let id = obj.get("id")?.clone();
        if !(id.is_string() || id.is_number()) {
            return None;
        }
        Some(Job {
            key: id.to_string(),
            id,
            params: obj.get("params").cloned().unwrap_or(Value::Null),
        })
    }

    /// Handles one JSON-RPC message synchronously. Returns the response, or `None` for
    /// notifications. (`tools/call` also runs here when it arrives inside a batch.)
    pub fn handle(&self, message: &Value) -> Option<Value> {
        self.handle_message(message, None)
    }

    fn handle_message(&self, message: &Value, state: Option<&Shared>) -> Option<Value> {
        let Some(obj) = message.as_object() else {
            return Some(error_response(
                &Value::Null,
                INVALID_REQUEST,
                "a JSON-RPC message must be an object",
            ));
        };
        let id = obj.get("id");
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            // A response to a request we never sent (we send none): ignore it.
            if id.is_some() && (obj.contains_key("result") || obj.contains_key("error")) {
                return None;
            }
            return Some(error_response(
                id.unwrap_or(&Value::Null),
                INVALID_REQUEST,
                "missing `method`",
            ));
        };
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            self.handle_notification(method, &params, state);
            return None;
        };
        if !(id.is_string() || id.is_number()) {
            return Some(error_response(
                &Value::Null,
                INVALID_REQUEST,
                "`id` must be a string or a number",
            ));
        }
        let result = match method {
            "initialize" => self.initialize(&params),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(self.list_tools()),
            "tools/call" => Ok(self.call_tool_checked(&params)),
            "resources/list" => Ok(self.list_resources()),
            "resources/templates/list" => Ok(self.list_resource_templates()),
            "resources/read" => self.read_resource(&params),
            "prompts/list" => Ok(json!({ "prompts": [] })),
            "logging/setLevel" => Ok(json!({})),
            other => Err(RpcError::new(
                METHOD_NOT_FOUND,
                format!("method not found: {other}"),
            )),
        };
        Some(self.finish_request(id, result))
    }

    fn finish_request(&self, id: &Value, result: RpcResult) -> Value {
        match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(err) => error_response(id, err.code, &err.message),
        }
    }

    fn handle_notification(&self, method: &str, params: &Value, state: Option<&Shared>) {
        match method {
            "notifications/cancelled" => {
                if let (Some(state), Some(id)) = (state, params.get("requestId")) {
                    state.cancel(&id.to_string());
                }
            }
            "notifications/initialized" | "notifications/roots/list_changed" => {}
            other => tracing::debug!("ignoring notification `{other}`"),
        }
    }

    // ------------------------------------------------------------------ lifecycle

    fn initialize(&self, params: &Value) -> RpcResult {
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        let version = match requested {
            Some(v) if SUPPORTED_PROTOCOL_VERSIONS.contains(&v) => v,
            _ => LATEST_PROTOCOL_VERSION,
        };
        if let Some(client) = params.get("clientInfo") {
            tracing::info!("MCP client: {client}");
        }
        let mut instructions = String::from(
            "Lector: a code reader and git review tool. Tools are Lector commands. Start with \
             git_list_changes (what changed, risk-ranked in the CLI) and git_show_diff, read files \
             with document_read, search with search_project. Paths are project-relative, lines are \
             0-based, ranges are [start, end).",
        );
        if self.options.allow_writes {
            instructions.push_str(
                " Write tools are enabled: every write is recorded in Lector's activity journal \
                 (origin mcp) and most are undoable from the Lector app or `lector undo <id>`.",
            );
        } else {
            instructions.push_str(" This server is read-only: tools that write are not available.");
        }
        Ok(json!({
            "protocolVersion": version,
            "capabilities": {
                "tools": { "listChanged": false },
                "resources": { "subscribe": false, "listChanged": false },
            },
            "serverInfo": {
                "name": self.options.name,
                "title": "Lector",
                "version": self.options.version,
            },
            "instructions": instructions,
        }))
    }

    // ------------------------------------------------------------------ tools

    fn visible(&self, command: &dyn Command) -> bool {
        self.options.allow_writes || !command.spec().writes
    }

    /// `tools/list`: one tool per visible registry command, rebuilt on every call.
    fn list_tools(&self) -> Value {
        let tools: Vec<Value> = self
            .core
            .registry()
            .list()
            .iter()
            .filter(|c| self.visible(c.as_ref()))
            .map(|c| tool_json(c.as_ref()))
            .collect();
        json!({ "tools": tools })
    }

    fn find_command(&self, name: &str) -> Option<Arc<dyn Command>> {
        self.core
            .registry()
            .list()
            .into_iter()
            .find(|c| tool_name(c.spec().id) == name)
    }

    /// `tools/call` outside the worker (batches): protocol errors become error results.
    fn call_tool_checked(&self, params: &Value) -> Value {
        match self.call_tool(params) {
            Ok(v) => v,
            Err(err) => tool_error(&err.message),
        }
    }

    /// Runs a tool. Unknown tools and malformed parameters are protocol errors
    /// (`-32602`); everything else, including refused writes and failing commands,
    /// is a result with `isError: true` so the model can read it.
    fn call_tool(&self, params: &Value) -> RpcResult {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::new(INVALID_PARAMS, "`name` (the tool name) is required"))?;
        let args = match params.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(v @ Value::Object(_)) => v.clone(),
            Some(_) => {
                return Err(RpcError::new(
                    INVALID_PARAMS,
                    "`arguments` must be an object",
                ))
            }
        };
        let command = self
            .find_command(name)
            .ok_or_else(|| RpcError::new(INVALID_PARAMS, format!("unknown tool: {name}")))?;
        let spec = command.spec();
        if spec.writes && !self.options.allow_writes {
            return Ok(tool_error(&format!(
                "`{}` writes to the user's files or repository, and this Lector MCP server is \
                 read-only. The user must restart it with `lector mcp --allow-writes` to enable \
                 write tools.",
                spec.id
            )));
        }
        let id = spec.id;
        let writes = spec.writes;
        let undoable = spec.undoable;
        let cx = self.context();
        let _guard = self.exec_guard();
        let result = catch_unwind(AssertUnwindSafe(|| {
            self.core.registry().invoke(id, args, &cx)
        }));
        Ok(match result {
            Ok(Ok(outcome)) => {
                let mut result = Map::new();
                let mut text = outcome.summary.clone();
                if !outcome.data.is_null() {
                    // Clients that ignore `structuredContent` still get the data.
                    let compact = serde_json::to_string(&outcome.data).unwrap_or_default();
                    if text.is_empty() {
                        text = compact;
                    } else {
                        text = format!("{text}\n{compact}");
                    }
                    let structured = if outcome.data.is_object() {
                        outcome.data.clone()
                    } else {
                        json!({ "result": outcome.data })
                    };
                    result.insert("structuredContent".into(), structured);
                }
                result.insert("content".into(), json!([{ "type": "text", "text": text }]));
                result.insert("isError".into(), json!(false));
                if writes {
                    let mut meta =
                        json!({ "command": id, "undoable": undoable && outcome.undo.is_some() });
                    if let Some(journal) = outcome.journal_id {
                        meta["journalId"] = json!(journal);
                    }
                    result.insert("_meta".into(), json!({ "lector": meta }));
                }
                Value::Object(result)
            }
            Ok(Err(err)) => tool_error(&command_error_text(&err)),
            Err(_) => tool_error(&format!("`{id}` crashed; this is a bug in Lector")),
        })
    }

    // ------------------------------------------------------------------ resources

    fn list_resources(&self) -> Value {
        let mut resources = Vec::new();
        if self.project.as_ref().is_some_and(|p| p.repo().is_some()) {
            for (key, title) in [
                (
                    "worktree",
                    "Working tree changes (HEAD to disk, including untracked)",
                ),
                ("staged", "Staged changes"),
                ("unstaged", "Unstaged changes"),
            ] {
                resources.push(json!({
                    "uri": format!("{URI_SCHEME}://changeset/{key}"),
                    "name": format!("changeset/{key}"),
                    "title": title,
                    "mimeType": "application/json",
                }));
            }
        }
        if let Some(project) = &self.project {
            for doc in DocumentStore::for_core(&self.core).documents() {
                let path = doc.path();
                let Some(rel) = project.relative_path(&path) else {
                    continue;
                };
                let mut v = json!({
                    "uri": document_uri(&rel),
                    "name": rel,
                    "mimeType": "text/plain",
                });
                if doc.is_dirty() {
                    v["description"] = json!("has unsaved changes");
                }
                resources.push(v);
            }
        }
        json!({ "resources": resources })
    }

    fn list_resource_templates(&self) -> Value {
        json!({ "resourceTemplates": [
            {
                "uriTemplate": format!("{URI_SCHEME}://document/{{path}}"),
                "name": "document",
                "title": "Project file (live buffer when open)",
                "mimeType": "text/plain",
            },
            {
                "uriTemplate": format!("{URI_SCHEME}://changeset/{{key}}"),
                "name": "changeset",
                "title": "Diff of a changeset: worktree, staged, unstaged, commit:<sha>, range:<a>..<b>, branch:<base>...<branch>",
                "mimeType": "application/json",
            },
        ]})
    }

    fn read_resource(&self, params: &Value) -> RpcResult {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::new(INVALID_PARAMS, "`uri` is required"))?;
        let not_found = |why: &str| {
            RpcError::new(
                RESOURCE_NOT_FOUND,
                format!("resource not found: {uri} ({why})"),
            )
        };
        let rest = uri
            .strip_prefix(&format!("{URI_SCHEME}://"))
            .ok_or_else(|| not_found("not a lector:// URI"))?;
        let (kind, tail) = rest.split_once('/').unwrap_or((rest, ""));
        let tail = percent_decode(tail);
        match kind {
            "document" => {
                let project = self
                    .project
                    .as_ref()
                    .ok_or_else(|| not_found("no folder is open"))?;
                let path = inside_project(project, &tail)
                    .ok_or_else(|| not_found("not a file inside the project"))?;
                let store = DocumentStore::for_core(&self.core);
                let _guard = self.exec_guard();
                let handle = match store.get(&path) {
                    Some(h) => h,
                    None => store
                        .open(&path, project)
                        .map_err(|e| not_found(&e.to_string()))?,
                };
                let text = handle
                    .text()
                    .ok_or_else(|| not_found("not a text file"))?
                    .to_string();
                Ok(json!({ "contents": [{
                    "uri": uri,
                    "mimeType": "text/plain",
                    "text": text,
                }]}))
            }
            "changeset" => {
                let key = if tail.is_empty() {
                    "worktree"
                } else {
                    tail.as_str()
                };
                let cx = self.context();
                let _guard = self.exec_guard();
                let outcome = self
                    .core
                    .registry()
                    .invoke("git.show_diff", json!({ "changeset": key }), &cx)
                    .map_err(|e| not_found(&command_error_text(&e)))?;
                Ok(json!({ "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": serde_json::to_string(&outcome.data).unwrap_or_default(),
                }]}))
            }
            _ => Err(not_found("unknown resource kind")),
        }
    }
}

// ---------------------------------------------------------------------- helpers

struct Job {
    key: String,
    id: Value,
    params: Value,
}

/// Cancellation and output state shared between the reader and the worker.
#[derive(Default)]
struct Shared {
    cancelled: Mutex<HashSet<String>>,
    pending: Mutex<HashSet<String>>,
    running: Mutex<Option<String>>,
    closed: AtomicBool,
}

impl Shared {
    fn register_pending(&self, key: &str) {
        lock(&self.pending).insert(key.to_owned());
    }

    /// `notifications/cancelled`: only requests that are queued or running can be cancelled.
    fn cancel(&self, key: &str) {
        let known =
            lock(&self.pending).contains(key) || lock(&self.running).as_deref() == Some(key);
        if known {
            lock(&self.cancelled).insert(key.to_owned());
        }
    }

    /// Whether `key` was cancelled (and forgets it).
    fn take_cancelled(&self, key: &str) -> bool {
        let cancelled = lock(&self.cancelled).remove(key);
        if cancelled {
            lock(&self.pending).remove(key);
        }
        cancelled
    }

    fn set_running(&self, key: Option<String>) {
        if key.is_none() {
            if let Some(done) = lock(&self.running).as_ref() {
                lock(&self.pending).remove(done);
            }
        }
        *lock(&self.running) = key;
    }

    fn output_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn write_message<W: Write>(out: &Mutex<W>, message: &Value, state: &Shared) {
    let mut line = match serde_json::to_string(message) {
        Ok(line) => line,
        Err(err) => {
            tracing::error!("cannot serialize a response: {err}");
            return;
        }
    };
    line.push('\n');
    let mut out = lock(out);
    if out
        .write_all(line.as_bytes())
        .and_then(|()| out.flush())
        .is_err()
    {
        // The client went away (broken pipe): stop reading.
        state.closed.store(true, Ordering::Relaxed);
    }
}

fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_error(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

fn command_error_text(err: &CommandError) -> String {
    match err {
        CommandError::Failed(e) => format!("{e:#}"),
        other => other.to_string(),
    }
}

/// The MCP description of a command.
fn tool_json(command: &dyn Command) -> Value {
    let spec = command.spec();
    let mut description = format!(
        "{} ({}). Lector command `{}`.",
        spec.title, spec.category, spec.id
    );
    if spec.writes {
        description.push_str(
            " Writes to the user's files or repository; recorded in the activity journal",
        );
        description.push_str(if spec.undoable {
            ", undoable."
        } else {
            ", not undoable."
        });
    } else {
        description.push_str(" Read-only.");
    }
    let mut annotations = json!({
        "title": spec.title,
        "readOnlyHint": !spec.writes,
        "openWorldHint": false,
    });
    if spec.writes {
        annotations["destructiveHint"] = json!(spec.destructive);
        annotations["idempotentHint"] = json!(false);
    }
    let mut schema = spec.args_schema.clone();
    if !schema.is_object() {
        schema = json!({ "type": "object" });
    }
    json!({
        "name": tool_name(spec.id),
        "title": spec.title,
        "description": description,
        "inputSchema": schema,
        "annotations": annotations,
    })
}

/// `lector://document/<percent-encoded project-relative path>`.
pub fn document_uri(rel_path: &str) -> String {
    format!("{URI_SCHEME}://document/{}", percent_encode(rel_path))
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The absolute path of project-relative `rel`, if it is an existing file inside the
/// project root after resolving symlinks (no `..` escapes), outside `.git`.
fn inside_project(project: &Project, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() {
        return None;
    }
    let path = project.absolute_path(rel);
    let canonical = dunce::canonicalize(&path).ok()?;
    let root = dunce::canonicalize(project.root()).ok()?;
    // `.git` holds credentials (remote URLs, config) and is not project content.
    let in_git_dir = canonical.strip_prefix(&root).is_ok_and(|rel| {
        rel.components()
            .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(".git"))
    });
    (canonical.starts_with(&root) && !in_git_dir && Path::new(&canonical).is_file())
        .then_some(canonical)
}
