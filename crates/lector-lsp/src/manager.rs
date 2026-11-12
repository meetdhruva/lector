//! Per-root server management: lazy start, init queueing, restart, shutdown.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::client::{Client, Responder, ServerHandler};
use crate::config::{language_id, which, ServerConfig};
use crate::convert;
use crate::error::{LspError, Result};
use crate::types::{
    CodeAction, Command, DocumentDiagnostics, DocumentState, DocumentSymbol, FormattingOptions,
    HoverInfo, Location, PrepareRename, ServerCapabilities, TextEdit, WorkspaceEdit,
    WorkspaceSymbol,
};
use crate::PointUtf16;

mod completion_api;

/// Tunables; the defaults suit real servers, tests shorten them.
#[derive(Debug, Clone)]
pub struct LspOptions {
    /// Per-request timeout.
    pub request_timeout: Duration,
    /// Timeout for the `initialize` handshake.
    pub init_timeout: Duration,
    /// First restart delay; doubles per consecutive crash.
    pub restart_base_delay: Duration,
    /// Consecutive crashes tolerated before the server is marked failed.
    pub max_restarts: u32,
    /// How long `shutdown` waits before killing the process.
    pub shutdown_grace: Duration,
}

impl Default for LspOptions {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(10),
            init_timeout: Duration::from_secs(30),
            restart_base_delay: Duration::from_millis(500),
            max_restarts: 3,
            shutdown_grace: Duration::from_secs(2),
        }
    }
}

/// Lifecycle state of one server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    /// Launching or restarting; requests are queued.
    Starting,
    /// Initialized and serving.
    Running,
    /// Gave up (spawn failure or repeated crashes).
    Failed(String),
    /// A language needs it but the executable is not on PATH.
    NotInstalled,
}

/// Status line for one server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    /// Server name from its [`ServerConfig`].
    pub name: String,
    /// Current state.
    pub state: ServerState,
}

struct Doc {
    language: String,
    text: String,
    version: i32,
    tag: Option<u64>,
}

/// A server's `workspace/applyEdit` request, delivered to the handler set
/// with [`LspManager::set_apply_edit_handler`]. Answer with
/// [`ApplyEditRequest::respond`]; dropping it answers `applied: false`.
pub struct ApplyEditRequest {
    /// Name of the requesting server.
    pub server: String,
    /// Optional label (e.g. for an undo entry).
    pub label: Option<String>,
    /// The edit to apply.
    pub edit: WorkspaceEdit,
    responder: Responder,
}

impl std::fmt::Debug for ApplyEditRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApplyEditRequest")
            .field("server", &self.server)
            .field("label", &self.label)
            .field("edit", &self.edit)
            .finish()
    }
}

impl ApplyEditRequest {
    /// Replies to the server (`applied`, and why not).
    pub fn respond(self, applied: bool, failure_reason: Option<String>) {
        let mut result = json!({ "applied": applied });
        if let Some(reason) = failure_reason {
            result["failureReason"] = Value::String(reason);
        }
        self.responder.respond(result);
    }
}

type DiagnosticsListener = Arc<dyn Fn(&Path) + Send + Sync>;
type ApplyEditHandler = Arc<dyn Fn(ApplyEditRequest) + Send + Sync>;

struct StoredDiagnostics {
    public: DocumentDiagnostics,
    /// Raw JSON of each diagnostic (code action contexts).
    raw: Vec<Value>,
}

/// State shared by all servers of a manager: diagnostics and UI hooks.
#[derive(Default)]
struct Events {
    diagnostics: Mutex<HashMap<PathBuf, StoredDiagnostics>>,
    listeners: Mutex<Vec<DiagnosticsListener>>,
    apply_edit: Mutex<Option<ApplyEditHandler>>,
    /// Completion / signature help: in-flight requests and the snippets setting.
    completion: completion_api::CompletionState,
}

impl Events {
    fn notify(&self, path: &Path) {
        let listeners = self.listeners.lock().clone();
        for l in listeners {
            l(path);
        }
    }

    /// Drops diagnostics published by `server` (it exited) or for `path`.
    fn clear(&self, server: Option<&str>, path: Option<&Path>) {
        let removed: Vec<PathBuf> = {
            let mut map = self.diagnostics.lock();
            let keys: Vec<PathBuf> = map
                .iter()
                .filter(|(p, d)| {
                    server.is_none_or(|s| d.public.server == s)
                        && path.is_none_or(|q| q == p.as_path())
                })
                .map(|(p, _)| p.clone())
                .collect();
            for k in &keys {
                map.remove(k);
            }
            keys
        };
        for p in removed {
            self.notify(&p);
        }
    }
}

