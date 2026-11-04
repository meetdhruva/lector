//! Workspace ↔ core-crate integration: the project (file list, git status,
//! symbol index, LSP), the file watcher, settings/keymap reload, navigation
//! history, pickers, definitions/references, Markdown preview and session
//! persistence. A child module of `workspace` so it can reach its state.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use gpui::{
    prelude::*, px, App, Axis, ClipboardItem, Entity, Focusable, SharedString, Subscription, Task,
    Window,
};
use lector_config::{PaneState, Session, SessionStore, TabKind, TabState};
use lector_lsp::ServerState;
use lector_search::CancelToken;
use lector_workspace::{FileList, GitStatusMap, Location, NavHistory, Project};

use super::Workspace;
use super::WorkspaceEvent;
use crate::actions::{editor, workspace::*};
use crate::code_view::Pos;
use crate::components::picker::Picker;
use crate::editor::{FileItem, FileView, FileViewEvent, Reveal};
use crate::settings::{self as app_settings, AppSettings};
use crate::views::file_finder::FileFinderDelegate;
use crate::views::file_tree::{DirIndex, TreeCommand};
use crate::views::markdown_preview::{MarkdownPreview, MarkdownPreviewEvent, PreviewItem};
use crate::views::outline_panel::OutlinePanel;
use crate::views::pane::{new_pane, PaneItem, PaneNode};
use crate::views::references::{ReferencesEvent, ReferencesItem, ReferencesView};
use crate::views::search_panel::{SearchPanel, SearchPanelEvent};
use crate::views::sidebar::SidebarPanel;
use crate::views::symbol_picker::{SymbolEntry, SymbolPickerDelegate};

/// Restores a non-file tab from the session (seam for extension crates).
pub type TabRestorer =
    Box<dyn Fn(&TabKind, &mut Window, &mut gpui::Context<Workspace>) -> Option<Box<dyn PaneItem>>>;

/// Integration state owned by the workspace.
pub(crate) struct Integration {
    pub nav: NavHistory,
    pub files: Arc<FileList>,
    pub git_status: Arc<GitStatusMap>,
    pub recent: Vec<PathBuf>,
    pub index_progress: Option<(usize, usize)>,
    pub lsp_status: Option<(SharedString, SharedString)>,
    pub search_panel: Entity<SearchPanel>,
    pub outline_panel: Entity<OutlinePanel>,
    pub restorers: Vec<TabRestorer>,
    pub session_ready: bool,
    pub settings_warnings: usize,
    watcher: Option<lector_fs::Watcher>,
    config_watcher: Option<lector_fs::Watcher>,
    rescan_task: Option<Task<()>>,
    git_task: Option<Task<()>>,
    index_task: Option<Task<()>>,
    session_task: Option<Task<()>>,
    nav_task: Option<Task<()>>,
    tasks: Vec<Task<()>>,
    subscriptions: Vec<Subscription>,
    last_active: Option<PathBuf>,
}

impl Integration {
    pub(crate) fn new(project: &Project, cx: &mut gpui::Context<Workspace>) -> Self {
        let search_panel = cx.new(|cx| SearchPanel::new(project.clone(), cx));
        let outline_panel = cx.new(OutlinePanel::new);
        Integration {
            nav: NavHistory::new(),
            files: Arc::new(FileList::default()),
            git_status: Arc::new(GitStatusMap::default()),
            recent: Vec::new(),
            index_progress: None,
            lsp_status: None,
            search_panel,
            outline_panel,
            restorers: Vec::new(),
            session_ready: false,
            settings_warnings: 0,
            watcher: None,
            config_watcher: None,
            rescan_task: None,
            git_task: None,
            index_task: None,
            session_task: None,
            nav_task: None,
            tasks: Vec::new(),
            subscriptions: Vec::new(),
            last_active: None,
        }
    }
}

/// Converts a code-view position to a navigation location.
fn location(path: PathBuf, pos: Pos) -> Location {
    Location {
        path,
        point: lector_text::Point::new(pos.row, pos.col as u32),
    }
}

impl Workspace {
    /// The project shown in this workspace.
    pub fn project(&self) -> &Project {
        &self.project
    }

    /// Latest file list snapshot.
    pub fn file_list(&self) -> &Arc<FileList> {
        &self.integ.files
    }

    /// Latest git status snapshot.
    pub fn git_status(&self) -> &Arc<GitStatusMap> {
        &self.integ.git_status
    }

    /// Registers a restorer for non-file tabs saved in the session (e.g.
    /// diffs): return `Some(item)` for kinds you own.
    pub fn register_tab_restorer(
        &mut self,
        f: impl Fn(&TabKind, &mut Window, &mut gpui::Context<Workspace>) -> Option<Box<dyn PaneItem>>
            + 'static,
    ) {
        self.integ.restorers.push(Box::new(f));
    }

    /// The file view of the active item, if any.
    pub fn active_file_view(&self, cx: &App) -> Option<Entity<FileView>> {
        self.active_pane
            .read(cx)
            .active_item()
            .and_then(|i| i.file_view())
    }

