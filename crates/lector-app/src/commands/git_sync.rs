//! Git sync commands (v0.7): fetch / pull / push, upstream state, branches and stash.
//!
//! All of them go through the git CLI (`lector_git::sync`, `lector_git` branch API) under
//! `lector-process`. The [`SyncHost`] owns credential prompts and progress (ADR 0002): it hosts
//! the askpass server ([`SyncHost::start_askpass`]; the UI only supplies the prompt handler),
//! network commands run with its askpass environment and can be cancelled through it, and their
//! progress is published to its subscribers. Failures keep their type: downcast a command error to
//! [`lector_git::Error`] and match `Error::Sync(SyncError { kind, hint, paths,
//! hook_output, alternatives, .. })`.
//!
//! Undo (journal): pull → `git.restore_ref` (the branch goes back to where it was, `reset
//! --keep` keeping staged changes; refused before writing when the move would touch files
//! with local changes); branch create / delete / rename / set upstream / switch → their
//! inverse; stash drop → `git.stash_store`; stash push → `git.stash_pop` with
//! `restore_index` (staged changes come back staged, or the undo is refused and the stash
//! kept); stash apply / pop → the files and index entries they can touch (the stash's paths
//! plus rename destinations), backed up in the shadow repo first (plus the stash entry for
//! pop). A stash apply that fails puts back what git changed before returning the error.
//! Fetch and push are journaled but not undoable.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use anyhow::{anyhow, bail};
use lector_base::CancelToken;
use lector_git::{
    FetchOptions, Oid, PullMode, PullStatus, PushOptions, Repo, StashApplyOptions,
    StashPushOptions, StatusEntry, SyncEvent, SyncOptions, APPLY_WITHOUT_INDEX,
};
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};

use super::git_write::{backup_files, backup_index, files_undo, index_undo, needs_repo, Backup};
use super::schema::{object, prop};
use super::{
    CommandContext, CommandRegistry, CommandSpec, FnCommand, NoticeLevel, Outcome, UiRequest,
};
use crate::core::AppCore;
use crate::journal::UndoRecord;
use crate::scheduler::{JobKey, JobKind, Priority, Scheduler, Subject};

// ---------------------------------------------------------------------------------------------
// Host: credentials environment, progress, cancellation
// ---------------------------------------------------------------------------------------------

/// A running sync operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningSync {
    /// Operation id (for [`SyncHost::cancel`]).
    pub id: u64,
    /// Repository work directory.
    pub repo: PathBuf,
    /// Command id (`git.fetch`).
    pub command: String,
}

/// A progress event of a running operation, delivered to [`SyncHost::subscribe`]rs on the
/// worker thread that runs the command.
#[derive(Debug, Clone)]
pub struct SyncProgress {
    /// Operation id.
    pub op: u64,
    /// Repository work directory.
    pub repo: PathBuf,
    /// Command id.
    pub command: String,
    /// The event.
    pub event: SyncEvent,
}

type Listener = Arc<dyn Fn(&SyncProgress) + Send + Sync>;

/// Process-wide state of git sync commands (an [`AppCore`] extension, [`SyncHost::of`]):
/// the askpass environment injected by the UI, the running operations (cancellable) and
/// progress subscribers.
#[derive(Default)]
pub struct SyncHost {
    env: RwLock<Vec<(OsString, OsString)>>,
    askpass: Mutex<AskpassState>,
    running: Mutex<BTreeMap<u64, (RunningSync, CancelToken)>>,
    listeners: Mutex<Vec<(u64, Listener)>>,
    next: AtomicU64,
    /// Runs askpass connections (a prompt waits for the user: the blocking lane).
    scheduler: Option<Scheduler>,
}

impl std::fmt::Debug for SyncHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncHost")
            .field("running", &self.running())
            .finish_non_exhaustive()
    }
}

/// Credential prompts from git and ssh (askpass): the request / reply types a prompt handler
/// passed to [`SyncHost::start_askpass`] works with, and the pure prompt-text helpers. UI
/// crates use these instead of depending on `lector-process` (ADR 0002).
pub mod askpass {
    pub use lector_process::askpass::{
        classify_prompt, parse_prompt, sanitize_prompt_text, ParsedPrompt, PromptKind, PromptReply,
        PromptRequest, Secret,
    };
}

/// The askpass server [`SyncHost`] owns, or why it could not start.
#[derive(Default)]
struct AskpassState {
    server: Option<lector_process::askpass::AskpassServer>,
    start_error: Option<String>,
    /// The handler the running server delegates to (replaced by each `start_askpass`).
    handler: Arc<RwLock<Option<PromptHandler>>>,
}

type PromptHandler =
    Arc<dyn Fn(askpass::PromptRequest) -> askpass::PromptReply + Send + Sync + 'static>;

/// Keeps a [`SyncHost::subscribe`] callback registered; dropping it unsubscribes.
pub struct SyncSubscription {
    host: Weak<SyncHost>,
    id: u64,
}

impl Drop for SyncSubscription {
    fn drop(&mut self) {
        if let Some(h) = self.host.upgrade() {
            h.listeners.lock().retain(|(id, _)| *id != self.id);
        }
    }
}

struct OpGuard {
    host: Arc<SyncHost>,
    id: u64,
    repo: PathBuf,
    command: &'static str,
    options: SyncOptions,
}

impl OpGuard {
    fn emit(&self, event: SyncEvent) {
        self.host.emit(&SyncProgress {
            op: self.id,
            repo: self.repo.clone(),
            command: self.command.to_owned(),
            event,
        });
    }
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        self.host.running.lock().remove(&self.id);
    }
}

impl SyncHost {
    /// The core's host (created on first use).
    pub fn of(core: &AppCore) -> Arc<SyncHost> {
        core.extension_or_init(|| {
            Arc::new(SyncHost {
                scheduler: Some(core.scheduler().clone()),
                ..SyncHost::default()
            })
        })
    }

