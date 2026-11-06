//! [`AgentSession`]: one conversation with one ACP agent in one project.
//!
//! Threads: the agent process's stdout is read by the RPC reader thread (notifications in
//! order); each agent → client request runs on its own thread (a permission prompt blocks
//! only itself); each prompt runs on a turn thread that takes the checkpoints, waits for
//! the answer (checking cancellation and the idle timeout) and finishes the turn.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime};

use lector_app::commands::Source;
use lector_app::documents::{DocumentStore, SaveRequest};
use lector_app::journal::{ActivityEntry, Journal, UndoError, UndoMode, UndoRecord};
use lector_app::{AppCore, CommandContext};
use lector_git::ShadowRepo;
use lector_workspace::Project;
use parking_lot::{Condvar, Mutex};
use serde_json::{json, Value};

use crate::config::AgentConfig;
use crate::context::build_prompt;
use crate::error::AgentError;
use crate::model::*;
use crate::paths::{relative, resolve_in_root};
use crate::rpc::{self, Connection, Handler, RpcError};

/// ACP protocol version Lector speaks.
pub const PROTOCOL_VERSION: u64 = 1;

/// Process-unique id of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct SessionId(pub u64);

/// Tunables of a session.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// Take working-tree checkpoints around every turn.
    pub checkpoints: bool,
    /// Allow `read` tool calls inside the project without asking.
    pub auto_allow_reads: bool,
    /// Timeout of `initialize` / `session/new`.
    pub request_timeout: Duration,
    /// A turn without any message from the agent for this long fails.
    pub turn_idle_timeout: Duration,
    /// After a cancel, how long to wait for the agent to confirm before giving up.
    pub cancel_grace: Duration,
}

impl Default for SessionOptions {
    fn default() -> Self {
        SessionOptions {
            checkpoints: true,
            auto_allow_reads: true,
            request_timeout: Duration::from_secs(60),
            turn_idle_timeout: Duration::from_secs(15 * 60),
            cancel_grace: Duration::from_secs(10),
        }
    }
}

impl SessionOptions {
    /// From the `[agent]` settings.
    pub fn from_settings(s: &lector_config::AgentSettings) -> Self {
        SessionOptions {
            checkpoints: s.checkpoints,
            auto_allow_reads: s.auto_allow_reads,
            ..Default::default()
        }
    }
}

/// Where the session is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum SessionStatus {
    /// Starting the process / initializing.
    Starting,
    /// Waiting for a prompt.
    Ready,
    /// Working on a turn.
    Busy(TurnId),
    /// The agent process exited unexpectedly (message incl. stderr tail). Restart to continue.
    Crashed(String),
    /// Stopped by the user.
    Stopped,
}

/// What changed in a session (subscribers re-read the session).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentEvent {
    /// [`AgentSession::status`] changed.
    StatusChanged,
    /// A turn started.
    TurnStarted(TurnId),
    /// A turn received messages, tool calls, plan updates or file writes.
    TurnUpdated(TurnId),
    /// A turn finished (its checkpoint and changed files are set).
    TurnFinished(TurnId),
    /// The agent asks for permission; see [`AgentSession::pending_permissions`].
    PermissionRequested(PermissionRequestId),
    /// A permission request was answered or cancelled.
    PermissionResolved(PermissionRequestId),
}

type Callback = Arc<dyn Fn(&AgentEvent) + Send + Sync>;

/// Unsubscribes on drop.
pub struct Subscription {
    id: u64,
    shared: Weak<Shared>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(s) = self.shared.upgrade() {
            s.subscribers.lock().retain(|(id, _)| *id != self.id);
        }
    }
}

struct Process {
    child: Child,
    conn: Connection,
    /// Keeps the connection's handler alive.
    _handler: Arc<ConnHandler>,
    acp_session: String,
    embedded_context: bool,
}

struct State {
    status: SessionStatus,
    process: Option<Process>,
    generation: u64,
    turns: Vec<Turn>,
    current: Option<TurnId>,
    cancel_requested: Option<Instant>,
    last_activity: Instant,
    next_permission: u64,
    always_allowed: HashSet<String>,
    agent_name: Option<String>,
}

struct Shared {
    id: SessionId,
    core: AppCore,
    project: Project,
    config: AgentConfig,
    options: SessionOptions,
    state: Mutex<State>,
    idle: Condvar,
    subscribers: Mutex<Vec<(u64, Callback)>>,
    next_sub: AtomicU64,
    waiters: Mutex<HashMap<PermissionRequestId, mpsc::Sender<Option<PermissionChoice>>>>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    /// Set when the current process's stderr reached EOF.
    stderr_eof: Arc<std::sync::atomic::AtomicBool>,
    shadow: Mutex<Option<ShadowRepo>>,
}

/// One conversation with one agent in one project. Cheap to clone.
#[derive(Clone)]
pub struct AgentSession {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for AgentSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSession")
            .field("id", &self.shared.id)
            .field("agent", &self.shared.config.name)
            .finish()
    }
}

