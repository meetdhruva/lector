//! The workspace window: title bar, sidebar, pane group, status bar and the
//! modal layer. Handles workspace-level actions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{
    div, prelude::*, px, Action, AnyElement, App, Axis, ClipboardItem, CursorStyle, Decorations,
    DragMoveEvent, Entity, FocusHandle, Focusable, MouseButton, Pixels, ResizeEdge, SharedString,
    Subscription, Window,
};

use super::command_palette::CommandPaletteDelegate;
use super::find_bar::FindBar;
use super::go_to_line::GoToLine;
use super::pane::{new_pane, CodeItem, Pane, PaneEvent, PaneItem, PaneNode};
use crate::editor::{FileView, Reveal};
use lector_workspace::Project;

mod chrome;
mod editing;
mod integration;
use super::sidebar::{Sidebar, SidebarPanel};
use super::theme_selector::ThemeSelectorDelegate;
use crate::actions::{editor, workspace::*};
use crate::code_view::{CodeSource, CodeView, CodeViewEvent, EditorSettings, InMemorySource};
use crate::components::modal::ModalLayer;
use crate::components::picker::Picker;
use crate::components::split::{clamp_width, panel_resize_handle, DraggedPanelEdge};
use crate::components::tree_view::TreeViewEvent;
use crate::components::{h_flex, metrics, v_flex, Button, Tooltip};
use crate::icons::{Icon, IconName, IconSize};
use crate::keymap::contexts;
use crate::theme::{ActiveTheme, ColorExt, ThemeState};
pub use chrome::{no_folder_project, no_folder_root};
pub use editing::quit_with_prompts;
use integration::Integration;
pub use integration::TabRestorer;

/// Per-file facts shown in the status bar.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileMeta {
    /// "UTF-8", "UTF-16LE", ...
    pub encoding: SharedString,
    /// "LF" or "CRLF".
    pub line_ending: SharedString,
    /// "Spaces: 4" or "Tabs".
    pub indentation: SharedString,
}

/// Detects line ending and indentation style from text.
pub fn detect_file_meta(text: &str) -> FileMeta {
    let crlf = text.contains("\r\n");
    let mut tab_lines = 0usize;
    let mut space_lines = 0usize;
    let mut deltas: HashMap<usize, usize> = HashMap::new();
    let mut prev = 0usize;
    for line in text.lines().take(5000) {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with('\t') {
            tab_lines += 1;
            continue;
        }
        let indent = line.len() - line.trim_start_matches(' ').len();
        if indent > 0 {
            space_lines += 1;
        }
        let delta = indent.abs_diff(prev);
        if (2..=8).contains(&delta) {
            *deltas.entry(delta).or_default() += 1;
        }
        prev = indent;
    }
    let indentation = if tab_lines > space_lines {
        "Tabs".to_string()
    } else {
        let unit = deltas
            .iter()
            .max_by_key(|(d, n)| (**n, std::cmp::Reverse(**d)))
            .map(|(d, _)| *d)
            .unwrap_or(4);
        format!("Spaces: {unit}")
    };
    FileMeta {
        encoding: "UTF-8".into(),
        line_ending: if crlf { "CRLF" } else { "LF" }.into(),
        indentation: indentation.into(),
    }
}

/// Reads `.git/HEAD` to find the current branch (read-only, no git needed).
pub fn read_git_branch(root: &Path) -> Option<SharedString> {
    let mut dir = Some(root);
    while let Some(d) = dir {
        let git = d.join(".git");
        let head_path = if git.is_file() {
            // Worktree: "gitdir: <path>"
            let content = std::fs::read_to_string(&git).ok()?;
            let gitdir = content.strip_prefix("gitdir:")?.trim();
            PathBuf::from(gitdir).join("HEAD")
        } else {
            git.join("HEAD")
        };
        if let Ok(head) = std::fs::read_to_string(&head_path) {
            let head = head.trim();
            return Some(match head.strip_prefix("ref: refs/heads/") {
                Some(branch) => branch.to_string().into(),
                None => head.chars().take(7).collect::<String>().into(),
            });
        }
        dir = d.parent();
    }
    None
}

/// Events emitted by the [`Workspace`] (subscribe with `cx.subscribe`).
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceEvent {
    /// The active pane item changed (another tab, pane or file).
    ActiveItemChanged,
    /// Files were created/modified/removed on disk (absolute paths).
    FilesChanged(Vec<PathBuf>),
    /// `.git` state changed (HEAD, index, refs); git status was refreshed.
    GitStateChanged,
    /// Settings were (re)loaded.
    SettingsChanged,
}

type ActionRegistration = Box<dyn Fn(gpui::Div, &mut Window, &mut Context<Workspace>) -> gpui::Div>;

