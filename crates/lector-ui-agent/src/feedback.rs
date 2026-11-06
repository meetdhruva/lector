//! "Send feedback to agent": the selection of the active code view (a file tab, a diff or a
//! review tab) plus the open review comments of the active changeset, as prompt context.

use gpui::{App, SharedString};
use lector_agent::PromptContext;
use lector_ui::views::Workspace;

/// A context chip of the prompt box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chip {
    /// Short label.
    pub label: SharedString,
    /// What is sent.
    pub context: PromptContext,
}

/// The selection (and review comments) the user is looking at, as feedback context.
pub fn selection_feedback(ws: &Workspace, cx: &App) -> Option<Chip> {
    let ctx = lector_ui::commands_bridge::command_context(ws, cx);
    let root = ws.root().to_path_buf();
    let place = ws
        .active_path(cx)
        .map(|p| {
            p.strip_prefix(&root)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| p.to_string_lossy().into_owned())
        })
        .or_else(|| {
            ws.active_pane()
                .read(cx)
                .active_item()
                .map(|i| i.title(cx).to_string())
        })
        .unwrap_or_else(|| "selection".into());
    let mut text = String::new();
    let mut label = String::new();
    if let Some(view) = ws.active_code_view(cx) {
        let view = view.read(cx);
        let sel = view.selection();
        let (a, b) = sel.range();
        let selected = view.selected_text();
        if !selected.trim().is_empty() {
            let source = view.source();
            let first = source.line_number(a.row).unwrap_or(a.row) + 1;
            let last_row = if b.col == 0 && b.row > a.row {
                b.row - 1
            } else {
                b.row
            };
            let last = source.line_number(last_row).unwrap_or(last_row) + 1;
            let lines = if first == last {
                format!("{place}:{first}")
            } else {
                format!("{place}:{first}-{last}")
            };
            text.push_str(&format!("{lines}\n"));
            for line in selected.lines().take(80) {
                text.push_str(&format!("> {line}\n"));
            }
            label = lines;
        }
    }
    let key = ctx
        .focus
        .changeset
        .as_ref()
        .map(lector_workspace::review::changeset_key);
    let comments = lector_agent::commands::feedback_text(&ctx, key.as_deref(), "");
    if !comments.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&comments);
        let n = comments.matches("\n> ").count().max(1);
        if label.is_empty() {
            label = format!("{n} review comment{}", if n == 1 { "" } else { "s" });
        } else {
            label.push_str(&format!(" + {n} comment{}", if n == 1 { "" } else { "s" }));
        }
    }
    if text.is_empty() {
        return None;
    }
    Some(Chip {
        label: format!("Feedback: {label}").into(),
        context: PromptContext::Comments { text },
    })
}