impl PartialEq for AgentSession {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
const STDERR_LINES: usize = 40;

impl AgentSession {
    /// Starts the agent in `project` and opens an ACP session (blocking: spawn,
    /// `initialize`, `session/new`). Options come from the `[agent]` settings.
    pub fn start(
        core: &AppCore,
        project: &Project,
        cfg: &AgentConfig,
    ) -> Result<AgentSession, AgentError> {
        let options = SessionOptions::from_settings(&project.settings().agent);
        Self::start_with(core, project, cfg, options)
    }

    /// [`AgentSession::start`] with explicit options.
    pub fn start_with(
        core: &AppCore,
        project: &Project,
        cfg: &AgentConfig,
        options: SessionOptions,
    ) -> Result<AgentSession, AgentError> {
        let shared = Arc::new(Shared {
            id: SessionId(NEXT_SESSION.fetch_add(1, Ordering::Relaxed)),
            core: core.clone(),
            project: project.clone(),
            config: cfg.clone(),
            options,
            state: Mutex::new(State {
                status: SessionStatus::Starting,
                process: None,
                generation: 0,
                turns: Vec::new(),
                current: None,
                cancel_requested: None,
                last_activity: Instant::now(),
                next_permission: 1,
                always_allowed: HashSet::new(),
                agent_name: None,
            }),
            idle: Condvar::new(),
            subscribers: Mutex::new(Vec::new()),
            next_sub: AtomicU64::new(1),
            waiters: Mutex::new(HashMap::new()),
            stderr: Arc::new(Mutex::new(VecDeque::new())),
            stderr_eof: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            shadow: Mutex::new(None),
        });
        let session = AgentSession { shared };
        session.launch()?;
        Ok(session)
    }

    /// Process-unique id.
    pub fn id(&self) -> SessionId {
        self.shared.id
    }

    /// The agent's configuration.
    pub fn config(&self) -> &AgentConfig {
        &self.shared.config
    }

    /// The project.
    pub fn project(&self) -> &Project {
        &self.shared.project
    }

    /// The core it runs against.
    pub fn core(&self) -> &AppCore {
        &self.shared.core
    }

    /// The options it was started with.
    pub fn options(&self) -> &SessionOptions {
        &self.shared.options
    }

    /// The shadow repository holding the turn checkpoints (`None` outside a git repository).
    pub fn shadow_repo(&self) -> Option<ShadowRepo> {
        self.shared.shadow()
    }

    /// The agent's own name (from `initialize`), else the configured name.
    pub fn agent_name(&self) -> String {
        self.shared
            .state
            .lock()
            .agent_name
            .clone()
            .unwrap_or_else(|| self.shared.config.name.clone())
    }

    /// Current status.
    pub fn status(&self) -> SessionStatus {
        self.shared.state.lock().status.clone()
    }

    /// The last lines the agent wrote to stderr.
    pub fn stderr_tail(&self) -> Vec<String> {
        self.shared.stderr.lock().iter().cloned().collect()
    }

    /// Calls `cb` on every change (from the thread that caused it) until the
    /// subscription is dropped.
    pub fn subscribe(&self, cb: impl Fn(&AgentEvent) + Send + Sync + 'static) -> Subscription {
        let id = self.shared.next_sub.fetch_add(1, Ordering::Relaxed);
        self.shared.subscribers.lock().push((id, Arc::new(cb)));
        Subscription {
            id,
            shared: Arc::downgrade(&self.shared),
        }
    }

    /// Every turn, oldest first (`undone` refreshed from the journal).
    pub fn turns(&self) -> Vec<Turn> {
        let mut turns = self.shared.state.lock().turns.clone();
        if let Some(journal) = Journal::of(&self.shared.core) {
            for t in &mut turns {
                if let Some(entry) = t.journal_id.and_then(|id| journal.get(id)) {
                    t.undone = entry.undone;
                }
            }
        }
        turns
    }

    /// One turn.
    pub fn turn(&self, id: TurnId) -> Option<Turn> {
        self.turns().into_iter().find(|t| t.id == id)
    }

    /// The newest turn.
    pub fn last_turn(&self) -> Option<Turn> {
        self.turns().pop()
    }

    /// Permission requests waiting for the user.
    pub fn pending_permissions(&self) -> Vec<PermissionRequest> {
        let st = self.shared.state.lock();
        st.turns
            .iter()
            .flat_map(|t| t.permissions.iter())
            .filter(|p| p.pending)
            .cloned()
            .collect()
    }