/// Routes server → client traffic of one slot.
struct SlotHandler {
    slot: std::sync::Weak<Slot>,
}

impl ServerHandler for SlotHandler {
    fn notification(&self, method: &str, params: Option<&Value>) {
        if method != "textDocument/publishDiagnostics" {
            return;
        }
        let (Some(slot), Some(params)) = (self.slot.upgrade(), params) else {
            return;
        };
        let Some((path, version, diags)) = convert::publish_diagnostics(params) else {
            return;
        };
        let tag = {
            let g = slot.inner.lock();
            match (g.docs.get(&path), version) {
                (Some(doc), Some(v)) if doc.version == v => doc.tag,
                (Some(doc), None) => doc.tag,
                _ => None,
            }
        };
        let (public, raw): (Vec<_>, Vec<_>) = diags.into_iter().unzip();
        let stored = StoredDiagnostics {
            public: DocumentDiagnostics {
                path: path.clone(),
                server: slot.config.name.clone(),
                version,
                tag,
                diagnostics: public,
            },
            raw,
        };
        slot.events.diagnostics.lock().insert(path.clone(), stored);
        slot.events.notify(&path);
    }

    fn request(
        &self,
        method: &str,
        params: Option<&Value>,
        responder: Responder,
    ) -> Option<Responder> {
        if method != "workspace/applyEdit" {
            return Some(responder);
        }
        let responder = responder.with_default(json!({"applied": false}));
        // A request for a slot that is gone: dropping answers `applied: false`.
        let slot = self.slot.upgrade()?;
        let params = params.cloned().unwrap_or(Value::Null);
        let edit = match convert::workspace_edit(params.get("edit").unwrap_or(&Value::Null)) {
            Ok(e) => e,
            Err(e) => {
                responder.respond(json!({"applied": false, "failureReason": e.to_string()}));
                return None;
            }
        };
        let handler = slot.events.apply_edit.lock().clone();
        match handler {
            Some(h) => h(ApplyEditRequest {
                server: slot.config.name.clone(),
                label: params
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                edit,
                responder,
            }),
            None => responder.respond(
                json!({"applied": false, "failureReason": "the client cannot apply edits now"}),
            ),
        }
        None
    }
}

enum Phase {
    Idle,
    Starting,
    Running(Arc<Client>),
    Restarting,
    Failed(String),
    NotInstalled,
    Shutdown,
}

struct SlotInner {
    phase: Phase,
    docs: HashMap<PathBuf, Doc>,
    waiters: Vec<oneshot::Sender<Option<Arc<Client>>>>,
    generation: u64,
    attempts: u32,
    running_since: Option<Instant>,
    caps: Option<ServerCapabilities>,
}

struct Slot {
    config: ServerConfig,
    resolved: Mutex<Option<Option<PathBuf>>>,
    inner: Mutex<SlotInner>,
    root: PathBuf,
    root_uri: String,
    opts: LspOptions,
    events: Arc<Events>,
}

fn client_capabilities() -> Value {
    json!({
        "general": {"positionEncodings": ["utf-16"]},
        "window": {"workDoneProgress": true},
        "workspace": {
            "configuration": true,
            "workspaceFolders": true,
            "symbol": {"dynamicRegistration": false},
            "applyEdit": true,
            "workspaceEdit": {
                "documentChanges": true,
                "resourceOperations": [],
                "failureHandling": "abort",
                "normalizesLineEndings": false
            },
            "executeCommand": {"dynamicRegistration": false}
        },
        "textDocument": {
            "synchronization": {"dynamicRegistration": false, "didSave": false},
            "publishDiagnostics": {
                "relatedInformation": true,
                "tagSupport": {"valueSet": [1, 2]},
                "versionSupport": true,
                "codeDescriptionSupport": false,
                "dataSupport": true
            },
            "formatting": {"dynamicRegistration": false},
            "rangeFormatting": {"dynamicRegistration": false},
            "rename": {"dynamicRegistration": false, "prepareSupport": true,
                       "prepareSupportDefaultBehavior": 1},
            "codeAction": {
                "dynamicRegistration": false,
                "codeActionLiteralSupport": {"codeActionKind": {"valueSet": [
                    "", "quickfix", "refactor", "refactor.extract", "refactor.inline",
                    "refactor.rewrite", "source", "source.organizeImports", "source.fixAll"
                ]}},
                "isPreferredSupport": true,
                "disabledSupport": true,
                "dataSupport": true,
                "resolveSupport": {"properties": ["edit"]}
            },
            "hover": {"dynamicRegistration": false, "contentFormat": ["markdown", "plaintext"]},
            "definition": {"dynamicRegistration": false, "linkSupport": true},
            "references": {"dynamicRegistration": false},
            "documentSymbol": {
                "dynamicRegistration": false,
                "hierarchicalDocumentSymbolSupport": true,
                "symbolKind": {"valueSet": (1..=26).collect::<Vec<i32>>()}
            }
        }
    })
}