    /// Sets the environment added to network git commands — the UI passes
    /// `server.env(&server.helper_program(&current_exe)?)` here once its askpass server runs.
    /// Never contains credentials themselves, only how to ask for them.
    pub fn set_askpass_env(&self, env: Vec<(OsString, OsString)>) {
        *self.env.write() = env;
    }

    /// Registers `handler` as the answer to credential prompts and starts the askpass server
    /// once per host, installing its environment for network git commands
    /// ([`SyncHost::set_askpass_env`]). Later calls only replace the handler and return the
    /// first start's result. `handler` runs on the server's connection thread and may block
    /// until the user answers; without one, prompts are cancelled. Its socket and helper program live in a private
    /// directory under `runtime_dir` (Lector's data directory in the app). A prompt names the
    /// operation that asked through [`askpass::PromptRequest::op`]: the id of a
    /// [`SyncHost::running`] operation, which the host tags each network command with.
    ///
    /// Failing to start is not fatal: git then fails with an authentication error where it
    /// would have prompted; [`SyncHost::askpass_start_error`] says why.
    pub fn start_askpass(
        &self,
        runtime_dir: &Path,
        handler: impl Fn(askpass::PromptRequest) -> askpass::PromptReply + Send + Sync + 'static,
    ) -> Result<(), String> {
        use lector_process::askpass::{AskpassError, AskpassServer};
        let mut state = self.askpass.lock();
        *state.handler.write() = Some(Arc::new(handler));
        if state.server.is_some() {
            return Ok(());
        }
        if let Some(err) = &state.start_error {
            return Err(err.clone());
        }
        let current = state.handler.clone();
        let delegate = move |request: askpass::PromptRequest| -> askpass::PromptReply {
            let handler = current.read().clone();
            match handler {
                Some(h) => h(request),
                None => askpass::PromptReply::Cancel,
            }
        };
        let runner = self
            .scheduler
            .clone()
            .unwrap_or_else(Scheduler::global)
            .task_runner(
                JobKey::new(JobKind::AskpassPrompt, Subject::App),
                Priority::Visible,
            );
        let started = AskpassServer::start(runtime_dir, runner, delegate).and_then(|server| {
            let exe = std::env::current_exe().map_err(AskpassError::Io)?;
            let helper = server.helper_program(&exe).map_err(AskpassError::Io)?;
            Ok((server.env(&helper), server))
        });
        match started {
            Ok((env, server)) => {
                self.set_askpass_env(env);
                state.server = Some(server);
                Ok(())
            }
            Err(err) => {
                tracing::warn!("askpass: {err}; git cannot ask for credentials in this session");
                state.start_error = Some(err.to_string());
                Err(err.to_string())
            }
        }
    }

    /// Whether the askpass server runs.
    pub fn askpass_running(&self) -> bool {
        self.askpass.lock().server.is_some()
    }

    /// Why the askpass server could not start, if it failed.
    pub fn askpass_start_error(&self) -> Option<String> {
        self.askpass.lock().start_error.clone()
    }

    /// Stops the askpass server and removes its private directory (socket, helper program);
    /// network commands started afterwards get no askpass environment. Called when the app
    /// quits.
    pub fn shutdown_askpass(&self) {
        let server = {
            let mut state = self.askpass.lock();
            *state.handler.write() = None;
            state.server.take()
        };
        if let Some(server) = server {
            self.set_askpass_env(Vec::new());
            server.shutdown();
        }
    }

    /// Calls `cb` for every progress event of every sync command (on worker threads).
    pub fn subscribe(
        self: &Arc<Self>,
        cb: impl Fn(&SyncProgress) + Send + Sync + 'static,
    ) -> SyncSubscription {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().push((id, Arc::new(cb)));
        SyncSubscription {
            host: Arc::downgrade(self),
            id,
        }
    }

    /// Operations running now.
    pub fn running(&self) -> Vec<RunningSync> {
        self.running
            .lock()
            .values()
            .map(|(r, _)| r.clone())
            .collect()
    }