    /// Sends a prompt; returns at once with the new turn's id (the turn runs on a
    /// background thread; follow it with [`AgentSession::subscribe`] / [`AgentSession::wait_turn`]).
    pub fn prompt(&self, text: String, context: Vec<PromptContext>) -> Result<TurnId, AgentError> {
        let id = {
            let mut st = self.shared.state.lock();
            match &st.status {
                SessionStatus::Ready => {}
                SessionStatus::Busy(t) => return Err(AgentError::Busy(*t)),
                SessionStatus::Starting => {
                    return Err(AgentError::NotRunning("still starting".into()))
                }
                SessionStatus::Crashed(m) => return Err(AgentError::NotRunning(m.clone())),
                SessionStatus::Stopped => return Err(AgentError::NotRunning("stopped".into())),
            }
            let id = TurnId(st.turns.len() as u64 + 1);
            let labels = context.iter().map(PromptContext::label).collect();
            st.turns.push(Turn::new(id, text.clone(), labels));
            st.current = Some(id);
            st.cancel_requested = None;
            st.last_activity = Instant::now();
            st.status = SessionStatus::Busy(id);
            id
        };
        self.shared.emit(AgentEvent::StatusChanged);
        self.shared.emit(AgentEvent::TurnStarted(id));
        let shared = self.shared.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("agent-turn-{}", id.0))
            .spawn(move || run_turn(shared, id, text, context));
        if let Err(err) = spawned {
            self.shared.finish_turn(
                id,
                TurnState::Failed(format!("cannot start a thread: {err}")),
            );
        }
        Ok(id)
    }

    /// Asks the agent to stop the current turn (`session/cancel`); pending permission
    /// requests are answered "cancelled". The turn ends as [`TurnState::Cancelled`].
    pub fn cancel(&self) {
        let (conn, session) = {
            let mut st = self.shared.state.lock();
            if st.current.is_none() {
                return;
            }
            st.cancel_requested.get_or_insert_with(Instant::now);
            match &st.process {
                Some(p) => (Some(p.conn.clone()), p.acp_session.clone()),
                None => (None, String::new()),
            }
        };
        self.shared.cancel_permissions();
        if let Some(conn) = conn {
            let _ = conn.notify("session/cancel", json!({ "sessionId": session }));
        }
    }

    /// Answers a permission request.
    pub fn answer_permission(&self, req: PermissionRequestId, choice: PermissionChoice) {
        let tx = self.shared.waiters.lock().remove(&req);
        if let Some(tx) = tx {
            let _ = tx.send(Some(choice));
        }
    }

    /// Blocks until the turn finished (or `timeout`); returns it.
    pub fn wait_turn(&self, id: TurnId, timeout: Duration) -> Option<Turn> {
        let deadline = Instant::now() + timeout;
        let mut st = self.shared.state.lock();
        loop {
            let done = st
                .turns
                .iter()
                .find(|t| t.id == id)
                .is_some_and(|t| t.state.is_finished() && st.current != Some(id));
            if done {
                break;
            }
            if self.shared.idle.wait_until(&mut st, deadline).timed_out() {
                return None;
            }
        }
        drop(st);
        self.turn(id)
    }

    /// Restarts a crashed or stopped agent (new process and ACP session; the turn history
    /// is kept, the agent's own memory of it is not).
    pub fn restart(&self) -> Result<(), AgentError> {
        self.kill_process();
        {
            let mut st = self.shared.state.lock();
            st.status = SessionStatus::Starting;
        }
        self.shared.emit(AgentEvent::StatusChanged);
        self.launch()
    }

    /// Stops the agent process.
    pub fn stop(&self) {
        self.cancel();
        self.kill_process();
        {
            let mut st = self.shared.state.lock();
            st.status = SessionStatus::Stopped;
        }
        self.shared.idle.notify_all();
        self.shared.emit(AgentEvent::StatusChanged);
    }

    /// The turn's changes as a changeset for the review views.
    pub fn turn_changeset(&self, id: TurnId) -> Option<lector_git::Changeset> {
        self.turn(id)?.changeset()
    }

    /// Restores the working tree files the turn changed to their state before the turn,
    /// through the activity journal (so the undo is itself undoable). Files edited since
    /// the turn make it fail with [`AgentError::UndoConflict`] unless `force`.
    /// Returns the journal entry of the undo.
    pub fn undo_turn(&self, id: TurnId, force: bool, source: Source) -> Result<u64, AgentError> {
        let turn = self.turn(id).ok_or(AgentError::NoTurn(id))?;
        if !turn.state.is_finished() {
            return Err(AgentError::NotUndoable("the turn is still running".into()));
        }
        if turn.undone {
            return Err(AgentError::NotUndoable(
                "the turn was already undone".into(),
            ));
        }
        let entry = turn.journal_id.ok_or_else(|| {
            AgentError::NotUndoable(if turn.before.is_none() {
                "no checkpoint was taken for this turn".into()
            } else {
                "the turn changed no files".into()
            })
        })?;
        let journal = Journal::of(&self.shared.core)
            .ok_or_else(|| AgentError::Other("no activity journal".into()))?;
        let cx = CommandContext::new(
            self.shared.core.clone(),
            Some(self.shared.project.clone()),
            source,
        );
        let mode = if force {
            UndoMode::Force
        } else {
            UndoMode::Check
        };
        let result = journal.undo(entry, &cx, mode);
        self.shared.emit(AgentEvent::TurnUpdated(id));
        match result {
            Ok(undo) => {
                if let Some(repo) = self.shared.core.git().repo(self.shared.project.root()) {
                    repo.refresh_and_notify();
                }
                Ok(undo.id)
            }
            Err(UndoError::Changed { paths, .. }) => Err(AgentError::UndoConflict { paths }),
            Err(other) => Err(AgentError::NotUndoable(other.to_string())),
        }
    }