fn send_open(client: &Client, path: &Path, doc: &Doc) {
    if let Some(uri) = convert::path_to_uri(path) {
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri,
                "languageId": language_id(&doc.language),
                "version": doc.version,
                "text": doc.text,
            }}),
        );
    }
}

impl Slot {
    fn resolve(&self) -> Option<PathBuf> {
        self.resolved
            .lock()
            .get_or_insert_with(|| which(&self.config.command))
            .clone()
    }

    fn status(&self) -> Option<ServerState> {
        Some(match &self.inner.lock().phase {
            Phase::Idle | Phase::Shutdown => return None,
            Phase::Starting | Phase::Restarting => ServerState::Starting,
            Phase::Running(_) => ServerState::Running,
            Phase::Failed(m) => ServerState::Failed(m.clone()),
            Phase::NotInstalled => ServerState::NotInstalled,
        })
    }

    /// Begin launching (caller holds the lock and has set `phase`).
    fn spawn_launch(self: &Arc<Self>, delay: Duration) {
        let slot = self.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("lsp-launch-{}", self.config.name))
            .spawn(move || slot.launch(delay));
        if let Err(e) = spawned {
            tracing::error!("cannot spawn launch thread: {e}");
        }
    }

    fn set_failed(&self, msg: String) {
        let waiters = {
            let mut g = self.inner.lock();
            if matches!(g.phase, Phase::Shutdown) {
                return;
            }
            g.generation += 1;
            g.phase = Phase::Failed(msg);
            std::mem::take(&mut g.waiters)
        };
        for w in waiters {
            let _ = w.send(None);
        }
    }

    fn launch(self: Arc<Self>, delay: Duration) {
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        let gen = {
            let mut g = self.inner.lock();
            if !matches!(g.phase, Phase::Starting | Phase::Restarting) {
                return;
            }
            g.phase = Phase::Starting;
            g.generation
        };
        let Some(cmd) = self.resolve() else {
            {
                let mut g = self.inner.lock();
                if !matches!(g.phase, Phase::Shutdown) {
                    g.phase = Phase::NotInstalled;
                }
            }
            self.set_not_installed();
            return;
        };
        let on_exit = {
            let slot = self.clone();
            move || slot.handle_exit(gen)
        };
        let client = match Client::spawn(
            &cmd,
            &self.config.args,
            &self.root,
            self.root_uri.clone(),
            self.opts.request_timeout,
            &self.config.name,
            Some(Arc::new(SlotHandler {
                slot: Arc::downgrade(&self),
            })),
            on_exit,
        ) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                tracing::warn!("{}: {e}", self.config.name);
                self.set_failed(e.to_string());
                return;
            }
        };
        let mut params = json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "lector", "version": env!("CARGO_PKG_VERSION")},
            "rootUri": self.root_uri,
            "workspaceFolders": [{"uri": self.root_uri, "name": "root"}],
            "capabilities": client_capabilities(),
            "trace": "off",
        });
        crate::completion::extend_client_capabilities(
            &mut params["capabilities"],
            self.events.completion.snippets(),
        );
        if let Some(options) = crate::config::read_only_settings(&self.config.name) {
            params["initializationOptions"] = options;
        }
        let res = futures::executor::block_on(client.request_with_timeout(
            "initialize",
            params,
            self.opts.init_timeout,
        ));
        let caps = match res {
            Ok(result) => convert::server_capabilities(&result),
            Err(e) => {
                tracing::warn!("{}: initialize failed: {e}", self.config.name);
                client.kill();
                self.handle_exit(gen);
                return;
            }
        };
        client.notify("initialized", json!({}));
        let waiters = {
            let mut g = self.inner.lock();
            if g.generation != gen || client.is_closed() {
                // Shut down or crashed meanwhile; the exit path owns recovery.
                client.kill();
                return;
            }
            for (path, doc) in &g.docs {
                send_open(&client, path, doc);
            }
            g.phase = Phase::Running(client.clone());
            g.caps = Some(caps);
            g.running_since = Some(Instant::now());
            std::mem::take(&mut g.waiters)
        };
        for w in waiters {
            let _ = w.send(Some(client.clone()));
        }
    }

    fn set_not_installed(&self) {
        let waiters = std::mem::take(&mut self.inner.lock().waiters);
        for w in waiters {
            let _ = w.send(None);
        }
    }

    /// The server process went away (or failed to initialize).
    fn handle_exit(self: &Arc<Self>, gen: u64) {
        let (delay, give_up, waiters) = {
            let mut g = self.inner.lock();
            if g.generation != gen || matches!(g.phase, Phase::Shutdown) {
                return;
            }
            g.generation += 1;
            if g.running_since
                .take()
                .is_some_and(|t| t.elapsed() > Duration::from_secs(30))
            {
                g.attempts = 0;
            }
            g.attempts += 1;
            if g.attempts > self.opts.max_restarts {
                g.phase = Phase::Failed("language server exited repeatedly".into());
                (Duration::ZERO, true, std::mem::take(&mut g.waiters))
            } else {
                g.phase = Phase::Restarting;
                let shift = (g.attempts - 1).min(16);
                (
                    self.opts.restart_base_delay * (1u32 << shift),
                    false,
                    vec![],
                )
            }
        };
        tracing::warn!("{} exited; give_up={give_up}", self.config.name);
        self.events.clear(Some(&self.config.name), None);
        if give_up {
            for w in waiters {
                let _ = w.send(None);
            }
        } else {
            self.spawn_launch(delay);
        }
    }

    async fn wait_ready(self: Arc<Self>) -> Option<Arc<Client>> {
        let rx = {
            let mut g = self.inner.lock();
            match &g.phase {
                Phase::Running(c) => return Some(c.clone()),
                Phase::Starting | Phase::Restarting => {
                    let (tx, rx) = oneshot::channel();
                    g.waiters.push(tx);
                    rx
                }
                _ => return None,
            }
        };
        rx.await.ok().flatten()
    }

    fn shutdown_take(&self) -> Option<Arc<Client>> {
        let (client, waiters) = {
            let mut g = self.inner.lock();
            g.generation += 1;
            let old = std::mem::replace(&mut g.phase, Phase::Shutdown);
            g.docs.clear();
            g.caps = None;
            let client = match old {
                Phase::Running(c) => Some(c),
                _ => None,
            };
            (client, std::mem::take(&mut g.waiters))
        };
        for w in waiters {
            let _ = w.send(None);
        }
        client
    }
}

