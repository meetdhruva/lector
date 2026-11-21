//! The workspace window: title bar, sidebar, pane group, status bar and the
//! modal layer. Handles workspace-level actions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::scale::px;
use gpui::{
    div, prelude::*, Action, App, Axis, ClipboardItem, Entity, FocusHandle, Focusable, Pixels,
    SharedString, Subscription, Window,
};

use super::command_palette::CommandPaletteDelegate;
use super::find_bar::FindBar;
use super::go_to_line::GoToLine;
use super::pane::{new_pane, CodeItem, Pane, PaneEvent, PaneItem, PaneNode};
use crate::editor::{FileView, Reveal};
use lector_app::Project;

mod chrome;
mod editing;
mod integration;
mod lsp_banner;
mod regions;
mod render;
mod undo_toast;
use super::sidebar::{Sidebar, SidebarPanel};
use super::theme_selector::ThemeSelectorDelegate;
use crate::actions::{editor, workspace::*};
use crate::code_view::{CodeSource, CodeView, CodeViewEvent, EditorSettings, InMemorySource};
use crate::components::modal::ModalLayer;
use crate::components::picker::Picker;
use crate::components::tree_view::TreeViewEvent;
use crate::components::{h_flex, metrics};
use crate::icons::{Icon, IconSize};
use crate::theme::{ActiveTheme, ColorExt, ThemeState};
pub use chrome::{no_folder_project, no_folder_root};
pub use editing::quit_with_prompts;
use integration::Integration;
pub use integration::TabRestorer;
pub use regions::Region;
pub use undo_toast::{ToastAction, ToastLevel, UNDO_TOAST_DURATION};

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

/// A summary of the uncommitted changes, for the welcome view and the status bar.
#[derive(Clone, Debug)]
pub struct ReviewHint {
    /// Number of changed files.
    pub uncommitted: usize,
    /// Whether the changes differ from what the user last looked at in a review.
    pub new_since_last: bool,
    /// Creates the action that opens the review (in risk order).
    pub open_review: fn() -> Box<dyn gpui::Action>,
}

