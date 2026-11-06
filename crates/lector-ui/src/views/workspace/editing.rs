//! Workspace-level editing: Save All, unsaved-changes prompts on window close
//! and quit, auto-save on focus change, before-save hook registration and the
//! status-bar edit indicators ("Modified", read-only lock).

use gpui::{div, prelude::*, px, App, Div, Entity, Task, Window};

use super::Workspace;
use crate::actions::{editor, workspace::SaveAll};
use crate::components::{Button, Tooltip};
use crate::editor::{DocumentModel, FileView};
use crate::icons::IconName;
use crate::settings::AppSettings;
use crate::theme::{ActiveTheme, ColorExt};

impl Workspace {
    /// Registers a hook run before every save in this app (format on save
    /// for the LSP integration). Thin wrapper over
    /// [`crate::editor::register_before_save`]; see [`crate::editor::BeforeSaveHook`].
    pub fn register_before_save(
        &mut self,
        hook: impl Fn(
                &Entity<DocumentModel>,
                &mut App,
            ) -> Option<Task<anyhow::Result<Vec<lector_buffer::TextEdit>>>>
            + 'static,
        cx: &mut Context<Self>,
    ) {
        crate::editor::register_before_save(cx, hook);
    }

    /// All file views of this window (every pane).
    pub fn all_file_views(&self, cx: &App) -> Vec<Entity<FileView>> {
        self.center
            .panes()
            .iter()
            .flat_map(|p| p.read(cx).file_views())
            .collect()
    }

    /// File views whose unsaved changes would be lost by closing this window
    /// (one per document).
    pub fn dirty_file_views(&self, cx: &App) -> Vec<Entity<FileView>> {
        crate::views::pane::dirty_unshared(&self.all_file_views(cx), cx)
    }

    /// Asks Save All / Don't Save / Cancel when closing would lose changes.
    /// Resolves to `true` when the window may close.
    pub fn confirm_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Task<bool> {
        let dirty = self.dirty_file_views(cx);
        if dirty.is_empty() {
            return Task::ready(true);
        }
        let task = crate::editor::confirm_discard_or_save(dirty, window, cx);
        cx.spawn(async move |_, _| task.await.unwrap_or(false))
    }

