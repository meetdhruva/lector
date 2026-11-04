//! Applying a [`WorkspaceEdit`] (rename, code actions, server
//! `workspace/applyEdit`) to buffers — never to disk:
//! * files with an open document: [`DocumentModel::apply_edits`] (one undo
//!   step per file, carets of every view mapped);
//! * other files: opened as background tabs, edited once loaded, left
//!   *unsaved* (dirty) for the user to review and save (Save All saves them).
//!
//! The whole edit is rejected before anything changes when an open file's
//! text is not the one the server computed the edit for (versioned edits
//! must match the synced version; the synced text must be the buffer text).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{App, Context, Entity, Task, Window};
use lector_lsp::{TextEdit, WorkspaceEdit};

use crate::editor::{document_for_path, DocumentModel};
use crate::views::Workspace;

/// How long to wait for a background-opened file to load.
const LOAD_TIMEOUT: Duration = Duration::from_secs(15);

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// Checks that `model`'s buffer is the text the edit was computed for.
fn check_open_document(
    model: &Entity<DocumentModel>,
    edit: &WorkspaceEdit,
    path: &Path,
    cx: &App,
) -> Result<(), String> {
    let m = model.read(cx);
    if !m.is_loaded() {
        return Ok(()); // checked against the loaded text later
    }
    let Some(buffer) = m.buffer() else {
        return Err(format!("{} cannot be edited", file_name(path)));
    };
    let version = buffer.read(cx).text().version();
    let state = m.project().lsp().and_then(|l| l.document_state(path));
    let stale = || format!("{} changed since the edit was computed", file_name(path));
    if let Some(expected) = edit.version_for(path) {
        if state.map(|s| s.version) != Some(expected) {
            return Err(stale());
        }
    }
    if let Some(tag) = state.and_then(|s| s.tag) {
        if tag != version {
            return Err(stale());
        }
    }
    Ok(())
}

/// Applies LSP `edits` to a loaded model's buffer (one undo step).
fn apply_to_model(
    model: &Entity<DocumentModel>,
    edits: &[TextEdit],
    cx: &mut App,
) -> Result<bool, String> {
    let Some(text) = model.read(cx).buffer().map(|b| b.read(cx).text().clone()) else {
        return Err(format!(
            "{} cannot be edited",
            file_name(model.read(cx).path())
        ));
    };
    let edits = super::buffer_edits(&text, edits);
    Ok(model.update(cx, |m, cx| m.apply_edits(edits, cx)))
}

/// Applies `edit` (see the module docs). Resolves to the number of files
/// changed, or the reason nothing was applied.
pub fn apply_workspace_edit(
    ws: &mut Workspace,
    edit: WorkspaceEdit,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<usize, String>> {
    let changes: Vec<(PathBuf, Vec<TextEdit>)> = edit
        .changes
        .iter()
        .filter(|(_, e)| !e.is_empty())
        .cloned()
        .collect();
    // Validate everything first: all or nothing.
    for (path, _) in &changes {
        if let Some(model) = document_for_path(path, cx) {
            if let Err(reason) = check_open_document(&model, &edit, path, cx) {
                return Task::ready(Err(reason));
            }
        } else if !path.is_file() {
            return Task::ready(Err(format!("{} does not exist", path.display())));
        }
    }
    let mut applied = 0usize;
    let mut pending: Vec<(Entity<DocumentModel>, Vec<TextEdit>)> = Vec::new();
    for (path, edits) in changes {
        match document_for_path(&path, cx) {
            Some(model) if model.read(cx).is_loaded() => match apply_to_model(&model, &edits, cx) {
                Ok(changed) => applied += usize::from(changed),
                Err(reason) => return Task::ready(Err(reason)),
            },
            Some(model) => pending.push((model, edits)),
            None => {
                let view = ws.open_path_in_background(path, window, cx);
                let model = view.read(cx).model().clone();
                pending.push((model, edits));
            }
        }
    }
    if pending.is_empty() {
        return Task::ready(Ok(applied));
    }
    cx.spawn(async move |_, cx| {
        let started = Instant::now();
        let mut errors = Vec::new();
        for (model, edits) in pending {
            loop {
                let loaded = cx.update(|cx| model.read(cx).is_loaded()).unwrap_or(true);
                if loaded || started.elapsed() > LOAD_TIMEOUT {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(20))
                    .await;
            }
            match cx.update(|cx| apply_to_model(&model, &edits, cx)) {
                Ok(Ok(changed)) => applied += usize::from(changed),
                Ok(Err(reason)) => errors.push(reason),
                Err(_) => errors.push("window closed".into()),
            }
        }
        match errors.into_iter().next() {
            Some(reason) if applied == 0 => Err(reason),
            _ => Ok(applied),
        }
    })
}
