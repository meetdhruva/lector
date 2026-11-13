//! The Terminal panel below the editor area: a tab strip (new, close, rename, drag to
//! reorder, middle-click close, activity / bell / failed-command marks, cwd tooltips) and the
//! active tab's splits. Inactive tabs are not rendered, so they never snapshot or repaint.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{
    div, prelude::*, AnyElement, App, Axis, Context, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, MouseButton, PromptLevel, ScrollHandle, SharedString, Subscription, Task,
    WeakEntity, Window,
};
use lector_terminal::TermSize;
use lector_ui::actions::terminal as actions;
use lector_ui::components::context_menu::{ContextMenu, ContextMenuState};
use lector_ui::components::split::{resizable_split, SplitState};
use lector_ui::components::text_input::TextInput;
use lector_ui::components::{h_flex, metrics, v_flex, IconButton, Tooltip};
use lector_ui::icons::{Icon, IconName, IconSize};
use lector_ui::scale::px;
use lector_ui::theme::{ActiveTheme, ColorExt};
use lector_ui::views::Workspace;

use crate::config::{self, CwdContext, CwdRequest};
use crate::model::{PanelModel, SplitAxis};
use crate::session::SavedTab;
use crate::view::{terminal_settings, TerminalView, TerminalViewEvent};

/// Emitted when the set of terminals changes (session saving).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalPanelEvent {
    /// Tabs, splits, names or folders changed.
    Changed,
}

/// Payload of a tab being dragged in the strip.
#[derive(Clone)]
struct DraggedTab {
    ix: usize,
    title: SharedString,
}

struct DragPreview(SharedString);

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = &cx.theme().ui;
        div()
            .px(px(10.))
            .py(px(4.))
            .rounded(px(4.))
            .bg(ui.elevated.hsla())
            .border_1()
            .border_color(ui.border.hsla())
            .text_size(metrics::text_sm())
            .text_color(ui.text.hsla())
            .child(self.0.clone())
    }
}

/// The Terminal panel of one workspace window.
pub struct TerminalPanel {
    workspace: WeakEntity<Workspace>,
    pub(crate) model: PanelModel<Entity<TerminalView>>,
    splits: HashMap<u64, (usize, SplitAxis, Entity<SplitState>)>,
    focus_handle: FocusHandle,
    renaming: Option<(u64, Entity<TextInput>)>,
    menu: ContextMenuState,
    tab_scroll: ScrollHandle,
    subscriptions: HashMap<EntityId, Subscription>,
    save_task: Option<Task<()>>,
    window: gpui::AnyWindowHandle,
    /// Off in tests: terminals get no process.
    spawn_processes: bool,
}

