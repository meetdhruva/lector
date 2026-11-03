//! The workspace window: title bar, sidebar, pane group, status bar and the
//! modal layer. Handles workspace-level actions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    div, prelude::*, px, Action, AnyElement, App, Axis, ClipboardItem, CursorStyle, Decorations,
    DragMoveEvent, Entity, FocusHandle, Focusable, MouseButton, Pixels, ResizeEdge, SharedString,
    Subscription, Window,
};

use super::command_palette::CommandPaletteDelegate;
use super::file_finder::{walk_project, FileFinderDelegate};
use super::go_to_line::GoToLine;
use super::pane::{new_pane, CodeItem, Pane, PaneEvent, PaneItem, PaneNode};
use super::sidebar::{Sidebar, SidebarPanel};
use super::theme_selector::ThemeSelectorDelegate;
use crate::actions::{editor, workspace::*};
use crate::code_view::{CodeSource, CodeView, CodeViewEvent, EditorSettings, InMemorySource, Pos};
use crate::components::modal::ModalLayer;
use crate::components::picker::Picker;
use crate::components::split::{clamp_width, panel_resize_handle, DraggedPanelEdge};
use crate::components::tree_view::TreeViewEvent;
use crate::components::{
    binding_text_for_action, h_flex, metrics, v_flex, Button, IconButton, Tooltip,
};
use crate::icons::{Icon, IconName, IconSize};
use crate::keymap::contexts;
use crate::theme::{ActiveTheme, ColorExt, ThemeState};

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

/// The workspace root view.
pub struct Workspace {
    root: PathBuf,
    name: SharedString,
    branch: Option<SharedString>,
    sidebar: Entity<Sidebar>,
    sidebar_visible: bool,
    sidebar_width: Pixels,
    center: PaneNode,
    active_pane: Entity<Pane>,
    modal_layer: Entity<ModalLayer>,
    focus_handle: FocusHandle,
    closed: Vec<PathBuf>,
    back: Vec<(PathBuf, Pos)>,
    forward: Vec<(PathBuf, Pos)>,
    files: Arc<Vec<String>>,
    file_meta: HashMap<PathBuf, FileMeta>,
    window_title: String,
    lsp_status: SharedString,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Workspace {
    /// Workspace over the folder `root`.
    pub fn new(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name: SharedString = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| root.display().to_string())
            .into();
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
        subscriptions.push(cx.observe_window_appearance(window, |_, window, cx| {
            ThemeState::sync_with_window(window, cx);
        }));
        ThemeState::sync_with_window(window, cx);

        // Index files in the background for the file finder.
        let walk_root = root.clone();
        let index = cx.background_spawn(async move { walk_project(&walk_root) });
        cx.spawn(async move |this, cx| {
            let files = index.await;
            let _ = this.update(cx, |this, cx| {
                this.files = Arc::new(files);
                cx.notify();
            });
        })
        .detach();

        pane.update(cx, |p, _| p.is_active_pane = true);
        Workspace {
            root,
            name,
            branch,
            sidebar,
            sidebar_visible: true,
            sidebar_width: px(272.),
            center: PaneNode::Pane(pane.clone()),
            active_pane: pane,
            modal_layer,
            focus_handle: cx.focus_handle(),
            closed: Vec::new(),
            back: Vec::new(),
            forward: Vec::new(),
            files: Arc::new(Vec::new()),
            file_meta: HashMap::new(),
            window_title: String::new(),
            lsp_status: "LSP: off".into(),
            _subscriptions: subscriptions,
        }
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
                    cx.notify();
                }
                PaneEvent::ItemClosed(path) => {
                    if let Some(path) = path {
                        this.closed.push(path.clone());
                    }
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
        cx.notify();
    }

    /// The project root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The sidebar.
    pub fn sidebar(&self) -> &Entity<Sidebar> {
        &self.sidebar
    }