async fn dispatch(
    slot: Option<Arc<Slot>>,
    method: &'static str,
    params: Value,
) -> Result<Option<Value>> {
    let Some(slot) = slot else { return Ok(None) };
    match slot.wait_ready().await {
        Some(client) => client.request(method, params).await.map(Some),
        None => Ok(None),
    }
}

/// A prepared request to the server of one document.
struct DocRequest {
    target: Option<(Arc<Slot>, Value)>,
    method: &'static str,
}

impl DocRequest {
    async fn send(self) -> Result<Option<Value>> {
        let Some((slot, p)) = self.target else {
            return Ok(None);
        };
        dispatch(Some(slot), self.method, p).await
    }
}

struct Inner {
    root: PathBuf,
    slots: Vec<Arc<Slot>>,
    events: Arc<Events>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        for s in &self.slots {
            if let Some(c) = s.shutdown_take() {
                c.kill();
            }
        }
    }
}

/// Manages the language servers of one project root.
#[derive(Clone)]
pub struct LspManager {
    inner: Arc<Inner>,
}

fn formatting_options(o: FormattingOptions) -> Value {
    json!({"tabSize": o.tab_size, "insertSpaces": o.insert_spaces,
           "trimTrailingWhitespace": false, "insertFinalNewline": false})
}

fn pos_params(uri: &str, pos: PointUtf16) -> Value {
    json!({"textDocument": {"uri": uri}, "position": {"line": pos.row, "character": pos.column}})
}

impl LspManager {
    /// Manager for `root` using `configs` in priority order.
    pub fn new(root: PathBuf, configs: Vec<ServerConfig>) -> Self {
        Self::with_options(root, configs, LspOptions::default())
    }

