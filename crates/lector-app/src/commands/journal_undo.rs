//! Undo as registry commands (ADR 0012, item 1): `journal.undo` and `journal.undo_last`.
//!
//! The activity journal's undo engine (`crate::journal`, `Journal::undo` is crate-private) is
//! the implementation of these two commands, and nothing else runs it: the Activity log, the Undo
//! toast and "Undo last operation" (`lector-ui-git`), `lector undo` (`lector-cli`),
//! `agent.undo_turn` (`lector-agent`) and MCP clients all invoke them through
//! [`CommandRegistry::invoke`], so sources, path confinement and the journal are decided here
//! like for every other write.
//!
//! * `journal.undo { entry, mode }` undoes one entry. `mode` is `check` (default: refuse with
//!   [`UndoError::Changed`] when the files changed since) or `force` ("Restore anyway": the
//!   present state is backed up first). A backup that is gone ([`UndoError::BackupGone`]) is
//!   never overridden.
//! * `journal.undo_last { scope, mode }` undoes the newest undoable entry that is not itself an
//!   undo: of the context's project (its repository's work tree, or its folder without git;
//!   any entry when no folder is open) with `scope: "project"` (default), of any project with
//!   `scope: "all"`.
//!
//! **Undo declaration.** Both are `Undoable`: an undo is itself undoable. The journal records the
//! undo as an entry that carries the redo (a `journal.undo` entry whose [`UndoRecord`] restores
//! what the undo overwrote, from a backup taken right before it; or, when the undone record was a
//! single inverse command, that command's own entry with its own undo), and
//! [`Outcome::undo`] / [`Outcome::journal_id`] name that entry. Undoing an entry that is itself an
//! undo is a redo: it records a new undo entry (`undo_of` the undo entry) and makes the original
//! operation current again (not undone), so it can be undone once more.
//!
//! **The commands record themselves** ([`CommandSpec::records_itself`]): the engine writes the
//! undo entry (with `args: { "entry": id }`, the caller's source and `undo_of`), so
//! [`CommandRegistry::invoke`] does not add a second entry for the invocation.
//!
//! **Sources** are `All`: the GUI, the CLI, internal callers, MCP clients (which, like every
//! write tool, need `lector mcp --allow-writes`) and, by declaration, agents (the agent protocol
//! gives an agent no registry commands at all; see `lector_agent`). A sandboxed caller (MCP,
//! agent: [`CommandContext::is_sandboxed`]) may only undo entries of its own project: the entry's
//! repository (or folder) must be the context's, and every path the entry wrote or would restore
//! must pass the project's confinement ([`CommandContext::confine_write_path`]). Inverse commands
//! run with the caller's context, so their own sources and confinement apply too.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};
use serde_json::{json, Value};

use super::schema::object;
use super::{
    AppCore, Availability, CommandContext, CommandError, CommandRegistry, CommandSpec, FnCommand,
    Outcome, UiRequest,
};
use crate::journal::{ActivityEntry, Journal, UndoError, UndoMode, UndoRecord};
use crate::project::Project;

/// Id of the command that undoes one journal entry.
pub const UNDO: &str = "journal.undo";
/// Id of the command that undoes the newest undoable entry.
pub const UNDO_LAST: &str = "journal.undo_last";

