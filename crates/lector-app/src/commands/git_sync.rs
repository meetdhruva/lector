//! Git sync commands (v0.7): fetch / pull / push, upstream state, branches and stash.
//!
//! All of them go through the git CLI (`lector_git::sync`, `lector_git` branch API) under
//! `lector-process`. Network commands run with the askpass environment the UI installs on
//! the [`SyncHost`] and can be cancelled through it; their progress is published to its
//! subscribers. Failures keep their type: downcast a command error to
//! [`lector_git::Error`] and match `Error::Sync(SyncError { kind, hint, paths,
//! hook_output, alternatives, .. })`.
//!
//! Undo (journal): pull → `git.restore_ref` (the branch goes back to where it was, `reset
//! --keep`); branch create / delete / rename / set upstream / switch → their inverse;
//! stash drop → `git.stash_store`; stash push → `git.stash_pop`; stash apply / pop →
//! the files and index entries they touched, backed up in the shadow repo first (plus the
//! stash entry for pop). Fetch and push are journaled but not undoable.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use anyhow::{anyhow, bail};
use lector_git::{
    CancelToken, FetchOptions, Oid, PullMode, PullStatus, PushOptions, Repo, StashPushOptions,
    SyncEvent, SyncOptions,
};
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};

use super::git_write::{backup_files, backup_index, files_undo, index_undo, needs_repo};
use super::schema::{object, prop};
use super::{CommandContext, CommandRegistry, CommandSpec, FnCommand, Outcome, UiRequest};
use crate::core::AppCore;
use crate::journal::UndoRecord;

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
    running: Mutex<BTreeMap<u64, (RunningSync, CancelToken)>>,
    listeners: Mutex<Vec<(u64, Listener)>>,
    next: AtomicU64,
}

impl std::fmt::Debug for SyncHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncHost")
            .field("running", &self.running())
            .finish_non_exhaustive()
    }
}

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
        core.extension_or_init(|| Arc::new(SyncHost::default()))
    }

    /// Sets the environment added to network git commands — the UI passes
    /// `server.env(&server.helper_program(&current_exe)?)` here once its askpass server runs.
    /// Never contains credentials themselves, only how to ask for them.
    pub fn set_askpass_env(&self, env: Vec<(OsString, OsString)>) {
        *self.env.write() = env;
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
                .with_env(self.env.read().clone())
                .with_cancel(token),
        }
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
        ),
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
        .undoable(),
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
            let outcome = Outcome::new(summary)
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
        ),
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
        .undoable(),
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
        .undoable(),
        |cx, args| {
            let repo = cx.require_repo()?;
            let name = str_arg(args, "name").unwrap_or_default();
            let host = SyncHost::of(&cx.core);
            let op = host.begin(repo, "git.switch_branch");
            let stash = if flag(args, "stash_changes") {
                repo.stash_push(&StashPushOptions {
                    message: Some(format!("lector: before switching to {name}")),
                    include_untracked: true,
                })?
            } else {
                None
            };
            let sw = match repo.switch_branch(name, &op.options) {
                Ok(sw) => sw,
                Err(err) => {
                    // Put the changes back where they were.
                    if let Some(st) = &stash {
                        if let Ok(Some(ix)) = repo.stash_index_of(st.oid) {
                            if let Err(e) = repo.stash_apply(ix, true) {
                                tracing::warn!("could not restore the stash after a failed switch: {e}");
                            }
                        }
                    }
                    return Err(err.into());
                }
            };
            let mut records = Vec::new();
            if let Some(st) = &stash {
                records.push(invoke("git.stash_pop", json!({ "oid": st.oid.to_string() })));
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
        .undoable(),
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
        .undoable(),
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
        .undoable(),
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
            repo.restore_ref(refname, expected, target)?;
            let branch = refname.strip_prefix("refs/heads/").unwrap_or(refname);
            if let (Some(up), Some(_)) = (str_arg(args, "upstream"), target) {
                if let Err(e) = repo.set_upstream(branch, Some(up)) {
                    tracing::warn!("restored {branch} without its upstream {up}: {e}");
                }
            }
            let summary = match target {
                Some(t) => format!("Restored {branch} to {}", t.short()),
                None => format!("Removed branch {branch}"),
            };
            Ok(Outcome::new(summary)
                .with_data(json!({ "ref": refname, "from": oid_json(expected), "to": oid_json(target) }))
                .with_undo(invoke(
                    "git.restore_ref",
                    json!({ "ref": refname, "expected": oid_json(target), "target": oid_json(expected) }),
                ))
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
        .undoable(),
        |cx, args| {
            let repo = cx.require_repo()?;
            let stash = repo.stash_push(&StashPushOptions {
                message: str_arg(args, "message").map(str::to_owned),
                include_untracked: flag(args, "include_untracked"),
            })?;
            let Some(stash) = stash else {
                return Ok(
                    Outcome::new("No local changes to stash").with_data(json!({ "stash": null }))
                );
            };
            Ok(Outcome::new(format!("Stashed: {}", stash.message))
                .with_data(json!({ "stash": data(&stash) }))
                .with_undo(invoke(
                    "git.stash_pop",
                    json!({ "oid": stash.oid.to_string() }),
                ))
                .with_paths(vec![repo.workdir().to_path_buf()])
                .with_ui(UiRequest::Refresh))
        },
    )
    .with_available(needs_repo)
}

fn stash_apply_impl(cx: &CommandContext, args: &Value, pop: bool) -> anyhow::Result<Outcome> {
    let repo = cx.require_repo()?;
    let index = stash_index(repo, args)?;
    let stash = repo.stash_at(index)?;
    let paths = repo.stash_paths(index)?;
    let note = format!(
        "before stash {} of {}",
        if pop { "pop" } else { "apply" },
        stash.oid.short()
    );
    // Applying merges into the working tree and index: back both up so Undo restores them.
    let files = backup_files(cx, repo, &paths, &note, flag(args, "allow_without_backup"))?;
    let index_backup = backup_index(cx, repo, &paths, &note);
    let applied = repo.stash_apply(index, pop)?;
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
    let outcome = Outcome::new(summary)
        .with_data(data(&applied))
        .with_paths(
            paths
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

fn stash_apply() -> FnCommand {
    let [ix, oid] = stash_ref_props();
    FnCommand::new(
        CommandSpec::write(
            "git.stash_apply",
            "Apply Stash",
            "Git",
            object(&[ix, oid, ("allow_without_backup", allow_prop())], &[]),
        )
        .undoable(),
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
            object(&[ix, oid, ("allow_without_backup", allow_prop())], &[]),
        )
        .undoable(),
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
        .undoable(),
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