    /// Cancels operation `id` (git and everything it started are killed); `false` if it is
    /// not running.
    pub fn cancel(&self, id: u64) -> bool {
        match self.running.lock().get(&id) {
            Some((_, token)) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Cancels every operation in repository `workdir`; returns how many.
    pub fn cancel_repo(&self, workdir: &Path) -> usize {
        let running = self.running.lock();
        let mut n = 0;
        for (info, token) in running.values() {
            if info.repo == workdir {
                token.cancel();
                n += 1;
            }
        }
        n
    }

    fn emit(&self, p: &SyncProgress) {
        let listeners: Vec<Listener> = self
            .listeners
            .lock()
            .iter()
            .map(|(_, l)| l.clone())
            .collect();
        for l in listeners {
            l(p);
        }
    }

    fn begin(self: &Arc<Self>, repo: &Repo, command: &'static str) -> OpGuard {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let token = CancelToken::new();
        let info = RunningSync {
            id,
            repo: repo.workdir().to_path_buf(),
            command: command.to_owned(),
        };
        self.running.lock().insert(id, (info, token.clone()));
        OpGuard {
            host: self.clone(),
            id,
            repo: repo.workdir().to_path_buf(),
            command,
            options: SyncOptions::default()
                .with_env({
                    // Tag the operation so a credential prompt can be matched to it.
                    let mut env = self.env.read().clone();
                    if !env.is_empty() {
                        env.push(lector_process::askpass::op_env(id));
                    }
                    env
                })
                .with_cancel(token),
        }
    }
}

/// The credential helpers git is configured with in `workdir` (`credential.helper`, every
/// value), for explaining an authentication failure. Empty when there are none or git cannot
/// tell within a few seconds. Blocking (runs `git config`).
pub fn credential_helpers(workdir: &Path) -> Vec<String> {
    let cmd = lector_process::ProcessCommand::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["config", "--get-all", "credential.helper"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .kind(lector_process::ProcessKind::Git)
        .label("git config credential.helper");
    let opts = lector_process::RunOptions {
        timeout: Some(std::time::Duration::from_secs(5)),
        ..Default::default()
    };
    match lector_process::run(&cmd, &opts) {
        Ok(out) if out.success() => out
            .stdout_text()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// Registration and helpers
// ---------------------------------------------------------------------------------------------

/// Registers the git sync commands.
pub fn register(r: &CommandRegistry) {
    r.register(upstream_status());
    r.register(list_remotes());
    r.register(stash_list());
    r.register(fetch());
    r.register(pull());
    r.register(push());
    r.register(create_branch());
    r.register(switch_branch());
    r.register(rename_branch());
    r.register(delete_branch());
    r.register(set_upstream());
    r.register(stash_push());
    r.register(stash_apply());
    r.register(stash_pop());
    r.register(stash_drop());
    r.register(stash_store());
    r.register(restore_ref());
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn bool_prop(desc: &str) -> Value {
    prop("boolean", desc)
}

fn nullable_string(desc: &str) -> Value {
    json!({ "type": ["string", "null"], "description": desc })
}

fn oid_arg(args: &Value, key: &str) -> anyhow::Result<Option<Oid>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => s
            .parse::<Oid>()
            .map(Some)
            .map_err(|_| anyhow!("`{key}` is not a commit id: {s}")),
        Some(_) => bail!("`{key}` must be a commit id or null"),
    }
}

fn oid_json(o: Option<Oid>) -> Value {
    o.map_or(Value::Null, |o| json!(o.to_string()))
}

fn current_branch(repo: &Repo) -> anyhow::Result<String> {
    let head = repo.head()?;
    match head.branch.filter(|_| !head.detached) {
        Some(b) => Ok(b),
        None => bail!("HEAD is detached; name a branch"),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

fn data<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn invoke(command: &str, args: Value) -> UndoRecord {
    UndoRecord::Invoke {
        command: command.to_owned(),
        args,
    }
}

fn with_undo(outcome: Outcome, undo: Option<UndoRecord>) -> Outcome {
    match undo {
        Some(u) => outcome.with_undo(u),
        None => outcome,
    }
}

// ---------------------------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------------------------

fn upstream_status() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "git.upstream_status",
            "Show Upstream Status",
            "Git",
            object(&[], &[]),
        ),
        |cx, _| {
            let repo = cx.require_repo()?;
            let st = repo.upstream_status()?;
            let summary = match (&st.branch, &st.upstream) {
                (Some(b), Some(u)) => {
                    format!("{b} → {u}: {} ahead, {} behind", st.ahead, st.behind)
                }
                (Some(b), None) if st.upstream_gone => format!("{b}: its upstream is gone"),
                (Some(b), None) => format!("{b} has no upstream"),
                (None, _) => "HEAD is detached".to_owned(),
            };
            Ok(Outcome::new(summary).with_data(data(&st)))
        },
    )
    .with_available(needs_repo)
}

fn list_remotes() -> FnCommand {
    FnCommand::new(
        CommandSpec::read("git.list_remotes", "List Remotes", "Git", object(&[], &[])),
        |cx, _| {
            let repo = cx.require_repo()?;
            let remotes = repo.remotes()?;
            let list: Vec<Value> = remotes
                .iter()
                .map(|r| json!({ "name": r.name, "url": lector_git::sync::redact_credentials(&r.url) }))
                .collect();
            Ok(Outcome::new(plural(remotes.len(), "remote", "remotes"))
                .with_data(json!({ "remotes": list })))
        },
    )
    .with_available(needs_repo)
}

fn stash_list() -> FnCommand {
    FnCommand::new(
        CommandSpec::read("git.stash_list", "List Stashes", "Git", object(&[], &[])),
        |cx, _| {
            let repo = cx.require_repo()?;
            let stashes = repo.stashes()?;
            Ok(Outcome::new(plural(stashes.len(), "stash", "stashes"))
                .with_data(json!({ "stashes": data(&stashes) })))
        },
    )
    .with_available(needs_repo)
}

// ---------------------------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------------------------

fn fetch() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.fetch",
            "Fetch",
            "Git",
            object(
                &[
                    (
                        "remote",
                        prop("string", "Remote to fetch (default: all remotes)"),
                    ),
                    (
                        "prune",
                        bool_prop("Remove remote-tracking branches deleted on the remote"),
                    ),
                ],
                &[],
            ),
        )
        .not_undoable("Fetching only moves remote-tracking branches to where the remote has them; the next fetch would move them again.")
        .with_ui_flow("git::Fetch"),
        |cx, args| {
            let repo = cx.require_repo()?;
            let host = SyncHost::of(&cx.core);
            let op = host.begin(repo, "git.fetch");
            let opts = FetchOptions {
                remote: str_arg(args, "remote").map(str::to_owned),
                prune: flag(args, "prune"),
            };
            let out = repo.fetch(&opts, &op.options, &mut |e| op.emit(e))?;
            let what = opts.remote.as_deref().unwrap_or("all remotes");
            let summary = if out.updated.is_empty() {
                format!("Fetched {what}: already up to date")
            } else {
                format!(
                    "Fetched {what}: {} updated",
                    plural(out.updated.len(), "ref", "refs")
                )
            };
            Ok(Outcome::new(summary)
                .with_level(if out.updated.is_empty() {
                    NoticeLevel::Info
                } else {
                    NoticeLevel::Success
                })
                .with_data(data(&out))
                .with_paths(vec![repo.git_dir().to_path_buf()])
                .with_ui(UiRequest::Refresh))
        },
    )
    .with_available(needs_repo)
}