/// Registers `journal.undo` and `journal.undo_last`.
pub fn register(r: &CommandRegistry) {
    r.register(
        FnCommand::new(
            CommandSpec::write(
                UNDO,
                "Undo Activity Entry",
                "Activity",
                object(
                    &[
                        (
                            "entry",
                            json!({"type": "integer", "minimum": 1, "description": "Activity journal entry id (`lector activity`, `_meta.lector.journalId`)"}),
                        ),
                        ("mode", mode_schema()),
                    ],
                    &["entry"],
                ),
            )
            .undoable()
            .records_itself(),
            |cx, args| {
                let id = args
                    .get("entry")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow!("`entry` (a journal entry id) is required"))?;
                let mode = mode_of(args)?;
                let journal = journal_of(&cx.core)?;
                let entry = journal.get(id).ok_or(UndoError::NotFound(id))?;
                check_scope(cx, &entry)?;
                let undo = journal.undo(id, cx, mode)?;
                Ok(outcome(id, undo))
            },
        )
        .with_available(has_journal),
    );
    r.register(
        FnCommand::new(
            CommandSpec::write(
                UNDO_LAST,
                "Undo Last Operation",
                "Activity",
                object(
                    &[
                        (
                            "scope",
                            json!({"type": "string", "enum": ["project", "all"], "description": "`project` (default): the newest undoable entry of this project; `all`: of any project (not for MCP clients and agents)"}),
                        ),
                        ("mode", mode_schema()),
                    ],
                    &[],
                ),
            )
            .undoable()
            .records_itself()
            // The GUI's `workspace::UndoLastOperation` runs it through the Activity log's flow
            // (the "Restore anyway" question, the Redo toast); one palette row.
            .with_ui_flow("workspace::UndoLastOperation"),
            |cx, args| {
                let mode = mode_of(args)?;
                let all = match args.get("scope").and_then(Value::as_str) {
                    None | Some("project") => false,
                    Some("all") => true,
                    Some(other) => bail!("unknown scope `{other}`: use `project` or `all`"),
                };
                if all && cx.is_sandboxed() {
                    bail!("MCP clients and agents may only undo entries of their own project");
                }
                let journal = journal_of(&cx.core)?;
                let dir = match (&cx.project, all) {
                    (_, true) => None,
                    (Some(project), false) => Some(activity_dir(project)),
                    (None, false) if cx.is_sandboxed() => {
                        cx.require_project()?;
                        None
                    }
                    (None, false) => None,
                };
                let entry = journal
                    .last_undoable_where(dir.as_deref(), |e| check_scope(cx, e).is_ok())
                    .ok_or_else(|| UndoError::NotUndoable("Nothing to undo.".into()))?;
                let undo = journal.undo(entry.id, cx, mode)?;
                Ok(outcome(entry.id, undo))
            },
        )
        .with_available(has_journal),
    );
}

fn mode_schema() -> Value {
    json!({"type": "string", "enum": ["check", "force"], "description": "`check` (default): refuse when the files changed since the operation; `force`: restore anyway (the present state is backed up first, so this is undoable too)"})
}

fn mode_of(args: &Value) -> anyhow::Result<UndoMode> {
    match args.get("mode").and_then(Value::as_str) {
        None | Some("check") => Ok(UndoMode::Check),
        Some("force") => Ok(UndoMode::Force),
        Some(other) => bail!("unknown mode `{other}`: use `check` or `force`"),
    }
}

fn mode_name(mode: UndoMode) -> &'static str {
    match mode {
        UndoMode::Check => "check",
        UndoMode::Force => "force",
    }
}

fn journal_of(core: &AppCore) -> anyhow::Result<Journal> {
    Journal::of(core).ok_or_else(|| anyhow!("the activity journal is not available"))
}

fn has_journal(cx: &CommandContext) -> Availability {
    match Journal::of(&cx.core) {
        Some(_) => Availability::Available,
        None => Availability::no("The activity journal is not available"),
    }
}

/// The directory a project's entries are recorded under: its repository's work tree, or its
/// folder when it is not a git repository (agent turns there, ADR 0011).
fn activity_dir(project: &Project) -> PathBuf {
    match project.repo() {
        Some(repo) => repo.workdir().to_path_buf(),
        None => project.root().to_path_buf(),
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let canonical = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canonical(a) == canonical(b)
}

/// Repository-relative paths an undo record restores (inverse commands confine their own
/// arguments).
fn record_paths<'a>(record: &'a UndoRecord, out: &mut Vec<&'a str>) {
    match record {
        UndoRecord::RestoreFiles { paths, .. } | UndoRecord::RestoreIndex { paths, .. } => {
            out.extend(paths.iter().map(String::as_str));
        }
        UndoRecord::Invoke { .. } => {}
        UndoRecord::Multi { records } => {
            for r in records {
                record_paths(r, out);
            }
        }
    }
}

