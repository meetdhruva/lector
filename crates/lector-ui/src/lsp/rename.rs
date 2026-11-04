//! Rename (F2): an inline box over the symbol (range and placeholder from
//! `textDocument/prepareRename`), Enter renames through the server's
//! workspace edit (all files, see `edits`), Escape cancels; a toast reports
//! "Renamed in N files".

use std::ops::Range;

use gpui::{
    div, prelude::*, px, App, Context, Corner, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, SharedString, Window,
};
use lector_lsp::PointUtf16;

use crate::actions::editor::Rename;
use crate::actions::picker::{Cancel, Confirm};
use crate::components::popover::surface;
use crate::components::text_input::TextInput;
use crate::components::{metrics, v_flex};
use crate::keymap::contexts;
use crate::theme::{ActiveTheme, ColorExt};
use crate::views::Workspace;

/// Emitted by [`RenameBox`] on Enter with the new name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameConfirmed(pub String);

/// The inline rename input.
pub struct RenameBox {
    input: Entity<TextInput>,
    original: String,
}

impl EventEmitter<DismissEvent> for RenameBox {}
impl EventEmitter<RenameConfirmed> for RenameBox {}

impl Focusable for RenameBox {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl RenameBox {
    /// A box pre-filled with `placeholder` (selected).
    pub fn new(placeholder: &str, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            let mut input = TextInput::new("New name", cx);
            input.set_text(placeholder.to_string(), cx);
            input.select_all_text(cx);
            input
        });
        RenameBox {
            input,
            original: placeholder.to_string(),
        }
    }

    /// The text typed so far.
    pub fn text(&self, cx: &App) -> SharedString {
        self.input.read(cx).text().clone()
    }

    fn confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        let name = self.text(cx).trim().to_string();
        if !name.is_empty() && name != self.original {
            cx.emit(RenameConfirmed(name));
        }
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Render for RenameBox {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        surface(cx)
            .key_context(contexts::PICKER)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            .border_color(ui.border_focused.hsla())
            .w(px(260.))
            .child(
                v_flex()
                    .px(px(6.))
                    .py(px(3.))
                    .gap(px(2.))
                    .child(div().text_size(metrics::TEXT).child(self.input.clone()))
                    .child(
                        div()
                            .text_size(metrics::TEXT_XS)
                            .text_color(ui.text_faint.hsla())
                            .child("Enter to rename, Escape to cancel"),
                    ),
            )
    }
}

pub(crate) fn rename_action(
    ws: &mut Workspace,
    _: &Rename,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some((view, model)) = super::active_file(ws, cx) else {
        cx.propagate();
        return;
    };
    let path = view.read(cx).path().to_path_buf();
    if view.read(cx).code_view().read(cx).is_read_only() {
        ws.show_toast("This tab is read-only", cx);
        return;
    }
    let Some(lsp) = ws.project().lsp().cloned() else {
        ws.show_toast("No language server for this file", cx);
        return;
    };
    if !lsp.capabilities(&path).is_some_and(|c| c.rename) {
        ws.show_toast("The language server cannot rename here", cx);
        return;
    }
    let Some(rope) = super::sync_now(&model, cx) else {
        return;
    };
    let caret = view.read(cx).code_view().read(cx).cursor();
    let caret_offset = crate::code_view::edit::pos_to_offset(&rope, caret);
    let pos = super::point_for_offset(&rope, caret_offset);
    let prepare = lsp.prepare_rename(&path, pos);
    let weak_view = view.downgrade();
    cx.spawn_in(window, async move |this, cx| {
        let prepared = prepare.await;
        let _ = this.update_in(cx, |ws, window, cx| {
            let Some(view) = weak_view.upgrade() else {
                return;
            };
            let prepared = match prepared {
                Err(err) => return ws.show_toast(format!("Cannot rename: {err}"), cx),
                Ok(None) => return ws.show_toast("Nothing to rename here", cx),
                Ok(Some(p)) => p,
            };
            let current = model.read(cx).buffer().map(|b| b.read(cx).text().clone());
            if current.as_ref().map(|t| t.version()) != Some(rope.version()) {
                return; // edited meanwhile
            }
            let range: Range<usize> = match &prepared.range {
                Some(r) => super::range_for_lsp(&rope, r),
                None => word_range(&rope, caret_offset),
            };
            if range.is_empty() {
                return ws.show_toast("Nothing to rename here", cx);
            }
            let placeholder = prepared
                .placeholder
                .unwrap_or_else(|| rope.text_for_range(range.clone()));
            open_box(ws, &view, &path, pos, range.start, &placeholder, window, cx);
        });
    })
    .detach();
}