    /// All open file views (every pane).
    pub fn file_views(&self, cx: &App) -> Vec<Entity<FileView>> {
        self.center
            .panes()
            .iter()
            .flat_map(|p| {
                p.read(cx)
                    .items()
                    .filter_map(|i| i.file_view())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    // Startup.

    pub(super) fn start_integration(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let search = self.integ.search_panel.clone();
        let outline = self.integ.outline_panel.clone();
        self.sidebar.update(cx, |s, cx| {
            s.set_panel_view(SidebarPanel::Search, search.clone().into(), cx);
            s.set_panel_view(SidebarPanel::Outline, outline.into(), cx);
        });
        let sub = cx.subscribe_in(
            &search,
            window,
            |this, _, event: &SearchPanelEvent, window, cx| match event {
                SearchPanelEvent::Open {
                    path,
                    row,
                    cols,
                    preview,
                } => {
                    let reveal = Reveal {
                        row: *row,
                        cols: cols.clone(),
                        center: true,
                    };
                    this.open_path_with(path.clone(), *preview, Some(reveal), true, window, cx);
                }
            },
        );
        self.integ.subscriptions.push(sub);
        let weak = cx.entity().downgrade();
        let tree = self.sidebar.read(cx).file_tree().clone();
        tree.update(cx, |t, _| {
            t.delegate
                .set_command_handler(move |command, path, window, cx| {
                    let _ = weak.update(cx, |ws, cx| ws.tree_command(command, path, window, cx));
                })
        });
        self.integ.subscriptions.push(cx.subscribe_in(
            &cx.entity(),
            window,
            |this, _, event: &WorkspaceEvent, window, cx| {
                if *event == WorkspaceEvent::ActiveItemChanged {
                    this.active_item_changed(window, cx);
                }
            },
        ));
        if !self.has_folder {
            // No folder: nothing to scan, index or watch (the placeholder root
            // is empty); only Lector's own config dir is watched.
            if lector_config::paths::config_dir().is_dir() {
                self.start_config_watcher_only(cx);
            }
            return;
        }
        self.rescan_files(true, cx);
        self.refresh_git_status(cx);
        self.start_watchers(cx);
        self.start_lsp_status(cx);
        cx.on_app_quit(|this, cx| {
            this.save_session_now(cx);
            async {}
        })
        .detach();
    }

    pub(super) fn rescan_files(&mut self, index_symbols: bool, cx: &mut gpui::Context<Self>) {
        let project = self.project.clone();
        self.integ.rescan_task = Some(cx.spawn(async move |this, cx| {
            let (files, index) = cx
                .background_executor()
                .spawn(async move {
                    let files = project.rescan_files(&CancelToken::new());
                    let index = Arc::new(DirIndex::from_list(&files));
                    (files, index)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.integ.files = files;
                let tree = this.sidebar.read(cx).file_tree().clone();
                tree.update(cx, |t, cx| {
                    t.delegate.set_index(index);
                    t.refresh(cx);
                });
                if let Some(active) = this.active_path(cx) {
                    this.auto_reveal(&active, cx);
                }
                if index_symbols {
                    this.index_symbols(cx);
                }
                cx.notify();
            });
        }));
    }

    fn index_symbols(&mut self, cx: &mut gpui::Context<Self>) {
        let project = self.project.clone();
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<(usize, usize)>();
        self.integ.index_progress = Some((0, 0));
        let worker = cx.background_executor().spawn(async move {
            project.index_symbols(&CancelToken::new(), |done, total| {
                let _ = tx.unbounded_send((done, total));
            });
        });
        self.integ.index_task = Some(cx.spawn(async move |this, cx| {
            let progress = async {
                while let Some(p) = rx.next().await {
                    if this
                        .update(cx, |this, cx| {
                            this.integ.index_progress = Some(p);
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            };
            futures::join!(worker, progress);
            let _ = this.update(cx, |this, cx| {
                this.integ.index_progress = None;
                cx.notify();
            });
        }));
    }

    pub(crate) fn refresh_git_status(&mut self, cx: &mut gpui::Context<Self>) {
        let project = self.project.clone();
        self.integ.git_task = Some(cx.spawn(async move |this, cx| {
            let status = cx
                .background_executor()
                .spawn(async move { project.refresh_git_status() })
                .await;
            let status = match status {
                Ok(s) => s,
                Err(err) => {
                    tracing::warn!("git status: {err}");
                    return;
                }
            };
            let _ = this.update(cx, |this, cx| {
                this.integ.git_status = status.clone();
                let tree = this.sidebar.read(cx).file_tree().clone();
                tree.update(cx, |t, cx| {
                    t.delegate.set_git_status(status);
                    t.refresh(cx);
                });
                cx.notify();
            });
        }));
    }

    fn start_watchers(&mut self, cx: &mut gpui::Context<Self>) {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<lector_fs::WatchEvent>();
        // Registering recursive watches walks the whole tree: off the main thread.
        let root = self.project.root().to_path_buf();
        let setup = cx.background_executor().spawn(async move {
            lector_fs::watch(&root, Duration::from_millis(120), move |ev| {
                let _ = tx.unbounded_send(ev);
            })
        });
        self.integ.tasks.push(cx.spawn(async move |this, cx| {
            let watcher = setup.await;
            let ok = this.update(cx, |this, _| match watcher {
                Ok(w) => this.integ.watcher = Some(w),
                Err(err) => tracing::warn!("file watcher unavailable: {err}"),
            });
            if ok.is_err() {
                return;
            }
            while let Some(event) = rx.next().await {
                let mut events = vec![event];
                while let Ok(e) = rx.try_recv() {
                    events.push(e);
                }
                if this
                    .update(cx, |this, cx| {
                        for e in events {
                            this.handle_watch_event(e, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));

        // Lector's own config dir: live-reload settings, keymap and themes.
        if lector_config::paths::config_dir().is_dir() {
            self.start_config_watcher_only(cx);
        }
        // Project settings (.lector/settings.toml) are covered by the project watcher.
    }

    fn handle_watch_event(&mut self, event: lector_fs::WatchEvent, cx: &mut gpui::Context<Self>) {
        match event {
            lector_fs::WatchEvent::Files(events) => {
                let mut structural = false;
                let mut changed: Vec<PathBuf> = Vec::new();
                let index = self
                    .sidebar
                    .read(cx)
                    .file_tree()
                    .read(cx)
                    .delegate
                    .index()
                    .clone();
                for ev in &events {
                    let rel = self.project.relative_path(&ev.path);
                    let Some(rel) = rel else { continue };
                    if rel.starts_with(".git/") || rel == ".git" {
                        continue;
                    }
                    let parent = rel.rsplit_once('/').map_or("", |(p, _)| p);
                    let known_dir = parent.is_empty() || !index.children(parent).is_empty();
                    let known_file = self.integ.files.contains(&rel);
                    if !known_dir && !known_file {
                        // Inside an ignored or excluded folder (e.g. target/).
                        continue;
                    }
                    if matches!(ev.kind, lector_fs::FsEventKind::Removed) && !known_file {
                        // A path we never listed (e.g. an editor's temp file) went away.
                        continue;
                    }
                    if !matches!(ev.kind, lector_fs::FsEventKind::Modified) || !known_file {
                        structural = true;
                    }
                    if rel == ".lector/settings.toml" {
                        self.reload_settings(cx);
                    }
                    changed.push(ev.path.clone());
                }
                if changed.is_empty() {
                    return;
                }
                let set: HashSet<&PathBuf> = changed.iter().collect();
                for view in self.file_views(cx) {
                    if set.contains(&view.read(cx).path().to_path_buf()) {
                        view.update(cx, |v, cx| v.reload(cx));
                    }
                }
                self.reload_previews(&set, cx);
                let project = self.project.clone();
                let reindex = changed.clone();
                cx.background_executor()
                    .spawn(async move {
                        for path in reindex {
                            project.reindex_file(&path);
                        }
                    })
                    .detach();
                if structural {
                    self.rescan_files(false, cx);
                }
                self.refresh_git_status(cx);
                cx.emit(WorkspaceEvent::FilesChanged(changed));
            }
            lector_fs::WatchEvent::GitStateChanged => {
                self.refresh_git_status(cx);
                for view in self.file_views(cx) {
                    view.update(cx, |v, cx| v.refresh_gutter(cx));
                }
                cx.emit(WorkspaceEvent::GitStateChanged);
            }
            lector_fs::WatchEvent::Rescan => {
                self.rescan_files(false, cx);
                self.refresh_git_status(cx);
                for view in self.file_views(cx) {
                    view.update(cx, |v, cx| v.reload(cx));
                }
                cx.emit(WorkspaceEvent::GitStateChanged);
            }
        }
    }

    fn start_lsp_status(&mut self, cx: &mut gpui::Context<Self>) {
        self.integ.tasks.push(cx.spawn(async move |this, cx| loop {
            let alive = this.update(cx, |this, cx| {
                let status = this.compute_lsp_status(cx);
                if status != this.integ.lsp_status {
                    this.integ.lsp_status = status;
                    cx.notify();
                }
            });
            if alive.is_err() {
                break;
            }
            cx.background_executor()
                .timer(Duration::from_millis(1000))
                .await;
        }));
    }

    fn compute_lsp_status(&self, cx: &App) -> Option<(SharedString, SharedString)> {
        let lsp = self.project.lsp()?;
        let statuses = lsp.status();
        let active = self.active_file_view(cx);
        let language = active
            .as_ref()
            .and_then(|v| v.read(cx).document(cx).and_then(|d| d.language().cloned()));
        let relevant: Vec<_> = match &language {
            Some(lang) => {
                let servers: Vec<String> = lector_lsp::builtin_servers()
                    .into_iter()
                    .filter(|c| c.languages.iter().any(|l| l == &lang.name))
                    .map(|c| c.name)
                    .collect();
                statuses
                    .iter()
                    .filter(|s| servers.contains(&s.name))
                    .cloned()
                    .collect()
            }
            None => statuses.clone(),
        };
        let status = relevant.first()?;
        let (label, tip) = match &status.state {
            ServerState::Running => (
                format!("{} ✓", status.name),
                format!("{} is running", status.name),
            ),
            ServerState::Starting => (
                format!("{} starting…", status.name),
                format!("{} is starting", status.name),
            ),
            ServerState::Failed(err) => (format!("{} ✗", status.name), format!("{} failed: {err}", status.name)),
            ServerState::NotInstalled => (
                format!("{}: not installed", status.name),
                format!(
                    "Install {} and put it on PATH for hover, definitions and references (tags are used meanwhile)",
                    status.name
                ),
            ),
        };
        Some((label.into(), tip.into()))
    }

    // Opening files.

    /// Opens `path`, optionally revealing a location. `record` pushes the
    /// current location on the navigation history first.
    pub fn open_path_with(
        &mut self,
        path: PathBuf,
        preview: bool,
        reveal: Option<Reveal>,
        record: bool,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Option<Entity<FileView>> {
        if path.is_dir() {
            self.reveal_path(&path, cx);
            return None;
        }
        if record {
            self.record_location(cx);
        }
        let view = self.open_file_view(path, preview, window, cx);
        if let Some(reveal) = reveal {
            view.update(cx, |v, cx| v.reveal(reveal, cx));
        }
        Some(view)
    }

    fn open_file_view(
        &mut self,
        path: PathBuf,
        preview: bool,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Entity<FileView> {
        let pane = self.active_pane.clone();
        if let Some(ix) = pane.read(cx).index_for_path(&path, cx) {
            pane.update(cx, |pane, cx| {
                if !preview {
                    pane.pin(ix, cx);
                }
                pane.activate(ix, true, window, cx);
            });
            if let Some(view) = pane
                .read(cx)
                .active_item()
                .and_then(|item| item.file_view())
            {
                cx.notify();
                return view;
            }
        }
        let project = self.project.clone();
        let view = cx.new(|cx| FileView::new(path, project, cx));
        self.subscribe_file_view(&view, window, cx);
        let item = Box::new(FileItem::new(view.clone(), cx));
        pane.update(cx, |pane, cx| pane.add_item(item, preview, window, cx));
        self.schedule_session_save(cx);
        cx.notify();
        view
    }

    pub(super) fn subscribe_file_view(
        &mut self,
        view: &Entity<FileView>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let sub = cx.subscribe_in(view, window, |this, view, event: &FileViewEvent, _, cx| {
            let active = this.active_file_view(cx).as_ref() == Some(view);
            match event {
                FileViewEvent::Loaded => {
                    if active {
                        let v = view.clone();
                        this.integ
                            .outline_panel
                            .update(cx, |p, cx| p.set_file_view(Some(v), cx));
                    }
                    this.warm_up_lsp(view, cx);
                }
                FileViewEvent::CursorMoved => {
                    if active {
                        this.integ
                            .outline_panel
                            .update(cx, |p, cx| p.follow_cursor(cx));
                        this.schedule_nav_record(cx);
                    }
                }
                FileViewEvent::OutlineChanged => {
                    if active {
                        let v = view.clone();
                        this.integ
                            .outline_panel
                            .update(cx, |p, cx| p.set_file_view(Some(v), cx));
                    }
                }
                FileViewEvent::StateChanged | FileViewEvent::PathChanged => {
                    // Tabs show the dirty dot / lock: repaint the panes. An
                    // edited preview tab becomes permanent (never replaced).
                    let dirty = view.read(cx).is_dirty(cx);
                    let path = view.read(cx).path().to_path_buf();
                    for pane in this.center.panes() {
                        pane.update(cx, |p, cx| {
                            if dirty {
                                if let Some(ix) = p.index_for_path(&path, cx) {
                                    if p.is_preview(ix) {
                                        p.pin(ix, cx);
                                    }
                                }
                            }
                            cx.notify()
                        });
                    }
                    if *event == FileViewEvent::PathChanged {
                        this.schedule_session_save(cx);
                    }
                }
            }
            cx.notify();
        });
        self.integ.subscriptions.push(sub);
    }

    /// Opens the document with the language server so it is warm for hover
    /// and navigation (idempotent per text; adapter until lector-workspace
    /// exposes it).
    fn warm_up_lsp(&self, view: &Entity<FileView>, cx: &mut gpui::Context<Self>) {
        let Some(lsp) = self.project.lsp().cloned() else {
            return;
        };
        let Some(doc) = view.read(cx).live_document(cx).cloned() else {
            return;
        };
        let (Some(lang), Some(text)) = (doc.language().cloned(), doc.text().cloned()) else {
            return;
        };
        if text.is_mapped() || doc.is_large() {
            return;
        }
        let path = doc.path().to_path_buf();
        cx.background_executor()
            .spawn(async move { lsp.open_document(&path, &lang.name, &text.to_string()) })
            .detach();
    }

    fn active_item_changed(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) {
        let view = self.active_file_view(cx);
        let path = view.as_ref().map(|v| v.read(cx).path().to_path_buf());
        if path != self.integ.last_active {
            let previous = self.integ.last_active.as_ref().and_then(|prev| {
                self.all_file_views(cx)
                    .into_iter()
                    .find(|v| v.read(cx).path() == prev.as_path())
            });
            self.active_file_changed(previous, cx);
            if let Some(p) = &path {
                self.integ.recent.retain(|r| r != p);
                self.integ.recent.insert(0, p.clone());
                self.integ.recent.truncate(100);
            }
            if let Some(p) = &path {
                self.auto_reveal(p, cx);
            }
            self.integ.last_active = path;
            self.integ
                .outline_panel
                .update(cx, |p, cx| p.set_file_view(view, cx));
        }
        self.schedule_session_save(cx);
    }

    /// Selects `path` in the file tree (expanding its folders) without
    /// switching panels or moving focus.
    fn auto_reveal(&mut self, path: &std::path::Path, cx: &mut gpui::Context<Self>) {
        let tree = self.sidebar.read(cx).file_tree().clone();
        let ancestors = tree.read(cx).delegate.ancestors_of(path);
        if ancestors.is_empty() {
            return;
        }
        let target = path.to_path_buf();
        tree.update(cx, |t, cx| t.reveal(&ancestors, &target, cx));
    }

    // Navigation history.

    fn current_location(&self, cx: &App) -> Option<Location> {
        let view = self.active_file_view(cx)?;
        let v = view.read(cx);
        let pos = v.code_view().read(cx).cursor();
        Some(location(v.path().to_path_buf(), pos))
    }

    /// Pushes the current location on the back stack (before a jump).
    pub fn record_location(&mut self, cx: &App) {
        if let Some(loc) = self.current_location(cx) {
            self.integ.nav.push(loc);
        }
    }

    /// Big cursor moves within a file (≥ 20 rows) are recorded after the
    /// cursor rests, so GoBack returns to where reading was interrupted.
    fn schedule_nav_record(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(start) = self.current_location(cx) else {
            return;
        };
        if self.integ.nav_task.is_some() {
            return;
        }
        self.integ.nav_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(800))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.integ.nav_task = None;
                if let Some(now) = this.current_location(cx) {
                    if now.path == start.path && now.point.row.abs_diff(start.point.row) >= 20 {
                        this.integ.nav.push(start);
                    }
                }
            });
        }));
    }

    fn go_to_location(&mut self, loc: Location, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let reveal = Reveal {
            row: loc.point.row,
            cols: loc.point.column as usize..loc.point.column as usize,
            center: true,
        };
        self.open_path_with(loc.path, false, Some(reveal), false, window, cx);
    }

    pub(super) fn nav_back(
        &mut self,
        _: &GoBack,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(current) = self.current_location(cx) else {
            return;
        };
        if let Some(loc) = self.integ.nav.back(current) {
            self.go_to_location(loc, window, cx);
        }
    }

    pub(super) fn nav_forward(
        &mut self,
        _: &GoForward,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(current) = self.current_location(cx) else {
            return;
        };
        if let Some(loc) = self.integ.nav.forward(current) {
            self.go_to_location(loc, window, cx);
        }
    }

    // Pickers.

    pub(super) fn toggle_file_finder(
        &mut self,
        _: &ToggleFileFinder,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let root = self.project.root().to_path_buf();
        let files = self.integ.files.clone();
        let active = self.active_path(cx);
        let recent: Vec<Arc<str>> = self
            .integ
            .recent
            .iter()
            .filter(|p| Some(*p) != active.as_ref())
            .filter_map(|p| self.project.relative_path(p))
            .map(Arc::from)
            .collect();
        let status = self.integ.git_status.clone();
        let this = cx.entity().downgrade();
        self.toggle_modal(window, cx, move |window, cx| {
            let delegate = FileFinderDelegate::new(
                root,
                files,
                recent,
                status,
                move |path, line, window, cx| {
                    let reveal = line.map(|(l, c)| {
                        let col = c.unwrap_or(1).saturating_sub(1) as usize;
                        Reveal {
                            row: l.saturating_sub(1),
                            cols: col..col,
                            center: true,
                        }
                    });
                    let _ = this.update(cx, |ws, cx| {
                        ws.open_path_with(path, false, reveal, true, window, cx);
                    });
                },
            );
            Picker::new(delegate, window, cx)
        });
    }

    fn open_symbol_entry(
        &mut self,
        entry: SymbolEntry,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let reveal = Reveal {
            row: entry.row,
            cols: entry.cols,
            center: true,
        };
        self.open_path_with(entry.path, false, Some(reveal), true, window, cx);
    }

    pub(super) fn toggle_outline_picker(
        &mut self,
        _: &ToggleOutline,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(view) = self.active_file_view(cx) else {
            return;
        };
        let v = view.read(cx);
        let Some(doc) = v.document(cx).cloned() else {
            return;
        };
        let Some(text) = doc.text() else { return };
        let path = v.path().to_path_buf();
        let cursor_row = v.code_view().read(cx).cursor().row;
        let entries: Vec<SymbolEntry> = doc
            .outline()
            .iter()
            .map(|item| {
                let p = text.offset_to_point(item.name_range.start);
                let end = text.offset_to_point(item.name_range.end);
                SymbolEntry {
                    name: item.name.clone().into(),
                    kind: Some(item.kind),
                    detail: SharedString::from(item.kind.label()),
                    depth: item.depth,
                    path: path.clone(),
                    row: p.row,
                    cols: p.column as usize..if end.row == p.row {
                        end.column as usize
                    } else {
                        p.column as usize
                    },
                }
            })
            .collect();
        let this = cx.entity().downgrade();
        self.toggle_modal(window, cx, move |window, cx| {
            let delegate = SymbolPickerDelegate::new(
                "Go to symbol in file…",
                entries,
                true,
                move |entry, window, cx| {
                    let _ = this.update(cx, |ws, cx| ws.open_symbol_entry(entry, window, cx));
                },
            )
            .with_empty_message("No symbols in this file");
            let mut picker = Picker::new(delegate, window, cx);
            picker.delegate.select_row(cursor_row);
            picker.scroll_to_selected();
            picker
        });
    }

    pub(super) fn toggle_symbol_finder(
        &mut self,
        _: &ToggleSymbolFinder,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let project = self.project.clone();
        let indexing = self.integ.index_progress.is_some();
        let this = cx.entity().downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let root = project.root().to_path_buf();
            let entries = cx
                .background_executor()
                .spawn(async move {
                    lector_workspace::symbols::project_symbols(&project)
                        .into_iter()
                        .map(|s| {
                            let rel = s
                                .path
                                .strip_prefix(&root)
                                .unwrap_or(&s.path)
                                .display()
                                .to_string();
                            SymbolEntry {
                                name: s.name.into(),
                                kind: Some(s.kind),
                                detail: rel.into(),
                                depth: 0,
                                path: s.path,
                                row: s.point.row,
                                cols: s.point.column as usize..s.point.column as usize,
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update_in(cx, |ws, window, cx| {
                let weak = cx.entity().downgrade();
                ws.toggle_modal(window, cx, move |window, cx| {
                    let delegate = SymbolPickerDelegate::new(
                        "Go to symbol in project…",
                        entries,
                        false,
                        move |entry, window, cx| {
                            let _ =
                                weak.update(cx, |ws, cx| ws.open_symbol_entry(entry, window, cx));
                        },
                    )
                    .loading(indexing);
                    Picker::new(delegate, window, cx)
                });
            });
        })
        .detach();
    }

    // Definitions, references.

    fn active_doc_offset(&self, cx: &App) -> Option<(lector_workspace::Document, usize)> {
        let view = self.active_file_view(cx)?;
        let v = view.read(cx);
        let doc = v.live_document(cx)?.clone();
        let offset = v.cursor_offset(cx)?;
        Some((doc, offset))
    }

    pub(super) fn go_to_definition(
        &mut self,
        _: &editor::GoToDefinition,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some((doc, offset)) = self.active_doc_offset(cx) else {
            return;
        };
        let project = self.project.clone();
        cx.spawn_in(window, async move |this, cx| {
            let targets = cx
                .background_executor()
                .spawn(
                    async move { lector_workspace::symbols::definitions(&project, &doc, offset) },
                )
                .await
                .unwrap_or_default();
            let _ = this.update_in(cx, |ws, window, cx| {
                ws.show_targets(targets, "Definitions", window, cx)
            });
        })
        .detach();
    }

    fn show_targets(
        &mut self,
        targets: Vec<lector_workspace::Target>,
        title: &str,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        match targets.len() {
            0 => {
                self.flash_status(format!("No {} found", title.to_lowercase()), cx);
            }
            1 => {
                let t = &targets[0];
                let reveal = Reveal {
                    row: t.range.start.row,
                    cols: t.range.start.column as usize..if t.range.end.row == t.range.start.row {
                        t.range.end.column as usize
                    } else {
                        t.range.start.column as usize
                    },
                    center: true,
                };
                self.open_path_with(t.path.clone(), false, Some(reveal), true, window, cx);
            }
            _ => {
                let root = self.project.root().to_path_buf();
                let entries: Vec<SymbolEntry> = targets
                    .into_iter()
                    .map(|t| {
                        let rel = t
                            .path
                            .strip_prefix(&root)
                            .unwrap_or(&t.path)
                            .display()
                            .to_string();
                        SymbolEntry {
                            name: format!("{rel}:{}", t.range.start.row + 1).into(),
                            kind: None,
                            detail: t.preview.trim().to_string().into(),
                            depth: 0,
                            path: t.path,
                            row: t.range.start.row,
                            cols: t.range.start.column as usize..t.range.start.column as usize,
                        }
                    })
                    .collect();
                let weak = cx.entity().downgrade();
                let placeholder = format!("{title}…");
                self.toggle_modal(window, cx, move |window, cx| {
                    let delegate = SymbolPickerDelegate::new(
                        placeholder,
                        entries,
                        true,
                        move |entry, window, cx| {
                            let _ =
                                weak.update(cx, |ws, cx| ws.open_symbol_entry(entry, window, cx));
                        },
                    );
                    Picker::new(delegate, window, cx)
                });
            }
        }
    }

    pub(super) fn find_references(
        &mut self,
        _: &editor::FindReferences,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some((doc, offset)) = self.active_doc_offset(cx) else {
            return;
        };
        let word = doc.word_at(offset).map(|(_, w)| w).unwrap_or_default();
        if word.is_empty() {
            return;
        }
        let project = self.project.clone();
        let view = cx.new(|cx| ReferencesView::new(word.clone(), project.clone(), cx));
        let sub = cx.subscribe_in(
            &view,
            window,
            |this, _, event: &ReferencesEvent, window, cx| match event {
                ReferencesEvent::Open { path, row, cols } => {
                    let reveal = Reveal {
                        row: *row,
                        cols: cols.clone(),
                        center: true,
                    };
                    this.open_path_in_other_pane(path.clone(), reveal, window, cx);
                }
            },
        );
        self.integ.subscriptions.push(sub);
        view.update(cx, |v, cx| v.load(doc, offset, cx));
        self.active_pane.update(cx, |p, cx| {
            p.add_item(Box::new(ReferencesItem::new(view, word)), false, window, cx)
        });
    }

    /// Opens a location from a results tab: in another pane if there is one,
    /// else in the active pane.
    fn open_path_in_other_pane(
        &mut self,
        path: PathBuf,
        reveal: Reveal,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let panes = self.center.panes();
        if let Some(other) = panes.iter().find(|p| **p != self.active_pane).cloned() {
            self.set_active_pane(other, cx);
        }
        self.open_path_with(path, true, Some(reveal), true, window, cx);
    }

    // Markdown preview.

    pub(super) fn toggle_markdown_preview(
        &mut self,
        _: &editor::ToggleMarkdownPreview,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(view) = self.active_file_view(cx) else {
            return;
        };
        let (path, is_md) = {
            let v = view.read(cx);
            let is_md = v
                .document(cx)
                .and_then(|d| d.language())
                .is_some_and(|l| l.name == "Markdown")
                || v.path().extension().is_some_and(|e| {
                    e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown")
                });
            (v.path().to_path_buf(), is_md)
        };
        if !is_md {
            self.flash_status("Markdown preview is available for Markdown files", cx);
            return;
        }
        let key: SharedString = format!("markdown-preview:{}", path.display()).into();
        // Toggle: close an open preview of this file.
        for pane in self.center.panes() {
            if let Some(ix) = pane.read(cx).index_for_key(&key, cx) {
                pane.update(cx, |p, cx| p.close(ix, window, cx));
                return;
            }
        }
        let project = self.project.clone();
        let preview = cx.new(|cx| MarkdownPreview::new(path.clone(), project, cx));
        let code_view = view.read(cx).code_view().clone();
        preview.update(cx, |p, cx| p.follow(code_view, cx));
        let sub = cx.subscribe_in(
            &preview,
            window,
            |this, _, event: &MarkdownPreviewEvent, window, cx| match event {
                MarkdownPreviewEvent::OpenFile(path) => {
                    this.open_path_with(path.clone(), false, None, true, window, cx);
                }
            },
        );
        self.integ.subscriptions.push(sub);
        let item = Box::new(PreviewItem::new(preview, key));
        // Side by side: reuse a pane to the right, else split.
        let source_pane = self.active_pane.clone();
        let target = self.center.panes().into_iter().find(|p| *p != source_pane);
        let target = match target {
            Some(p) => p,
            None => {
                let new = new_pane(Some(self.root.clone()), cx);
                self.center
                    .split(&source_pane, new.clone(), Axis::Horizontal, cx);
                let sub = Self::subscribe_pane(&new, window, cx);
                self._subscriptions.push(sub);
                new
            }
        };
        target.update(cx, |p, cx| p.add_item_in_background(item, cx));
        // Keep reading the source.
        self.set_active_pane(source_pane.clone(), cx);
        let focus = source_pane.focus_handle(cx);
        window.focus(&focus);
        cx.notify();
    }

    fn reload_previews(&mut self, changed: &HashSet<&PathBuf>, cx: &mut gpui::Context<Self>) {
        for pane in self.center.panes() {
            let previews: Vec<_> = pane
                .read(cx)
                .items()
                .filter_map(|i| i.view().downcast::<MarkdownPreview>().ok())
                .collect();
            for preview in previews {
                if changed.contains(&preview.read(cx).path().to_path_buf()) {
                    preview.update(cx, |p, cx| p.reload(cx));
                }
            }
        }
    }

    // Project search.

    pub(super) fn toggle_project_search(
        &mut self,
        _: &ToggleProjectSearch,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let selected = self
            .active_code_view(cx)
            .map(|v| v.read(cx).selected_text())
            .filter(|s| !s.is_empty() && !s.contains('\n'));
        self.sidebar_visible = true;
        self.sidebar
            .update(cx, |s, cx| s.set_active_panel(SidebarPanel::Search, cx));
        let panel = self.integ.search_panel.clone();
        panel.update(cx, |p, cx| {
            if let Some(text) = selected {
                p.set_query(&text, window, cx);
            }
            p.focus_query(window, cx);
        });
        cx.notify();
    }

    fn tree_command(
        &mut self,
        command: TreeCommand,
        path: PathBuf,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        match command {
            TreeCommand::Open => {
                self.open_path_with(path, false, None, true, window, cx);
            }
            TreeCommand::OpenToSide => {
                let source = self.active_pane.clone();
                let target = self.center.panes().into_iter().find(|p| *p != source);
                let target = match target {
                    Some(t) => t,
                    None => {
                        let new = new_pane(Some(self.root.clone()), cx);
                        self.center
                            .split(&source, new.clone(), Axis::Horizontal, cx);
                        let sub = Self::subscribe_pane(&new, window, cx);
                        self._subscriptions.push(sub);
                        new
                    }
                };
                self.set_active_pane(target, cx);
                self.open_path_with(path, false, None, true, window, cx);
            }
            TreeCommand::RevealInOs => crate::os::reveal_in_file_manager(&path),
            TreeCommand::FindInFolder => {
                let rel = self.project.relative_path(&path).unwrap_or_default();
                self.sidebar_visible = true;
                self.sidebar
                    .update(cx, |s, cx| s.set_active_panel(SidebarPanel::Search, cx));
                let panel = self.integ.search_panel.clone();
                panel.update(cx, |p, cx| {
                    p.set_include(
                        &if rel.is_empty() {
                            String::new()
                        } else {
                            format!("{rel}/**")
                        },
                        window,
                        cx,
                    );
                    p.focus_query(window, cx);
                });
            }
            TreeCommand::CollapseAll => {
                let tree = self.sidebar.read(cx).file_tree().clone();
                tree.update(cx, |t, cx| {
                    t.collapse_all(cx);
                    let root = self.project.root().to_path_buf();
                    t.set_expanded(&root, true, cx);
                });
            }
        }
        cx.notify();
    }

    // Settings & keymap.

    pub(super) fn open_settings_file(
        &mut self,
        _: &OpenSettings,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let path = app_settings::ensure_settings_file();
        self.open_config_file(path, window, cx);
    }

    pub(super) fn open_keymap_file(
        &mut self,
        _: &OpenKeymap,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let path = app_settings::ensure_keymap_file();
        self.open_config_file(path, window, cx);
    }

    fn open_config_file(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        // Watch the config dir now that it surely exists.
        if self.integ.config_watcher.is_none() {
            self.start_config_watcher_only(cx);
        }
        self.open_path_with(path, false, None, true, window, cx);
    }

    pub(super) fn start_config_watcher_only(&mut self, cx: &mut gpui::Context<Self>) {
        let config_dir = lector_config::paths::config_dir();
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<lector_fs::WatchEvent>();
        match lector_fs::watch(&config_dir, Duration::from_millis(200), move |ev| {
            let _ = tx.unbounded_send(ev);
        }) {
            Ok(w) => self.integ.config_watcher = Some(w),
            Err(err) => {
                tracing::warn!("config watcher unavailable: {err}");
                return;
            }
        }
        self.integ.tasks.push(cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.next().await {
                let paths: Vec<PathBuf> = match ev {
                    lector_fs::WatchEvent::Files(evs) => evs.into_iter().map(|e| e.path).collect(),
                    _ => Vec::new(),
                };
                if this
                    .update(cx, |this, cx| {
                        this.reload_settings(cx);
                        let set: HashSet<&PathBuf> = paths.iter().collect();
                        for view in this.file_views(cx) {
                            if set.contains(&view.read(cx).path().to_path_buf()) {
                                view.update(cx, |v, cx| v.reload(cx));
                            }
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    /// Reloads settings, keymap and user themes, and applies them.
    pub fn reload_settings(&mut self, cx: &mut gpui::Context<Self>) {
        let (settings, warnings) = lector_config::load_settings_for(Some(self.project.root()));
        self.integ.settings_warnings = warnings.len();
        self.project.set_settings(settings.clone());
        let mut registry = crate::theme::ThemeState::registry_clone(cx);
        let (_, errors) = registry.load_dir(&lector_config::paths::themes_dir());
        for err in errors {
            tracing::warn!("theme: {err}");
        }
        crate::theme::ThemeState::replace_registry(cx, registry);
        app_settings::apply_settings(cx, settings, warnings);
        app_settings::bind_effective_keymap(cx);
        for view in self.file_views(cx) {
            view.update(cx, |v, cx| v.refresh_gutter(cx));
        }
        cx.emit(WorkspaceEvent::SettingsChanged);
        cx.notify();
    }

    pub(super) fn reload_settings_action(
        &mut self,
        _: &ReloadSettings,
        _: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.reload_settings(cx);
        self.flash_status("Settings reloaded", cx);
    }

    pub(super) fn copy_permalink_action(
        &mut self,
        _: &editor::CopyPermalink,
        _: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let (Some(view), Some(repo)) = (self.active_file_view(cx), self.project.repo().cloned())
        else {
            return;
        };
        let v = view.read(cx);
        let Some(rel) = self.project.relative_path(v.path()) else {
            return;
        };
        let sel = v.code_view().read(cx).selection();
        let (s, e) = sel.range();
        let lines = s.row..e.row + 1;
        let repo_path = self.project.repo_path(&rel);
        match repo.permalink(&repo_path, Some(lines), None) {
            Ok(Some(url)) => {
                cx.write_to_clipboard(ClipboardItem::new_string(url));
                self.flash_status("Permalink copied", cx);
            }
            Ok(None) => self.flash_status("No web remote for this repository", cx),
            Err(err) => self.flash_status(format!("Permalink: {err}"), cx),
        }
    }

    /// Shows a transient message (toast).
    pub fn flash_status(&mut self, message: impl Into<SharedString>, cx: &mut gpui::Context<Self>) {
        self.show_toast(message, cx);
    }

    // Session.

    pub(super) fn schedule_session_save(&mut self, cx: &mut gpui::Context<Self>) {
        if !self.integ.session_ready {
            return;
        }
        self.integ.session_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1000))
                .await;
            let _ = this.update(cx, |this, cx| this.save_session_now(cx));
        }));
    }

    fn session(&self, cx: &App) -> Session {
        let panes = self.center.panes();
        let active_pane = panes
            .iter()
            .position(|p| *p == self.active_pane)
            .unwrap_or(0);
        let axis = match &self.center {
            PaneNode::Split { state, .. } if state.read(cx).axis() == Axis::Vertical => {
                lector_config::Axis::Vertical
            }
            _ => lector_config::Axis::Horizontal,
        };
        let panes = panes
            .iter()
            .map(|pane| {
                let p = pane.read(cx);
                let active = p.active_index();
                PaneState {
                    tabs: p
                        .items()
                        .enumerate()
                        .filter_map(|(ix, item)| {
                            let kind = item.tab_kind(cx)?;
                            let scroll_row = item
                                .code_view()
                                .map(|v| v.read(cx).top_visible_row())
                                .unwrap_or(0);
                            Some(TabState {
                                kind,
                                scroll_row,
                                active: ix == active,
                            })
                        })
                        .collect(),
                }
            })
            .collect();
        Session {
            panes,
            axis,
            active_pane,
            sidebar_visible: self.sidebar_visible,
            sidebar_width: f32::from(self.sidebar_width),
            sidebar_tab: self.sidebar.read(cx).active_panel().title().to_string(),
            window: None,
        }
    }

    pub(crate) fn save_session_now(&mut self, cx: &mut gpui::Context<Self>) {
        if !self.has_folder || !self.integ.session_ready || !AppSettings::get(cx).session.restore {
            return;
        }
        let session = self.session(cx);
        let root = self.project.root().to_path_buf();
        if let Err(err) = SessionStore::new().save(&root, &session) {
            tracing::warn!("session save failed: {err}");
        }
    }

    /// Restores tabs, splits and sidebar state from the last session.
    /// Returns whether anything was restored.
    pub fn restore_session(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> bool {
        let restore = AppSettings::get(cx).session.restore && self.has_folder;
        let session = if restore {
            SessionStore::new().load(self.project.root())
        } else {
            None
        };
        let Some(session) = session else {
            self.integ.session_ready = true;
            return false;
        };
        self.sidebar_visible = session.sidebar_visible;
        if session.sidebar_width >= 120. {
            self.sidebar_width = px(session.sidebar_width.min(900.));
        }
        if let Some(panel) = SidebarPanel::ALL
            .iter()
            .find(|p| p.title() == session.sidebar_tab)
        {
            self.sidebar
                .update(cx, |s, cx| s.set_active_panel(*panel, cx));
        }
        let axis = match session.axis {
            lector_config::Axis::Vertical => Axis::Vertical,
            _ => Axis::Horizontal,
        };
        let mut restored_any = false;
        let mut panes = vec![self.active_pane.clone()];
        for (ix, pane_state) in session.panes.iter().enumerate() {
            if pane_state.tabs.is_empty() {
                continue;
            }
            if ix > 0 && restored_any {
                let last = panes
                    .last()
                    .cloned()
                    .unwrap_or_else(|| self.active_pane.clone());
                let new = new_pane(Some(self.root.clone()), cx);
                self.center.split(&last, new.clone(), axis, cx);
                let sub = Self::subscribe_pane(&new, window, cx);
                self._subscriptions.push(sub);
                panes.push(new.clone());
                self.set_active_pane(new, cx);
            }
            let mut active_ix = None;
            for tab in &pane_state.tabs {
                let opened = match &tab.kind {
                    TabKind::File { path } => {
                        if !path.is_file() {
                            continue;
                        }
                        let view = self.open_file_view(path.clone(), false, window, cx);
                        view.update(cx, |v, cx| {
                            v.reveal(
                                Reveal {
                                    row: tab.scroll_row,
                                    cols: 0..0,
                                    center: false,
                                },
                                cx,
                            )
                        });
                        true
                    }
                    kind => {
                        let restorers = std::mem::take(&mut self.integ.restorers);
                        let item = restorers.iter().find_map(|r| r(kind, window, cx));
                        self.integ.restorers = restorers;
                        match item {
                            Some(item) => {
                                self.active_pane
                                    .update(cx, |p, cx| p.add_item(item, false, window, cx));
                                true
                            }
                            None => false,
                        }
                    }
                };
                if opened {
                    restored_any = true;
                    if tab.active {
                        active_ix = Some(self.active_pane.read(cx).active_index());
                    }
                }
            }
            if let Some(ix) = active_ix {
                self.active_pane
                    .update(cx, |p, cx| p.activate(ix, false, window, cx));
            }
        }
        let panes = self.center.panes();
        if let Some(p) = panes.get(session.active_pane).cloned() {
            self.set_active_pane(p, cx);
        }
        if let Some(item) = self.active_pane.read(cx).active_item() {
            let focus = item.focus_handle(cx);
            window.focus(&focus);
        }
        self.integ.session_ready = true;
        cx.notify();
        restored_any
    }
}
