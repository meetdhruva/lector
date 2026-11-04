//! Formatting: `editor::Format` formats the document (or the selection when
//! the server supports range formatting) as one undo step; with
//! `editor.format_on_save` a before-save hook formats first, giving up
//! after [`FORMAT_ON_SAVE_TIMEOUT`] (the file is then saved unformatted and
//! a notice is shown). Formatting only edits the buffer; saving writes.

use std::time::Duration;

use futures::FutureExt;
use gpui::{App, Context, Entity, Window};
use lector_lsp::{FormattingOptions, LspManager};

use crate::actions::editor::Format;
use crate::editor::{register_before_save, DocumentModel};
use crate::settings::AppSettings;
use crate::views::Workspace;

/// Format-on-save gives up after this long.
pub const FORMAT_ON_SAVE_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) fn init(cx: &mut App) {
    register_before_save(cx, |model, cx| {
        if !AppSettings::get(cx).editor.format_on_save {
            return None;
        }
        let (lsp, path) = {
            let m = model.read(cx);
            (m.project().lsp()?.clone(), m.path().to_path_buf())
        };
        if !lsp.capabilities(&path).is_some_and(|c| c.formatting) {
            return None;
        }
        let rope = super::sync_now(model, cx)?;
        let request = lsp.formatting(&path, options(model, cx));
        let timer = cx.background_executor().timer(FORMAT_ON_SAVE_TIMEOUT);
        Some(cx.spawn(async move |cx| {
            let request = Box::pin(request);
            match futures::future::select(request, timer).await {
                futures::future::Either::Left((Ok(edits), _)) => {
                    anyhow::Ok(super::buffer_edits(&rope, &edits))
                }
                futures::future::Either::Left((Err(err), _)) => {
                    notify(&path, format!("Format on save failed: {err}"), cx);
                    Ok(Vec::new())
                }
                futures::future::Either::Right(_) => {
                    notify(
                        &path,
                        "Formatting took too long; saved without formatting",
                        cx,
                    );
                    Ok(Vec::new())
                }
            }
        }))
    });
}

/// Shows `message` in the workspace windows of the project holding `path`.
fn notify(path: &std::path::Path, message: impl Into<gpui::SharedString>, cx: &mut gpui::AsyncApp) {
    let message = message.into();
    let path = path.to_path_buf();
    let _ = cx.update(|cx| {
        for window in cx.windows() {
            let Some(root) = window.downcast::<crate::views::WorkspaceRoot>() else {
                continue;
            };
            let _ = root.update(cx, |root, _, cx| {
                root.workspace.update(cx, |ws, cx| {
                    if path.starts_with(ws.root()) {
                        ws.show_toast(message.clone(), cx);
                    }
                })
            });
        }
    });
}

/// Formatting options from the buffer's indentation settings.
pub fn options(model: &Entity<DocumentModel>, cx: &App) -> FormattingOptions {
    let buffer_options = model
        .read(cx)
        .buffer()
        .map(|b| b.read(cx).buffer().options().clone())
        .unwrap_or_default();
    match buffer_options.indent {
        lector_buffer::IndentStyle::Tabs => FormattingOptions {
            tab_size: buffer_options.tab_size,
            insert_spaces: false,
        },
        lector_buffer::IndentStyle::Spaces(n) => FormattingOptions {
            tab_size: n,
            insert_spaces: true,
        },
    }
}

pub(crate) fn format_action(
    ws: &mut Workspace,
    _: &Format,
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
    let Some(caps) = lsp.capabilities(&path) else {
        ws.show_toast("The language server is not running", cx);
        return;
    };
    let Some(rope) = super::sync_now(&model, cx) else {
        return;
    };
    let selection = view.read(cx).code_view().read(cx).selection();
    let opts = options(&model, cx);
    let request = if !selection.is_empty() && caps.range_formatting {
        let (start, end) = selection.range();
        let off = |p| crate::code_view::edit::pos_to_offset(&rope, p);
        let range =
            super::point_for_offset(&rope, off(start))..super::point_for_offset(&rope, off(end));
        lsp.range_formatting(&path, range, opts).boxed()
    } else if caps.formatting {
        lsp.formatting(&path, opts).boxed()
    } else {
        ws.show_toast("The language server cannot format this file", cx);
        return;
    };
    format_with(model, rope, request, lsp, window, cx);
}

fn format_with(
    model: Entity<DocumentModel>,
    rope: lector_text::Rope,
    request: futures::future::BoxFuture<'static, lector_lsp::Result<Vec<lector_lsp::TextEdit>>>,
    _lsp: LspManager,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    cx.spawn_in(window, async move |this, cx| {
        let result = request.await;
        let _ = this.update(cx, |ws, cx| match result {
            Err(err) => ws.show_toast(format!("Format failed: {err}"), cx),
            Ok(edits) => {
                let current = model.read(cx).buffer().map(|b| b.read(cx).text().version());
                if current != Some(rope.version()) {
                    ws.show_toast("The file changed while formatting; try again", cx);
                    return;
                }
                let edits = super::buffer_edits(&rope, &edits);
                model.update(cx, |m, cx| m.apply_edits(edits, cx));
            }
        });
    })
    .detach();
}
