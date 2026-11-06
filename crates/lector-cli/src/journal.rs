//! `lector activity` (the activity journal) and `lector undo`.

use std::path::{Path, PathBuf};

use anyhow::anyhow;
use lector_app::journal::{ActivityEntry, Journal, UndoError, UndoMode};
use serde_json::{json, Value};

use crate::args::{ActivityArgs, UndoArgs};
use crate::out::{print_json, rfc3339, truncate, Io};
use crate::session::{context, open_core, open_project, resolve_dir};

/// One journal entry as JSON (`lector activity --json`, `lector undo --json`).
pub fn entry_json(e: &ActivityEntry) -> Value {
    let mut v = json!({
        "id": e.id,
        "time": rfc3339(e.time),
        "command": e.command,
        "source": serde_json::to_value(e.source).unwrap_or(Value::Null),
        "summary": e.summary,
        "repo": e.repo.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "paths": e.paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        "args": e.args,
        "undoable": e.undo.is_some() && !e.undone,
        "undone": e.undone,
    });
    if let Some(id) = e.undo_of {
        v["undo_of"] = json!(id);
    }
    if let Some(id) = e.undone_by {
        v["undone_by"] = json!(id);
    }
    v
}

fn source_name(e: &ActivityEntry) -> String {
    serde_json::to_value(e.source)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn same_repo(entry: &Path, repo: &Path) -> bool {
    let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(entry) == canon(repo)
}

/// Runs `lector activity`.
pub fn run_activity(args: &ActivityArgs, io: &mut Io) -> anyhow::Result<()> {
    let core = open_core();
    let journal =
        Journal::of(&core).ok_or_else(|| anyhow!("the activity journal is not available"))?;
    // Default: the repository of the current directory, when there is one.
    let repo: Option<PathBuf> = if args.all {
        None
    } else {
        let dir = resolve_dir(args.directory.as_deref())?;
        open_project(&dir)
            .ok()
            .and_then(|p| p.repo().map(|r| r.workdir().to_path_buf()))
    };
    let mut entries = Vec::new();
    let mut before = None;
    // Page through the journal until enough entries of the repository are found.
    while entries.len() < args.limit {
        let page = journal.list_before(200, before);
        let Some(last) = page.last() else { break };
        before = Some(last.id);
        let page_len = page.len();
        entries.extend(page.into_iter().filter(|e| match (&repo, &e.repo) {
            (Some(r), Some(er)) => same_repo(er, r),
            (Some(_), None) => false,
            (None, _) => true,
        }));
        if page_len < 200 {
            break;
        }
    }
    entries.truncate(args.limit);

    if args.json {
        let list: Vec<Value> = entries.iter().map(entry_json).collect();
        return Ok(print_json(io.out, json!({ "entries": list }))?);
    }
    let s = io.style;
    if entries.is_empty() {
        writeln!(io.out, "{}", s.dim("No recorded activity."))?;
        return Ok(());
    }
    for e in &entries {
        let src = source_name(e);
        let mut line = format!(
            "{} {} {} {} {}",
            s.bold(&format!("#{:<4}", e.id)),
            s.dim(&rfc3339(e.time).replace('T', " ").replace('Z', "")),
            s.cyan(&format!("{:<20}", truncate(&e.command, 20))),
            s.dim(&format!("[{src}]")),
            e.summary,
        );
        if e.undone {
            line.push_str(&format!(" {}", s.dim("(undone)")));
        } else if e.undo.is_some() {
            line.push_str(&format!(" {}", s.dim("(undoable)")));
        }
        writeln!(io.out, "{line}")?;
    }
    Ok(())
}

/// Runs `lector undo`.
pub fn run_undo(args: &UndoArgs, io: &mut Io) -> anyhow::Result<()> {
    let core = open_core();
    let journal =
        Journal::of(&core).ok_or_else(|| anyhow!("the activity journal is not available"))?;
    let entry = journal
        .get(args.id)
        .ok_or_else(|| anyhow!("no activity entry #{} (see `lector activity`)", args.id))?;
    // Inverse commands run against the entry's repository, wherever we are.
    let project = match &entry.repo {
        Some(repo) => Some(open_project(repo)?),
        None => open_project(&resolve_dir(None)?).ok(),
    };
    let cx = context(&core, project);
    let mode = if args.force {
        UndoMode::Force
    } else {
        UndoMode::Check
    };
    let undo_entry = journal.undo(args.id, &cx, mode).map_err(|e| match e {
        UndoError::Changed { .. } => {
            anyhow!("{e}; use --force to restore anyway (the current state is backed up first)")
        }
        other => anyhow!("{other}"),
    })?;
    if args.json {
        return Ok(print_json(
            io.out,
            json!({ "undone": args.id, "entry": entry_json(&undo_entry) }),
        )?);
    }
    writeln!(io.out, "Undid #{}: {}", args.id, entry.summary)?;
    if undo_entry.undo.is_some() {
        writeln!(
            io.out,
            "{}",
            io.style.dim(&format!(
                "recorded as #{} (undo that to redo)",
                undo_entry.id
            ))
        )?;
    }
    Ok(())
}