impl PartialEq for ReviewHint {
    fn eq(&self, other: &Self) -> bool {
        self.uncommitted == other.uncommitted
            && self.new_since_last == other.new_since_last
            && std::ptr::fn_addr_eq(self.open_review, other.open_review)
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
    /// Replaces the status-bar branch button when set (lector-ui-git's sync control).
    branch_status_view: Option<gpui::AnyView>,
    /// Quiet status bar items other crates put left of the editor status (v0.7: the update notice).
    extra_status_views: Vec<(&'static str, gpui::AnyView)>,
    sidebar: Entity<Sidebar>,
    sidebar_visible: bool,
    /// Logical (unscaled) width; multiplied by the UI scale when drawn.
    sidebar_width: Pixels,
    center: PaneNode,
    active_pane: Entity<Pane>,
    modal_layer: Entity<ModalLayer>,
    focus_handle: FocusHandle,
    /// Contains the title bar's controls (F6 region).
    title_focus: FocusHandle,
    /// Contains the status bar's buttons (F6 region).
    status_focus: FocusHandle,
    closed: Vec<PathBuf>,
    project: Project,
    integ: Integration,
    window_title: String,
    lsp_status: SharedString,
    toast: Option<(SharedString, u64)>,
    toast_level: ToastLevel,
    action_toast: Option<undo_toast::ActionToast>,
    lsp_banner: Option<lsp_banner::LspBanner>,
    has_folder: bool,
    app_menu: Entity<super::app_menu::AppMenuBar>,
    welcome: Option<Entity<super::welcome::WelcomeView>>,
    /// Uncommitted-changes summary from lector-ui-git (welcome view and status-bar chip).
    review_hint: Option<ReviewHint>,
    title_drag_pending: bool,
    /// Until then a keyboard close-window request is swallowed once (see
    /// [`Workspace::guard_close_window`]).
    close_window_guard: Option<std::time::Instant>,
    chrome_menu: crate::components::ContextMenuState,
    lsp_popover: crate::components::popover::Popover<super::lsp_status::LspStatusPopover>,
    /// A panel below the editor area (the Terminal panel), set by an extension crate.
    bottom_panel: Option<BottomPanel>,
    _subscriptions: Vec<Subscription>,
}

/// A panel below the editor area (v0.7: the Terminal panel), installed with
/// [`Workspace::set_bottom_panel`].
struct BottomPanel {
    view: gpui::AnyView,
    focus: FocusHandle,
    visible: bool,
    /// Logical (unscaled) height.
    height: Pixels,
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
            branch_status_view: None,
            extra_status_views: Vec::new(),
            sidebar,
            sidebar_visible: true,
            sidebar_width: gpui::px(272.),
            center: PaneNode::Pane(pane.clone()),
            active_pane: pane,
            modal_layer,
            focus_handle: cx.focus_handle(),
            title_focus: cx.focus_handle(),
            status_focus: cx.focus_handle(),
            closed: Vec::new(),
            project,
            integ,
            window_title: String::new(),
            lsp_status: "LSP: off".into(),
            toast: None,
            toast_level: ToastLevel::Success,
            action_toast: None,
            lsp_banner: None,
            has_folder,
            app_menu: cx.new(super::app_menu::AppMenuBar::new),
            welcome: None,
            review_hint: None,
            title_drag_pending: false,
            close_window_guard: None,
            chrome_menu: Default::default(),
            lsp_popover: Default::default(),
            bottom_panel: None,
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

    /// Swallows the next Close Window request that arrives within `duration` (with a hint).
    /// Used when a shortcut just closed something (the last terminal) and key repeat or a
    /// second press would otherwise close the whole window.
    pub fn guard_close_window(&mut self, duration: std::time::Duration) {
        self.close_window_guard = Some(std::time::Instant::now() + duration);
    }

    /// Whether a Close Window request right now is swallowed by [`Self::guard_close_window`]
    /// (consumes the guard).
    pub(crate) fn close_window_guarded(&mut self) -> bool {
        match self.close_window_guard.take() {
            Some(until) => std::time::Instant::now() < until,
            None => false,
        }
    }

    /// Shows a short message ("Permalink copied") above the status bar for
    /// a few seconds.
    pub fn show_toast(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.show_toast_level(message, ToastLevel::Success, cx);
    }

    /// [`Self::show_toast`] with the icon of `level`: a check only for success, an info mark
    /// for notes ("nothing to do"), a warning for partial results.
    pub fn show_toast_level(
        &mut self,
        message: impl Into<SharedString>,
        level: ToastLevel,
        cx: &mut Context<Self>,
    ) {
        let id = self.toast.as_ref().map(|t| t.1 + 1).unwrap_or(0);
        self.toast_level = level;
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
        let level = self.toast_level;
        let ui = &cx.theme().ui;
        Some(
            div()
                .absolute()
                .bottom(metrics::status_bar_height() + px(12.))
                .right(px(16.))
                .px(px(12.))
                .py(px(7.))
                .rounded(metrics::radius_lg())
                .bg(ui.elevated.hsla())
                .border_1()
                .border_color(ui.border.hsla())
                .shadow_lg()
                .text_size(metrics::text_sm())
                .text_color(ui.text.hsla())
                .child(
                    h_flex()
                        .gap(px(6.))
                        .child(
                            Icon::new(level.icon())
                                .size(IconSize::Small)
                                .color(level.color(ui)),
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

    /// Like [`Workspace::register_action`] for an action known only by value
    /// (built from its name), e.g. GPUI actions bound to registry commands
    /// (`crate::commands_bridge`).
    pub fn register_boxed_action(
        &mut self,
        action: Box<dyn Action>,
        callback: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> &mut Self {
        let callback = Rc::new(callback);
        self.action_registrations
            .push(Box::new(move |div, _window, cx| {
                let callback = callback.clone();
                let this = cx.entity().downgrade();
                div.on_boxed_action(action.as_ref(), move |_, window, cx| {
                    if let Some(ws) = this.upgrade() {
                        ws.update(cx, |ws, cx| callback(ws, window, cx));
                    }
                })
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

    /// Widens the sidebar to at least `width` (panels that need room, e.g. the Agent panel).
    pub fn ensure_sidebar_width(&mut self, width: Pixels, cx: &mut Context<Self>) {
        if self.sidebar_width < width {
            self.sidebar_width = width;
            cx.notify();
        }
    }

    /// Installs the panel shown below the editor area (hidden until
    /// [`Workspace::set_bottom_panel_visible`]). `focus` must contain the panel's focus: it is
    /// the panel's F6 region.
    pub fn set_bottom_panel(
        &mut self,
        view: gpui::AnyView,
        focus: FocusHandle,
        cx: &mut Context<Self>,
    ) {
        self.bottom_panel = Some(BottomPanel {
            view,
            focus,
            visible: false,
            height: gpui::px(260.),
        });
        cx.notify();
    }

    /// Shows or hides the bottom panel. Hiding it while it holds the focus moves the focus
    /// back to the active pane.
    pub fn set_bottom_panel_visible(
        &mut self,
        visible: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.bottom_panel.as_mut() else {
            return;
        };
        if panel.visible == visible {
            return;
        }
        panel.visible = visible;
        if !visible && panel.focus.contains_focused(window, cx) {
            let focus = self.active_pane.focus_handle(cx);
            window.focus(&focus);
        }
        cx.notify();
    }

    /// Whether the bottom panel is installed and shown.
    pub fn is_bottom_panel_visible(&self) -> bool {
        self.bottom_panel.as_ref().is_some_and(|p| p.visible)
    }

    /// The bottom panel's focus handle, when one is installed.
    pub fn bottom_panel_focus(&self) -> Option<FocusHandle> {
        self.bottom_panel.as_ref().map(|p| p.focus.clone())
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

    /// Sets a view drawn in the status bar in place of the branch button (v0.7: the git sync
    /// control of lector-ui-git: branch, ahead/behind, state, progress, dropdown); `None`
    /// brings the plain branch button back.
    pub fn set_branch_status_view(&mut self, view: Option<gpui::AnyView>, cx: &mut Context<Self>) {
        self.branch_status_view = view;
        cx.notify();
    }

    /// Puts (or, with `None`, removes) a view in the status bar's right group under `key`
    /// (v0.7: the update notice of lector-ui-agent). Views appear in the order they were first set.
    pub fn set_status_view(
        &mut self,
        key: &'static str,
        view: Option<gpui::AnyView>,
        cx: &mut Context<Self>,
    ) {
        match (
            self.extra_status_views.iter().position(|(k, _)| *k == key),
            view,
        ) {
            (Some(ix), Some(view)) => self.extra_status_views[ix].1 = view,
            (Some(ix), None) => {
                self.extra_status_views.remove(ix);
            }
            (None, Some(view)) => self.extra_status_views.push((key, view)),
            (None, None) => return,
        }
        cx.notify();
    }

    /// Sets what the welcome view and the status bar tell about uncommitted changes (v0.4 review
    /// UX); `None` clears it. Used by lector-ui-git.
    pub fn set_review_hint(&mut self, hint: Option<ReviewHint>, cx: &mut Context<Self>) {
        if self.review_hint == hint {
            return;
        }
        self.review_hint = hint.clone();
        if let Some(welcome) = &self.welcome {
            welcome.update(cx, |w, cx| w.set_review_hint(hint, cx));
        }
        cx.notify();
    }

    /// The current [`ReviewHint`].
    pub fn review_hint(&self) -> Option<&ReviewHint> {
        self.review_hint.as_ref()
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

    /// Opens `path` as the target of a link that points outside the project (a file path in
    /// a terminal) and moves the cursor to 1-based `line`/`col`.
    ///
    /// The tab is created locked read-only ([`FileView::set_read_only_lock`]) before its
    /// document starts loading, so the load, reloads, changes on disk and a restored session
    /// all keep it read-only. A tab of this file that is already open keeps whatever it was:
    /// a link never takes editing away from a tab the user opened on purpose. Explicit opens
    /// (File > Open, the file tree, recent files, "Go to Definition"...) are
    /// [`Workspace::open_path_at`] and stay editable.
    pub fn open_linked_path_read_only(
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
        self.open_path_locked(path, reveal, window, cx)
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
        let entries = crate::commands_bridge::palette_entries(
            &crate::commands_bridge::command_context(self, cx),
        );
        let workspace = cx.entity().downgrade();
        self.toggle_modal(window, cx, move |window, cx| {
            let delegate = CommandPaletteDelegate::with_registry(previous, entries, workspace, cx);
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

    /// Shows (or, when already shown, hides) a sidebar panel.
    pub fn show_panel(&mut self, panel: SidebarPanel, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_panel_shown(panel, cx) && panel != SidebarPanel::Files {
            self.sidebar_visible = false;
            cx.notify();
        } else {
            self.reveal_panel(panel, window, cx);
        }
    }

    /// Shows a sidebar panel and focuses it; never hides it (unlike [`Self::show_panel`]).
    pub fn reveal_panel(
        &mut self,
        panel: SidebarPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_visible = true;
        self.sidebar
            .update(cx, |s, cx| s.set_active_panel(panel, cx));
        let focus = self.sidebar.focus_handle(cx);
        window.focus(&focus);
        cx.notify();
    }

    /// Whether the sidebar is visible and showing `panel`.
    pub fn is_panel_shown(&self, panel: SidebarPanel, cx: &App) -> bool {
        self.sidebar_visible && self.sidebar.read(cx).active_panel() == panel
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
            Some(d) => gpui::px((f32::from(settings.font_size) + d).clamp(8., 32.)),
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
mod tests;