    /// Like [`LspManager::new`] with explicit [`LspOptions`].
    pub fn with_options(root: PathBuf, configs: Vec<ServerConfig>, opts: LspOptions) -> Self {
        let root_uri = convert::path_to_uri(&root).unwrap_or_else(|| "file:///".into());
        let events = Arc::new(Events::default());
        let slots = configs
            .into_iter()
            .map(|config| {
                Arc::new(Slot {
                    config,
                    resolved: Mutex::new(None),
                    inner: Mutex::new(SlotInner {
                        phase: Phase::Idle,
                        docs: HashMap::new(),
                        waiters: vec![],
                        generation: 0,
                        attempts: 0,
                        running_since: None,
                        caps: None,
                    }),
                    root: root.clone(),
                    root_uri: root_uri.clone(),
                    opts: opts.clone(),
                    events: events.clone(),
                })
            })
            .collect();
        Self {
            inner: Arc::new(Inner {
                root,
                slots,
                events,
            }),
        }
    }

    /// The project root.
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Servers that were needed so far and their state.
    pub fn status(&self) -> Vec<ServerStatus> {
        self.inner
            .slots
            .iter()
            .filter_map(|s| {
                s.status().map(|state| ServerStatus {
                    name: s.config.name.clone(),
                    state,
                })
            })
            .collect()
    }

    fn slot_for_language(&self, language: &str) -> Option<Arc<Slot>> {
        let mut first_missing: Option<&Arc<Slot>> = None;
        for slot in &self.inner.slots {
            if !slot.config.languages.iter().any(|l| l == language) {
                continue;
            }
            if slot.resolve().is_some() {
                return Some(slot.clone());
            }
            first_missing.get_or_insert(slot);
        }
        if let Some(slot) = first_missing {
            let mut g = slot.inner.lock();
            if matches!(g.phase, Phase::Idle) {
                g.phase = Phase::NotInstalled;
            }
        }
        None
    }

    fn slot_for_path(&self, path: &Path) -> Option<Arc<Slot>> {
        self.inner
            .slots
            .iter()
            .find(|s| s.inner.lock().docs.contains_key(path))
            .cloned()
    }

    /// Tell the server for `language` about a document, starting it on first use.
    /// Re-opening with identical text is a no-op; with new text it syncs a change.
    pub fn open_document(&self, path: &Path, language: &str, text: &str) {
        self.sync(path, language, text, None);
    }

    /// [`LspManager::open_document`] remembering `tag` (e.g. the editor's
    /// text version) for the synced text: diagnostics published for it carry
    /// the tag ([`DocumentDiagnostics::tag`]) and [`LspManager::document_state`]
    /// reports it.
    pub fn open_document_tagged(&self, path: &Path, language: &str, text: &str, tag: u64) {
        self.sync(path, language, text, Some(tag));
    }

    fn sync(&self, path: &Path, language: &str, text: &str, tag: Option<u64>) {
        // A path belongs to one server: reuse the slot that already has it.
        let slot = match self.slot_for_path(path) {
            Some(s) => s,
            None => match self.slot_for_language(language) {
                Some(s) => s,
                None => return,
            },
        };
        let mut g = slot.inner.lock();
        if matches!(
            g.phase,
            Phase::Failed(_) | Phase::NotInstalled | Phase::Shutdown
        ) {
            return;
        }
        let client = match &g.phase {
            Phase::Running(c) => Some(c.clone()),
            _ => None,
        };
        match g.docs.get_mut(path) {
            Some(doc) if doc.text == text => {
                if tag.is_some() {
                    doc.tag = tag;
                }
                return;
            }
            Some(doc) => {
                doc.version += 1;
                doc.tag = tag;
                doc.text = text.to_string();
                if let (Some(c), Some(uri)) = (&client, convert::path_to_uri(path)) {
                    c.notify(
                        "textDocument/didChange",
                        json!({"textDocument": {"uri": uri, "version": doc.version},
                               "contentChanges": [{"text": doc.text}]}),
                    );
                }
            }
            None => {
                let doc = Doc {
                    language: language.to_string(),
                    text: text.to_string(),
                    version: 1,
                    tag,
                };
                if let Some(c) = &client {
                    send_open(c, path, &doc);
                }
                g.docs.insert(path.to_path_buf(), doc);
            }
        }
        if matches!(g.phase, Phase::Idle) {
            g.phase = Phase::Starting;
            drop(g);
            slot.spawn_launch(Duration::ZERO);
        }
    }