/// The workspace root view.
pub struct Workspace {
    action_registrations: Vec<ActionRegistration>,
    root: PathBuf,
    name: SharedString,
    branch: Option<SharedString>,
    branch_status: Option<SharedString>,
    sidebar: Entity<Sidebar>,
    sidebar_visible: bool,
    sidebar_width: Pixels,
    center: PaneNode,
    active_pane: Entity<Pane>,
    modal_layer: Entity<ModalLayer>,
    focus_handle: FocusHandle,
    closed: Vec<PathBuf>,
    project: Project,
    integ: Integration,
    window_title: String,
    lsp_status: SharedString,
    toast: Option<(SharedString, u64)>,
    has_folder: bool,
    app_menu: Entity<super::app_menu::AppMenuBar>,
    welcome: Option<Entity<super::welcome::WelcomeView>>,
    title_drag_pending: bool,
    chrome_menu: crate::components::ContextMenuState,
    lsp_popover: crate::components::popover::Popover<super::lsp_status::LspStatusPopover>,
    _subscriptions: Vec<Subscription>,
}

impl gpui::EventEmitter<WorkspaceEvent> for Workspace {}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Workspace {
    /// Workspace over `project` (its root folder). The file tree, git
    /// status, symbol index and watchers start in the background.
    pub fn new(project: Project, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_with(project, true, window, cx)
    }

    fn new_with(
        project: Project,
        has_folder: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let root = project.root().to_path_buf();
        let name: SharedString = project.name().to_string().into();
        let branch = read_git_branch(&root);
        let sidebar = cx.new(|cx| Sidebar::new(root.clone(), cx));
        let pane = new_pane(Some(root.clone()), cx);
        let modal_layer = cx.new(|_| ModalLayer::new());

        let mut subscriptions = Vec::new();
        let tree = sidebar.read(cx).file_tree().clone();
        subscriptions.push(cx.subscribe_in(
            &tree,
            window,
            |this, _, event: &TreeViewEvent<PathBuf>, window, cx| {
                if let TreeViewEvent::Confirmed { id, preview } = event {
                    this.open_path(id.clone(), *preview, window, cx);
                }
            },
        ));
        subscriptions.push(Self::subscribe_pane(&pane, window, cx));
        subscriptions.push(cx.subscribe(
            &sidebar,
            |this, _, event: &super::sidebar::SidebarEvent, cx| match event {
                super::sidebar::SidebarEvent::RefreshFiles => {
                    if this.has_folder {
                        this.rescan_files(false, cx);
                        this.refresh_git_status(cx);
                    }
                }
            },
        ));
        subscriptions.push(cx.observe_window_appearance(window, |_, window, cx| {
            ThemeState::sync_with_window(window, cx);
        }));
        ThemeState::sync_with_window(window, cx);

        pane.update(cx, |p, _| p.is_active_pane = true);
        let integ = Integration::new(&project, cx);
        let mut this = Workspace {
            action_registrations: Vec::new(),
            root,
            name,
            branch,
            branch_status: None,
            sidebar,
            sidebar_visible: true,
            sidebar_width: px(272.),
            center: PaneNode::Pane(pane.clone()),
            active_pane: pane,
            modal_layer,
            focus_handle: cx.focus_handle(),
            closed: Vec::new(),
            project,
            integ,
            window_title: String::new(),
            lsp_status: "LSP: off".into(),
            toast: None,
            has_folder,
            app_menu: cx.new(super::app_menu::AppMenuBar::new),
            welcome: None,
            title_drag_pending: false,
            chrome_menu: Default::default(),
            lsp_popover: Default::default(),
            _subscriptions: subscriptions,
        };
        if !has_folder {
            this.name = "Lector".into();
            this.branch = None;
            this.sidebar.update(cx, |s, cx| s.set_has_folder(false, cx));
        }
        this.subscribe_app_menu(window, cx);
        this.start_integration(window, cx);
        this.start_editing(window, cx);
        this
    }

    /// Shows a short message ("Permalink copied") above the status bar for
    /// a few seconds.
    pub fn show_toast(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        let id = self.toast.as_ref().map(|t| t.1 + 1).unwrap_or(0);
        self.toast = Some((message.into(), id));
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(2500))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.toast.as_ref().is_some_and(|t| t.1 == id) {
                    this.toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn render_toast(&self, cx: &App) -> Option<impl IntoElement> {
        let (message, _) = self.toast.clone()?;
        let ui = &cx.theme().ui;
        Some(
            div()
                .absolute()
                .bottom(metrics::STATUS_BAR_HEIGHT + px(12.))
                .right(px(16.))
                .px(px(12.))
                .py(px(7.))
                .rounded(metrics::RADIUS_LG)
                .bg(ui.elevated.hsla())
                .border_1()
                .border_color(ui.border.hsla())
                .shadow_lg()
                .text_size(metrics::TEXT_SM)
                .text_color(ui.text.hsla())
                .child(
                    h_flex()
                        .gap(px(6.))
                        .child(
                            Icon::new(IconName::CircleCheck)
                                .size(IconSize::Small)
                                .color(ui.accent.hsla()),
                        )
                        .child(message),
                ),
        )
    }

    fn subscribe_pane(
        pane: &Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            pane,
            window,
            |this, pane, event: &PaneEvent, window, cx| match event {
                PaneEvent::Activated => this.set_active_pane(pane.clone(), cx),
                PaneEvent::Empty => {
                    if this.center.panes().len() > 1 {
                        this.center.remove(pane, cx);
                        if let Some(first) = this.center.panes().first().cloned() {
                            this.set_active_pane(first.clone(), cx);
                            let focus = first.focus_handle(cx);
                            window.focus(&focus);
                        }
                    }
                    this.focus_welcome_if_empty(window, cx);
                    cx.notify();
                }
                PaneEvent::ItemClosed(path) => {
                    if let Some(path) = path {
                        this.closed.push(path.clone());
                    }
                    cx.emit(WorkspaceEvent::ActiveItemChanged);
                    cx.notify();
                }
                PaneEvent::RevealPath(path) => {
                    let path = path.clone();
                    this.reveal_path(&path, cx);
                }
            },
        )
    }