    /// The modal layer.
    pub fn modal_layer(&self) -> &Entity<ModalLayer> {
        &self.modal_layer
    }

    /// The active pane.
    pub fn active_pane(&self) -> &Entity<Pane> {
        &self.active_pane
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

    fn active_path(&self, cx: &App) -> Option<PathBuf> {
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
        if path.is_dir() {
            self.reveal_path(&path, cx);
            return;
        }
        if let (Some(current), Some(view)) = (self.active_path(cx), self.active_code_view(cx)) {
            if current != path {
                self.back.push((current, view.read(cx).cursor()));
                self.forward.clear();
            }
        }
        self.open_path_internal(path, preview, window, cx);
    }

    fn open_path_internal(
        &mut self,
        path: PathBuf,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title: SharedString = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .into();
        let pane = self.active_pane.clone();
        if let Some(ix) = pane.read(cx).index_for_path(&path, cx) {
            pane.update(cx, |pane, cx| {
                if !preview {
                    pane.pin(ix, cx);
                }
                pane.activate(ix, true, window, cx);
            });
            cx.notify();
            return;
        }
        let (source, meta) = load_source(&path, &title);
        self.file_meta.insert(path.clone(), meta);
        let view_path = path.clone();
        let view = cx.new(|cx| {
            let mut v = CodeView::new(source, cx);
            v.set_path(Some(view_path));
            v
        });
        self._subscriptions
            .push(cx.subscribe(&view, |_, _, event: &CodeViewEvent, cx| {
                if matches!(event, CodeViewEvent::SelectionChanged) {
                    cx.notify();
                }
            }));
        let item = Box::new(CodeItem::new(view, Some(path), title));
        pane.update(cx, |pane, cx| pane.add_item(item, preview, window, cx));
        cx.notify();
    }

    /// Opens `path` and moves the cursor to 1-based `line`/`col`.
    pub fn open_path_at(
        &mut self,
        path: PathBuf,
        line: Option<u32>,
        col: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_path(path, false, window, cx);
        if let (Some(line), Some(view)) = (line, self.active_code_view(cx)) {
            view.update(cx, |v, cx| {
                let row = line.saturating_sub(1);
                let col = col.map(|c| c.saturating_sub(1) as usize).unwrap_or(0);
                v.set_cursor(Pos::new(row, col), cx);
                v.scroll_to_row(row, true, cx);
            });
        }
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

    fn toggle_file_finder(
        &mut self,
        _: &ToggleFileFinder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let files = self.files.clone();
        let root = self.root.clone();
        let this = cx.entity().downgrade();
        self.toggle_modal(window, cx, move |window, cx| {
            let delegate = FileFinderDelegate::new(root, files, move |path, window, cx| {
                let _ = this.update(cx, |ws, cx| ws.open_path(path, false, window, cx));
            });
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

    fn find(&mut self, _: &editor::Find, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_code_view(cx) else {
            return;
        };
        self.toggle_modal(window, cx, move |window, cx| {
            super::find_bar::FindBar::new(view, window, cx)
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
        let active = self
            .active_pane
            .read(cx)
            .active_item()
            .and_then(|i| i.code_view().map(|v| (v, i.path(cx), i.title(cx))));
        let cloned = active.map(|(view, path, title)| {
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
        });
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

    fn go_back(&mut self, _: &GoBack, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((path, pos)) = self.back.pop() {
            if let (Some(cur), Some(view)) = (self.active_path(cx), self.active_code_view(cx)) {
                self.forward.push((cur, view.read(cx).cursor()));
            }
            self.open_path_internal(path, false, window, cx);
            if let Some(view) = self.active_code_view(cx) {
                view.update(cx, |v, cx| v.set_cursor(pos, cx));
            }
        }
    }

    fn go_forward(&mut self, _: &GoForward, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((path, pos)) = self.forward.pop() {
            if let (Some(cur), Some(view)) = (self.active_path(cx), self.active_code_view(cx)) {
                self.back.push((cur, view.read(cx).cursor()));
            }
            self.open_path_internal(path, false, window, cx);
            if let Some(view) = self.active_code_view(cx) {
                view.update(cx, |v, cx| v.set_cursor(pos, cx));
            }
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

    fn copy_permalink(
        &mut self,
        _: &editor::CopyPermalink,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Placeholder until lector-git provides remotes: path#Lline.
        if let (Some(path), Some(view)) = (self.active_path(cx), self.active_code_view(cx)) {
            let rel = path
                .strip_prefix(&self.root)
                .unwrap_or(&path)
                .display()
                .to_string();
            let line = view.read(cx).cursor().row + 1;
            cx.write_to_clipboard(ClipboardItem::new_string(format!("{rel}#L{line}")));
        }
    }

    fn toggle_blame(&mut self, _: &editor::ToggleBlame, _: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_code_view(cx) else {
            return;
        };
        view.update(cx, |v, cx| {
            let row = v.cursor().row;
            let mut notes = std::collections::BTreeMap::new();
            notes.insert(
                row,
                SharedString::from("Blame unavailable — git integration pending"),
            );
            v.set_annotations(notes, cx);
        });
    }

    fn open_text_tab(
        &mut self,
        title: &str,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.new(|cx| CodeView::from_text(&text, Some(title), cx));
        let item = Box::new(CodeItem::new(
            view,
            None,
            SharedString::from(title.to_string()),
        ));
        self.active_pane
            .update(cx, |p, cx| p.add_item(item, false, window, cx));
    }

    fn open_keymap(&mut self, _: &OpenKeymap, window: &mut Window, cx: &mut Context<Self>) {
        let mut text = String::from("# Lector default key bindings (read-only)\n\n");
        let mut last_ctx: Option<String> = None;
        for spec in crate::keymap::default_keymap() {
            if spec.context != last_ctx {
                text.push_str(&format!(
                    "\n[{}]\n",
                    spec.context.clone().unwrap_or_else(|| "global".into())
                ));
                last_ctx = spec.context.clone();
            }
            text.push_str(&format!("\"{}\" = \"{}\"\n", spec.keys, spec.action));
        }
        self.open_text_tab("keymap.toml", text, window, cx);
    }

    fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        let s = EditorSettings::get(cx);
        let theme = cx.theme().name.clone();
        let text = format!(
            "# Lector settings (preview — editable settings arrive with lector-config)\n\n\
             theme = \"{theme}\"\n\
             font_family = \"{}\"\n\
             font_size = {}\n\
             line_height = {}\n\
             tab_size = {}\n\
             soft_wrap = {}\n\
             minimap = {}\n\
             indent_guides = {}\n\
             sticky_headers = {}\n",
            s.font_family,
            f32::from(s.font_size),
            s.line_height,
            s.tab_size,
            s.soft_wrap,
            s.show_minimap,
            s.indent_guides,
            s.sticky_headers
        );
        self.open_text_tab("settings.toml", text, window, cx);
    }

    fn open_folder(&mut self, _: &OpenFolder, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open Folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                if let Some(root) = paths.into_iter().next() {
                    let _ = this.update_in(cx, |_, window, cx| {
                        let ws = cx.new(|cx| Workspace::new(root, window, cx));
                        window.replace_root(cx, |_, _| WorkspaceRoot { workspace: ws });
                    });
                }
            }
        })
        .detach();
    }

    // Rendering.

    fn render_title_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let csd = matches!(window.window_decorations(), Decorations::Client { .. });
        let mac = cfg!(target_os = "macos");
        let palette_key = binding_text_for_action(&ToggleCommandPalette, cx).unwrap_or_default();
        let finder_key = binding_text_for_action(&ToggleFileFinder, cx).unwrap_or_default();
        let _ = finder_key;
        h_flex()
            .id("title-bar")
            .h(metrics::TITLE_BAR_HEIGHT)
            .flex_none()
            .w_full()
            .px(px(10.))
            .when(mac, |el| el.pl(px(80.)))
            .bg(ui.title_bar_bg.hsla())
            .border_b_1()
            .border_color(ui.border.hsla())
            .text_size(metrics::TEXT)
            .on_mouse_down(MouseButton::Left, move |e, window, _| {
                if e.click_count == 2 {
                    window.titlebar_double_click();
                } else if csd {
                    window.start_window_move();
                }
            })
            .child(
                h_flex()
                    .flex_1()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(20.))
                            .rounded(px(5.))
                            .bg(ui.accent.hsla())
                            .text_color(ui.accent_foreground.hsla())
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .child("L"),
                    )
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(ui.title_bar_fg.hsla())
                            .child(self.name.clone()),
                    )
                    .when_some(self.branch.clone(), |el, branch| {
                        el.child(
                            h_flex()
                                .id("branch-pill")
                                .gap(px(4.))
                                .h(px(22.))
                                .px(px(8.))
                                .rounded(px(11.))
                                .bg(ui.hover.hsla())
                                .border_1()
                                .border_color(ui.border_variant.hsla())
                                .text_size(metrics::TEXT_SM)
                                .text_color(ui.text_muted.hsla())
                                .cursor_pointer()
                                .hover(|s| s.bg(ui.active.hsla()))
                                .tooltip(Tooltip::for_action("Branches", &ShowBranches))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(ShowBranches), cx)
                                })
                                .child(
                                    Icon::new(IconName::GitBranch)
                                        .size(IconSize::XSmall)
                                        .color(ui.text_muted.hsla()),
                                )
                                .child(branch),
                        )
                    }),
            )
            .child(
                h_flex()
                    .id("palette-trigger")
                    .flex_none()
                    .w(px(440.))
                    .h(px(26.))
                    .px(px(10.))
                    .gap(px(8.))
                    .rounded(px(6.))
                    .bg(ui.input_bg.hsla())
                    .border_1()
                    .border_color(ui.border.hsla())
                    .text_size(metrics::TEXT_SM)
                    .text_color(ui.text_faint.hsla())
                    .cursor_pointer()
                    .hover(|s| s.border_color(ui.text_faint.hsla()))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ToggleCommandPalette), cx)
                    })
                    .child(
                        Icon::new(IconName::Search)
                            .size(IconSize::XSmall)
                            .color(ui.text_faint.hsla()),
                    )
                    .child(div().flex_1().child("Search files, symbols, commands…"))
                    .child(div().text_size(metrics::TEXT_XS).child(palette_key)),
            )
            .child(
                h_flex()
                    .flex_1()
                    .justify_end()
                    .gap(px(2.))
                    .child(
                        IconButton::new("toggle-sidebar", IconName::PanelLeft)
                            .icon_size(IconSize::Small)
                            .selected(self.sidebar_visible)
                            .tooltip(Tooltip::for_action("Toggle Sidebar", &ToggleSidebar))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(ToggleSidebar), cx)
                            }),
                    )
                    .child(
                        IconButton::new("split", IconName::Columns)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::for_action("Split Right", &SplitRight))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(SplitRight), cx)
                            }),
                    )
                    .child(
                        IconButton::new(
                            "theme",
                            if cx.theme().appearance.is_dark() {
                                IconName::Sun
                            } else {
                                IconName::Moon
                            },
                        )
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::for_action("Toggle Light/Dark", &ToggleAppearance))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(ToggleAppearance), cx)
                        }),
                    )
                    .when(csd && !mac, |el| {
                        el.child(div().w(px(8.)))
                            .child(
                                IconButton::new("win-min", IconName::Minus)
                                    .icon_size(IconSize::Small)
                                    .on_click(|_, window, _| window.minimize_window()),
                            )
                            .child(
                                IconButton::new("win-max", IconName::Square)
                                    .icon_size(IconSize::XSmall)
                                    .on_click(|_, window, _| window.zoom_window()),
                            )
                            .child(
                                IconButton::new("win-close", IconName::Close)
                                    .icon_size(IconSize::Small)
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(Box::new(CloseWindow), cx)
                                    }),
                            )
                    }),
            )
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let settings = EditorSettings::get(cx);
        let view = self.active_code_view(cx);
        let path = self.active_path(cx);
        let meta = path.as_ref().and_then(|p| self.file_meta.get(p)).cloned();
        let (cursor, language, selection_len) = match &view {
            Some(view) => {
                let v = view.read(cx);
                let (line, col) = v.cursor_line_col(settings.tab_size);
                let sel = v.selection();
                let sel_len = if sel.is_empty() {
                    None
                } else {
                    Some(v.selected_text().chars().count())
                };
                (
                    Some(format!("Ln {line}, Col {col}")),
                    v.source().language_name(),
                    sel_len,
                )
            }
            None => (None, None, None),
        };
        let fg = ui.status_bar_fg.hsla();
        let item =
            |id: &'static str, label: SharedString| Button::new(id, label).compact().color(fg);
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
                    .gap(px(2.))
                    .when_some(self.branch.clone(), |el, branch| {
                        el.child(
                            item("status-branch", branch)
                                .icon(IconName::GitBranch)
                                .tooltip(Tooltip::for_action("Branches", &ShowBranches))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(ShowBranches), cx)
                                }),
                        )
                    })
                    .child(
                        item("status-changes", "Changes".into())
                            .icon(IconName::GitCompare)
                            .tooltip(Tooltip::for_action("Show Changes", &ShowChanges))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(ShowChanges), cx)
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(2.))
                    .when_some(selection_len, |el, n| {
                        el.child(item("status-sel", format!("{n} selected").into()))
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
                        el.child(item("status-indent", m.indentation))
                    })
                    .when_some(meta.clone(), |el, m| {
                        el.child(item("status-encoding", m.encoding))
                    })
                    .when_some(meta, |el, m| el.child(item("status-eol", m.line_ending)))
                    .when_some(language, |el, l| el.child(item("status-language", l)))
                    .child(item("status-lsp", self.lsp_status.clone()).icon(IconName::CircleDot)),
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
                "{} — {} — Lector",
                p.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                self.name
            ),
            None => format!("{} — Lector", self.name),
        };
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }

        let sidebar_width = self.sidebar_width;
        let center = self.center.render(0, cx);
        v_flex()
            .id("workspace")
            .key_context(contexts::WORKSPACE)
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .font_family(crate::assets::UI_FONT)
            .text_size(metrics::TEXT)
            .text_color(ui.text.hsla())
            .bg(ui.background.hsla())
            .on_action(cx.listener(Self::toggle_command_palette))
            .on_action(cx.listener(Self::toggle_file_finder))
            .on_action(cx.listener(Self::select_theme))
            .on_action(cx.listener(Self::go_to_line))
            .on_action(cx.listener(Self::find))
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
            .on_action(cx.listener(|this, _: &ToggleProjectSearch, w, cx| {
                this.show_panel(SidebarPanel::Search, w, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleOutline, w, cx| {
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
            .on_action(cx.listener(Self::go_back))
            .on_action(cx.listener(Self::go_forward))
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
            .on_action(cx.listener(Self::copy_permalink))
            .on_action(cx.listener(Self::toggle_blame))
            .on_action(cx.listener(Self::open_keymap))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(|_, _: &CloseWindow, window, _| window.remove_window()))
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
                                .child(self.sidebar.clone())
                                .child(panel_resize_handle("sidebar-edge", cx)),
                        )
                    })
                    .child(div().flex_1().min_w_0().h_full().child(center)),
            )
            .child(self.render_status_bar(cx))
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
        div().size_full().child(self.workspace.clone())
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