    /// Saves every dirty document of this window.
    pub fn save_all(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Task<bool> {
        let views = self.dirty_file_views_all(cx);
        let tasks: Vec<_> = views
            .into_iter()
            .map(|v| v.update(cx, |v, cx| v.save(window, cx)))
            .collect();
        cx.spawn(async move |_, _| {
            let mut ok = true;
            for task in tasks {
                ok &= task.await.unwrap_or(false);
            }
            ok
        })
    }

    /// One dirty view per dirty document (shared or not).
    fn dirty_file_views_all(&self, cx: &App) -> Vec<Entity<FileView>> {
        let mut out: Vec<Entity<FileView>> = Vec::new();
        for view in self.all_file_views(cx) {
            let v = view.read(cx);
            if v.is_dirty(cx) && !out.iter().any(|o| o.read(cx).model() == v.model()) {
                out.push(view);
            }
        }
        out
    }

    fn save_all_action(&mut self, _: &SaveAll, window: &mut Window, cx: &mut Context<Self>) {
        self.save_all(window, cx).detach();
    }

    /// Saves dirty documents when `editor.auto_save = "on_focus_change"`.
    pub(super) fn auto_save_on_focus_change(
        &mut self,
        views: Vec<Entity<FileView>>,
        cx: &mut Context<Self>,
    ) {
        if AppSettings::get(cx).editor.auto_save != lector_config::AutoSave::OnFocusChange {
            return;
        }
        for view in views {
            // The shared auto-save policy decides (dirty, no conflict, not deleted).
            let model = view.read(cx).model().clone();
            model.update(cx, |m, cx| m.focus_lost(cx));
        }
    }

    /// Window hooks for editing: unsaved-changes prompt on close, auto-save
    /// when the window loses focus. Called from `Workspace::new`.
    pub(super) fn start_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            let Some(ws) = weak.upgrade() else {
                return true;
            };
            if ws.read(cx).dirty_file_views(cx).is_empty() {
                return true;
            }
            let confirm = ws.update(cx, |ws, cx| ws.confirm_close(window, cx));
            window
                .spawn(cx, async move |cx| {
                    if confirm.await {
                        let _ = cx.update(|window, _| window.remove_window());
                    }
                })
                .detach();
            false
        });
        self._subscriptions
            .push(cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() {
                    let views = this.all_file_views(cx);
                    this.auto_save_on_focus_change(views, cx);
                }
            }));
    }

    /// The previously active file tab lost focus (tab switch).
    pub(super) fn active_file_changed(
        &mut self,
        previous: Option<Entity<FileView>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(prev) = previous {
            self.auto_save_on_focus_change(vec![prev], cx);
        }
    }

    /// Registers the editing action handlers on the workspace element.
    pub(super) fn register_editing_actions(&mut self, div: Div, cx: &mut Context<Self>) -> Div {
        div.on_action(cx.listener(Self::save_all_action))
    }

    /// Status-bar items: "Modified" and the read-only lock (click toggles).
    pub(super) fn render_edit_status(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let Some(view) = self.active_file_view(cx) else {
            return Vec::new();
        };
        let Some(meta) = view.read(cx).meta(cx) else {
            return Vec::new();
        };
        let ui = cx.theme().ui.clone();
        let fg = ui.status_bar_fg.hsla();
        let mut out: Vec<gpui::AnyElement> = Vec::new();
        if meta.dirty {
            out.push(
                Button::new("status-modified", "Modified")
                    .compact()
                    .color(cx.theme().git.modified.hsla())
                    .icon(IconName::Save)
                    .tooltip(Tooltip::for_action("Save", &editor::Save))
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(editor::Save), cx))
                    .into_any_element(),
            );
        }
        if meta.editable {
            let code_view = view.read(cx).code_view().clone();
            let toggle = move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                code_view.update(cx, |v, cx| {
                    let ro = v.is_read_only();
                    v.set_read_only(!ro, cx);
                });
            };
            if meta.read_only {
                out.push(
                    Button::new("status-read-only", "Read-Only")
                        .compact()
                        .color(fg)
                        .icon(IconName::Lock)
                        .tooltip(Tooltip::for_action(
                            "Read-only — click to allow editing",
                            &editor::ToggleReadOnly,
                        ))
                        .on_click(toggle)
                        .into_any_element(),
                );
            } else {
                out.push(
                    crate::components::IconButton::new("status-read-only", IconName::LockOpen)
                        .icon_size(crate::icons::IconSize::Small)
                        .color(ui.text_faint.hsla())
                        .tooltip(Tooltip::for_action(
                            "Editable — click to make read-only",
                            &editor::ToggleReadOnly,
                        ))
                        .on_click(toggle)
                        .into_any_element(),
                );
            }
        } else if meta.read_only {
            out.push(
                div()
                    .px(px(6.))
                    .child(
                        Button::new("status-read-only", "Read-Only")
                            .compact()
                            .color(ui.text_faint.hsla())
                            .icon(IconName::Lock)
                            .tooltip(Tooltip::text(
                                "This file cannot be edited (binary, very large or not losslessly decodable)",
                            )),
                    )
                    .into_any_element(),
            );
        }
        out
    }

    /// "N selections" suffix for the cursor item.
    pub(super) fn selection_count_suffix(&self, cx: &App) -> Option<String> {
        let view = self.active_code_view(cx)?;
        let n = view.read(cx).selection_count();
        (n > 1).then(|| format!(" ({n} selections)"))
    }

    /// Window title prefix: "● " when the active file has unsaved changes.
    pub fn title_dirty_marker(&self, cx: &App) -> &'static str {
        match self.active_file_view(cx) {
            Some(v) if v.read(cx).is_dirty(cx) => "● ",
            _ => "",
        }
    }
}

/// Quits after asking about unsaved changes in every window (one window at
/// a time). Replaces the plain `cx.quit()` handler of `workspace::Quit`.
pub fn quit_with_prompts(cx: &mut App) {
    let windows: Vec<gpui::WindowHandle<crate::views::WorkspaceRoot>> = cx
        .windows()
        .into_iter()
        .filter_map(|w| w.downcast::<crate::views::WorkspaceRoot>())
        .collect();
    cx.spawn(async move |cx| {
        for handle in windows {
            let confirm = handle.update(cx, |root, window, cx| {
                let ws = root.workspace.clone();
                if ws.read(cx).dirty_file_views(cx).is_empty() {
                    return None;
                }
                window.activate_window();
                Some(ws.update(cx, |ws, cx| ws.confirm_close(window, cx)))
            });
            if let Ok(Some(task)) = confirm {
                if !task.await {
                    return;
                }
            }
        }
        let _ = cx.update(|cx| {
            // Dirty buffers were saved or deliberately discarded.
            cx.quit()
        });
    })
    .detach();
}