    fn set_active_pane(&mut self, pane: Entity<Pane>, cx: &mut Context<Self>) {
        if self.active_pane != pane {
            self.active_pane.update(cx, |p, cx| {
                p.is_active_pane = false;
                cx.notify();
            });
            pane.update(cx, |p, cx| {
                p.is_active_pane = true;
                cx.notify();
            });
            self.active_pane = pane;
        }
        cx.emit(WorkspaceEvent::ActiveItemChanged);
        cx.notify();
    }

    /// Registers a handler for action `A` on the workspace element, so
    /// extension crates can hook in from `cx.observe_new::<Workspace>(..)`:
    ///
    /// ```ignore
    /// cx.observe_new(|ws: &mut Workspace, _window, _cx| {
    ///     ws.register_action(|ws, _: &ShowChanges, window, cx| { /* ... */ });
    /// }).detach();
    /// ```
    ///
    /// Registered handlers run before the workspace's built-in handlers for
    /// the same action (call `cx.propagate()` to fall through to them).
    pub fn register_action<A: Action>(
        &mut self,
        callback: impl Fn(&mut Workspace, &A, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> &mut Self {
        let callback = Rc::new(callback);
        self.action_registrations
            .push(Box::new(move |div, _window, cx| {
                let callback = callback.clone();
                div.on_action(
                    cx.listener(move |ws, action: &A, window, cx| callback(ws, action, window, cx)),
                )
            }));
        self
    }

    /// The project root.
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// The sidebar.
    pub fn sidebar(&self) -> &Entity<Sidebar> {
        &self.sidebar
    }

    /// Whether the sidebar is shown.
    pub fn is_sidebar_visible(&self) -> bool {
        self.sidebar_visible
    }

    /// The modal layer.
    pub fn modal_layer(&self) -> &Entity<ModalLayer> {
        &self.modal_layer
    }

    /// The active pane.
    pub fn active_pane(&self) -> &Entity<Pane> {
        &self.active_pane
    }

    /// Every pane of the editor area, in layout order.
    pub fn panes(&self) -> Vec<Entity<Pane>> {
        self.center.panes()
    }

    /// Sets the branch shown in the title bar and, optionally, a richer
    /// status-bar label (e.g. "main ↑2 ↓1"); used by lector-ui-git.
    pub fn set_branch(
        &mut self,
        branch: Option<SharedString>,
        status_label: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        if self.branch != branch || self.branch_status != status_label {
            self.branch = branch;
            self.branch_status = status_label;
            cx.notify();
        }
    }

    /// Sets the LSP status text shown in the status bar.
    pub fn set_lsp_status(&mut self, status: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.lsp_status = status.into();
        cx.notify();
    }

    /// The code view of the active item, if any.
    pub fn active_code_view(&self, cx: &App) -> Option<Entity<CodeView>> {
        self.active_pane
            .read(cx)
            .active_item()
            .and_then(|i| i.code_view())
    }

    /// Path of the active item, if it shows a file.
    pub fn active_path(&self, cx: &App) -> Option<PathBuf> {
        self.active_pane
            .read(cx)
            .active_item()
            .and_then(|i| i.path(cx))
    }

    /// Adds an arbitrary item to the active pane (integration: diff views).
    pub fn add_item(
        &mut self,
        item: Box<dyn PaneItem>,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_pane
            .update(cx, |p, cx| p.add_item(item, preview, window, cx));
    }

    /// Opens a file (or activates its tab). `preview` reuses the preview tab.
    pub fn open_path(
        &mut self,
        path: PathBuf,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_path_with(path, preview, None, true, window, cx);
    }

    /// Opens `path` and moves the cursor to 1-based `line`/`col`.
    pub fn open_path_at(
        &mut self,
        path: PathBuf,
        line: Option<u32>,
        col: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<FileView>> {
        let reveal = line.map(|line| {
            let col = col.map(|c| c.saturating_sub(1) as usize).unwrap_or(0);
            Reveal {
                row: line.saturating_sub(1),
                cols: col..col,
                center: true,
            }
        });
        self.open_path_with(path, false, reveal, true, window, cx)
    }

    fn reveal_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        let tree = self.sidebar.read(cx).file_tree().clone();
        let ancestors = tree.read(cx).delegate.ancestors_of(path);
        tree.update(cx, |t, cx| t.reveal(&ancestors, &path.to_path_buf(), cx));
        self.sidebar
            .update(cx, |s, cx| s.set_active_panel(SidebarPanel::Files, cx));
        self.sidebar_visible = true;
        cx.notify();
    }

    fn toggle_modal<V: gpui::ManagedView>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        build: impl FnOnce(&mut Window, &mut Context<V>) -> V,
    ) {
        self.modal_layer
            .update(cx, |layer, cx| layer.toggle_modal(window, cx, build));
    }

    // Actions.

    fn toggle_command_palette(
        &mut self,
        _: &ToggleCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = window.focused(cx);
        self.toggle_modal(window, cx, move |window, cx| {
            let delegate = CommandPaletteDelegate::new(previous, cx);
            Picker::new(delegate, window, cx)
        });
    }

    fn select_theme(&mut self, _: &SelectTheme, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_modal(window, cx, |window, cx| {
            let delegate = ThemeSelectorDelegate::new(cx);
            Picker::new(delegate, window, cx)
        });
    }

    fn go_to_line(&mut self, _: &editor::GoToLine, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_code_view(cx) else {
            return;
        };
        self.toggle_modal(window, cx, move |window, cx| {
            GoToLine::new(view, window, cx)
        });
    }

    /// The open in-file find bar, if any.
    pub fn find_bar(&self, cx: &App) -> Option<Entity<FindBar>> {
        self.modal_layer.read(cx).active_modal::<FindBar>()
    }

    fn find(&mut self, _: &editor::Find, window: &mut Window, cx: &mut Context<Self>) {
        // Find again, with the bar open: show / hide its replace row.
        if let Some(bar) = self.modal_layer.read(cx).active_modal::<FindBar>() {
            bar.update(cx, |bar, cx| bar.toggle_replace(window, cx));
            return;
        }
        self.open_find(false, window, cx);
    }

    fn find_replace(
        &mut self,
        _: &editor::FindReplace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(bar) = self.modal_layer.read(cx).active_modal::<FindBar>() {
            bar.update(cx, |bar, cx| bar.open_replace(window, cx));
            return;
        }
        self.open_find(true, window, cx);
    }

    /// `Replace Next` / `Replace All` from the menu or a key while the bar is
    /// closed: open it on the replace row (it needs a query first).
    fn replace_next(
        &mut self,
        _: &editor::ReplaceNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.modal_layer.read(cx).active_modal::<FindBar>() {
            Some(bar) => bar.update(cx, |bar, cx| bar.replace_next(window, cx)),
            None => self.open_find(true, window, cx),
        }
    }

    fn replace_all(&mut self, _: &editor::ReplaceAll, window: &mut Window, cx: &mut Context<Self>) {
        match self.modal_layer.read(cx).active_modal::<FindBar>() {
            Some(bar) => bar.update(cx, |bar, cx| bar.replace_all(window, cx)),
            None => self.open_find(true, window, cx),
        }
    }

    fn open_find(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_code_view(cx) else {
            return;
        };
        let model = self
            .active_file_view(cx)
            .map(|file| file.read(cx).model().clone());
        self.toggle_modal(window, cx, move |window, cx| {
            let mut bar = FindBar::new(view, model, replace, window, cx);
            if replace {
                bar.open_replace(window, cx);
            }
            bar
        });
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        if !self.sidebar_visible {
            if let Some(item) = self.active_pane.read(cx).active_item() {
                let f = item.focus_handle(cx);
                window.focus(&f);
            }
        }
        cx.notify();
    }

    fn show_panel(&mut self, panel: SidebarPanel, window: &mut Window, cx: &mut Context<Self>) {
        let already = self.sidebar_visible && self.sidebar.read(cx).active_panel() == panel;
        if already && panel != SidebarPanel::Files {
            self.sidebar_visible = false;
        } else {
            self.sidebar_visible = true;
            self.sidebar
                .update(cx, |s, cx| s.set_active_panel(panel, cx));
            let focus = self.sidebar.focus_handle(cx);
            window.focus(&focus);
        }
        cx.notify();
    }

    fn split(&mut self, axis: Axis, window: &mut Window, cx: &mut Context<Self>) {
        let new = new_pane(Some(self.root.clone()), cx);
        let cloned: Option<Box<dyn PaneItem>> = if let Some(file_view) = self.active_file_view(cx) {
            // A second view of the same file, at the same position.
            let (path, head) = {
                let v = file_view.read(cx);
                (v.path().to_path_buf(), v.code_view().read(cx).cursor())
            };
            let project = self.project.clone();
            let view = cx.new(|cx| {
                let mut v = FileView::new(path, project, cx);
                v.reveal(
                    Reveal {
                        row: head.row,
                        cols: head.col..head.col,
                        center: true,
                    },
                    cx,
                );
                v
            });
            self.subscribe_file_view(&view, window, cx);
            Some(Box::new(crate::editor::FileItem::new(view, cx)))
        } else {
            let active = self
                .active_pane
                .read(cx)
                .active_item()
                .and_then(|i| i.code_view().map(|v| (v, i.path(cx), i.title(cx))));
            active.map(|(view, path, title)| {
                let source = view.read(cx).source().clone();
                let selection = view.read(cx).selection();
                let p = path.clone();
                let v = cx.new(|cx| {
                    let mut v = CodeView::new(source, cx);
                    v.set_path(p.clone());
                    v.set_selection(selection, cx);
                    v
                });
                self._subscriptions
                    .push(cx.subscribe(&v, |_, _, event: &CodeViewEvent, cx| {
                        if matches!(event, CodeViewEvent::SelectionChanged) {
                            cx.notify();
                        }
                    }));
                Box::new(CodeItem::new(v, path, title)) as Box<dyn PaneItem>
            })
        };
        let target = self.active_pane.clone();
        self.center.split(&target, new.clone(), axis, cx);
        self._subscriptions
            .push(Self::subscribe_pane(&new, window, cx));
        if let Some(item) = cloned {
            new.update(cx, |p, cx| p.add_item(item, false, window, cx));
        }
        self.set_active_pane(new, cx);
        cx.notify();
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        self.active_pane
            .update(cx, |p, cx| p.close_active(window, cx));
    }

    fn close_other_tabs(
        &mut self,
        _: &CloseOtherTabs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_pane.update(cx, |p, cx| {
            let ix = p.active_index();
            p.close_others(ix, window, cx)
        });
    }

    fn close_tabs_to_right(
        &mut self,
        _: &CloseTabsToRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_pane.update(cx, |p, cx| {
            let ix = p.active_index();
            p.close_to_right(ix, window, cx)
        });
    }

    fn reopen_closed_tab(
        &mut self,
        _: &ReopenClosedTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = self.closed.pop() {
            self.open_path(path, false, window, cx);
        }
    }

    fn reveal_active_file(&mut self, _: &RevealActiveFile, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.active_path(cx) {
            self.reveal_path(&path, cx);
        }
    }

    fn zoom(&mut self, delta: Option<f32>, cx: &mut Context<Self>) {
        let mut settings = EditorSettings::get(cx);
        settings.font_size = match delta {
            Some(d) => px((f32::from(settings.font_size) + d).clamp(8., 32.)),
            None => EditorSettings::default().font_size,
        };
        cx.set_global(settings);
        cx.refresh_windows();
    }

    fn copy_path(&mut self, relative: bool, cx: &mut Context<Self>) {
        if let Some(path) = self.active_path(cx) {
            let text = if relative {
                path.strip_prefix(&self.root)
                    .unwrap_or(&path)
                    .display()
                    .to_string()
            } else {
                path.display().to_string()
            };
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    // Rendering.

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.entity().downgrade();
        let ui = cx.theme().ui.clone();
        let settings = EditorSettings::get(cx);
        let view = self.active_code_view(cx);
        let meta = self.active_file_view(cx).and_then(|v| v.read(cx).meta(cx));
        let (cursor, language, selection) = match &view {
            Some(view) => {
                let v = view.read(cx);
                let (line, col) = v.cursor_line_col(settings.tab_size);
                let sel = v.selection();
                let selection = if sel.is_empty() {
                    None
                } else {
                    let (start, end) = sel.range();
                    let lines =
                        end.row - start.row + u32::from(end.col > 0 || end.row == start.row);
                    Some(if end.row - start.row < 2000 {
                        let chars = v.selected_text().chars().count();
                        if lines > 1 {
                            format!("{chars} selected ({lines} lines)")
                        } else {
                            format!("{chars} selected")
                        }
                    } else {
                        format!("{lines} lines selected")
                    })
                };
                let carets = self.selection_count_suffix(cx).unwrap_or_default();
                (
                    Some(format!("Ln {line}, Col {col}{carets}")),
                    v.source().language_name(),
                    selection,
                )
            }
            None => (None, None, None),
        };
        let fg = ui.status_bar_fg.hsla();
        let warn = cx.theme().git.modified.hsla();
        let item =
            |id: &'static str, label: SharedString| Button::new(id, label).compact().color(fg);
        let indexing = self.integ.index_progress.map(|(done, total)| {
            if total == 0 {
                "Indexing symbols…".to_string()
            } else {
                format!("Indexing symbols {}%", done * 100 / total.max(1))
            }
        });
        h_flex()
            .h(metrics::STATUS_BAR_HEIGHT)
            .flex_none()
            .w_full()
            .px(px(6.))
            .bg(ui.status_bar_bg.hsla())
            .border_t_1()
            .border_color(ui.border.hsla())
            .text_size(metrics::TEXT_SM)
            .text_color(fg)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .gap(px(2.))
                    .when_some(self.branch_status.clone().or(self.branch.clone()), |el, branch| {
                        el.child(
                            item("status-branch", branch)
                                .icon(IconName::GitBranch)
                                .tooltip(Tooltip::for_action("Branches", &ShowBranches))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(ShowBranches), cx)
                                }),
                        )
                    })
                    .when(self.project.repo().is_some(), |el| el.child({
                        let changed = self.integ.git_status.len();
                        let label: SharedString = if changed == 0 {
                            "Changes".into()
                        } else {
                            format!("{changed} changed").into()
                        };
                        item("status-changes", label)
                            .icon(IconName::GitCompare)
                            .tooltip(Tooltip::for_action("Show Changes", &ShowChanges))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(ShowChanges), cx)
                            })
                    }))
                    .when_some(indexing, |el, text| {
                        el.child(
                            item("status-indexing", text.into())
                                .icon(IconName::Loader)
                                .tooltip(Tooltip::text(
                                    "Building the project symbol index (Go to Symbol, navigation without LSP)",
                                )),
                        )
                    }),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(2.))
                    .children(self.render_edit_status(cx))
                    .children(crate::lsp::problems::status_item(self, cx))
                    .when_some(selection, |el, text| {
                        el.child(item("status-sel", text.into()))
                    })
                    .when_some(cursor, |el, c| {
                        el.child(
                            item("status-cursor", c.into())
                                .tooltip(Tooltip::for_action("Go to Line", &editor::GoToLine))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(editor::GoToLine), cx)
                                }),
                        )
                    })
                    .when_some(meta.clone(), |el, m| {
                        el.child(item("status-indent", m.indentation).tooltip(
                            Tooltip::with_meta(
                                "Indentation",
                                "Detected from the file (tab size from settings)",
                            ),
                        ))
                    })
                    .when_some(meta.clone(), |el, m| {
                        if m.lossy {
                            el.child(
                                item("status-encoding", format!("{} (lossy)", m.encoding).into())
                                    .color(warn)
                                    .icon(IconName::AlertTriangle)
                                    .tooltip(Tooltip::text(
                                        "Some bytes are invalid in this encoding and are shown as \u{FFFD}",
                                    )),
                            )
                        } else {
                            el.child(item("status-encoding", m.encoding).tooltip(
                                Tooltip::with_meta("Encoding", "Kept as is when the file is saved"),
                            ))
                        }
                    })
                    .when_some(meta, |el, m| {
                        let tip = if m.line_ending.as_ref() == "CRLF" {
                            "Windows line endings (\\r\\n), kept when saving"
                        } else {
                            "Unix line endings (\\n), kept when saving"
                        };
                        el.child(
                            item("status-eol", m.line_ending)
                                .tooltip(Tooltip::with_meta("Line Endings", tip)),
                        )
                    })
                    .when_some(language, |el, l| {
                        el.child(item("status-language", l).tooltip(Tooltip::with_meta(
                            "Language",
                            "Detected from the file name and contents",
                        )))
                    })
                    .when(self.has_folder, |el| {
                        let (label, tip) = self.integ.lsp_status.clone().unwrap_or_else(|| {
                            ("LSP".into(), "No language server for this file".into())
                        });
                        let popover_open = self.lsp_popover.is_open();
                        el.child(
                            item("status-lsp", label)
                                .icon(IconName::CircleDot)
                                .selected(popover_open)
                                .when(!popover_open, |b| {
                                    b.tooltip(Tooltip::with_meta(
                                        tip,
                                        "Click for all language servers",
                                    ))
                                })
                                .on_click(move |_, window, cx| {
                                    // Above the status bar, right-aligned.
                                    let size = window.viewport_size();
                                    let position = gpui::point(
                                        size.width - px(8.),
                                        size.height - metrics::STATUS_BAR_HEIGHT - px(4.),
                                    );
                                    let _ = weak.update(cx, |ws, cx| {
                                        ws.toggle_lsp_popover(position, window, cx)
                                    });
                                }),
                        )
                    }),
            )
    }

    fn render_resize_edges(&self, window: &mut Window) -> Vec<AnyElement> {
        let Decorations::Client { .. } = window.window_decorations() else {
            return Vec::new();
        };
        if window.is_maximized() {
            return Vec::new();
        }
        let edge = |id: &'static str, e: ResizeEdge, cursor: CursorStyle| {
            div()
                .id(id)
                .absolute()
                .occlude()
                .cursor(cursor)
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    cx.stop_propagation();
                    window.start_window_resize(e);
                })
        };
        let t = px(4.);
        vec![
            edge("edge-top", ResizeEdge::Top, CursorStyle::ResizeUpDown)
                .top_0()
                .left(t)
                .right(t)
                .h(t)
                .into_any_element(),
            edge("edge-bottom", ResizeEdge::Bottom, CursorStyle::ResizeUpDown)
                .bottom_0()
                .left(t)
                .right(t)
                .h(t)
                .into_any_element(),
            edge("edge-left", ResizeEdge::Left, CursorStyle::ResizeLeftRight)
                .left_0()
                .top(t)
                .bottom(t)
                .w(t)
                .into_any_element(),
            edge(
                "edge-right",
                ResizeEdge::Right,
                CursorStyle::ResizeLeftRight,
            )
            .right_0()
            .top(t)
            .bottom(t)
            .w(t)
            .into_any_element(),
            edge(
                "edge-tl",
                ResizeEdge::TopLeft,
                CursorStyle::ResizeUpLeftDownRight,
            )
            .top_0()
            .left_0()
            .size(t)
            .into_any_element(),
            edge(
                "edge-tr",
                ResizeEdge::TopRight,
                CursorStyle::ResizeUpRightDownLeft,
            )
            .top_0()
            .right_0()
            .size(t)
            .into_any_element(),
            edge(
                "edge-bl",
                ResizeEdge::BottomLeft,
                CursorStyle::ResizeUpRightDownLeft,
            )
            .bottom_0()
            .left_0()
            .size(t)
            .into_any_element(),
            edge(
                "edge-br",
                ResizeEdge::BottomRight,
                CursorStyle::ResizeUpLeftDownRight,
            )
            .bottom_0()
            .right_0()
            .size(t)
            .into_any_element(),
        ]
    }
}