fn pull() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.pull",
            "Pull",
            "Git",
            object(
                &[(
                    "mode",
                    json!({"type": "string", "enum": ["ff_only", "rebase"],
                           "description": "ff_only (default; never creates a merge commit) or rebase"}),
                )],
                &[],
            ),
        )
        .undoable()
        .with_ui_flow("git::Pull"),
        |cx, args| {
            let repo = cx.require_repo()?;
            let mode = match str_arg(args, "mode") {
                Some("rebase") => PullMode::Rebase,
                _ => PullMode::FastForwardOnly,
            };
            let host = SyncHost::of(&cx.core);
            let op = host.begin(repo, "git.pull");
            let out = repo.pull(mode, &op.options, &mut |e| op.emit(e))?;
            let summary = match out.status {
                PullStatus::UpToDate => format!("{} is up to date with {}", out.branch, out.upstream),
                PullStatus::FastForwarded => format!(
                    "Pulled {} into {} (fast-forward)",
                    plural(out.incoming as usize, "commit", "commits"),
                    out.branch
                ),
                PullStatus::Rebased => format!(
                    "Rebased {} onto {} ({} incoming)",
                    plural(out.rebased as usize, "local commit", "local commits"),
                    out.upstream,
                    out.incoming
                ),
                PullStatus::Conflicts => format!(
                    "Pull stopped with conflicts in {}: resolve them, then continue or abort the rebase",
                    plural(out.conflicts.len(), "file", "files")
                ),
            };
            let undo = match (out.status, out.before, out.after) {
                (PullStatus::FastForwarded | PullStatus::Rebased, Some(before), Some(after)) => {
                    Some(invoke(
                        "git.restore_ref",
                        json!({
                            "ref": format!("refs/heads/{}", out.branch),
                            "expected": after.to_string(),
                            "target": before.to_string(),
                        }),
                    ))
                }
                _ => None,
            };
            let level = match out.status {
                PullStatus::UpToDate => NoticeLevel::Info,
                PullStatus::Conflicts => NoticeLevel::Warning,
                _ => NoticeLevel::Success,
            };
            let outcome = Outcome::new(summary)
                .with_level(level)
                .with_data(data(&out))
                .with_paths(vec![repo.workdir().to_path_buf()])
                .with_ui(UiRequest::Refresh);
            Ok(with_undo(outcome, undo))
        },
    )
    .with_available(needs_repo)
}

fn push() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.push",
            "Push",
            "Git",
            object(
                &[
                    ("remote", prop("string", "Remote (default: the branch's push remote)")),
                    ("branch", prop("string", "Local branch (default: the current branch)")),
                    ("set_upstream", bool_prop("Set the upstream when the branch has none (default true)")),
                    ("force_with_lease", bool_prop("Overwrite the remote branch only if it is where we last fetched it (rewrites remote history; plain force is never used)")),
                ],
                &[],
            ),
        )
        .not_undoable("A push changes the remote, which others may already have fetched.")
        .with_ui_flow("git::Push"),
        |cx, args| {
            let repo = cx.require_repo()?;
            let opts = PushOptions {
                remote: str_arg(args, "remote").map(str::to_owned),
                branch: str_arg(args, "branch").map(str::to_owned),
                set_upstream: args.get("set_upstream").and_then(Value::as_bool).unwrap_or(true),
                force_with_lease: flag(args, "force_with_lease"),
            };
            let host = SyncHost::of(&cx.core);
            let op = host.begin(repo, "git.push");
            let out = repo.push(&opts, &op.options, &mut |e| op.emit(e))?;
            let summary = if out.up_to_date {
                format!("{} is already up to date on {}", out.branch, out.remote)
            } else {
                let mut s = format!("Pushed {} to {}/{}", out.branch, out.remote, out.remote_branch);
                if out.set_upstream {
                    s.push_str(" and set it as upstream");
                }
                s.push_str(" (pushes cannot be undone)");
                s
            };
            Ok(Outcome::new(summary)
                .with_level(if out.up_to_date {
                    NoticeLevel::Info
                } else {
                    NoticeLevel::Success
                })
                .with_data(data(&out))
                .with_paths(vec![repo.git_dir().to_path_buf()])
                .with_ui(UiRequest::Refresh))
        },
    )
    .with_available(needs_repo)
}

// ---------------------------------------------------------------------------------------------
// Branches
// ---------------------------------------------------------------------------------------------

fn create_branch() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.create_branch",
            "Create Branch",
            "Git",
            object(
                &[
                    ("name", prop("string", "New branch name")),
                    ("start_point", prop("string", "Commit or branch to start from (default: HEAD)")),
                    ("switch", bool_prop("Switch to it")),
                ],
                &["name"],
            ),
        )
        .undoable()
        .with_ui_flow("git::NewBranch"),
        |cx, args| {
            let repo = cx.require_repo()?;
            let name = str_arg(args, "name").unwrap_or_default();
            let tip = repo.create_branch(name, str_arg(args, "start_point"))?;
            let delete = invoke(
                "git.restore_ref",
                json!({ "ref": format!("refs/heads/{name}"), "expected": tip.to_string(), "target": null }),
            );
            let mut summary = format!("Created branch {name} at {}", tip.short());
            let mut undo = Some(delete.clone());
            let mut switched = Value::Null;
            if flag(args, "switch") {
                let host = SyncHost::of(&cx.core);
                let op = host.begin(repo, "git.create_branch");
                let sw = match repo.switch_branch(name, &op.options) {
                    Ok(sw) => sw,
                    Err(err) => {
                        // Leave nothing behind: the branch was only created to switch to it.
                        let _ = repo.restore_ref(&format!("refs/heads/{name}"), Some(tip), None);
                        return Err(err.into());
                    }
                };
                summary = format!("Created and switched to branch {name}");
                undo = sw.from.as_ref().map(|from| UndoRecord::Multi {
                    records: vec![delete, invoke("git.switch_branch", json!({ "name": from }))],
                });
                switched = data(&sw);
            }
            let outcome = Outcome::new(summary)
                .with_data(json!({ "name": name, "oid": tip.to_string(), "switch": switched }))
                .with_paths(vec![repo.git_dir().to_path_buf()])
                .with_ui(UiRequest::Refresh);
            Ok(with_undo(outcome, undo))
        },
    )
    .with_available(needs_repo)
}