/// The identifier around `offset` (letters, digits, `_`, `$`).
fn word_range(text: &lector_text::Rope, offset: usize) -> Range<usize> {
    let row = text.offset_to_point(offset).row;
    let line_range = text.line_range(row);
    let line = text.text_for_range(line_range.clone());
    let col = offset - line_range.start;
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let start = line[..col.min(line.len())]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(col, |(i, _)| i);
    let end = line[col.min(line.len())..]
        .char_indices()
        .find(|(_, c)| !is_word(*c))
        .map_or(line.len(), |(i, _)| col + i);
    line_range.start + start..line_range.start + end
}

#[allow(clippy::too_many_arguments)]
fn open_box(
    ws: &mut Workspace,
    view: &Entity<crate::editor::FileView>,
    path: &std::path::Path,
    pos: PointUtf16,
    start: usize,
    placeholder: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let code_view = view.read(cx).code_view().clone();
    let anchor = {
        let v = code_view.read(cx);
        let text = view
            .read(cx)
            .model()
            .read(cx)
            .buffer()
            .map(|b| b.read(cx).text().clone());
        let p = text
            .map(|t| crate::code_view::edit::offset_to_pos(&t, start))
            .unwrap_or(v.cursor());
        v.point_for_pos(p).unwrap_or_default()
    };
    let rename_box = cx.new(|cx| RenameBox::new(placeholder, cx));
    let lsp = ws.project().lsp().cloned();
    let path = path.to_path_buf();
    cx.subscribe_in(
        &rename_box,
        window,
        move |_, _, event: &RenameConfirmed, window, cx| {
            let Some(lsp) = lsp.clone() else { return };
            let new_name = event.0.clone();
            if let Some(model) = crate::editor::document_for_path(&path, cx) {
                super::sync_now(&model, cx);
            }
            let request = lsp.rename(&path, pos, &new_name);
            cx.spawn_in(window, async move |this, cx| {
                let result = request.await;
                let task = this.update_in(cx, |ws, window, cx| match result {
                    Err(err) => {
                        ws.show_toast(format!("Rename failed: {err}"), cx);
                        None
                    }
                    Ok(None) => {
                        ws.show_toast("Nothing to rename", cx);
                        None
                    }
                    Ok(Some(edit)) => Some(super::apply_workspace_edit(ws, edit, window, cx)),
                });
                let Ok(Some(task)) = task else { return };
                let outcome = task.await;
                let _ = this.update(cx, |ws, cx| match outcome {
                    Ok(n) => ws.show_toast(
                        if n == 1 {
                            "Renamed in 1 file".to_string()
                        } else {
                            format!("Renamed in {n} files")
                        },
                        cx,
                    ),
                    Err(reason) => ws.show_toast(format!("Rename not applied: {reason}"), cx),
                });
            })
            .detach();
        },
    )
    .detach();
    let anchor = gpui::point(anchor.x - px(7.), anchor.y - px(4.));
    code_view.update(cx, |v, cx| {
        v.show_overlay(rename_box, anchor, Corner::TopLeft, window, cx)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_range_around_caret() {
        let t = lector_text::Rope::from_str("let foo_bar = 1;\nx");
        assert_eq!(word_range(&t, 6), 4..11);
        assert_eq!(word_range(&t, 4), 4..11);
        assert_eq!(word_range(&t, 11), 4..11);
        assert_eq!(word_range(&t, 12), 12..12);
        assert_eq!(word_range(&t, 17), 17..18);
    }
}