/// Loads a file into a source, with status-bar metadata. Never panics on
/// unreadable or binary files.
pub fn load_source(path: &Path, title: &str) -> (Rc<dyn CodeSource>, FileMeta) {
    match std::fs::read(path) {
        Ok(bytes) => {
            let head = &bytes[..bytes.len().min(8192)];
            if head.contains(&0) {
                let text = format!(
                    "Binary file — {} bytes\n\nLector does not display binary content.",
                    bytes.len()
                );
                let meta = FileMeta {
                    encoding: "Binary".into(),
                    line_ending: "—".into(),
                    indentation: "—".into(),
                };
                return (Rc::new(InMemorySource::new(&text)), meta);
            }
            let (text, encoding) = match String::from_utf8(bytes) {
                Ok(s) => (s, "UTF-8"),
                Err(e) => (
                    String::from_utf8_lossy(e.as_bytes()).into_owned(),
                    "UTF-8 (lossy)",
                ),
            };
            let text = text
                .strip_prefix('\u{feff}')
                .map(str::to_string)
                .unwrap_or(text);
            let mut meta = detect_file_meta(&text);
            meta.encoding = encoding.into();
            (Rc::new(InMemorySource::for_file(&text, title)), meta)
        }
        Err(err) => {
            let text = format!("Could not read {}:\n{err}", path.display());
            (Rc::new(InMemorySource::new(&text)), FileMeta::default())
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;

        let title = match self.active_path(cx) {
            Some(p) => format!(
                "{} — {}",
                p.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                self.name
            ),
            None if !self.has_folder => "Lector".to_string(),
            None => format!("{} — Lector", self.name),
        };
        let title = format!("{}{title}", self.title_dirty_marker(cx));
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }

        let sidebar_width = self.sidebar_width;
        let center = if self.center_is_empty(cx) {
            let welcome = self.welcome_view(window, cx);
            gpui::AnyView::from(welcome)
                .cached(gpui::StyleRefinement::default().size_full())
                .into_any_element()
        } else {
            self.center.render(0, cx)
        };
        let mut root = v_flex();
        let registrations = std::mem::take(&mut self.action_registrations);
        for register in &registrations {
            root = register(root, window, cx);
        }
        self.action_registrations = registrations;
        let root = self.register_chrome_actions(root, cx);
        let root = self.register_editing_actions(root, cx);
        root.id("workspace")
            .key_context(contexts::WORKSPACE)
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .font_family(crate::settings::UiFont::get(cx).family)
            .text_size(px(crate::settings::UiFont::get(cx).size))
            .text_color(ui.text.hsla())
            .bg(ui.background.hsla())
            .on_action(cx.listener(Self::toggle_command_palette))
            .on_action(cx.listener(Self::toggle_file_finder))
            .on_action(cx.listener(Self::toggle_outline_picker))
            .on_action(cx.listener(Self::toggle_symbol_finder))
            .on_action(cx.listener(Self::go_to_definition))
            .on_action(cx.listener(Self::find_references))
            .on_action(cx.listener(Self::toggle_markdown_preview))
            .on_action(cx.listener(Self::reload_settings_action))
            .on_action(cx.listener(Self::copy_permalink_action))
            .on_action(cx.listener(Self::select_theme))
            .on_action(cx.listener(Self::go_to_line))
            .on_action(cx.listener(Self::find))
            .on_action(cx.listener(Self::find_replace))
            .on_action(cx.listener(Self::replace_next))
            .on_action(cx.listener(Self::replace_all))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(
                cx.listener(|this, _: &ShowFiles, w, cx| {
                    this.show_panel(SidebarPanel::Files, w, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ShowChanges, w, cx| {
                this.show_panel(SidebarPanel::Changes, w, cx)
            }))
            .on_action(cx.listener(|this, _: &ShowHistory, w, cx| {
                this.show_panel(SidebarPanel::History, w, cx)
            }))
            .on_action(cx.listener(|this, _: &ShowBranches, w, cx| {
                this.show_panel(SidebarPanel::Branches, w, cx)
            }))
            .on_action(cx.listener(Self::toggle_project_search))
            .on_action(cx.listener(|this, _: &ShowOutline, w, cx| {
                this.show_panel(SidebarPanel::Outline, w, cx)
            }))
            .on_action(
                cx.listener(|this, _: &SplitRight, w, cx| this.split(Axis::Horizontal, w, cx)),
            )
            .on_action(cx.listener(|this, _: &SplitDown, w, cx| this.split(Axis::Vertical, w, cx)))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::close_other_tabs))
            .on_action(cx.listener(Self::close_tabs_to_right))
            .on_action(cx.listener(Self::reopen_closed_tab))
            .on_action(cx.listener(|this, _: &NextTab, w, cx| {
                this.active_pane
                    .update(cx, |p, cx| p.activate_relative(1, w, cx))
            }))
            .on_action(cx.listener(|this, _: &PrevTab, w, cx| {
                this.active_pane
                    .update(cx, |p, cx| p.activate_relative(-1, w, cx))
            }))
            .on_action(cx.listener(Self::nav_back))
            .on_action(cx.listener(Self::nav_forward))
            .on_action(cx.listener(Self::reveal_active_file))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom(Some(1.), cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom(Some(-1.), cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _, cx| this.zoom(None, cx)))
            .on_action(
                cx.listener(|_, _: &ToggleAppearance, _, cx| ThemeState::toggle_appearance(cx)),
            )
            .on_action(cx.listener(|_, _: &ToggleColorblindDiff, _, cx| {
                let on = cx.global::<ThemeState>().colorblind_diff();
                ThemeState::set_colorblind_diff(cx, !on);
            }))
            .on_action(cx.listener(|this, _: &editor::CopyPath, _, cx| this.copy_path(false, cx)))
            .on_action(
                cx.listener(|this, _: &editor::CopyRelativePath, _, cx| this.copy_path(true, cx)),
            )
            .on_action(cx.listener(Self::open_keymap_file))
            .on_action(cx.listener(Self::open_settings_file))
            .on_drag_move(
                cx.listener(|this, e: &DragMoveEvent<DraggedPanelEdge>, window, cx| {
                    if e.drag(cx).id == "sidebar-edge" {
                        let w = e.event.position.x - e.bounds.left();
                        this.sidebar_width =
                            clamp_width(w - px(46.), px(180.), window.viewport_size().width * 0.6);
                        cx.notify();
                    }
                }),
            )
            .child(self.render_title_bar(window, cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .when(self.sidebar_visible, |el| {
                        el.child(
                            div()
                                .relative()
                                .h_full()
                                .flex_none()
                                .w(sidebar_width + px(46.))
                                .border_r_1()
                                .border_color(ui.border.hsla())
                                .child(
                                    gpui::AnyView::from(self.sidebar.clone())
                                        .cached(gpui::StyleRefinement::default().size_full()),
                                )
                                .child(panel_resize_handle("sidebar-edge", cx)),
                        )
                    })
                    .child(div().flex_1().min_w_0().h_full().child(center)),
            )
            .child(self.render_status_bar(cx))
            .children(self.render_toast(cx))
            .children(self.chrome_menu.render())
            .children(self.lsp_popover.render())
            .child(self.modal_layer.clone())
            .children(self.render_resize_edges(window))
    }
}

/// Window root wrapping the workspace (lets `OpenFolder` swap workspaces).
pub struct WorkspaceRoot {
    /// The workspace.
    pub workspace: Entity<Workspace>,
}

impl Render for WorkspaceRoot {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(self.workspace.clone())
            .children(crate::startup::first_paint_probe())
    }
}

/// Dispatches `action` to the workspace's focused element.
pub fn dispatch(action: &dyn Action, window: &mut Window, cx: &mut App) {
    window.dispatch_action(action.boxed_clone(), cx);
}

#[cfg(test)]
mod tests {
    use super::detect_file_meta;

    #[test]
    fn detects_indentation_and_line_endings() {
        let m = detect_file_meta("fn a() {\n    if x {\n        y();\n    }\n}\n");
        assert_eq!(m.indentation.as_ref(), "Spaces: 4");
        assert_eq!(m.line_ending.as_ref(), "LF");
        let m = detect_file_meta("a:\r\n  b:\r\n    c\r\n");
        assert_eq!(m.indentation.as_ref(), "Spaces: 2");
        assert_eq!(m.line_ending.as_ref(), "CRLF");
        assert_eq!(
            detect_file_meta("a\n\tb\n\tc\n").indentation.as_ref(),
            "Tabs"
        );
    }
}
