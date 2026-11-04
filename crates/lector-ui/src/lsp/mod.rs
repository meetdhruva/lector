//! Language-server editing features in the UI (v0.3): diagnostics
//! (squiggles, gutter icons, hover, status bar counts, Problems list,
//! F8 navigation), formatting (+ format on save), code actions (menu and
//! lightbulb), rename, and multi-file workspace edits.
//!
//! Everything edits *buffers* through [`DocumentModel::apply_edits`] (one undo
//! step per file); nothing here writes files — the user's save does (write
//! policy). Hooked in by [`init`]:
//! * [`LanguageFeatureProvider`] reports Format / Rename / Code Actions
//!   availability from the server capabilities;
//! * a before-save hook formats when `editor.format_on_save` is on;
//! * every [`Workspace`] gets the action handlers and its project's
//!   [`LspManager`] hooks (diagnostics changes, `workspace/applyEdit`).

pub mod code_actions;
pub mod diagnostics;
pub mod edits;
pub mod format;
pub mod problems;
pub mod rename;
#[cfg(test)]
mod ui_tests;

use std::ops::Range;
use std::path::Path;

use futures::StreamExt;
use gpui::{App, Context, Entity, Window};
use lector_lsp::{LspManager, PointUtf16};
use lector_text::Rope;

use crate::code_view::{LanguageFeatureProvider, LanguageFeatures};
use crate::editor::{document_for_path, DocumentModel, FileView};
use crate::views::Workspace;

pub use diagnostics::{DiagnosticEntry, DiagnosticsStore};
pub use edits::apply_workspace_edit;

/// Installs the LSP editing integration (called by [`crate::init`]).
pub fn init(cx: &mut App) {
    LanguageFeatureProvider::set(cx, features_for_path);
    diagnostics::init(cx);
    format::init(cx);
    cx.observe_new(|ws: &mut Workspace, window, cx| {
        if let Some(window) = window {
            wire_workspace(ws, window, cx);
        }
    })
    .detach();
}

/// Format / rename / code action availability for `path` from the running
/// server's capabilities.
pub fn features_for_path(path: &Path, cx: &App) -> LanguageFeatures {
    let Some(caps) = lsp_for_path(path, cx).and_then(|lsp| lsp.capabilities(path)) else {
        return LanguageFeatures::default();
    };
    LanguageFeatures {
        format: caps.formatting || caps.range_formatting,
        rename: caps.rename,
        code_actions: caps.code_actions,
    }
}

/// The language server manager of the project `path` is open in.
pub fn lsp_for_path(path: &Path, cx: &App) -> Option<LspManager> {
    document_for_path(path, cx).and_then(|m| m.read(cx).project().lsp().cloned())
}

fn wire_workspace(ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    ws.register_action(format::format_action)
        .register_action(code_actions::code_actions_action)
        .register_action(rename::rename_action)
        .register_action(diagnostics::next_diagnostic)
        .register_action(diagnostics::prev_diagnostic)
        .register_action(problems::show_problems);
    // Repaint the status bar counts when diagnostics change.
    let store = DiagnosticsStore::global(cx);
    cx.observe(&store, |_, _, cx| cx.notify()).detach();
    let Some(lsp) = ws.project().lsp().cloned() else {
        return;
    };
    // Diagnostics: LSP reader thread -> channel -> store (main thread).
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<std::path::PathBuf>();
    lsp.on_diagnostics(move |path| {
        let _ = tx.unbounded_send(path.to_path_buf());
    });
    let lsp_for_diags = lsp.clone();
    cx.spawn(async move |_, cx| {
        while let Some(first) = rx.next().await {
            let mut paths = vec![first];
            while let Ok(Some(p)) = rx.try_recv().map(Some) {
                if !paths.contains(&p) {
                    paths.push(p);
                }
            }
            let ok = cx.update(|cx| {
                let store = DiagnosticsStore::global(cx);
                for path in paths {
                    let published = lsp_for_diags.diagnostics(&path);
                    store.update(cx, |s, cx| s.published(&path, published, cx));
                }
            });
            if ok.is_err() {
                break;
            }
        }
    })
    .detach();
    // Server-initiated edits (`workspace/applyEdit`, e.g. from commands).
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<lector_lsp::ApplyEditRequest>();
    lsp.set_apply_edit_handler(move |req| {
        let _ = tx.unbounded_send(req);
    });
    cx.spawn_in(window, async move |this, cx| {
        while let Some(req) = rx.next().await {
            let task = this.update_in(cx, |ws, window, cx| {
                apply_workspace_edit(ws, req.edit.clone(), window, cx)
            });
            match task {
                Ok(task) => match task.await {
                    Ok(_) => req.respond(true, None),
                    Err(reason) => {
                        let _ = this.update(cx, |ws, cx| {
                            ws.show_toast(format!("Edit not applied: {reason}"), cx)
                        });
                        req.respond(false, Some(reason));
                    }
                },
                Err(_) => break,
            }
        }
    })
    .detach();
}