    /// Forget a document.
    pub fn close_document(&self, path: &Path) {
        let Some(slot) = self.slot_for_path(path) else {
            return;
        };
        {
            let mut g = slot.inner.lock();
            g.docs.remove(path);
            if let (Phase::Running(c), Some(uri)) = (&g.phase, convert::path_to_uri(path)) {
                c.notify(
                    "textDocument/didClose",
                    json!({"textDocument": {"uri": uri}}),
                );
            }
        }
        self.inner.events.clear(None, Some(path));
    }

    fn target(&self, path: &Path) -> Option<(Arc<Slot>, String)> {
        Some((self.slot_for_path(path)?, convert::path_to_uri(path)?))
    }

    /// Hover information at `pos`.
    pub fn hover(
        &self,
        path: &Path,
        pos: PointUtf16,
    ) -> impl Future<Output = Result<Option<HoverInfo>>> + Send + 'static {
        let t = self.target(path);
        async move {
            let Some((slot, u)) = t else { return Ok(None) };
            match dispatch(Some(slot), "textDocument/hover", pos_params(&u, pos)).await? {
                Some(v) => convert::hover(v),
                None => Ok(None),
            }
        }
    }

    /// Definition locations for the symbol at `pos`.
    pub fn definition(
        &self,
        path: &Path,
        pos: PointUtf16,
    ) -> impl Future<Output = Result<Vec<Location>>> + Send + 'static {
        let t = self.target(path);
        async move {
            let Some((slot, u)) = t else {
                return Ok(vec![]);
            };
            match dispatch(Some(slot), "textDocument/definition", pos_params(&u, pos)).await? {
                Some(v) => convert::locations(v),
                None => Ok(vec![]),
            }
        }
    }

    /// References (including the declaration) to the symbol at `pos`.
    pub fn references(
        &self,
        path: &Path,
        pos: PointUtf16,
    ) -> impl Future<Output = Result<Vec<Location>>> + Send + 'static {
        let t = self.target(path);
        async move {
            let Some((slot, u)) = t else {
                return Ok(vec![]);
            };
            let mut p = pos_params(&u, pos);
            p["context"] = json!({"includeDeclaration": true});
            match dispatch(Some(slot), "textDocument/references", p).await? {
                Some(v) => convert::locations(v),
                None => Ok(vec![]),
            }
        }
    }

    /// Hierarchical outline of a document.
    pub fn document_symbols(
        &self,
        path: &Path,
    ) -> impl Future<Output = Result<Vec<DocumentSymbol>>> + Send + 'static {
        let t = self.target(path);
        async move {
            let Some((slot, u)) = t else {
                return Ok(vec![]);
            };
            let p = json!({"textDocument": {"uri": u}});
            match dispatch(Some(slot), "textDocument/documentSymbol", p).await? {
                Some(v) => convert::document_symbols(v),
                None => Ok(vec![]),
            }
        }
    }

    /// Search symbols across all active servers.
    pub fn workspace_symbols(
        &self,
        query: &str,
    ) -> impl Future<Output = Result<Vec<WorkspaceSymbol>>> + Send + 'static {
        let futs: Vec<_> = self
            .inner
            .slots
            .iter()
            .filter(|s| {
                matches!(
                    s.inner.lock().phase,
                    Phase::Running(_) | Phase::Starting | Phase::Restarting
                )
            })
            .map(|s| dispatch(Some(s.clone()), "workspace/symbol", json!({"query": query})))
            .collect();
        async move {
            let mut out = Vec::new();
            let mut first_err = None;
            let mut any_ok = false;
            for r in futures::future::join_all(futs).await {
                match r {
                    Ok(Some(v)) => match convert::workspace_symbols(v) {
                        Ok(mut s) => {
                            any_ok = true;
                            out.append(&mut s);
                        }
                        Err(e) => first_err = first_err.or(Some(e)),
                    },
                    Ok(None) => any_ok = true,
                    Err(e) => first_err = first_err.or(Some(e)),
                }
            }
            match first_err {
                Some(e) if !any_ok => Err(e),
                _ => Ok(out),
            }
        }
    }

    // Editing features (v0.3).

    /// Version and tag of the text last synced for `path`.
    pub fn document_state(&self, path: &Path) -> Option<DocumentState> {
        let slot = self.slot_for_path(path)?;
        let g = slot.inner.lock();
        g.docs.get(path).map(|d| DocumentState {
            version: d.version,
            tag: d.tag,
        })
    }

    /// Capabilities of the running server handling `path` (`None` while it
    /// starts or when there is none).
    pub fn capabilities(&self, path: &Path) -> Option<ServerCapabilities> {
        self.slot_for_path(path)?.inner.lock().caps.clone()
    }

    /// Capabilities of the running server called `name`.
    pub fn server_capabilities(&self, name: &str) -> Option<ServerCapabilities> {
        self.inner
            .slots
            .iter()
            .find(|s| s.config.name == name)?
            .inner
            .lock()
            .caps
            .clone()
    }

    /// Diagnostics last published for `path`.
    pub fn diagnostics(&self, path: &Path) -> Option<DocumentDiagnostics> {
        self.inner
            .events
            .diagnostics
            .lock()
            .get(path)
            .map(|d| d.public.clone())
    }

    /// All published diagnostics (non-empty ones), sorted by path.
    pub fn all_diagnostics(&self) -> Vec<DocumentDiagnostics> {
        let mut all: Vec<_> = self
            .inner
            .events
            .diagnostics
            .lock()
            .values()
            .filter(|d| !d.public.diagnostics.is_empty())
            .map(|d| d.public.clone())
            .collect();
        all.sort_by(|a, b| a.path.cmp(&b.path));
        all
    }

    /// Calls `f` (on an LSP reader thread) with the path whenever its
    /// diagnostics change (published, or cleared on close / server exit).
    pub fn on_diagnostics(&self, f: impl Fn(&Path) + Send + Sync + 'static) {
        self.inner.events.listeners.lock().push(Arc::new(f));
    }

    /// Handles servers' `workspace/applyEdit` requests (called on an LSP
    /// reader thread; answer from anywhere). Without a handler they are
    /// answered `applied: false`.
    pub fn set_apply_edit_handler(&self, f: impl Fn(ApplyEditRequest) + Send + Sync + 'static) {
        *self.inner.events.apply_edit.lock() = Some(Arc::new(f));
    }

    fn doc_request(
        &self,
        path: &Path,
        method: &'static str,
        params: impl FnOnce(&str) -> Value,
    ) -> DocRequest {
        DocRequest {
            target: self.target(path).map(|(slot, uri)| (slot, params(&uri))),
            method,
        }
    }

    /// Edits formatting the whole document (`textDocument/formatting`).
    pub fn formatting(
        &self,
        path: &Path,
        options: FormattingOptions,
    ) -> impl Future<Output = Result<Vec<TextEdit>>> + Send + 'static {
        let fut = self.doc_request(
            path,
            "textDocument/formatting",
            |uri| json!({"textDocument": {"uri": uri}, "options": formatting_options(options)}),
        );
        async move {
            match fut.send().await? {
                Some(v) => convert::text_edits(&v),
                None => Ok(vec![]),
            }
        }
    }

    /// Edits formatting `range` (`textDocument/rangeFormatting`).
    pub fn range_formatting(
        &self,
        path: &Path,
        range: std::ops::Range<PointUtf16>,
        options: FormattingOptions,
    ) -> impl Future<Output = Result<Vec<TextEdit>>> + Send + 'static {
        let fut = self.doc_request(path, "textDocument/rangeFormatting", |uri| {
            json!({"textDocument": {"uri": uri}, "range": convert::range_json(&range),
                   "options": formatting_options(options)})
        });
        async move {
            match fut.send().await? {
                Some(v) => convert::text_edits(&v),
                None => Ok(vec![]),
            }
        }
    }

    /// Code actions for `range`; the published diagnostics overlapping it are
    /// sent as context. `automatic` marks background requests (lightbulb).
    pub fn code_actions(
        &self,
        path: &Path,
        range: std::ops::Range<PointUtf16>,
        automatic: bool,
    ) -> impl Future<Output = Result<Vec<CodeAction>>> + Send + 'static {
        let context: Vec<Value> = self
            .inner
            .events
            .diagnostics
            .lock()
            .get(path)
            .map(|d| {
                d.public
                    .diagnostics
                    .iter()
                    .zip(&d.raw)
                    .filter(|(diag, _)| {
                        diag.range.start <= range.end && range.start <= diag.range.end
                    })
                    .map(|(_, raw)| raw.clone())
                    .collect()
            })
            .unwrap_or_default();
        let fut = self.doc_request(path, "textDocument/codeAction", |uri| {
            json!({"textDocument": {"uri": uri}, "range": convert::range_json(&range),
                   "context": {"diagnostics": context, "triggerKind": if automatic { 2 } else { 1 }}})
        });
        async move {
            match fut.send().await? {
                Some(v) => convert::code_actions(&v),
                None => Ok(vec![]),
            }
        }
    }

    /// Fills in a code action's edit (`codeAction/resolve`) when the server
    /// supports it and the action has none; otherwise returns it unchanged.
    pub fn resolve_code_action(
        &self,
        path: &Path,
        action: CodeAction,
    ) -> impl Future<Output = Result<CodeAction>> + Send + 'static {
        let t = self.target(path);
        async move {
            let Some((slot, _)) = t else {
                return Ok(action);
            };
            if action.edit.is_some() || action.command.is_some() {
                return Ok(action);
            }
            if slot.clone().wait_ready().await.is_none() {
                return Ok(action);
            }
            let resolvable = slot
                .inner
                .lock()
                .caps
                .as_ref()
                .is_some_and(|c| c.code_action_resolve);
            if !resolvable {
                return Ok(action);
            }
            let req = DocRequest {
                target: Some((slot, action.raw.clone())),
                method: "codeAction/resolve",
            };
            match req.send().await? {
                Some(v) if !v.is_null() => convert::code_action(&v),
                _ => Ok(action),
            }
        }
    }

    /// Runs a server command (`workspace/executeCommand`) on the server of
    /// `path`. Edits it makes arrive as `workspace/applyEdit` requests.
    pub fn execute_command(
        &self,
        path: &Path,
        command: &Command,
    ) -> impl Future<Output = Result<()>> + Send + 'static {
        let params = json!({"command": command.command, "arguments": command.arguments});
        let fut = self.doc_request(path, "workspace/executeCommand", move |_| params);
        async move { fut.send().await.map(|_| ()) }
    }

    /// `textDocument/prepareRename`: the range to rename at `pos`.
    /// `Ok(None)`: nothing to rename there. Servers without prepare support
    /// answer `PrepareRename { range: None, .. }` (the caller picks the word).
    pub fn prepare_rename(
        &self,
        path: &Path,
        pos: PointUtf16,
    ) -> impl Future<Output = Result<Option<PrepareRename>>> + Send + 'static {
        let t = self.target(path);
        async move {
            let Some((slot, uri)) = t else {
                return Ok(None);
            };
            if slot.clone().wait_ready().await.is_none() {
                return Ok(None);
            }
            let caps = slot.inner.lock().caps.clone().unwrap_or_default();
            if !caps.rename {
                return Ok(None);
            }
            if !caps.prepare_rename {
                return Ok(Some(PrepareRename {
                    range: None,
                    placeholder: None,
                }));
            }
            let req = DocRequest {
                target: Some((slot, pos_params(&uri, pos))),
                method: "textDocument/prepareRename",
            };
            Ok(req.send().await?.and_then(|v| convert::prepare_rename(&v)))
        }
    }

    /// `textDocument/rename`: the edit renaming the symbol at `pos`.
    pub fn rename(
        &self,
        path: &Path,
        pos: PointUtf16,
        new_name: &str,
    ) -> impl Future<Output = Result<Option<WorkspaceEdit>>> + Send + 'static {
        let new_name = new_name.to_string();
        let fut = self.doc_request(path, "textDocument/rename", move |u| {
            let mut p = pos_params(u, pos);
            p["newName"] = Value::String(new_name);
            p
        });
        async move {
            match fut.send().await? {
                Some(v) if !v.is_null() => convert::workspace_edit(&v).map(Some),
                Some(_) => Ok(None),
                None => Err(LspError::Protocol(
                    "no language server for this file".into(),
                )),
            }
        }
    }

    /// Stop all servers: `shutdown` + `exit`, killing after the grace period.
    /// Blocks until done. The manager can no longer start servers afterwards.
    pub fn shutdown(&self) {
        let grace = self
            .inner
            .slots
            .first()
            .map_or(Duration::from_secs(2), |s| s.opts.shutdown_grace);
        let clients: Vec<_> = self
            .inner
            .slots
            .iter()
            .filter_map(|s| s.shutdown_take())
            .collect();
        let handles: Vec<_> = clients
            .into_iter()
            .map(|c| std::thread::spawn(move || c.shutdown(grace)))
            .collect();
        for h in handles {
            let _ = h.join();
        }
    }
}