fn switch_branch() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.switch_branch",
            "Switch Branch",
            "Git",
            object(
                &[
                    ("name", prop("string", "Branch to switch to (a remote branch name creates a tracking branch)")),
                    ("stash_changes", bool_prop("Stash local changes (including untracked files) first — the explicit \"stash and switch\" choice offered when local changes would be overwritten")),
                ],
                &["name"],
            ),
        )
        .undoable()
        .with_ui_flow("git::SwitchBranch"),
        |cx, args| {
            let repo = cx.require_repo()?;
            let name = str_arg(args, "name").unwrap_or_default();
            let host = SyncHost::of(&cx.core);
            let op = host.begin(repo, "git.switch_branch");
            let stash = if flag(args, "stash_changes") {
                repo.stash_push(&StashPushOptions {
                    message: Some(format!("{SWITCH_STASH_PREFIX}{name}")),
                    include_untracked: true,
                })?
            } else {
                None
            };
            let sw = match repo.switch_branch(name, &op.options) {
                Ok(sw) => sw,
                Err(err) => {
                    // Put the changes back where they were, staged changes staged again.
                    let mut err = err;
                    if let Some(st) = &stash {
                        let restored = repo.stash_index_of(st.oid).and_then(|ix| match ix {
                            Some(ix) => repo
                                .stash_apply_with(
                                    ix,
                                    &StashApplyOptions {
                                        pop: true,
                                        expected: Some(st.oid),
                                        restore_index: true,
                                    },
                                )
                                .map(|a| a.dropped),
                            None => Ok(false),
                        });
                        if !matches!(restored, Ok(true)) {
                            tracing::warn!("could not restore the stash after a failed switch: {restored:?}");
                            err = with_note(
                                err,
                                &format!(" (your changes are kept in the stash: {})", st.message),
                            );
                        }
                    }
                    return Err(err.into());
                }
            };
            let mut records = Vec::new();
            if let Some(st) = &stash {
                records.push(invoke(
                    "git.stash_pop",
                    json!({ "oid": st.oid.to_string(), "restore_index": true }),
                ));
            }
            if let Some(from) = &sw.from {
                records.push(invoke("git.switch_branch", json!({ "name": from })));
            }
            let undo = match (sw.from.is_some(), records.len()) {
                (false, _) | (_, 0) => None,
                (true, 1) => records.pop(),
                _ => Some(UndoRecord::Multi { records }),
            };
            let mut summary = format!("Switched to {}", sw.to);
            if let Some(st) = &stash {
                summary.push_str(&format!(" (your changes are stashed: {})", st.message));
            }
            let outcome = Outcome::new(summary)
                .with_data(json!({ "switch": data(&sw), "stash": data(&stash) }))
                .with_paths(vec![repo.workdir().to_path_buf()])
                .with_ui(UiRequest::Refresh);
            Ok(with_undo(outcome, undo))
        },
    )
    .with_available(needs_repo)
}

fn rename_branch() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.rename_branch",
            "Rename Branch",
            "Git",
            object(
                &[
                    (
                        "name",
                        prop("string", "Branch to rename (default: the current branch)"),
                    ),
                    ("new_name", prop("string", "Its new name")),
                ],
                &["new_name"],
            ),
        )
        .undoable()
        .with_ui_flow("git::RenameBranch"),
        |cx, args| {
            let repo = cx.require_repo()?;
            let old = match str_arg(args, "name") {
                Some(n) => n.to_owned(),
                None => current_branch(repo)?,
            };
            let new = str_arg(args, "new_name").unwrap_or_default();
            repo.rename_branch(&old, new)?;
            Ok(Outcome::new(format!("Renamed branch {old} to {new}"))
                .with_data(json!({ "old": old, "new": new }))
                .with_undo(invoke(
                    "git.rename_branch",
                    json!({ "name": new, "new_name": old }),
                ))
                .with_paths(vec![repo.git_dir().to_path_buf()])
                .with_ui(UiRequest::Refresh))
        },
    )
    .with_available(needs_repo)
}

fn delete_branch() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.delete_branch",
            "Delete Branch",
            "Git",
            object(
                &[
                    ("name", prop("string", "Branch to delete")),
                    ("force", bool_prop("Delete even if it is not fully merged")),
                ],
                &["name"],
            ),
        )
        .destructive()
        .undoable()
        .with_ui_flow("git::DeleteBranch"),
        |cx, args| {
            let repo = cx.require_repo()?;
            let name = str_arg(args, "name").unwrap_or_default();
            let gone = repo.delete_branch(name, flag(args, "force"))?;
            let mut undo_args = json!({
                "ref": format!("refs/heads/{name}"),
                "expected": null,
                "target": gone.oid.to_string(),
            });
            if let Some(up) = &gone.upstream {
                undo_args["upstream"] = json!(up);
            }
            Ok(
                Outcome::new(format!("Deleted branch {name} (was {})", gone.oid.short()))
                    .with_data(data(&gone))
                    .with_undo(invoke("git.restore_ref", undo_args))
                    .with_paths(vec![repo.git_dir().to_path_buf()])
                    .with_ui(UiRequest::Refresh),
            )
        },
    )
    .with_available(needs_repo)
}