/// For sandboxed callers (MCP clients, agents): `entry` must belong to the context's project.
/// Its directory must be the project's ([`activity_dir`]) and every path it wrote or would restore
/// must pass the project's confinement. Other callers act for the user and may undo any entry.
fn check_scope(cx: &CommandContext, entry: &ActivityEntry) -> anyhow::Result<()> {
    if !cx.is_sandboxed() {
        return Ok(());
    }
    let project = cx.require_project()?;
    let refuse = || {
        anyhow!(
            "activity entry #{} is not in this project ({}): MCP clients and agents may only undo entries of the project they were given",
            entry.id,
            project.root().display()
        )
    };
    let dir = match &entry.repo {
        Some(dir) if same_dir(dir, &activity_dir(project)) => dir,
        _ => return Err(refuse()),
    };
    let mut rels = Vec::new();
    if let Some(record) = &entry.undo {
        record_paths(record, &mut rels);
    }
    let restored = rels.into_iter().map(|rel| dir.join(rel));
    for path in entry.paths.iter().cloned().chain(restored) {
        cx.confine_write_path(path).map_err(|_| refuse())?;
    }
    Ok(())
}

/// The outcome of an undo: the entry that records it (`journal_id`), its summary, paths and redo.
fn outcome(undone: u64, entry: ActivityEntry) -> Outcome {
    let mut outcome = Outcome::new(entry.summary.clone())
        .with_data(json!({
            "undone": undone,
            "entry": entry.id,
            "redoable": entry.undo.is_some(),
        }))
        .with_paths(entry.paths.clone())
        .with_ui(UiRequest::Refresh);
    outcome.undo = entry.undo;
    outcome.journal_id = Some(entry.id);
    outcome
}

/// Arguments of `journal.undo` for `entry` in `mode`.
pub fn undo_args(entry: u64, mode: UndoMode) -> Value {
    json!({ "entry": entry, "mode": mode_name(mode) })
}

/// Arguments of `journal.undo_last` (scope: the context's project) in `mode`.
pub fn undo_last_args(mode: UndoMode) -> Value {
    json!({ "mode": mode_name(mode) })
}

/// The typed result of invoking `journal.undo` / `journal.undo_last`: the entry that records the
/// undo, or why nothing was undone ([`UndoError::Changed`] with the paths for "Restore anyway").
/// Refusals of the registry itself (sources, arguments, availability) become
/// [`UndoError::Failed`] with their message.
pub fn undo_result(
    core: &AppCore,
    result: Result<Outcome, CommandError>,
) -> Result<ActivityEntry, UndoError> {
    match result {
        Ok(outcome) => outcome
            .journal_id
            .and_then(|id| Journal::of(core)?.get(id))
            .ok_or_else(|| UndoError::Failed("the undo worked but its entry is missing".into())),
        Err(CommandError::Failed(err)) => Err(err
            .downcast::<UndoError>()
            .unwrap_or_else(|err| UndoError::Failed(err.to_string()))),
        Err(other) => Err(UndoError::Failed(other.to_string())),
    }
}

/// Invokes `journal.undo` for `entry` through the registry with `cx` (blocking: call it from a
/// background job) and returns the entry that records the undo.
pub fn invoke_undo(
    cx: &CommandContext,
    entry: u64,
    mode: UndoMode,
) -> Result<ActivityEntry, UndoError> {
    let result = cx.core.registry().invoke(UNDO, undo_args(entry, mode), cx);
    undo_result(&cx.core, result)
}

/// Invokes `journal.undo_last` (the context's project) through the registry with `cx` (blocking)
/// and returns the entry that records the undo.
pub fn invoke_undo_last(cx: &CommandContext, mode: UndoMode) -> Result<ActivityEntry, UndoError> {
    let result = cx
        .core
        .registry()
        .invoke(UNDO_LAST, undo_last_args(mode), cx);
    undo_result(&cx.core, result)
}

#[cfg(test)]
mod tests;
