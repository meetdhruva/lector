//! Code actions: `editor::CodeActions` (ctrl/cmd-.) opens a menu at the
//! caret — quick fixes for the diagnostics there first (the preferred one
//! selected), then other actions, then disabled ones with their reason. The
//! lightbulb in the gutter (see `diagnostics`) marks the caret row when
//! actions exist; it is computed after the caret rests, never per keystroke.

use std::future::Future;
use std::path::PathBuf;

use gpui::{App, Context, Corner, Entity, Focusable, WeakEntity, Window};
use lector_lsp::{CodeAction, LspManager};

use crate::actions::editor::CodeActions;
use crate::components::context_menu::ContextMenu;
use crate::editor::FileView;
use crate::icons::IconName;
use crate::views::Workspace;

/// The code action request for the lightbulb of `view` (caret selection,
/// `automatic` trigger), with the caret row and text version it is for.
#[allow(clippy::type_complexity)]
pub(crate) fn lightbulb_request(
    view: &Entity<FileView>,
    cx: &App,
) -> Option<(
    impl Future<Output = lector_lsp::Result<Vec<CodeAction>>> + 'static,
    u32,
    u64,
)> {
    let v = view.read(cx);
    let code_view = v.code_view().read(cx);
    if code_view.is_read_only() {
        return None;
    }
    let path = v.path().to_path_buf();
    let lsp = v.project().lsp()?.clone();
    if !lsp.capabilities(&path)?.code_actions {
        return None;
    }
    let row = code_view.cursor().row;
    let rope = super::sync_now(v.model(), cx)?;
    let range = selection_range(view, &rope, cx);
    Some((lsp.code_actions(&path, range, true), row, rope.version()))
}

fn selection_range(
    view: &Entity<FileView>,
    rope: &lector_text::Rope,
    cx: &App,
) -> std::ops::Range<lector_lsp::PointUtf16> {
    let sel = view.read(cx).code_view().read(cx).selection();
    let (start, end) = sel.range();
    let off = |p: crate::code_view::Pos| crate::code_view::edit::pos_to_offset(rope, p);
    super::point_for_offset(rope, off(start))..super::point_for_offset(rope, off(end))
}

/// Orders actions for the menu: quick fixes (preferred first), other
/// enabled actions, then disabled ones.
pub fn sort_actions(actions: &mut [CodeAction]) {
    actions.sort_by_key(|a| {
        (
            a.disabled_reason.is_some(),
            !a.is_quick_fix(),
            !a.is_preferred,
        )
    });
}

pub(crate) fn code_actions_action(
    ws: &mut Workspace,
    _: &CodeActions,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some((view, model)) = super::active_file(ws, cx) else {
        cx.propagate();
        return;
    };
    let path = view.read(cx).path().to_path_buf();
    let Some(lsp) = ws.project().lsp().cloned() else {
        ws.show_toast("No language server for this file", cx);
        return;
    };
    if view.read(cx).code_view().read(cx).is_read_only() {
        ws.show_toast("This tab is read-only", cx);
        return;
    }
    if !lsp.capabilities(&path).is_some_and(|c| c.code_actions) {
        ws.show_toast("The language server offers no code actions", cx);
        return;
    }
    let Some(rope) = super::sync_now(&model, cx) else {
        return;
    };
    let range = selection_range(&view, &rope, cx);
    let request = lsp.code_actions(&path, range, false);
    let weak_view = view.downgrade();
    cx.spawn_in(window, async move |this, cx| {
        let result = request.await;
        let _ = this.update_in(cx, |ws, window, cx| match result {
            Err(err) => ws.show_toast(format!("Code actions failed: {err}"), cx),
            Ok(actions) if actions.is_empty() => ws.show_toast("No code actions here", cx),
            Ok(mut actions) => {
                let Some(view) = weak_view.upgrade() else {
                    return;
                };
                sort_actions(&mut actions);
                show_menu(
                    &view,
                    path,
                    lsp,
                    actions,
                    cx.entity().downgrade(),
                    window,
                    cx,
                );
            }
        });
    })
    .detach();
}