fn set_upstream() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.set_upstream",
            "Set Upstream Branch",
            "Git",
            object(
                &[
                    (
                        "branch",
                        prop("string", "Local branch (default: the current branch)"),
                    ),
                    (
                        "upstream",
                        nullable_string("Upstream (`origin/main`), or null to remove it"),
                    ),
                ],
                &["upstream"],
            ),
        )
        .undoable()
        .with_ui_flow("git::SetUpstream"),
        |cx, args| {
            let repo = cx.require_repo()?;
            let branch = match str_arg(args, "branch") {
                Some(b) => b.to_owned(),
                None => current_branch(repo)?,
            };
            let upstream = str_arg(args, "upstream");
            let previous = repo.set_upstream(&branch, upstream)?;
            let summary = match upstream {
                Some(u) => format!("{branch} now tracks {u}"),
                None => format!("{branch} no longer has an upstream"),
            };
            Ok(Outcome::new(summary)
                .with_data(json!({ "branch": branch, "upstream": upstream, "previous": previous }))
                .with_undo(invoke(
                    "git.set_upstream",
                    json!({ "branch": branch, "upstream": previous }),
                ))
                .with_paths(vec![repo.git_dir().to_path_buf()])
                .with_ui(UiRequest::Refresh))
        },
    )
    .with_available(needs_repo)
}

/// Undo target of pull / branch create / delete: puts a branch ref back.
fn restore_ref() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.restore_ref",
            "Restore Branch Position",
            "Git",
            object(
                &[
                    ("ref", prop("string", "Full branch ref (refs/heads/<name>)")),
                    ("expected", nullable_string("Where it must be now (null: must not exist); otherwise nothing is changed")),
                    ("target", nullable_string("Where to put it (null: delete it)")),
                    ("upstream", prop("string", "Upstream to set after restoring a branch")),
                ],
                &["ref", "expected", "target"],
            ),
        )
        .undoable(),
        |cx, args| {
            let repo = cx.require_repo()?;
            let refname = str_arg(args, "ref").unwrap_or_default();
            let expected = oid_arg(args, "expected")?;
            let target = oid_arg(args, "target")?;
            let branch = refname.strip_prefix("refs/heads/").unwrap_or(refname);
            // Removing a branch removes its upstream setting too, as `git branch -d` does, and
            // the inverse puts it back; `update-ref -d` alone would leave it in the config.
            let mut removed_upstream = None;
            if target.is_none() && expected.is_some() && repo.branch_tip(branch)? == expected {
                removed_upstream = repo.set_upstream(branch, None)?;
            }
            if let Err(err) = repo.restore_ref(refname, expected, target) {
                if let Some(up) = &removed_upstream {
                    let _ = repo.set_upstream(branch, Some(up));
                }
                return Err(err.into());
            }
            if let (Some(up), Some(_)) = (str_arg(args, "upstream"), target) {
                if let Err(e) = repo.set_upstream(branch, Some(up)) {
                    tracing::warn!("restored {branch} without its upstream {up}: {e}");
                }
            }
            let summary = match target {
                Some(t) => format!("Restored {branch} to {}", t.short()),
                None => format!("Removed branch {branch}"),
            };
            let mut undo_args = json!({ "ref": refname, "expected": oid_json(target), "target": oid_json(expected) });
            if let Some(up) = &removed_upstream {
                undo_args["upstream"] = json!(up);
            }
            Ok(Outcome::new(summary)
                .with_data(json!({ "ref": refname, "from": oid_json(expected), "to": oid_json(target) }))
                .with_undo(invoke("git.restore_ref", undo_args))
                .with_paths(vec![repo.workdir().to_path_buf()])
                .with_ui(UiRequest::Refresh))
        },
    )
    .with_available(needs_repo)
}

// ---------------------------------------------------------------------------------------------
// Stash
// ---------------------------------------------------------------------------------------------

fn stash_ref_props() -> [(&'static str, Value); 2] {
    [
        (
            "index",
            json!({"type": "integer", "minimum": 0, "description": "stash@{index} (default 0)"}),
        ),
        (
            "oid",
            prop(
                "string",
                "The stash commit (stable across pushes and drops; wins over index)",
            ),
        ),
    ]
}

fn stash_index(repo: &Repo, args: &Value) -> anyhow::Result<usize> {
    if let Some(oid) = oid_arg(args, "oid")? {
        return repo
            .stash_index_of(oid)?
            .ok_or_else(|| anyhow!("no stash entry {}", oid.short()));
    }
    let ix = args.get("index").and_then(Value::as_u64).unwrap_or(0);
    usize::try_from(ix).map_err(|_| anyhow!("stash index out of range"))
}

fn stash_push() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.stash_push",
            "Stash Changes",
            "Git",
            object(
                &[
                    ("message", prop("string", "Stash message")),
                    ("include_untracked", bool_prop("Include untracked files")),
                ],
                &[],
            ),
        )
        .undoable()
        .with_ui_flow("git::StashChanges"),
        |cx, args| {
            let repo = cx.require_repo()?;
            let stash = repo.stash_push(&StashPushOptions {
                message: str_arg(args, "message").map(str::to_owned),
                include_untracked: flag(args, "include_untracked"),
            })?;
            let Some(stash) = stash else {
                return Ok(Outcome::new("No local changes to stash")
                    .with_level(NoticeLevel::Info)
                    .with_data(json!({ "stash": null })));
            };
            Ok(Outcome::new(format!("Stashed: {}", stash.message))
                .with_data(json!({ "stash": data(&stash) }))
                .with_undo(invoke(
                    "git.stash_pop",
                    json!({ "oid": stash.oid.to_string(), "restore_index": true }),
                ))
                .with_paths(vec![repo.workdir().to_path_buf()])
                .with_ui(UiRequest::Refresh))
        },
    )
    .with_available(needs_repo)
}

