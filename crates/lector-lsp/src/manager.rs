//! Per-root server management: lazy start, init queueing, restart, shutdown.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::client::Client;
use crate::config::{language_id, which, ServerConfig};
use crate::convert;
use crate::error::Result;
use crate::types::{DocumentSymbol, HoverInfo, Location, WorkspaceSymbol};
use crate::PointUtf16;

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
}

struct Slot {
    config: ServerConfig,
    resolved: Mutex<Option<Option<PathBuf>>>,
    inner: Mutex<SlotInner>,
    root: PathBuf,
    root_uri: String,
    opts: LspOptions,
}

fn client_capabilities() -> Value {
    json!({
        "general": {"positionEncodings": ["utf-16"]},
        "window": {"workDoneProgress": true},
        "workspace": {
            "configuration": true,
            "workspaceFolders": true,
            "symbol": {"dynamicRegistration": false}
        },
        "textDocument": {
            "synchronization": {"dynamicRegistration": false, "didSave": false},
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
        if let Some(options) = crate::config::read_only_settings(&self.config.name) {
            params["initializationOptions"] = options;
        }
        let res = futures::executor::block_on(client.request_with_timeout(
            "initialize",
            params,
            self.opts.init_timeout,
        ));
        if let Err(e) = res {
            tracing::warn!("{}: initialize failed: {e}", self.config.name);
            client.kill();
            self.handle_exit(gen);
            return;
        }
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

struct Inner {
    root: PathBuf,
    slots: Vec<Arc<Slot>>,
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
                    }),
                    root: root.clone(),
                    root_uri: root_uri.clone(),
                    opts: opts.clone(),
                })
            })
            .collect();
        Self {
            inner: Arc::new(Inner { root, slots }),
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
            Some(doc) if doc.text == text => return,
            Some(doc) => {
                doc.version += 1;
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
        let mut g = slot.inner.lock();
        g.docs.remove(path);
        if let (Phase::Running(c), Some(uri)) = (&g.phase, convert::path_to_uri(path)) {
            c.notify(
                "textDocument/didClose",
                json!({"textDocument": {"uri": uri}}),
            );
        }
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