    // -- process -------------------------------------------------------------------------

    fn kill_process(&self) {
        let process = self.shared.state.lock().process.take();
        if let Some(mut p) = process {
            p.conn.mark_closed("stopped".into());
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
    }

    fn launch(&self) -> Result<(), AgentError> {
        let shared = &self.shared;
        let cfg = &shared.config;
        let fail = |message: String| AgentError::Start {
            name: cfg.name.clone(),
            message,
        };
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .envs(cfg.env.iter().map(|(k, v)| (k, v)))
            .current_dir(shared.project.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(err) => {
                let message = format!("`{}`: {err}", cfg.command_line());
                self.set_status(SessionStatus::Crashed(message.clone()));
                return Err(fail(message));
            }
        };
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            return Err(fail("no stdio pipes".into()));
        };
        if let Some(stderr) = child.stderr.take() {
            let tail = shared.stderr.clone();
            let eof = shared.stderr_eof.clone();
            eof.store(false, Ordering::Release);
            let _ = std::thread::Builder::new()
                .name("agent-stderr".into())
                .spawn(move || {
                    let _done = SetOnDrop(eof);
                    for line in BufReader::new(stderr).lines() {
                        let Ok(line) = line else { break };
                        tracing::debug!(target: "lector_agent::stderr", "{line}");
                        let mut t = tail.lock();
                        if t.len() == STDERR_LINES {
                            t.pop_front();
                        }
                        t.push_back(line);
                    }
                });
        }
        let generation = {
            let mut st = shared.state.lock();
            st.generation += 1;
            st.generation
        };
        let handler = Arc::new(ConnHandler {
            shared: Arc::downgrade(shared),
            generation,
        });
        let weak: Weak<dyn Handler> = Arc::downgrade(&(handler.clone() as Arc<dyn Handler>));
        let conn = Connection::new(stdout, stdin, weak);
        match handshake(&conn, shared) {
            Ok((acp_session, embedded_context, agent_name)) => {
                let mut st = shared.state.lock();
                st.process = Some(Process {
                    child,
                    conn,
                    _handler: handler,
                    acp_session,
                    embedded_context,
                });
                if agent_name.is_some() {
                    st.agent_name = agent_name;
                }
                st.status = SessionStatus::Ready;
                drop(st);
                shared.emit(AgentEvent::StatusChanged);
                Ok(())
            }
            Err(err) => {
                conn.mark_closed("start failed".into());
                let _ = child.kill();
                let _ = child.wait();
                shared.settle_stderr();
                let message = with_stderr(&err.to_string(), &shared.stderr.lock());
                self.set_status(SessionStatus::Crashed(message.clone()));
                Err(fail(message))
            }
        }
    }

    fn set_status(&self, status: SessionStatus) {
        self.shared.state.lock().status = status;
        self.shared.idle.notify_all();
        self.shared.emit(AgentEvent::StatusChanged);
    }
}