impl EventEmitter<TerminalPanelEvent> for TerminalPanel {}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalPanel {
    /// An empty panel for `workspace`.
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        // F6 lands on the panel's own handle: hand the focus to the active terminal.
        cx.on_focus(&focus_handle, window, |panel, window, cx| {
            panel.focus_active(window, cx);
        })
        .detach();
        TerminalPanel {
            workspace,
            model: PanelModel::default(),
            splits: HashMap::new(),
            focus_handle,
            renaming: None,
            menu: ContextMenuState::default(),
            tab_scroll: ScrollHandle::new(),
            subscriptions: HashMap::new(),
            save_task: None,
            window: window.window_handle(),
            spawn_processes: true,
        }
    }

    /// Terminals opened from now on get no process (tests of the panel's model and
    /// commands).
    #[cfg(test)]
    pub(crate) fn without_processes(&mut self) {
        self.spawn_processes = false;
    }

    /// The workspace this panel belongs to.
    pub(crate) fn workspace_handle(&self) -> &WeakEntity<Workspace> {
        &self.workspace
    }

    /// The focused terminal of the active tab.
    pub fn active_terminal(&self) -> Option<Entity<TerminalView>> {
        self.model.active_terminal().cloned()
    }

    /// Every terminal of the panel.
    pub fn terminals(&self) -> Vec<Entity<TerminalView>> {
        self.model.all().cloned().collect()
    }

    /// Whether the panel holds `view`.
    pub fn contains(&self, view: &Entity<TerminalView>) -> bool {
        self.model.position(view).is_some()
    }

    fn is_visible(&self, cx: &App) -> bool {
        self.workspace
            .upgrade()
            .is_some_and(|ws| ws.read(cx).is_bottom_panel_visible())
    }

    fn set_visible(&self, visible: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ws) = self.workspace.upgrade() {
            ws.update(cx, |ws, cx| {
                ws.set_bottom_panel_visible(visible, window, cx)
            });
        }
    }

    /// Focuses the active terminal (or the panel itself when empty).
    pub fn focus_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active_terminal() {
            let handle = view.read(cx).focus_handle.clone();
            window.focus(&handle);
        }
    }

    /// Ctrl+`: hidden → show (and open a first terminal); shown but focus elsewhere → focus
    /// it; shown and focused → hide.
    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_visible(cx) {
            self.set_visible(true, window, cx);
            if self.model.is_empty() {
                self.new_terminal(CwdRequest::Default, None, None, window, cx);
            } else {
                self.focus_active(window, cx);
            }
            return;
        }
        if self.focus_handle.contains_focused(window, cx) {
            self.set_visible(false, window, cx);
        } else if self.model.is_empty() {
            self.new_terminal(CwdRequest::Default, None, None, window, cx);
        } else {
            self.focus_active(window, cx);
        }
    }

    /// Opens a terminal (a new tab, or a split of the active tab) and focuses it.
    pub fn new_terminal(
        &mut self,
        request: CwdRequest,
        profile: Option<&str>,
        split: Option<SplitAxis>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TerminalView>> {
        let view = self.build_view(request, profile, split, false, window, cx);
        self.adopt(view.clone(), split, cx);
        self.set_visible(true, window, cx);
        let handle = view.read(cx).focus_handle.clone();
        window.focus(&handle);
        Some(view)
    }

    /// Creates a terminal view for `request`. `lazy`: a placeholder whose shell starts when it
    /// is first shown ([`TerminalView::is_pending`]).
    fn build_view(
        &mut self,
        request: CwdRequest,
        profile: Option<&str>,
        split: Option<SplitAxis>,
        lazy: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalView> {
        let settings = terminal_settings(cx);
        let (root, active, has_folder) = match self.workspace.upgrade() {
            Some(ws) => {
                let ws = ws.read(cx);
                (ws.root().to_path_buf(), ws.active_path(cx), ws.has_folder())
            }
            None => (PathBuf::new(), None, false),
        };
        // Splits start where the focused terminal is.
        let request = match (&request, split) {
            (CwdRequest::Default, Some(_)) => self
                .active_terminal()
                .map(|v| CwdRequest::Explicit(v.read(cx).cwd().to_path_buf()))
                .unwrap_or(request),
            _ => request,
        };
        let home = dirs_home();
        let cwd = config::resolve_cwd(
            &request,
            CwdContext {
                project_root: has_folder.then_some(root.as_path()),
                active_file: active.as_deref(),
                home: home.as_deref(),
                setting: settings.new_terminal_cwd,
            },
            |p| p.is_dir(),
        );
        let profile = config::resolve_profile(&settings, profile);
        let size = self
            .active_terminal()
            .map(|v| v.read(cx).size)
            .unwrap_or(TermSize::new(80, 24));
        let options = config::spawn_options(&settings, profile, cwd, size);
        let id = crate::next_id(cx);
        let workspace = self.workspace.clone();
        let spawn = self.spawn_processes;
        cx.new(|cx| {
            if lazy {
                TerminalView::pending(id, options, workspace, window, cx)
            } else if spawn {
                TerminalView::new(id, options, workspace, window, cx)
            } else {
                let cwd = options.cwd.clone().unwrap_or_default();
                TerminalView::detached(
                    id,
                    cwd,
                    options.profile.name.clone(),
                    options.size,
                    workspace,
                    window,
                    cx,
                )
            }
        })
    }

    /// Starts the shells of restored tabs that are showing now (the active tab, panel shown).
    fn start_visible(&mut self, cx: &mut Context<Self>) {
        let spawn = self.spawn_processes;
        if let Some(tab) = self.model.active_tab() {
            for view in tab.panes.clone() {
                if view.read(cx).is_pending() {
                    view.update(cx, |v, cx| v.start_pending(spawn, cx));
                }
            }
        }
    }

    /// Takes `view` into the panel (new tab or split).
    pub(crate) fn adopt(
        &mut self,
        view: Entity<TerminalView>,
        split: Option<SplitAxis>,
        cx: &mut Context<Self>,
    ) {
        let sub = cx.subscribe(&view, |panel, view, event: &TerminalViewEvent, cx| {
            panel.on_view_event(view, event, cx)
        });
        self.subscriptions.insert(view.entity_id(), sub);
        crate::set_in_panel(view.read(cx).id, true, cx);
        match split {
            Some(axis) => self.model.split(view, axis),
            None => {
                self.model.add_tab(view);
            }
        }
        self.tab_scroll.scroll_to_item(self.model.active_index());
        self.changed(cx);
    }

    /// Lets go of `view` without killing it (moved to the editor area).
    pub(crate) fn release(&mut self, view: &Entity<TerminalView>, cx: &mut Context<Self>) -> bool {
        if !self.model.remove(view) {
            return false;
        }
        self.subscriptions.remove(&view.entity_id());
        crate::set_in_panel(view.read(cx).id, false, cx);
        self.changed(cx);
        true
    }

    fn on_view_event(
        &mut self,
        view: Entity<TerminalView>,
        event: &TerminalViewEvent,
        cx: &mut Context<Self>,
    ) {
        let active = self.active_terminal().as_ref() == Some(&view);
        match event {
            // A visible terminal repaints itself; only the strip of hidden ones changes.
            TerminalViewEvent::Activity | TerminalViewEvent::Bell => {
                if !active {
                    cx.notify();
                }
            }
            TerminalViewEvent::TitleChanged => self.changed(cx),
            TerminalViewEvent::Exited => {
                if terminal_settings(cx).close_on_exit {
                    self.close_view(&view, cx);
                } else {
                    cx.notify();
                }
            }
            TerminalViewEvent::CloseRequested => self.close_view(&view, cx),
            TerminalViewEvent::Focused => {
                if self.model.position(&view).is_some() {
                    self.model.focus(&view);
                    cx.notify();
                }
            }
        }
    }

    fn close_view(&mut self, view: &Entity<TerminalView>, cx: &mut Context<Self>) {
        view.update(cx, |v, _| v.kill());
        if self.release(view, cx) {
            self.splits
                .retain(|id, _| self.model.index_of(*id).is_some());
            if self.model.is_empty() {
                // Hide the panel; the workspace refocuses the editor.
                if let Some(ws) = self.workspace.upgrade() {
                    let handle = self.window;
                    cx.defer(move |cx| {
                        let _ = handle.update(cx, |_, window, cx| {
                            ws.update(cx, |ws, cx| ws.set_bottom_panel_visible(false, window, cx));
                        });
                    });
                }
            } else {
                let next = self.active_terminal();
                let window = self.window;
                cx.defer(move |cx| {
                    let Some(next) = next else { return };
                    let handle = next.read(cx).focus_handle.clone();
                    let _ = window.update(cx, |_, window, _| window.focus(&handle));
                });
            }
        }
    }

    /// Kill Terminal: asks first when a command is still running (known with shell
    /// integration), then closes the terminal and kills its processes.
    pub fn kill(
        &mut self,
        view: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !view.read(cx).is_command_running() {
            self.close_view(&view, cx);
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "A command is still running in this terminal.",
            Some("Closing the terminal stops it and every process it started."),
            &["Kill Terminal", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await.ok() == Some(0) {
                let _ = this.update(cx, |panel, cx| panel.close_view(&view, cx));
            }
        })
        .detach();
    }

    /// Next / previous tab.
    pub fn activate_relative(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        self.model.activate_relative(delta);
        self.tab_scroll.scroll_to_item(self.model.active_index());
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Next / previous split of the active tab.
    pub fn focus_split_relative(
        &mut self,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model.focus_split_relative(delta);
        self.focus_active(window, cx);
        cx.notify();
    }

    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.model.activate(ix);
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Starts renaming the active tab inline.
    pub fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.model.active_tab() else {
            return;
        };
        let id = tab.id;
        let current = tab.custom_title.clone().unwrap_or_else(|| {
            tab.focused_pane()
                .map(|v| v.read(cx).display_title())
                .unwrap_or_default()
        });
        let input = cx.new(|cx| {
            let mut i = TextInput::new("Terminal name", cx);
            i.set_text(current, cx);
            i.select_all_text(cx);
            i
        });
        let handle = input.focus_handle(cx);
        self.renaming = Some((id, input));
        if !self.is_visible(cx) {
            self.set_visible(true, window, cx);
        }
        window.focus(&handle);
        cx.notify();
    }

    fn finish_rename(&mut self, commit: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, input)) = self.renaming.take() else {
            return;
        };
        if commit {
            let text = input.read(cx).text().to_string();
            if let Some(ix) = self.model.index_of(id) {
                self.model.rename(ix, Some(text));
            }
            self.changed(cx);
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    /// "New Terminal with Profile": a menu of the configured profiles.
    pub fn choose_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let names = config::profile_names(&terminal_settings(cx));
        if !self.is_visible(cx) {
            self.set_visible(true, window, cx);
        }
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let mut menu = menu.header("New Terminal with Profile");
            let t = this.clone();
            menu = menu.icon_entry("Default Shell", IconName::Terminal, move |window, cx| {
                let _ = t.update(cx, |p, cx| {
                    p.new_terminal(CwdRequest::Default, None, None, window, cx)
                });
            });
            if names.is_empty() {
                menu = menu.disabled_entry("No profiles in settings ([[terminal.profiles]])", None);
            }
            for name in names {
                let t = this.clone();
                let label: SharedString = name.clone().into();
                menu = menu.entry(label, move |window, cx| {
                    let name = name.clone();
                    let _ = t.update(cx, |p, cx| {
                        p.new_terminal(CwdRequest::Default, Some(&name), None, window, cx)
                    });
                });
            }
            menu.select_first()
        });
        let position = window.mouse_position();
        self.menu
            .deploy(menu, position, window, cx, |this: &mut Self| &mut this.menu);
        cx.notify();
    }

    fn deploy_tab_menu(
        &mut self,
        ix: usize,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate(ix, window, cx);
        let Some(view) = self.active_terminal() else {
            return;
        };
        let focus = view.read(cx).focus_handle.clone();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            menu.context(focus)
                .action("Rename…", Box::new(actions::Rename))
                .action("Split Terminal", Box::new(actions::SplitTerminal))
                .action("Split Terminal Down", Box::new(actions::SplitTerminalDown))
                .action("Move to Editor Area", Box::new(actions::MoveToEditor))
                .separator()
                .action("Clear", Box::new(actions::Clear))
                .action("Kill Terminal", Box::new(actions::KillTerminal))
        });
        self.menu
            .deploy(menu, position, window, cx, |this: &mut Self| &mut this.menu);
        cx.notify();
    }

    // ------------------------------------------------------------------ session

    /// The tabs to save for session restore.
    pub fn saved_tabs(&self, cx: &App) -> Vec<SavedTab> {
        self.model
            .tabs()
            .iter()
            .map(|t| SavedTab {
                cwds: t
                    .panes
                    .iter()
                    .map(|v| v.read(cx).cwd().to_path_buf())
                    .collect(),
                title: t.custom_title.clone(),
                vertical: t.axis == SplitAxis::Vertical,
            })
            .collect()
    }

    /// Reopens saved tabs as placeholders (title and folder) without starting anything. A
    /// tab's shell starts, in its folder with fresh state (nothing is run in it), when the tab
    /// is first shown: opening a project never runs the user's shell start-up files, and the
    /// panel stays hidden until the user opens it.
    pub fn restore(&mut self, tabs: Vec<SavedTab>, window: &mut Window, cx: &mut Context<Self>) {
        for tab in tabs {
            let axis = if tab.vertical {
                SplitAxis::Vertical
            } else {
                SplitAxis::Horizontal
            };
            for (i, cwd) in tab.cwds.iter().enumerate() {
                let split = (i > 0).then_some(axis);
                let request = CwdRequest::Explicit(cwd.clone());
                let view = self.build_view(request, None, split, true, window, cx);
                self.adopt(view, split, cx);
            }
            let ix = self.model.active_index();
            self.model.rename(ix, tab.title.clone());
        }
        cx.notify();
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(TerminalPanelEvent::Changed);
        cx.notify();
    }

    pub(crate) fn schedule_save(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let tabs = self.saved_tabs(cx);
        self.save_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(1))
                .await;
            let path = crate::session::session_path();
            let write =
                cx.background_spawn(async move { crate::session::store(&path, &root, &tabs) });
            if let Err(err) = write.await {
                tracing::warn!("terminal session: {err}");
            }
        }));
    }

    // ------------------------------------------------------------------ render

    fn split_state(
        &mut self,
        tab_id: u64,
        n: usize,
        axis: SplitAxis,
        cx: &mut Context<Self>,
    ) -> Entity<SplitState> {
        if let Some((count, a, state)) = self.splits.get(&tab_id) {
            if *count == n && *a == axis {
                return state.clone();
            }
        }
        let gpui_axis = match axis {
            SplitAxis::Horizontal => Axis::Horizontal,
            SplitAxis::Vertical => Axis::Vertical,
        };
        let state = cx.new(|_| SplitState::new(gpui_axis, n));
        self.splits.insert(tab_id, (n, axis, state.clone()));
        state
    }

    /// The label of tab `ix` before the split marker: its name or the focused terminal's
    /// title, numbered when an earlier tab shows the same one ("repo", "repo (2)").
    fn base_title(&self, ix: usize, cx: &App) -> String {
        let Some(tab) = self.model.tabs().get(ix) else {
            return String::new();
        };
        tab.custom_title.clone().unwrap_or_else(|| {
            tab.focused_pane()
                .map(|v| v.read(cx).display_title())
                .unwrap_or_else(|| "Terminal".to_owned())
        })
    }

    /// [`Self::base_title`] made unique among the tabs by a counter.
    pub(crate) fn unique_title(&self, ix: usize, cx: &App) -> String {
        let base = self.base_title(ix, cx);
        let earlier = (0..ix).filter(|&i| self.base_title(i, cx) == base).count();
        if earlier == 0 {
            base
        } else {
            format!("{base} ({})", earlier + 1)
        }
    }

    fn render_tab(&self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let ui = cx.theme().ui.clone();
        let tab = &self.model.tabs()[ix];
        let active = ix == self.model.active_index();
        let focused_panel = self.focus_handle.contains_focused(window, cx);
        if let Some((id, input)) = &self.renaming {
            if *id == tab.id {
                return div()
                    .id(("term-tab-rename", tab.id))
                    .flex_none()
                    .w(px(180.))
                    .h_full()
                    .px(px(6.))
                    .flex()
                    .items_center()
                    .capture_key_down(cx.listener(|panel, e: &gpui::KeyDownEvent, window, cx| {
                        match e.keystroke.key.as_str() {
                            "enter" => {
                                cx.stop_propagation();
                                panel.finish_rename(true, window, cx);
                            }
                            "escape" => {
                                cx.stop_propagation();
                                panel.finish_rename(false, window, cx);
                            }
                            _ => {}
                        }
                    }))
                    .child(input.clone())
                    .into_any_element();
            }
        }
        let views: Vec<&Entity<TerminalView>> = tab.panes.iter().collect();
        let focused = tab.focused_pane().cloned();
        let (title, cwd, exited, failed) = match &focused {
            Some(v) => {
                let v = v.read(cx);
                (
                    self.unique_title(ix, cx),
                    v.cwd_label(),
                    v.has_exited(),
                    v.last_command_failed(),
                )
            }
            None => (String::from("Terminal"), String::new(), false, false),
        };
        let activity = !active && views.iter().any(|v| v.read(cx).has_new_output);
        let bell = !active && views.iter().any(|v| v.read(cx).bell);
        let split_count = views.len();
        let title: SharedString = if split_count > 1 {
            format!("{title} [{split_count}]").into()
        } else {
            title.into()
        };
        let tooltip: SharedString = if cwd.is_empty() {
            title.clone()
        } else {
            format!("{title}\n{cwd}").into()
        };
        let fg = if active {
            ui.tab_active_fg.hsla()
        } else {
            ui.tab_inactive_fg.hsla()
        };
        let bg = if active {
            ui.tab_active_bg.hsla()
        } else {
            ui.tab_inactive_bg.hsla()
        };
        let hover = ui.hover.hsla();
        let indicator = if focused_panel {
            ui.focus_ring.hsla()
        } else {
            ui.text_faint.hsla()
        };
        let drag_title = title.clone();
        let view_for_close = focused.clone();
        h_flex()
            .id(("term-tab", tab.id))
            .group("term-tab")
            .relative()
            .flex_none()
            .h_full()
            .pl(px(10.))
            .pr(px(4.))
            .gap(px(6.))
            .bg(bg)
            .border_r_1()
            .border_color(ui.border.hsla())
            .text_size(metrics::text_sm())
            .text_color(fg)
            .cursor_pointer()
            .when(!active, |el| el.hover(move |s| s.bg(hover)))
            .when(active, |el| {
                el.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(2.))
                        .bg(indicator),
                )
            })
            .tooltip(Tooltip::text(tooltip))
            .child(
                Icon::new(IconName::Terminal)
                    .size(IconSize::Small)
                    .color(if failed { ui.error.hsla() } else { fg }),
            )
            .child(
                div()
                    .whitespace_nowrap()
                    .max_w(px(220.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .when(exited, |el| el.text_color(ui.text_faint.hsla()))
                    .child(title),
            )
            .when(exited, |el| {
                el.child(
                    div()
                        .text_size(metrics::text_xs())
                        .text_color(ui.text_faint.hsla())
                        .child("exited"),
                )
            })
            .when(activity || bell, |el| {
                el.child(div().size(px(6.)).rounded_full().bg(if bell {
                    ui.warning.hsla()
                } else {
                    ui.accent.hsla()
                }))
            })
            .child(
                IconButton::new(("term-tab-close", tab.id), IconName::Close)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::for_action("Kill Terminal", &actions::KillTerminal))
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        cx.stop_propagation();
                        if let Some(v) = view_for_close.clone() {
                            panel.kill(v, window, cx);
                        }
                    })),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |panel, _, window, cx| panel.activate(ix, window, cx)),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |panel, _, window, cx| {
                    cx.stop_propagation();
                    if let Some(v) = panel
                        .model
                        .tabs()
                        .get(ix)
                        .and_then(|t| t.focused_pane().cloned())
                    {
                        panel.kill(v, window, cx);
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |panel, e: &gpui::MouseDownEvent, window, cx| {
                    panel.deploy_tab_menu(ix, e.position, window, cx)
                }),
            )
            .on_drag(
                DraggedTab {
                    ix,
                    title: drag_title,
                },
                |d, _, _, cx| cx.new(|_| DragPreview(d.title.clone())),
            )
            .drag_over::<DraggedTab>(move |s, _, _, _| s.bg(hover))
            .on_drop(cx.listener(move |panel, d: &DraggedTab, _, cx| {
                panel.model.move_tab(d.ix, ix);
                panel.changed(cx);
            }))
            .into_any_element()
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        // The panel renders only while shown: restored tabs that are on screen now start.
        let waiting = self
            .model
            .active_tab()
            .is_some_and(|t| t.panes.iter().any(|v| v.read(cx).is_pending()));
        if waiting {
            cx.defer_in(window, |panel, _, cx| panel.start_visible(cx));
        }
        let tabs: Vec<AnyElement> = (0..self.model.tabs().len())
            .map(|ix| self.render_tab(ix, window, cx))
            .collect();
        let body: AnyElement = match self.model.active_tab().cloned() {
            Some(tab) if tab.panes.len() > 1 => {
                let state = self.split_state(tab.id, tab.panes.len(), tab.axis, cx);
                let children = tab
                    .panes
                    .iter()
                    .map(|v| div().size_full().child(v.clone()).into_any_element())
                    .collect();
                resizable_split(("terminal-split", tab.id as usize), &state, children, cx)
                    .into_any_element()
            }
            Some(tab) => match tab.panes.first() {
                Some(view) => div().size_full().child(view.clone()).into_any_element(),
                None => div().into_any_element(),
            },
            None => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .text_color(ui.text_muted.hsla())
                .child("No terminals")
                .child(
                    lector_ui::components::Button::new("terminal-empty-new", "New Terminal")
                        .icon(IconName::Plus)
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(actions::NewTerminal), cx)
                        }),
                )
                .into_any_element(),
        };
        let button = |id: &'static str,
                      icon: IconName,
                      tip: &'static str,
                      action: Box<dyn gpui::Action>| {
            let action2 = action.boxed_clone();
            IconButton::new(id, icon)
                .icon_size(IconSize::Small)
                .tooltip(move |window, cx| Tooltip::for_action(tip, action2.as_ref())(window, cx))
                .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
        };
        v_flex()
            .id("terminal-panel")
            .key_context("TerminalPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(ui.background.hsla())
            .child(
                h_flex()
                    .flex_none()
                    .w_full()
                    .h(metrics::tab_bar_height())
                    .bg(ui.tab_bar_bg.hsla())
                    .border_b_1()
                    .border_color(ui.border.hsla())
                    .child(
                        h_flex()
                            .id("terminal-tabs")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_x_scroll()
                            .track_scroll(&self.tab_scroll)
                            .children(tabs),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .gap(px(2.))
                            .px(px(4.))
                            .child(button(
                                "terminal-new",
                                IconName::Plus,
                                "New Terminal",
                                Box::new(actions::NewTerminal),
                            ))
                            .child(button(
                                "terminal-split",
                                IconName::Columns,
                                "Split Terminal",
                                Box::new(actions::SplitTerminal),
                            ))
                            .child(button(
                                "terminal-kill",
                                IconName::Trash,
                                "Kill Terminal",
                                Box::new(actions::KillTerminal),
                            ))
                            .child(
                                IconButton::new("terminal-hide", IconName::ChevronDown)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Hide Terminal Panel"))
                                    .on_click(cx.listener(|panel, _, window, cx| {
                                        panel.set_visible(false, window, cx)
                                    })),
                            ),
                    ),
            )
            .child(div().flex_1().min_h_0().w_full().child(body))
            .children(self.menu.render())
    }
}