/// Message prefix of the stash Lector's "stash and switch" creates; applying or popping such a
/// stash restores its index by default (`restore_index` absent).
const SWITCH_STASH_PREFIX: &str = "lector: before switching to ";

/// Appends `note` to the message of a typed git error (other errors are returned as is).
fn with_note(mut err: lector_git::Error, note: &str) -> lector_git::Error {
    if let lector_git::Error::Sync(s) = &mut err {
        s.message.push_str(note);
    }
    err
}

fn is_index_refusal(err: &lector_git::Error) -> bool {
    matches!(err, lector_git::Error::Sync(s) if s.alternatives.iter().any(|a| a == APPLY_WITHOUT_INDEX))
}

/// Puts back the files and index entries of `rels` that changed since `before` (a failed
/// apply); returns the paths put back, or why that failed.
fn roll_back(
    files: Option<&Backup>,
    index: Option<&Backup>,
    rels: &[String],
    before: &(BTreeMap<String, String>, BTreeMap<String, String>),
) -> Result<Vec<String>, String> {
    let refs: Vec<&str> = rels.iter().map(String::as_str).collect();
    let mut restored: Vec<String> = Vec::new();
    if let Some(b) = files {
        let now = b.shadow().fingerprints(&refs).map_err(|e| e.to_string())?;
        let changed: Vec<&str> = refs
            .iter()
            .copied()
            .filter(|p| now.get(*p) != before.0.get(*p))
            .collect();
        if !changed.is_empty() {
            b.shadow()
                .restore_paths(b.snapshot(), &changed)
                .map_err(|e| e.to_string())?;
            restored.extend(changed.iter().map(|p| p.to_string()));
        }
    }
    if let Some(b) = index {
        let now = b
            .shadow()
            .index_fingerprints(&refs)
            .map_err(|e| e.to_string())?;
        let changed: Vec<&str> = refs
            .iter()
            .copied()
            .filter(|p| now.get(*p) != before.1.get(*p))
            .collect();
        if !changed.is_empty() {
            b.shadow()
                .restore_index(b.snapshot(), &changed)
                .map_err(|e| e.to_string())?;
            restored.extend(changed.iter().map(|p| p.to_string()));
        }
    }
    restored.sort();
    restored.dedup();
    Ok(restored)
}

fn status_map(entries: Vec<StatusEntry>) -> BTreeMap<String, StatusEntry> {
    entries.into_iter().map(|e| (e.path.clone(), e)).collect()
}

fn stash_apply_impl(cx: &CommandContext, args: &Value, pop: bool) -> anyhow::Result<Outcome> {
    let repo = cx.require_repo()?;
    let index = stash_index(repo, args)?;
    let stash = repo.stash_at(index)?;
    // git's default is to flatten the stash's index into unstaged changes; Lector's own
    // stash-and-switch stash and undo (explicit `restore_index: true`) restore it.
    let explicit = args.get("restore_index").and_then(Value::as_bool);
    let restore_index = explicit.unwrap_or_else(|| stash.message.contains(SWITCH_STASH_PREFIX));
    // Every path the apply can write now, rename destinations included.
    let paths = repo.stash_apply_paths(index)?;
    let note = format!(
        "before stash {} of {}",
        if pop { "pop" } else { "apply" },
        stash.oid.short()
    );
    let status_before = status_map(repo.status()?);
    // Applying merges into the working tree and index: back both up so Undo restores them.
    let files = backup_files(cx, repo, &paths, &note, flag(args, "allow_without_backup"))?;
    let index_backup = backup_index(cx, repo, &paths, &note);
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let before = (
        files
            .as_ref()
            .and_then(|b| b.shadow().fingerprints(&refs).ok())
            .unwrap_or_default(),
        index_backup
            .as_ref()
            .and_then(|b| b.shadow().index_fingerprints(&refs).ok())
            .unwrap_or_default(),
    );
    let fail = |err: lector_git::Error| -> anyhow::Error {
        // Whatever git changed before failing goes back: a failed apply changes nothing.
        let note = match roll_back(files.as_ref(), index_backup.as_ref(), &paths, &before) {
            Ok(restored) if restored.is_empty() => String::new(),
            Ok(restored) => format!(
                " (Lector put back the {} git had changed)",
                plural(restored.len(), "file", "files")
            ),
            Err(e) => format!(
                " (git changed some files and they could not be put back: {e}; \
                 Lector's backup \"{note}\" has their previous versions)"
            ),
        };
        let note = if files.is_none() && note.is_empty() {
            " (no backup was taken: check the files the stash touches)".to_owned()
        } else {
            note
        };
        with_note(err, &note).into()
    };
    let opts = StashApplyOptions {
        pop,
        expected: Some(stash.oid),
        restore_index,
    };
    let mut flattened = false;
    let applied = match repo.stash_apply_with(index, &opts) {
        Ok(a) => a,
        // Lector's stash-and-switch stash on an index that moved on: apply it the way git
        // does by default and say so (an explicit `restore_index: true` never falls back).
        Err(err) if explicit.is_none() && restore_index && is_index_refusal(&err) => {
            flattened = true;
            let opts = StashApplyOptions {
                restore_index: false,
                ..opts
            };
            repo.stash_apply_with(index, &opts).map_err(fail)?
        }
        Err(err) => return Err(fail(err)),
    };
    // Paths git changed that were not backed up (rename detection the merge did differently):
    // Undo cannot restore those, so say so instead of reporting a complete undo.
    let status_after = status_map(repo.status()?);
    let mut not_backed_up: Vec<String> = status_before
        .keys()
        .chain(status_after.keys())
        .filter(|p| status_before.get(*p) != status_after.get(*p) && !paths.contains(*p))
        .cloned()
        .collect();
    not_backed_up.sort();
    not_backed_up.dedup();
    let mut records: Vec<UndoRecord> = Vec::new();
    records.extend(index_undo(index_backup, &paths));
    records.extend(files_undo(files, &paths));
    if applied.dropped {
        records.push(invoke(
            "git.stash_store",
            json!({ "oid": stash.oid.to_string(), "message": stash.message }),
        ));
    }
    let undo = match records.len() {
        0 => None,
        1 => records.pop(),
        _ => Some(UndoRecord::Multi { records }),
    };
    let verb = if pop { "Popped" } else { "Applied" };
    let mut summary = format!("{verb} {}", stash.message);
    if !applied.conflicts.is_empty() {
        summary = format!(
            "{verb} {} with conflicts in {}{}",
            stash.message,
            plural(applied.conflicts.len(), "file", "files"),
            if pop { " (the stash was kept)" } else { "" }
        );
    }
    let index_lost = flattened || (restore_index && !applied.index_restored);
    if index_lost {
        summary.push_str(
            "; its staged changes could not be restored and are unstaged now (Undo puts \
             everything back)",
        );
    }
    if !not_backed_up.is_empty() {
        summary.push_str(&format!(
            "; Undo will not restore {}",
            not_backed_up.join(", ")
        ));
    }
    let level = if applied.conflicts.is_empty() && !index_lost && not_backed_up.is_empty() {
        NoticeLevel::Success
    } else {
        NoticeLevel::Warning
    };
    let mut out_data = data(&applied);
    if let Value::Object(map) = &mut out_data {
        map.insert("restore_index".into(), json!(restore_index && !flattened));
        map.insert("not_backed_up".into(), json!(not_backed_up));
    }
    let mut touched = paths.clone();
    touched.extend(not_backed_up.iter().cloned());
    let outcome = Outcome::new(summary)
        .with_level(level)
        .with_data(out_data)
        .with_paths(
            touched
                .iter()
                .map(|p| repo.workdir().join(p))
                .collect::<Vec<PathBuf>>(),
        )
        .with_ui(UiRequest::Refresh);
    Ok(with_undo(outcome, undo))
}