struct SetOnDrop(Arc<std::sync::atomic::AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn with_stderr(message: &str, tail: &VecDeque<String>) -> String {
    let last: Vec<&str> = tail
        .iter()
        .rev()
        .take(6)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if last.is_empty() {
        message.to_string()
    } else {
        format!("{message}\n{}", last.join("\n"))
    }
}

/// `initialize` + `session/new` (+ `authenticate` when the agent asks for it).
fn handshake(
    conn: &Connection,
    shared: &Shared,
) -> Result<(String, bool, Option<String>), RpcError> {
    let timeout = Some(shared.options.request_timeout);
    let init = conn.request(
        "initialize",
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": { "readTextFile": true, "writeTextFile": true },
                "terminal": false
            },
            "clientInfo": { "name": "lector", "version": env!("CARGO_PKG_VERSION") }
        }),
        timeout,
    )?;
    let caps = init
        .get("agentCapabilities")
        .cloned()
        .unwrap_or(Value::Null);
    let embedded = caps
        .pointer("/promptCapabilities/embeddedContext")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let agent_name = init
        .pointer("/agentInfo/title")
        .or_else(|| init.pointer("/agentInfo/name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let auth_methods: Vec<String> = init
        .get("authMethods")
        .and_then(Value::as_array)
        .map(|m| {
            m.iter()
                .filter_map(|x| x.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let params = json!({
        "cwd": shared.project.root().to_string_lossy(),
        "mcpServers": []
    });
    let created = match conn.request("session/new", params.clone(), timeout) {
        Err(RpcError::Remote { code: -32000, .. }) if !auth_methods.is_empty() => {
            conn.request(
                "authenticate",
                json!({ "methodId": auth_methods[0] }),
                Some(Duration::from_secs(300)),
            )?;
            conn.request("session/new", params, timeout)?
        }
        other => other?,
    };
    let session = created
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::remote(rpc::INVALID_PARAMS, "session/new returned no sessionId"))?
        .to_string();
    Ok((session, embedded, agent_name))
}

/// The turn thread.
fn run_turn(shared: Arc<Shared>, id: TurnId, text: String, context: Vec<PromptContext>) {
    // Checkpoint before the agent touches anything.
    let before = if shared.options.checkpoints {
        shared.snapshot(&format!("Before agent turn {id}: {}", first_line(&text)))
    } else {
        None
    };
    let (conn, session, embedded) = {
        let mut st = shared.state.lock();
        if let Some(turn) = st.turns.iter_mut().find(|t| t.id == id) {
            turn.before = before;
            turn.state = TurnState::Running;
        }
        match &st.process {
            Some(p) => (
                Some(p.conn.clone()),
                p.acp_session.clone(),
                p.embedded_context,
            ),
            None => (None, String::new(), false),
        }
    };
    shared.emit(AgentEvent::TurnUpdated(id));
    let Some(conn) = conn else {
        shared.finish_turn(id, TurnState::Failed("the agent is not running".into()));
        return;
    };
    let blocks = build_prompt(&shared.core, &shared.project, &text, &context, embedded);
    let state = match conn.start_request(
        "session/prompt",
        json!({ "sessionId": session, "prompt": blocks }),
    ) {
        Err(err) => TurnState::Failed(err.to_string()),
        Ok(pending) => loop {
            if let Some(result) = pending.wait(Duration::from_millis(100)) {
                let cancelled = shared.state.lock().cancel_requested.is_some();
                break match result {
                    Ok(v) => {
                        let reason = v
                            .get("stopReason")
                            .and_then(Value::as_str)
                            .unwrap_or("end_turn")
                            .to_string();
                        if reason == "cancelled" || cancelled {
                            TurnState::Cancelled
                        } else {
                            TurnState::Completed(reason)
                        }
                    }
                    Err(RpcError::Closed(reason)) => {
                        shared.settle_stderr();
                        TurnState::Failed(with_stderr(
                            &format!("The agent exited ({reason})."),
                            &shared.stderr.lock(),
                        ))
                    }
                    Err(err) if cancelled => {
                        tracing::debug!("cancelled prompt ended with {err}");
                        TurnState::Cancelled
                    }
                    Err(err) => TurnState::Failed(err.to_string()),
                };
            }
            let (cancel_at, last) = {
                let st = shared.state.lock();
                (st.cancel_requested, st.last_activity)
            };
            if cancel_at.is_some_and(|t| t.elapsed() > shared.options.cancel_grace) {
                break TurnState::Cancelled;
            }
            if last.elapsed() > shared.options.turn_idle_timeout {
                break TurnState::Failed(format!(
                    "No answer from the agent for {} s.",
                    shared.options.turn_idle_timeout.as_secs()
                ));
            }
        },
    };
    shared.finish_turn(id, state);
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    rpc::truncate(line, 80).to_string()
}

impl Shared {
    /// The agent exited: give its stderr reader a moment to collect the last lines.
    fn settle_stderr(&self) {
        let deadline = Instant::now() + Duration::from_millis(500);
        while !self.stderr_eof.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn emit(&self, ev: AgentEvent) {
        let subs: Vec<Callback> = self
            .subscribers
            .lock()
            .iter()
            .map(|(_, cb)| cb.clone())
            .collect();
        for cb in subs {
            cb(&ev);
        }
    }

    fn shadow(&self) -> Option<ShadowRepo> {
        let mut slot = self.shadow.lock();
        if slot.is_none() {
            let repo = self.project.repo()?;
            let journal = Journal::of(&self.core)?;
            match ShadowRepo::open(journal.data_dir(), repo) {
                Ok(s) => *slot = Some(s),
                Err(err) => {
                    tracing::warn!("agent checkpoints unavailable: {err}");
                    return None;
                }
            }
        }
        slot.clone()
    }

    fn snapshot(&self, note: &str) -> Option<lector_git::ShadowSnapshot> {
        let shadow = self.shadow()?;
        match shadow.snapshot_worktree(note) {
            Ok(s) => Some(s),
            Err(err) => {
                tracing::warn!("agent checkpoint failed: {err}");
                None
            }
        }
    }

    fn cancel_permissions(&self) {
        let waiters: Vec<_> = self.waiters.lock().drain().collect();
        for (_, tx) in waiters {
            let _ = tx.send(None);
        }
    }

    /// Takes the "after" checkpoint, records the turn in the journal, marks it finished.
    fn finish_turn(&self, id: TurnId, state: TurnState) {
        self.cancel_permissions();
        let (before, prompt) = {
            let st = self.state.lock();
            match st.turns.iter().find(|t| t.id == id) {
                Some(t) => (t.before.clone(), t.prompt.clone()),
                None => return,
            }
        };
        let mut after = None;
        let mut changed = Vec::new();
        let mut journal_id = None;
        if let Some(before) = &before {
            after = self.snapshot(&format!("Agent turn {id}: {}", first_line(&prompt)));
            if let (Some(after), Some(shadow)) = (&after, self.shadow()) {
                match shadow.changes_between(before, after) {
                    Ok(changes) => {
                        changed = changes
                            .iter()
                            .map(|c| ChangedFile {
                                path: c.path().to_string(),
                                kind: c.kind,
                            })
                            .collect();
                    }
                    Err(err) => tracing::warn!("cannot compare agent checkpoints: {err}"),
                }
                if !changed.is_empty() {
                    journal_id = self.journal_turn(id, &prompt, before, &shadow, &changed);
                }
            }
        }
        {
            let mut st = self.state.lock();
            if let Some(turn) = st.turns.iter_mut().find(|t| t.id == id) {
                turn.after = after;
                turn.changed_files = changed;
                turn.journal_id = journal_id;
                turn.state = state;
                turn.finished = Some(now_secs());
                for p in &mut turn.permissions {
                    p.pending = false;
                }
            }
        }
        if let Some(repo) = self.core.git().repo(self.project.root()) {
            repo.refresh_and_notify();
        }
        self.emit(AgentEvent::TurnFinished(id));
        // The session becomes ready (and `wait_turn` returns) only after subscribers
        // heard about the finished turn.
        {
            let mut st = self.state.lock();
            if st.current == Some(id) {
                st.current = None;
                st.cancel_requested = None;
            }
            if st.status == SessionStatus::Busy(id) {
                st.status = SessionStatus::Ready;
            }
        }
        self.idle.notify_all();
        self.emit(AgentEvent::StatusChanged);
    }

    fn journal_turn(
        &self,
        id: TurnId,
        prompt: &str,
        before: &lector_git::ShadowSnapshot,
        shadow: &ShadowRepo,
        changed: &[ChangedFile],
    ) -> Option<u64> {
        let journal = Journal::of(&self.core)?;
        let rels: Vec<String> = changed.iter().map(|c| c.path.clone()).collect();
        let refs: Vec<&str> = rels.iter().map(String::as_str).collect();
        let after = shadow.fingerprints(&refs).unwrap_or_default();
        let n = rels.len();
        journal.append(ActivityEntry {
            id: 0,
            time: SystemTime::now(),
            command: "agent.turn".into(),
            summary: format!(
                "{} turn {id}: {} ({n} file{})",
                self.config.name,
                first_line(prompt),
                if n == 1 { "" } else { "s" }
            ),
            repo: Some(shadow.workdir().to_path_buf()),
            paths: rels.iter().map(|r| shadow.workdir().join(r)).collect(),
            undo: Some(UndoRecord::RestoreFiles {
                shadow: before.clone(),
                paths: rels.clone(),
                after,
            }),
            undone: false,
            args: json!({ "session": self.id.0, "turn": id.0, "prompt": prompt }),
            source: Source::Agent,
            undo_of: None,
            undone_by: None,
        })
    }

    fn update_turn(&self, f: impl FnOnce(&mut Turn)) -> Option<TurnId> {
        let mut st = self.state.lock();
        st.last_activity = Instant::now();
        let id = st.current?;
        let turn = st.turns.iter_mut().find(|t| t.id == id)?;
        f(turn);
        Some(id)
    }

    // -- agent → client requests --------------------------------------------------------

    fn read_text_file(&self, params: &Value) -> Result<Value, RpcError> {
        let path = params.get("path").and_then(Value::as_str).unwrap_or("");
        let abs = resolve_in_root(self.project.root(), path)
            .map_err(|e| RpcError::remote(rpc::INVALID_PARAMS, e))?;
        let store = DocumentStore::for_core(&self.core);
        let text = match store.get(&abs).and_then(|h| {
            let snap = h.snapshot();
            snap.text()
                .map(|t| t.text_for_range(0..t.len()).into_owned())
        }) {
            Some(text) => text,
            None => {
                let bytes = std::fs::read(&abs)
                    .map_err(|e| RpcError::remote(rpc::INTERNAL_ERROR, format!("{path}: {e}")))?;
                String::from_utf8(bytes).map_err(|_| {
                    RpcError::remote(rpc::INTERNAL_ERROR, format!("{path} is not UTF-8 text"))
                })?
            }
        };
        let line = params.get("line").and_then(Value::as_u64);
        let limit = params.get("limit").and_then(Value::as_u64);
        let content = if line.is_some() || limit.is_some() {
            let start = line.unwrap_or(1).saturating_sub(1) as usize;
            let take = limit.map(|l| l as usize).unwrap_or(usize::MAX);
            text.split_inclusive('\n')
                .skip(start)
                .take(take)
                .collect::<String>()
        } else {
            text
        };
        self.update_turn(|_| {});
        Ok(json!({ "content": content }))
    }

    fn write_text_file(&self, params: &Value) -> Result<Value, RpcError> {
        let path = params.get("path").and_then(Value::as_str).unwrap_or("");
        let content = params
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::remote(rpc::INVALID_PARAMS, "missing content"))?;
        let root = self.project.root();
        let abs = resolve_in_root(root, path)
            .map_err(|e| RpcError::remote(rpc::INVALID_PARAMS, format!("refused: {e}")))?;
        let rel = relative(root, &abs);
        let (turn, has_checkpoint) = {
            let st = self.state.lock();
            let turn = st.current;
            let has = turn
                .and_then(|id| st.turns.iter().find(|t| t.id == id))
                .is_some_and(|t| t.before.is_some());
            (turn, has)
        };
        // Without a turn checkpoint, back the file up on its own so the write is undoable.
        // Checkpoints leave ignored files out (`.env`, build output...): back those up too.
        let covered = has_checkpoint && !self.shadow().is_some_and(|s| s.is_ignored(&rel));
        let backup = if covered {
            None
        } else {
            self.shadow().and_then(|s| {
                s.snapshot_paths(&[rel.as_str()], "Before an agent write")
                    .ok()
            })
        };
        let err = |e: String| RpcError::remote(rpc::INTERNAL_ERROR, format!("{rel}: {e}"));
        let store = DocumentStore::for_core(&self.core);
        match store.get(&abs) {
            Some(handle) => {
                if handle.is_read_only() {
                    return Err(err("the file is locked (read-only) in Lector".into()));
                }
                handle
                    .replace_all(content)
                    .map_err(|e| err(e.to_string()))?;
                handle
                    .save(SaveRequest {
                        force: false,
                        trim_trailing_whitespace: false,
                        ensure_final_newline: false,
                        run_hooks: false,
                        path: None,
                    })
                    .map_err(|e| err(e.to_string()))?;
            }
            None => {
                if let Some(parent) = abs.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| err(e.to_string()))?;
                }
                lector_fs::write_atomic(&abs, content.as_bytes())
                    .map_err(|e| err(e.to_string()))?;
            }
        }
        self.update_turn(|t| {
            if !t.written.contains(&rel) {
                t.written.push(rel.clone());
            }
        });
        self.journal_write(&abs, &rel, turn, backup);
        if let Some(id) = turn {
            self.emit(AgentEvent::TurnUpdated(id));
        }
        Ok(json!({}))
    }

    fn journal_write(
        &self,
        abs: &Path,
        rel: &str,
        turn: Option<TurnId>,
        backup: Option<lector_git::ShadowSnapshot>,
    ) {
        let Some(journal) = Journal::of(&self.core) else {
            return;
        };
        let undo = backup.map(|shadow| {
            let after = self
                .shadow()
                .and_then(|s| s.fingerprints(&[rel]).ok())
                .unwrap_or_default();
            UndoRecord::RestoreFiles {
                shadow,
                paths: vec![rel.to_string()],
                after,
            }
        });
        let summary = match turn {
            Some(t) => format!("{} wrote {rel} (turn {t})", self.config.name),
            None => format!("{} wrote {rel}", self.config.name),
        };
        journal.append(ActivityEntry {
            id: 0,
            time: SystemTime::now(),
            command: "agent.write_file".into(),
            summary,
            repo: self.project.repo().map(|r| r.workdir().to_path_buf()),
            paths: vec![abs.to_path_buf()],
            undo,
            undone: false,
            args: json!({ "session": self.id.0, "turn": turn.map(|t| t.0), "path": rel }),
            source: Source::Agent,
            undo_of: None,
            undone_by: None,
        });
    }

    fn request_permission(&self, params: &Value) -> Result<Value, RpcError> {
        let call = params.get("toolCall").cloned().unwrap_or(Value::Null);
        let tool_call_id = call
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let options: Vec<PermissionOption> = params
            .get("options")
            .and_then(Value::as_array)
            .map(|opts| {
                opts.iter()
                    .filter_map(|o| {
                        Some(PermissionOption {
                            id: o.get("optionId")?.as_str()?.to_string(),
                            name: o
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            kind: o
                                .get("kind")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if options.is_empty() {
            return Err(RpcError::remote(
                rpc::INVALID_PARAMS,
                "no permission options",
            ));
        }
        let (req_id, turn, auto, kind) = {
            let mut st = self.state.lock();
            st.last_activity = Instant::now();
            let turn = st.current;
            let known = turn
                .and_then(|id| st.turns.iter().find(|t| t.id == id))
                .and_then(|t| t.tool_calls.iter().find(|c| c.id == tool_call_id))
                .cloned();
            let title = call
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| known.as_ref().map(|k| k.title.clone()))
                .unwrap_or_else(|| "Run a tool".into());
            let kind = call
                .get("kind")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| known.as_ref().map(|k| k.kind.clone()))
                .unwrap_or_else(|| "other".into());
            let locations_inside = call
                .get("locations")
                .and_then(Value::as_array)
                .map(|locs| {
                    locs.iter().all(|l| {
                        l.get("path")
                            .and_then(Value::as_str)
                            .is_some_and(|p| resolve_in_root(self.project.root(), p).is_ok())
                    })
                })
                .unwrap_or(true);
            let auto = if st.always_allowed.contains(&kind) {
                Some(PermissionChoice::AllowAlways)
            } else if self.options.auto_allow_reads && kind == "read" && locations_inside {
                Some(PermissionChoice::AllowOnce)
            } else {
                None
            };
            let req_id = PermissionRequestId(st.next_permission);
            st.next_permission += 1;
            let req = PermissionRequest {
                id: req_id,
                turn,
                tool_call_id,
                title,
                kind: kind.clone(),
                options: options.clone(),
                answer: auto,
                pending: auto.is_none(),
                automatic: auto.is_some(),
            };
            if let Some(t) = turn.and_then(|id| st.turns.iter_mut().find(|t| t.id == id)) {
                t.permissions.push(req);
                t.timeline
                    .push(TurnItem::Permission(t.permissions.len() - 1));
            }
            (req_id, turn, auto, kind)
        };
        let choice = match auto {
            Some(c) => Some(c),
            None => {
                let (tx, rx) = mpsc::channel();
                self.waiters.lock().insert(req_id, tx);
                // A cancel that raced with this request.
                if self.state.lock().cancel_requested.is_some() || turn.is_none() {
                    self.waiters.lock().remove(&req_id);
                    None
                } else {
                    self.emit(AgentEvent::PermissionRequested(req_id));
                    rx.recv().ok().flatten()
                }
            }
        };
        {
            let mut st = self.state.lock();
            st.last_activity = Instant::now();
            if choice == Some(PermissionChoice::AllowAlways) {
                st.always_allowed.insert(kind);
            }
            for t in &mut st.turns {
                if let Some(p) = t.permissions.iter_mut().find(|p| p.id == req_id) {
                    p.pending = false;
                    p.answer = choice;
                }
            }
        }
        self.emit(AgentEvent::PermissionResolved(req_id));
        if let Some(t) = turn {
            self.emit(AgentEvent::TurnUpdated(t));
        }
        let outcome = match choice.and_then(|c| pick_option(&options, c)) {
            Some(option) => json!({ "outcome": "selected", "optionId": option }),
            None => json!({ "outcome": "cancelled" }),
        };
        Ok(json!({ "outcome": outcome }))
    }
}

/// The agent's option id for the user's choice.
fn pick_option(options: &[PermissionOption], choice: PermissionChoice) -> Option<String> {
    let prefs: &[&str] = match choice {
        PermissionChoice::AllowOnce => &["allow_once", "allow_always"],
        PermissionChoice::AllowAlways => &["allow_always", "allow_once"],
        PermissionChoice::Reject => &["reject_once", "reject_always"],
    };
    prefs
        .iter()
        .find_map(|k| options.iter().find(|o| o.kind == *k))
        .or_else(|| match choice {
            PermissionChoice::Reject => None,
            _ => options.first(),
        })
        .map(|o| o.id.clone())
}

/// The RPC handler of one agent process (a restart gets a new one; stale events are ignored).
struct ConnHandler {
    shared: Weak<Shared>,
    generation: u64,
}

impl ConnHandler {
    fn shared(&self) -> Option<Arc<Shared>> {
        let shared = self.shared.upgrade()?;
        let current = shared.state.lock().generation == self.generation;
        current.then_some(shared)
    }
}

impl Handler for ConnHandler {
    fn on_request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let Some(shared) = self.shared() else {
            return Err(RpcError::remote(rpc::INTERNAL_ERROR, "session ended"));
        };
        match method {
            "fs/read_text_file" => shared.read_text_file(&params),
            "fs/write_text_file" => shared.write_text_file(&params),
            "session/request_permission" => shared.request_permission(&params),
            other => Err(RpcError::remote(
                rpc::METHOD_NOT_FOUND,
                format!("Lector does not support `{other}`"),
            )),
        }
    }

    fn on_notification(&self, method: &str, params: Value) {
        let Some(shared) = self.shared() else {
            return;
        };
        if method != "session/update" {
            tracing::trace!("ignored agent notification `{method}`");
            return;
        }
        let Some(update) = params.get("update") else {
            return;
        };
        if let Some(id) = shared.update_turn(|t| t.apply_update(update)) {
            shared.emit(AgentEvent::TurnUpdated(id));
        }
    }

    fn on_closed(&self, reason: String) {
        let Some(shared) = self.shared() else {
            return;
        };
        shared.settle_stderr();
        let changed = {
            let mut st = shared.state.lock();
            match st.status {
                SessionStatus::Stopped | SessionStatus::Crashed(_) => false,
                _ => {
                    st.status = SessionStatus::Crashed(with_stderr(
                        &format!("The agent exited ({reason})."),
                        &shared.stderr.lock(),
                    ));
                    true
                }
            }
        };
        shared.cancel_permissions();
        if changed {
            shared.idle.notify_all();
            shared.emit(AgentEvent::StatusChanged);
        }
    }
}

/// Absolute path of a project-relative path (helper for front-ends).
pub fn absolute(project: &Project, rel: &str) -> PathBuf {
    project.root().join(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_are_picked_by_kind() {
        let opts = vec![
            PermissionOption {
                id: "a".into(),
                name: "Allow".into(),
                kind: "allow_once".into(),
            },
            PermissionOption {
                id: "r".into(),
                name: "Reject".into(),
                kind: "reject_once".into(),
            },
        ];
        assert_eq!(
            pick_option(&opts, PermissionChoice::AllowAlways).as_deref(),
            Some("a")
        );
        assert_eq!(
            pick_option(&opts, PermissionChoice::Reject).as_deref(),
            Some("r")
        );
        assert_eq!(pick_option(&opts[..1], PermissionChoice::Reject), None);
    }
}