fn show_menu(
    view: &Entity<FileView>,
    path: PathBuf,
    lsp: LspManager,
    actions: Vec<CodeAction>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let code_view = view.read(cx).code_view().clone();
    let (position, line_height) = {
        let v = code_view.read(cx);
        (
            v.point_for_pos(v.cursor()).unwrap_or_default(),
            v.line_height().unwrap_or_default(),
        )
    };
    let focus = code_view.focus_handle(cx);
    let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
        menu = menu.context(focus);
        let mut section: Option<bool> = None;
        for action in actions {
            if let Some(reason) = &action.disabled_reason {
                if section != Some(false) {
                    menu = menu.separator();
                    section = Some(false);
                }
                menu = menu.disabled_entry(format!("{} — {reason}", action.title), None);
                continue;
            }
            let fix = action.is_quick_fix();
            if section.is_some_and(|s| s != fix) {
                menu = menu.separator();
            }
            section = Some(fix);
            let icon = (action.is_preferred || fix).then_some(IconName::Lightbulb);
            let label = action.title.clone();
            let (path, lsp, workspace) = (path.clone(), lsp.clone(), workspace.clone());
            let run = move |window: &mut Window, cx: &mut App| {
                run_action(&workspace, &path, &lsp, action.clone(), window, cx)
            };
            menu = match icon {
                Some(icon) => menu.icon_entry(label, icon, run),
                None => menu.entry(label, run),
            };
        }
        menu.select_first()
    });
    let anchor = gpui::point(position.x, position.y + line_height);
    code_view.update(cx, |v, cx| {
        v.show_overlay(menu, anchor, Corner::TopLeft, window, cx)
    });
}

/// Resolves `action` if needed, applies its edit, then runs its command.
pub fn run_action(
    workspace: &WeakEntity<Workspace>,
    path: &std::path::Path,
    lsp: &LspManager,
    action: CodeAction,
    window: &mut Window,
    cx: &mut App,
) {
    let resolve = lsp.resolve_code_action(path, action);
    let (path, lsp, workspace) = (path.to_path_buf(), lsp.clone(), workspace.clone());
    window
        .spawn(cx, async move |cx| {
            let action = match resolve.await {
                Ok(a) => a,
                Err(err) => {
                    let _ = workspace.update(cx, |ws, cx| {
                        ws.show_toast(format!("Code action failed: {err}"), cx)
                    });
                    return;
                }
            };
            if let Some(edit) = action.edit.clone() {
                let Ok(task) = workspace.update_in(cx, |ws, window, cx| {
                    super::apply_workspace_edit(ws, edit, window, cx)
                }) else {
                    return;
                };
                if let Err(reason) = task.await {
                    let _ = workspace.update(cx, |ws, cx| {
                        ws.show_toast(format!("Code action not applied: {reason}"), cx)
                    });
                    return;
                }
            }
            if let Some(command) = action.command {
                // The server computes the command against the new text.
                let _ = cx.update(|_, cx| {
                    if let Some(model) = crate::editor::document_for_path(&path, cx) {
                        super::sync_now(&model, cx);
                    }
                });
                if let Err(err) = lsp.execute_command(&path, &command).await {
                    let _ = workspace.update(cx, |ws, cx| {
                        ws.show_toast(format!("Command failed: {err}"), cx)
                    });
                }
            }
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(title: &str, kind: Option<&str>, preferred: bool, disabled: bool) -> CodeAction {
        CodeAction {
            title: title.into(),
            kind: kind.map(Into::into),
            is_preferred: preferred,
            disabled_reason: disabled.then(|| "no".to_string()),
            edit: None,
            command: None,
            diagnostics: vec![],
            raw: Default::default(),
        }
    }

    #[test]
    fn quick_fixes_first_preferred_on_top_disabled_last() {
        let mut a = vec![
            action("off", Some("quickfix"), true, true),
            action("refactor", Some("refactor"), false, false),
            action("fix", Some("quickfix"), false, false),
            action("best", Some("quickfix"), true, false),
        ];
        sort_actions(&mut a);
        let titles: Vec<_> = a.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, ["best", "fix", "refactor", "off"]);
    }
}