fn allow_prop() -> Value {
    bool_prop("Proceed even if the backup that makes this undoable cannot be taken (only after the user confirmed)")
}

fn restore_index_prop() -> Value {
    bool_prop("Also restore the stash's staged changes into the index (git stash --index). true: refuse (nothing changed, stash kept) when git cannot; false: git's default, staged changes come back unstaged. Default: true only for the stash Lector's \"stash and switch\" created (falling back to git's default, with a warning, when the index cannot be restored)")
}

fn stash_apply() -> FnCommand {
    let [ix, oid] = stash_ref_props();
    FnCommand::new(
        CommandSpec::write(
            "git.stash_apply",
            "Apply Stash",
            "Git",
            object(
                &[
                    ix,
                    oid,
                    ("allow_without_backup", allow_prop()),
                    ("restore_index", restore_index_prop()),
                ],
                &[],
            ),
        )
        .undoable()
        .with_ui_flow("git::ApplyStash"),
        |cx, args| stash_apply_impl(cx, args, false),
    )
    .with_available(needs_repo)
}

fn stash_pop() -> FnCommand {
    let [ix, oid] = stash_ref_props();
    FnCommand::new(
        CommandSpec::write(
            "git.stash_pop",
            "Pop Stash",
            "Git",
            object(
                &[
                    ix,
                    oid,
                    ("allow_without_backup", allow_prop()),
                    ("restore_index", restore_index_prop()),
                ],
                &[],
            ),
        )
        .undoable()
        .with_ui_flow("git::PopStash"),
        |cx, args| stash_apply_impl(cx, args, true),
    )
    .with_available(needs_repo)
}

fn stash_drop() -> FnCommand {
    let [ix, oid] = stash_ref_props();
    FnCommand::new(
        CommandSpec::write(
            "git.stash_drop",
            "Drop Stash",
            "Git",
            object(&[ix, oid], &[]),
        )
        .destructive()
        .undoable()
        .with_ui_flow("git::DropStash"),
        |cx, args| {
            let repo = cx.require_repo()?;
            let index = stash_index(repo, args)?;
            let dropped = repo.stash_drop(index)?;
            Ok(Outcome::new(format!("Dropped {}", dropped.message))
                .with_data(json!({ "stash": data(&dropped) }))
                .with_undo(invoke(
                    "git.stash_store",
                    json!({ "oid": dropped.oid.to_string(), "message": dropped.message }),
                ))
                .with_paths(vec![repo.git_dir().to_path_buf()])
                .with_ui(UiRequest::Refresh))
        },
    )
    .with_available(needs_repo)
}

fn stash_store() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.stash_store",
            "Restore Dropped Stash",
            "Git",
            object(
                &[
                    ("oid", prop("string", "The stash commit")),
                    ("message", prop("string", "Its message")),
                ],
                &["oid"],
            ),
        )
        .undoable(),
        |cx, args| {
            let repo = cx.require_repo()?;
            let oid = oid_arg(args, "oid")?.ok_or_else(|| anyhow!("`oid` is required"))?;
            let message = str_arg(args, "message").unwrap_or("restored stash");
            repo.stash_store(oid, message)?;
            Ok(Outcome::new(format!("Restored stash {message}"))
                .with_data(json!({ "oid": oid.to_string() }))
                .with_undo(invoke("git.stash_drop", json!({ "oid": oid.to_string() })))
                .with_paths(vec![repo.git_dir().to_path_buf()])
                .with_ui(UiRequest::Refresh))
        },
    )
    .with_available(needs_repo)
}