// Coordinates.

/// Byte offset of a UTF-16 LSP position in `text`, clamped to the text
/// (rows past the end → end of text; columns past the line end → line end).
pub fn offset_for_point(text: &Rope, p: PointUtf16) -> usize {
    if p.row >= text.line_count() {
        return text.len();
    }
    let range = text.line_range(p.row);
    let line = text.text_for_range(range.clone());
    let mut units = 0u32;
    for (i, c) in line.char_indices() {
        if units >= p.column {
            return range.start + i;
        }
        units += c.len_utf16() as u32;
    }
    range.end
}

/// UTF-16 LSP position of byte `offset` in `text`.
pub fn point_for_offset(text: &Rope, offset: usize) -> PointUtf16 {
    text.offset_to_point_utf16(offset.min(text.len()))
}

/// Byte range of a UTF-16 LSP range in `text`.
pub fn range_for_lsp(text: &Rope, range: &Range<PointUtf16>) -> Range<usize> {
    let start = offset_for_point(text, range.start);
    let end = offset_for_point(text, range.end).max(start);
    start..end
}

/// LSP edits (UTF-16, against `text`) as buffer edits (bytes).
pub fn buffer_edits(text: &Rope, edits: &[lector_lsp::TextEdit]) -> Vec<lector_buffer::TextEdit> {
    edits
        .iter()
        .map(|e| lector_buffer::TextEdit {
            range: range_for_lsp(text, &e.range),
            new_text: e.new_text.clone(),
        })
        .collect()
}

/// Sends the model's current buffer text to its language server right away
/// (tagged with the text version) so a request that follows sees it.
/// Returns the buffer text that was synced.
pub fn sync_now(model: &Entity<DocumentModel>, cx: &App) -> Option<Rope> {
    let m = model.read(cx);
    let lsp = m.project().lsp()?;
    let doc = m.live_document()?;
    let lang = doc.language()?;
    let rope = m.buffer()?.read(cx).text().clone();
    lsp.open_document_tagged(m.path(), &lang.name, &rope.to_string(), rope.version());
    Some(rope)
}

/// The active editable file tab and its model, if any.
pub(crate) fn active_file(
    ws: &Workspace,
    cx: &App,
) -> Option<(Entity<FileView>, Entity<DocumentModel>)> {
    let view = ws.active_file_view(cx)?;
    let model = view.read(cx).model().clone();
    Some((view, model))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_points_to_offsets() {
        let text = Rope::from_str("aé😀b\r\nxy\n");
        // é = 1 unit / 2 bytes, 😀 = 2 units / 4 bytes.
        assert_eq!(offset_for_point(&text, PointUtf16 { row: 0, column: 0 }), 0);
        assert_eq!(offset_for_point(&text, PointUtf16 { row: 0, column: 2 }), 3);
        assert_eq!(offset_for_point(&text, PointUtf16 { row: 0, column: 4 }), 7);
        // Past the line end clamps before the CRLF.
        assert_eq!(
            offset_for_point(&text, PointUtf16 { row: 0, column: 99 }),
            8
        );
        assert_eq!(
            offset_for_point(&text, PointUtf16 { row: 1, column: 1 }),
            11
        );
        assert_eq!(
            offset_for_point(&text, PointUtf16 { row: 9, column: 0 }),
            text.len()
        );
        assert_eq!(point_for_offset(&text, 7), PointUtf16 { row: 0, column: 4 });
        let edits = buffer_edits(
            &text,
            &[lector_lsp::TextEdit {
                range: PointUtf16 { row: 1, column: 0 }..PointUtf16 { row: 1, column: 2 },
                new_text: "z".into(),
            }],
        );
        assert_eq!(edits[0].range, 10..12);
    }

    /// Servers send stale or out-of-range positions (past EOF, past the line end,
    /// inside a surrogate pair, CRLF, empty text, no final newline): every
    /// conversion must clamp to a char boundary and never panic.
    #[test]
    fn out_of_range_lsp_positions_clamp_to_char_boundaries() {
        for src in [
            "",
            "x",
            "a😀b",
            "é\r\n😀\r\n",
            "\n\n",
            "fn a() {}\nlet s = \"👍🏽\";",
        ] {
            let text = Rope::from_str(src);
            let s = text.to_string();
            for row in 0..text.line_count() + 3 {
                for column in 0..12 {
                    let off = offset_for_point(&text, PointUtf16 { row, column });
                    assert!(
                        off <= s.len() && s.is_char_boundary(off),
                        "{src:?} {row}:{column}"
                    );
                    let range = range_for_lsp(
                        &text,
                        &(PointUtf16 { row, column }..PointUtf16 { row: 0, column: 0 }),
                    );
                    assert!(range.start <= range.end && s.is_char_boundary(range.end));
                }
            }
            for off in 0..s.len() + 3 {
                let p = point_for_offset(&text, off);
                assert!(p.row < text.line_count().max(1));
            }
        }
    }
}
