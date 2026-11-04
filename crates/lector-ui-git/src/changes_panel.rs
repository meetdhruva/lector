//! The Changes sidebar panel: working tree sections (Conflicts / Staged /
//! Unstaged / Untracked) or any other changeset, with per-file stats.

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    div, prelude::*, px, App, ClickEvent, ClipboardItem, Entity, FocusHandle, Focusable,
    FontWeight, MouseButton, MouseDownEvent, Pixels, Point, SharedString, Subscription, Task,
    Window,
};
use lector_diff::DiffStats;
use lector_git::{ChangeKind, Changeset, FileChange, Side};
use lector_ui::actions::git as git_actions;
use lector_ui::actions::picker as picker_actions;
use lector_ui::actions::workspace as ws_actions;
use lector_ui::components::context_menu::{ContextMenu, ContextMenuState};
use lector_ui::components::list::{virtual_list, VirtualListState};
use lector_ui::components::{h_flex, metrics, v_flex, Button, IconButton, Tooltip};
use lector_ui::icons::{FileIcon, Icon, IconName, IconSize};
use lector_ui::keymap::contexts;
use lector_ui::views::WorkspaceEvent;
use lector_ui::{ActiveTheme, ColorExt};
use lector_workspace::review::{self, ChangesetView, ReviewFile};

use crate::commit_composer::{CommitComposer, CommitComposerEvent};
use crate::confirm::{confirm, discard_spec};
use crate::diff_view::{open_diff, settings_diff_options, DiffTarget, ViewedTarget};
use crate::operation_banner::OperationBanner;
use crate::ref_picker::{pick_ref, ref_choices};
use crate::staging::{run_op, unstage_paths_for, GitOp};
use crate::store::{GitStore, GitStoreEvent};
use crate::util::{kind_color, kind_letter, split_path};

/// What the panel lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangesMode {
    /// Status sections of the working tree.
    WorkingTree,
    /// Any other changeset.
    Changeset(Changeset),
}

/// A status section.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    /// Merge conflicts.
    Conflicts,
    /// `HEAD` → index.
    Staged,
    /// Index → working tree.
    Unstaged,
    /// Not tracked.
    Untracked,
    /// The files of a non-working-tree changeset.
    Files,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Section::Conflicts => "Conflicts",
            Section::Staged => "Staged",
            Section::Unstaged => "Unstaged",
            Section::Untracked => "Untracked",
            Section::Files => "Files",
        }
    }
}

/// A per-file operation requested from outside the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathOp {
    /// Stage the file.
    Stage,
    /// Unstage the file.
    Unstage,
    /// Discard its working-tree changes (asks first).
    Discard,
}

/// Loaded panel content.
#[derive(Clone, Default)]
struct Content {
    sections: Vec<(Section, ChangesetView)>,
}

#[derive(Clone)]
enum Row {
    Header(Section, usize),
    File(usize, usize),
    Empty,
}

/// The Changes panel.
pub struct ChangesPanel {
    store: Entity<GitStore>,
    mode: ChangesMode,
    content: Option<Arc<Content>>,
    rows: Rc<Vec<Row>>,
    collapsed: HashSet<Section>,
    selected: Option<(Section, String)>,
    list: VirtualListState,
    load_task: Option<Task<()>>,
    reload_pending: bool,
    error: Option<SharedString>,
    /// Last failed git operation (shown inline until dismissed).
    op_error: Option<SharedString>,
    op_task: Option<Task<()>>,
    /// Visual row to select after the next reload if the selected file moved away.
    pending_select: Option<usize>,
    composer: Entity<CommitComposer>,
    banner: Entity<OperationBanner>,
    menu: ContextMenuState,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for ChangesPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ChangesPanel {
    /// The panel for `store`'s workspace.
    pub fn new(store: Entity<GitStore>, cx: &mut Context<Self>) -> Self {
        let mut subs = vec![cx.subscribe(&store, |this, _, _: &GitStoreEvent, cx| this.reload(cx))];
        if let Some(ws) = store.read(cx).workspace.upgrade() {
            subs.push(cx.subscribe(&ws, |this, _, event: &WorkspaceEvent, cx| {
                // Content changes of already-modified files don't change status.
                if matches!(event, WorkspaceEvent::FilesChanged(_))
                    && this.mode == ChangesMode::WorkingTree
                {
                    this.reload(cx);
                }
            }));
        }
        let composer = cx.new(|cx| CommitComposer::new(store.clone(), cx));
        subs.push(
            cx.subscribe(&composer, |this, _, _: &CommitComposerEvent, cx| {
                this.op_error = None;
                cx.notify();
            }),
        );
        let banner = {
            let (store, composer) = (store.clone(), composer.clone());
            cx.new(|cx| OperationBanner::new(store, composer, cx))
        };
        let mut panel = ChangesPanel {
            store,
            mode: ChangesMode::WorkingTree,
            content: None,
            rows: Rc::new(Vec::new()),
            collapsed: HashSet::new(),
            selected: None,
            list: VirtualListState::new(),
            load_task: None,
            reload_pending: false,
            error: None,
            op_error: None,
            op_task: None,
            pending_select: None,
            composer,
            banner,
            menu: ContextMenuState::default(),
            focus_handle: cx.focus_handle(),
            _subscriptions: subs,
        };
        panel.reload(cx);
        panel
    }

    /// Shows `mode` (reloads).
    pub fn set_mode(&mut self, mode: ChangesMode, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.mode = mode;
            self.content = None;
            self.selected = None;
            self.rows = Rc::new(Vec::new());
            self.reload(cx);
        }
    }

    /// Re-reads status and reloads the list (`git::Refresh`).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| s.refresh(cx));
        self.composer.update(cx, |c, cx| c.load_identity(cx));
        self.reload(cx);
    }

    /// The changeset "Review all" opens.
    pub fn review_changeset(&self) -> Changeset {
        match &self.mode {
            ChangesMode::WorkingTree => Changeset::WorkingTree,
            ChangesMode::Changeset(cs) => cs.clone(),
        }
    }

    /// Reloads in the background (coalesced).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.load_task.is_some() {
            self.reload_pending = true;
            return;
        }
        let store = self.store.read(cx);
        let Some(repo) = store.repo().cloned() else {
            self.error = Some("Not a git repository".into());
            cx.notify();
            return;
        };
        let review = store.review.clone();
        let opts = settings_diff_options(store, cx);
        let status = store.status.clone();
        let mode = self.mode.clone();
        let work = cx.background_spawn(async move {
            let mut sections = Vec::new();
            match mode {
                ChangesMode::WorkingTree => {
                    let conflicted: HashSet<&str> = status
                        .iter()
                        .filter(|e| e.conflicted)
                        .map(|e| e.path.as_str())
                        .collect();
                    let staged =
                        review::load_changeset_with(&repo, &Changeset::Staged, &review, &opts)
                            .map_err(|e| e.to_string())?;
                    let mut unstaged =
                        review::load_changeset_with(&repo, &Changeset::Unstaged, &review, &opts)
                            .map_err(|e| e.to_string())?;
                    let untracked =
                        review::load_changeset_with(&repo, &Changeset::Untracked, &review, &opts)
                            .map_err(|e| e.to_string())?;
                    if !conflicted.is_empty() {
                        let mut conflicts = unstaged.clone();
                        conflicts.files.retain(|f| conflicted.contains(f.path()));
                        for path in &conflicted {
                            if conflicts.index_of(path).is_none() {
                                let change = FileChange {
                                    old_path: Some(path.to_string()),
                                    new_path: Some(path.to_string()),
                                    kind: ChangeKind::Modified,
                                    old: Side::Index(path.to_string()),
                                    new: Side::Workdir(path.to_string()),
                                    binary: None,
                                };
                                conflicts.files.push(ReviewFile {
                                    display_path: path.to_string(),
                                    change,
                                    stats: None,
                                    viewed: false,
                                    content_hash: String::new(),
                                    binary: false,
                                });
                            }
                        }
                        unstaged.files.retain(|f| !conflicted.contains(f.path()));
                        sections.push((Section::Conflicts, conflicts));
                    }
                    sections.push((Section::Staged, staged));
                    sections.push((Section::Unstaged, unstaged));
                    sections.push((Section::Untracked, untracked));
                }
                ChangesMode::Changeset(cs) => {
                    let view = review::load_changeset_with(&repo, &cs, &review, &opts)
                        .map_err(|e| e.to_string())?;
                    sections.push((Section::Files, view));
                }
            }
            Ok::<_, String>(Content { sections })
        });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                this.load_task = None;
                match result {
                    Ok(content) => {
                        this.error = None;
                        this.content = Some(Arc::new(content));
                        this.rebuild_rows();
                        this.restore_selection();
                        if this.mode == ChangesMode::WorkingTree {
                            let staged = this.section_files(Section::Staged).len();
                            this.composer
                                .update(cx, |c, cx| c.set_staged_count(staged, cx));
                        }
                    }
                    Err(e) => this.error = Some(e.into()),
                }
                cx.notify();
                if std::mem::take(&mut this.reload_pending) {
                    this.reload(cx);
                }
            });
        }));
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        if let Some(content) = &self.content {
            for (si, (section, view)) in content.sections.iter().enumerate() {
                if view.files.is_empty() && *section != Section::Files {
                    continue;
                }
                if *section != Section::Files {
                    rows.push(Row::Header(*section, si));
                }
                if !self.collapsed.contains(section) {
                    rows.extend((0..view.files.len()).map(|fi| Row::File(si, fi)));
                }
            }
            if rows.is_empty() {
                rows.push(Row::Empty);
            }
        }
        self.rows = Rc::new(rows);
    }

    fn file(&self, si: usize, fi: usize) -> Option<(Section, &ChangesetView, &ReviewFile)> {
        let content = self.content.as_ref()?;
        let (section, view) = content.sections.get(si)?;
        Some((*section, view, view.files.get(fi)?))
    }

    /// The files of `section` (empty when the section is absent).
    fn section_files(&self, section: Section) -> Vec<&ReviewFile> {
        self.content
            .as_ref()
            .and_then(|c| c.sections.iter().find(|(s, _)| *s == section))
            .map(|(_, v)| v.files.iter().collect())
            .unwrap_or_default()
    }

    /// The merge / rebase banner (Continue / Abort).
    pub fn banner(&self) -> &Entity<OperationBanner> {
        &self.banner
    }

    /// The commit composer.
    pub fn composer(&self) -> &Entity<CommitComposer> {
        &self.composer
    }

    fn selected_row_index(&self) -> Option<usize> {
        let (section, path) = self.selected.as_ref()?;
        self.rows.iter().position(|r| match r {
            Row::File(si, fi) => self
                .file(*si, *fi)
                .is_some_and(|(s, _, f)| s == *section && f.path() == path),
            _ => false,
        })
    }

    fn select_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(Row::File(si, fi)) = self.rows.get(ix).cloned() {
            if let Some((section, _, file)) = self.file(si, fi) {
                self.selected = Some((section, file.path().to_string()));
                self.list.reveal(ix);
                cx.notify();
            }
        }
    }

    /// After a reload: keep the selected file if it is still in its section,
    /// otherwise select whatever now occupies the remembered row (so pressing
    /// space repeatedly walks down a section).
    fn restore_selection(&mut self) {
        let pending = self.pending_select.take();
        if self.selected.is_some() && self.selected_row_index().is_some() {
            return;
        }
        let Some(ix) = pending else {
            return;
        };
        let pick = |ix: usize| match self.rows.get(ix) {
            Some(Row::File(si, fi)) => self
                .file(*si, *fi)
                .map(|(s, _, f)| (s, f.path().to_string())),
            _ => None,
        };
        let found = (ix..self.rows.len())
            .find_map(|i| pick(i).map(|p| (i, p)))
            .or_else(|| {
                (0..ix.min(self.rows.len()))
                    .rev()
                    .find_map(|i| pick(i).map(|p| (i, p)))
            });
        if let Some((i, sel)) = found {
            self.selected = Some(sel);
            self.list.reveal(i);
        } else {
            self.selected = None;
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        let start = self.selected_row_index();
        let mut ix = match start {
            Some(i) => i as isize,
            None if delta > 0 => -1,
            None => n as isize,
        };
        loop {
            ix += delta;
            if ix < 0 || ix >= n as isize {
                return;
            }
            if matches!(self.rows[ix as usize], Row::File(..)) {
                self.select_row(ix as usize, cx);
                return;
            }
        }
    }

    /// Runs a git write and refreshes; failures are shown inline.
    fn run(&mut self, op: GitOp, cx: &mut Context<Self>) {
        self.pending_select = self.selected_row_index();
        let task = run_op(&self.store, op, cx);
        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.op_task = None;
                this.op_error = result.err().map(Into::into);
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Stages or unstages the file at (`si`, `fi`) depending on its section.
    fn toggle_stage(&mut self, si: usize, fi: usize, cx: &mut Context<Self>) {
        let Some((section, _, file)) = self.file(si, fi) else {
            return;
        };
        let op = match section {
            Section::Staged => GitOp::Unstage(unstage_paths_for(&file.change)),
            Section::Unstaged | Section::Untracked | Section::Conflicts => {
                GitOp::Stage(vec![file.path().to_string()])
            }
            Section::Files => return,
        };
        self.selected = Some((section, file.path().to_string()));
        self.run(op, cx);
    }

    /// Stages (or unstages) the selected file (`git::Stage` / space).
    fn toggle_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(Row::File(si, fi)) = self
            .selected_row_index()
            .and_then(|i| self.rows.get(i).cloned())
        {
            self.toggle_stage(si, fi, cx);
        }
    }

    fn unstage_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(Row::File(si, fi)) = self
            .selected_row_index()
            .and_then(|i| self.rows.get(i).cloned())
        {
            if let Some((Section::Staged, _, _)) = self.file(si, fi) {
                self.toggle_stage(si, fi, cx);
            }
        }
    }

    /// Stages every file of a section (`StageAll` for the whole tree when `None`).
    pub fn stage_all(&mut self, section: Option<Section>, cx: &mut Context<Self>) {
        let op = match section {
            Some(section) => {
                let paths: Vec<String> = self
                    .section_files(section)
                    .iter()
                    .map(|f| f.path().to_string())
                    .collect();
                if paths.is_empty() {
                    return;
                }
                GitOp::Stage(paths)
            }
            None => GitOp::StageAll,
        };
        self.run(op, cx);
    }

    /// Unstages everything.
    pub fn unstage_all(&mut self, cx: &mut Context<Self>) {
        if self.section_files(Section::Staged).is_empty() {
            return;
        }
        self.run(GitOp::UnstageAll, cx);
    }

    /// Asks (per `git.confirm_discard`), then discards `tracked` and deletes `untracked`.
    pub fn request_discard(
        &mut self,
        tracked: Vec<String>,
        untracked: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if tracked.is_empty() && untracked.is_empty() {
            return;
        }
        let paths: Vec<String> = tracked.iter().chain(&untracked).cloned().collect();
        if !self.store.read(cx).settings(cx).git.confirm_discard {
            self.run(GitOp::Discard(paths), cx);
            return;
        }
        let Some(ws) = self.store.read(cx).workspace.upgrade() else {
            return;
        };
        let spec = discard_spec(&tracked, &untracked);
        let this = cx.entity().downgrade();
        confirm(&ws, spec, window, cx, move |_, cx| {
            let _ = this.update(cx, |p, cx| p.run(GitOp::Discard(paths), cx));
        });
    }

    /// Discards the file at (`si`, `fi`) after confirmation.
    fn discard_file(&mut self, si: usize, fi: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((section, _, file)) = self.file(si, fi) else {
            return;
        };
        let path = file.path().to_string();
        self.selected = Some((section, path.clone()));
        match section {
            Section::Unstaged => self.request_discard(vec![path], Vec::new(), window, cx),
            Section::Untracked => self.request_discard(Vec::new(), vec![path], window, cx),
            _ => {}
        }
    }

    /// Discards the selected file (`git::DiscardChanges`).
    fn discard_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Row::File(si, fi)) = self
            .selected_row_index()
            .and_then(|i| self.rows.get(i).cloned())
        {
            self.discard_file(si, fi, window, cx);
        }
    }

    /// Discards all unstaged changes and deletes all untracked files (confirmed),
    /// or just one section's.
    pub fn discard_all(
        &mut self,
        section: Option<Section>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = |s: Section| -> Vec<String> {
            self.section_files(s)
                .iter()
                .map(|f| f.path().to_string())
                .collect()
        };
        let (tracked, untracked) = match section {
            Some(Section::Unstaged) => (paths(Section::Unstaged), Vec::new()),
            Some(Section::Untracked) => (Vec::new(), paths(Section::Untracked)),
            Some(_) => return,
            None => (paths(Section::Unstaged), paths(Section::Untracked)),
        };
        self.request_discard(tracked, untracked, window, cx);
    }

    /// Stages / unstages / discards a repo-relative path from outside the panel
    /// (palette or menu with a file focused).
    pub fn path_op(&mut self, rel: &str, op: PathOp, window: &mut Window, cx: &mut Context<Self>) {
        let find = |section: Section| {
            self.section_files(section)
                .into_iter()
                .find(|f| f.path() == rel)
                .map(|f| f.change.clone())
        };
        match op {
            PathOp::Stage => {
                if find(Section::Unstaged).is_some()
                    || find(Section::Untracked).is_some()
                    || find(Section::Conflicts).is_some()
                {
                    self.run(GitOp::Stage(vec![rel.to_string()]), cx);
                }
            }
            PathOp::Unstage => {
                if let Some(change) = find(Section::Staged) {
                    self.run(GitOp::Unstage(unstage_paths_for(&change)), cx);
                }
            }
            PathOp::Discard => {
                if find(Section::Unstaged).is_some() {
                    self.request_discard(vec![rel.to_string()], Vec::new(), window, cx);
                } else if find(Section::Untracked).is_some() {
                    self.request_discard(Vec::new(), vec![rel.to_string()], window, cx);
                }
            }
        }
    }

    /// Whether the panel's file list has keyboard focus.
    pub fn list_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    /// The working-tree section a repo-relative path is listed in (staged first).
    pub fn sections_of(&self, rel: &str) -> Vec<Section> {
        [
            Section::Staged,
            Section::Unstaged,
            Section::Untracked,
            Section::Conflicts,
        ]
        .into_iter()
        .filter(|s| self.section_files(*s).iter().any(|f| f.path() == rel))
        .collect()
    }

    fn open_file_diff(
        &mut self,
        si: usize,
        fi: usize,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((section, view, file)) = self.file(si, fi) else {
            return;
        };
        if section == Section::Conflicts {
            // Conflicts open in the merge editor.
            let rel = file.path().to_string();
            self.selected = Some((section, rel.clone()));
            let store = self.store.clone();
            window.defer(cx, move |window, cx| {
                crate::merge_editor::open_merge_editor(&store, rel, window, cx)
            });
            cx.notify();
            return;
        }
        let selected = (section, file.path().to_string());
        let target = DiffTarget::Change(file.change.clone());
        let viewed = (!file.content_hash.is_empty()).then(|| ViewedTarget {
            key: view.key.clone(),
            file: file.clone(),
        });
        let key = format!("{}:{}", view.key, file.path());
        self.selected = Some(selected);
        let ws = self.store.read(cx).workspace.clone();
        open_diff(
            &ws,
            &self.store,
            target,
            viewed,
            None,
            key,
            preview,
            window,
            cx,
        );
        if preview {
            // Keep the keyboard in the list (space stages, arrows move).
            window.focus(&self.focus_handle);
        }
        cx.notify();
    }

    fn absolute(&self, rel: &str, cx: &App) -> Option<PathBuf> {
        Some(self.store.read(cx).repo()?.workdir().join(rel))
    }

    fn open_file(&self, rel: &str, split: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.absolute(rel, cx) else {
            return;
        };
        if !path.exists() {
            return;
        }
        let ws = self.store.read(cx).workspace.clone();
        window.defer(cx, move |window, cx| {
            let _ = ws.update(cx, |ws, cx| ws.open_path(path, false, window, cx));
            if split {
                window.dispatch_action(Box::new(ws_actions::SplitRight), cx);
            }
        });
    }

    fn deploy_menu(
        &mut self,
        si: usize,
        fi: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((section, _, file)) = self.file(si, fi) else {
            return;
        };
        let rel = file.path().to_string();
        let exists = !matches!(file.change.kind, ChangeKind::Deleted);
        self.selected = Some((section, rel.clone()));
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let t = this.clone();
            let r = rel.clone();
            let toggle = {
                let t = this.clone();
                move |_: &mut Window, cx: &mut App| {
                    let _ = t.update(cx, |p, cx| p.toggle_stage(si, fi, cx));
                }
            };
            let discard = {
                let t = this.clone();
                move |window: &mut Window, cx: &mut App| {
                    let _ = t.update(cx, |p, cx| p.discard_file(si, fi, window, cx));
                }
            };
            let mut menu = match section {
                Section::Staged => menu
                    .icon_entry("Unstage", IconName::Minus, toggle)
                    .separator(),
                Section::Unstaged => menu
                    .icon_entry("Stage", IconName::Plus, toggle)
                    .icon_entry("Discard Changes…", IconName::Undo, discard)
                    .separator(),
                Section::Untracked => menu
                    .icon_entry("Stage", IconName::Plus, toggle)
                    .icon_entry("Delete File…", IconName::Trash, discard)
                    .separator(),
                Section::Conflicts => {
                    let (t, r) = (this.clone(), rel.clone());
                    menu.icon_entry(
                        "Open Merge Editor",
                        IconName::GitMerge,
                        move |window, cx| {
                            let _ = t.update(cx, |p, cx| {
                                let store = p.store.clone();
                                let rel = r.clone();
                                window.defer(cx, move |window, cx| {
                                    crate::merge_editor::open_merge_editor(&store, rel, window, cx)
                                });
                            });
                        },
                    )
                    .icon_entry("Mark Resolved (Stage)", IconName::Check, toggle)
                    .separator()
                }
                Section::Files => menu,
            };
            menu = menu.icon_entry("Open Diff", IconName::Diff, move |window, cx| {
                let _ = t.update(cx, |p, cx| p.open_file_diff(si, fi, false, window, cx));
            });
            if exists {
                let (t1, r1) = (this.clone(), r.clone());
                let (t2, r2) = (this.clone(), r.clone());
                menu = menu
                    .icon_entry("Open File", IconName::FileCode, move |window, cx| {
                        let _ = t1.update(cx, |p, cx| p.open_file(&r1, false, window, cx));
                    })
                    .icon_entry("Open in Split", IconName::Columns, move |window, cx| {
                        let _ = t2.update(cx, |p, cx| p.open_file(&r2, true, window, cx));
                    });
            }
            let (t3, r3) = (this.clone(), r.clone());
            let (t4, r4) = (this.clone(), r.clone());
            let (t5, r5) = (this.clone(), r.clone());
            let r6 = r.clone();
            menu = menu
                .separator()
                .icon_entry("Copy Path", IconName::Copy, move |_, cx| {
                    let abs = t3
                        .upgrade()
                        .and_then(|p| p.read(cx).absolute(&r3, cx))
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| r3.clone());
                    cx.write_to_clipboard(ClipboardItem::new_string(abs));
                })
                .entry("Copy Relative Path", move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(r6.clone()));
                })
                .separator()
                .icon_entry("File History", IconName::History, move |window, cx| {
                    let Some(p) = t4.upgrade() else { return };
                    let store = p.read(cx).store.clone();
                    crate::show_file_history(&store, r4.clone(), window, cx);
                });
            if exists {
                menu = menu.icon_entry(
                    "Reveal in File Tree",
                    IconName::ListTree,
                    move |window, cx| {
                        let Some(p) = t5.upgrade() else { return };
                        let Some(path) = p.read(cx).absolute(&r5, cx) else {
                            return;
                        };
                        let ws = p.read(cx).store.read(cx).workspace.clone();
                        let _ = ws.update(cx, |ws, cx| ws.open_path(path, true, window, cx));
                        window.dispatch_action(Box::new(ws_actions::RevealActiveFile), cx);
                    },
                );
            }
            menu
        });
        self.menu
            .deploy(menu, position, window, cx, |this: &mut Self| &mut this.menu);
        cx.notify();
    }

    fn deploy_mode_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        let store = self.store.clone();
        let branch = self
            .store
            .read(cx)
            .head
            .as_ref()
            .and_then(|h| h.branch.clone());
        let repo = self.store.read(cx).repo().cloned();
        let default_base = match (&repo, &branch) {
            (Some(r), Some(b)) => review::default_base(r, b),
            _ => None,
        };
        let set = move |mode: ChangesMode| {
            let t = this.clone();
            move |_: &mut Window, cx: &mut App| {
                let _ = t.update(cx, |p, cx| p.set_mode(mode.clone(), cx));
            }
        };
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let mut menu = menu
                .header("Show changes")
                .icon_entry(
                    "Working Tree",
                    IconName::Files,
                    set(ChangesMode::WorkingTree),
                )
                .entry("Staged", set(ChangesMode::Changeset(Changeset::Staged)))
                .entry("Unstaged", set(ChangesMode::Changeset(Changeset::Unstaged)))
                .separator();
            if let (Some(b), Some(base)) = (branch.clone(), default_base.clone()) {
                menu = menu.icon_entry(
                    format!("Branch vs {base}"),
                    IconName::GitPullRequest,
                    set(ChangesMode::Changeset(Changeset::BranchVsBase {
                        branch: b,
                        base,
                    })),
                );
            }
            let s1 = store.clone();
            let b1 = branch.clone();
            let set1 = set.clone();
            let s2 = store.clone();
            let set2 = set.clone();
            menu.entry("Branch vs Base…", move |window, cx| {
                let Some(branch) = b1.clone() else { return };
                let set1 = set1.clone();
                pick_from_repo(
                    &s1,
                    "Compare current branch with base…",
                    window,
                    cx,
                    move |c, window, cx| {
                        set1(ChangesMode::Changeset(Changeset::BranchVsBase {
                            branch: branch.clone(),
                            base: c.rev,
                        }))(window, cx)
                    },
                );
            })
            .icon_entry(
                "Commit Range…",
                IconName::GitCompare,
                move |window, cx| {
                    let s3 = s2.clone();
                    let set2 = set2.clone();
                    pick_from_repo(
                        &s2,
                        "From revision (older)…",
                        window,
                        cx,
                        move |from, window, cx| {
                            let set2 = set2.clone();
                            pick_from_repo(
                                &s3,
                                "To revision (newer)…",
                                window,
                                cx,
                                move |to, window, cx| {
                                    set2(ChangesMode::Changeset(Changeset::Range {
                                        from: from.rev.clone(),
                                        to: to.rev,
                                    }))(window, cx)
                                },
                            );
                        },
                    );
                },
            )
        });
        self.menu
            .deploy(menu, position, window, cx, |this: &mut Self| &mut this.menu);
        cx.notify();
    }

    fn mode_label(&self, cx: &App) -> String {
        match &self.mode {
            ChangesMode::WorkingTree => "Working Tree".into(),
            ChangesMode::Changeset(cs) => {
                if let Some(content) = &self.content {
                    if let Some((_, v)) = content.sections.first() {
                        return v.title.clone();
                    }
                }
                review::changeset_title(self.store.read(cx).repo(), cs)
            }
        }
    }

    fn totals(&self) -> (usize, DiffStats) {
        let mut n = 0;
        let mut s = DiffStats::default();
        if let Some(c) = &self.content {
            for (_, v) in &c.sections {
                n += v.files.len();
                let t = v.total_stats();
                s.added += t.added;
                s.removed += t.removed;
            }
        }
        (n, s)
    }

    /// Icon buttons of a section header (Stage All, Unstage All, Discard All).
    fn header_actions(&self, section: Section, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let busy = self.op_task.is_some();
        let button = |id: &'static str, icon: IconName, tip: &'static str| {
            IconButton::new(id, icon)
                .icon_size(IconSize::XSmall)
                .disabled(busy)
                .tooltip(Tooltip::text(tip))
        };
        match section {
            Section::Staged => vec![button("unstage-all", IconName::Minus, "Unstage All")
                .on_click(cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.unstage_all(cx);
                }))
                .into_any_element()],
            Section::Unstaged => vec![
                button(
                    "discard-all-unstaged",
                    IconName::Undo,
                    "Discard All Changes…",
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.discard_all(Some(Section::Unstaged), window, cx);
                }))
                .into_any_element(),
                button("stage-all-unstaged", IconName::Plus, "Stage All Changes")
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.stage_all(Some(Section::Unstaged), cx);
                    }))
                    .into_any_element(),
            ],
            Section::Untracked => vec![
                button(
                    "delete-all-untracked",
                    IconName::Trash,
                    "Delete All Untracked Files…",
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.discard_all(Some(Section::Untracked), window, cx);
                }))
                .into_any_element(),
                button(
                    "stage-all-untracked",
                    IconName::Plus,
                    "Stage All Untracked Files",
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.stage_all(Some(Section::Untracked), cx);
                }))
                .into_any_element(),
            ],
            Section::Conflicts | Section::Files => Vec::new(),
        }
    }

    /// Hover buttons of a file row (stage / unstage / discard).
    fn row_actions(
        &self,
        section: Section,
        si: usize,
        fi: usize,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let busy = self.op_task.is_some();
        let button = |name: &'static str, icon: IconName, tip: &'static str| {
            IconButton::new((name, ix), icon)
                .icon_size(IconSize::Small)
                .square(px(20.))
                .disabled(busy)
                .tooltip(Tooltip::text(tip))
        };
        let stage = || {
            button("row-stage", IconName::Plus, "Stage").on_click(cx.listener(
                move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_stage(si, fi, cx);
                },
            ))
        };
        match section {
            Section::Staged => vec![button("row-unstage", IconName::Minus, "Unstage")
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_stage(si, fi, cx);
                }))
                .into_any_element()],
            Section::Unstaged => vec![
                button("row-discard", IconName::Undo, "Discard Changes…")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.discard_file(si, fi, window, cx);
                    }))
                    .into_any_element(),
                stage().into_any_element(),
            ],
            Section::Untracked => vec![
                button("row-delete", IconName::Trash, "Delete File…")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.discard_file(si, fi, window, cx);
                    }))
                    .into_any_element(),
                stage().into_any_element(),
            ],
            Section::Conflicts => {
                vec![
                    button("row-resolve", IconName::Check, "Mark Resolved (Stage)")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_stage(si, fi, cx);
                        }))
                        .into_any_element(),
                ]
            }
            Section::Files => Vec::new(),
        }
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        match self.rows.get(ix).cloned() {
            Some(Row::Header(section, si)) => {
                let count = self
                    .content
                    .as_ref()
                    .and_then(|c| c.sections.get(si))
                    .map(|(_, v)| v.files.len())
                    .unwrap_or(0);
                let collapsed = self.collapsed.contains(&section);
                h_flex()
                    .id(("changes-header", ix))
                    .w_full()
                    .h(metrics::ROW_HEIGHT)
                    .pl(px(6.))
                    .pr(px(10.))
                    .gap(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(ui.hover.hsla()))
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::XSmall)
                        .color(ui.text_faint.hsla()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(metrics::TEXT_XS)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(if section == Section::Conflicts {
                                theme.git.conflicted.hsla()
                            } else {
                                ui.text_muted.hsla()
                            })
                            .child(section.title().to_uppercase()),
                    )
                    .children(self.header_actions(section, cx))
                    .child(
                        div()
                            .ml(px(2.))
                            .px(px(6.))
                            .rounded(px(8.))
                            .bg(ui.hover.hsla())
                            .text_size(metrics::TEXT_XS)
                            .text_color(ui.text_muted.hsla())
                            .child(count.to_string()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.collapsed.remove(&section) {
                            this.collapsed.insert(section);
                        }
                        this.rebuild_rows();
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Some(Row::File(si, fi)) => {
                let Some((section, _, file)) = self.file(si, fi) else {
                    return div().into_any_element();
                };
                let path = file.path().to_string();
                let (name, dir) = split_path(&path);
                let dir = match (&file.change.old_path, &file.change.new_path) {
                    (Some(o), Some(n)) if o != n => format!("← {o}"),
                    _ => dir.to_string(),
                };
                let selected = self.selected.as_ref() == Some(&(section, path.clone()));
                let kind = if section == Section::Conflicts {
                    None
                } else {
                    Some(file.change.kind)
                };
                let letter_color = match kind {
                    Some(k) if section == Section::Untracked => {
                        let _ = k;
                        theme.git.untracked.hsla()
                    }
                    Some(k) => kind_color(k, cx),
                    None => theme.git.conflicted.hsla(),
                };
                let letter = match (section, kind) {
                    (Section::Untracked, _) => "U",
                    (_, Some(k)) => kind_letter(k),
                    (_, None) => "!",
                };
                let deleted = file.change.kind == ChangeKind::Deleted;
                let stats = file.stats;
                let row_bg = if selected {
                    ui.active.hsla()
                } else {
                    ui.hover.hsla()
                };
                let actions = self.row_actions(section, si, fi, ix, cx);
                let has_actions = !actions.is_empty();
                h_flex()
                    .id(("changes-file", ix))
                    .group("changes-row")
                    .relative()
                    .w_full()
                    .h(metrics::ROW_HEIGHT)
                    .pl(px(22.))
                    .pr(px(10.))
                    .gap(px(6.))
                    .cursor_pointer()
                    .when(selected, |el| el.bg(ui.active.hsla()))
                    .when(!selected, |el| el.hover(|s| s.bg(ui.hover.hsla())))
                    .when(!actions.is_empty(), |el| {
                        el.child(
                            h_flex()
                                .absolute()
                                .right(px(26.))
                                .top_0()
                                .h_full()
                                .pl(px(4.))
                                .gap(px(1.))
                                .bg(row_bg)
                                .invisible()
                                .group_hover("changes-row", |s| s.visible())
                                .children(actions),
                        )
                    })
                    .child(FileIcon::file(name.to_string()).size(px(16.)))
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(6.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(metrics::TEXT)
                                    .text_color(if deleted {
                                        ui.text_muted.hsla()
                                    } else {
                                        ui.text.hsla()
                                    })
                                    .when(deleted, |el| el.line_through())
                                    .child(name.to_string()),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_size(metrics::TEXT_SM)
                                    .text_color(ui.text_faint.hsla())
                                    .child(dir),
                            ),
                    )
                    .when_some(stats.filter(|s| s.added + s.removed > 0), |el, s| {
                        el.child(
                            h_flex()
                                .flex_none()
                                .gap(px(4.))
                                .when(has_actions, |el| {
                                    el.group_hover("changes-row", |s| s.invisible())
                                })
                                .text_size(metrics::TEXT_XS)
                                .when(s.added > 0, |el| {
                                    el.child(
                                        div()
                                            .text_color(theme.diff.added_text.hsla())
                                            .child(format!("+{}", s.added)),
                                    )
                                })
                                .when(s.removed > 0, |el| {
                                    el.child(
                                        div()
                                            .text_color(theme.diff.removed_text.hsla())
                                            .child(format!("−{}", s.removed)),
                                    )
                                }),
                        )
                    })
                    .when(file.binary, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(metrics::TEXT_XS)
                                .text_color(ui.text_faint.hsla())
                                .child("bin"),
                        )
                    })
                    .when(file.viewed, |el| {
                        el.child(
                            Icon::new(IconName::Check)
                                .size(IconSize::XSmall)
                                .color(ui.text_faint.hsla()),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .w(px(12.))
                            .text_size(metrics::TEXT_SM)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(letter_color)
                            .child(letter),
                    )
                    .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                        window.focus(&this.focus_handle);
                        this.open_file_diff(si, fi, e.click_count() < 2, window, cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                            this.deploy_menu(si, fi, e.position, window, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .into_any_element()
            }
            Some(Row::Empty) => v_flex()
                .w_full()
                .h(metrics::ROW_HEIGHT * 3.)
                .items_center()
                .justify_center()
                .text_size(metrics::TEXT_SM)
                .text_color(ui.text_faint.hsla())
                .child(match self.mode {
                    ChangesMode::WorkingTree => "Working tree clean",
                    _ => "No changes",
                })
                .into_any_element(),
            None => div().into_any_element(),
        }
    }
}

/// Loads ref choices in the background, then shows a picker.
pub fn pick_from_repo(
    store: &Entity<GitStore>,
    placeholder: &'static str,
    window: &mut Window,
    cx: &mut App,
    on_pick: impl Fn(crate::ref_picker::RefChoice, &mut Window, &mut App) + 'static,
) {
    let Some(repo) = store.read(cx).repo().cloned() else {
        return;
    };
    let Some(ws) = store.read(cx).workspace.upgrade() else {
        return;
    };
    let task = cx.background_spawn(async move { ref_choices(&repo) });
    window
        .spawn(cx, async move |cx| {
            let choices = task.await;
            let _ =
                cx.update(|window, cx| pick_ref(&ws, placeholder, choices, on_pick, window, cx));
        })
        .detach();
}

impl Render for ChangesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let (count, totals) = self.totals();
        let label = self.mode_label(cx);
        let rows = self.rows.len();
        let entity = cx.entity();
        let working_tree = self.mode == ChangesMode::WorkingTree;
        v_flex()
            .id("changes-panel")
            .size_full()
            .child(self.banner.clone())
            .child(
                v_flex()
                    .flex_none()
                    .px(px(8.))
                    .pb(px(6.))
                    .gap(px(6.))
                    .border_b_1()
                    .border_color(ui.border_variant.hsla())
                    .child(
                        h_flex()
                            .gap(px(4.))
                            .child(
                                h_flex()
                                    .id("changes-mode")
                                    .flex_1()
                                    .min_w_0()
                                    .h(px(26.))
                                    .px(px(8.))
                                    .gap(px(6.))
                                    .rounded(px(5.))
                                    .border_1()
                                    .border_color(ui.border.hsla())
                                    .bg(ui.input_bg.hsla())
                                    .cursor_pointer()
                                    .hover(|s| s.border_color(ui.text_faint.hsla()))
                                    .child(
                                        Icon::new(IconName::GitCompare)
                                            .size(IconSize::XSmall)
                                            .color(ui.text_muted.hsla()),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .text_size(metrics::TEXT_SM)
                                            .child(label),
                                    )
                                    .child(
                                        Icon::new(IconName::ChevronDown)
                                            .size(IconSize::XSmall)
                                            .color(ui.text_faint.hsla()),
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, e: &MouseDownEvent, window, cx| {
                                            this.deploy_mode_menu(e.position, window, cx);
                                            cx.stop_propagation();
                                        }),
                                    ),
                            )
                            .child(
                                IconButton::new("changes-refresh", IconName::Refresh)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Refresh"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.refresh(cx);
                                    })),
                            ),
                    )
                    .when(working_tree, |el| el.child(self.composer.clone()))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .pl(px(2.))
                            .text_size(metrics::TEXT_SM)
                            .child(div().text_color(ui.text_muted.hsla()).child(format!(
                                "{count} file{}",
                                if count == 1 { "" } else { "s" }
                            )))
                            .when(totals.added + totals.removed > 0, |el| {
                                el.child(crate::diff_view::stats_label(totals, &theme))
                            })
                            .child(div().flex_1())
                            .child(
                                Button::new("review-all", "Review")
                                    .icon(IconName::Eye)
                                    .compact()
                                    .disabled(count == 0)
                                    .tooltip(Tooltip::text("Review all files in a tab"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let cs = this.review_changeset();
                                        crate::review_tab::open_review(&this.store, cs, window, cx);
                                    })),
                            ),
                    ),
            )
            .when_some(self.op_error.clone(), |el, e| {
                el.child(
                    h_flex()
                        .mx(px(8.))
                        .mt(px(6.))
                        .items_start()
                        .gap(px(6.))
                        .p(px(8.))
                        .rounded(metrics::RADIUS)
                        .border_1()
                        .border_color(ui.error.hsla().opacity(0.7))
                        .bg(ui.error.hsla().opacity(0.08))
                        .child(
                            Icon::new(IconName::CircleX)
                                .size(IconSize::XSmall)
                                .color(ui.error.hsla()),
                        )
                        .child(
                            div()
                                .id("changes-op-error")
                                .flex_1()
                                .min_w_0()
                                .max_h(px(120.))
                                .overflow_y_scroll()
                                .text_size(metrics::TEXT_XS)
                                .text_color(ui.text.hsla())
                                .child(e),
                        )
                        .child(
                            IconButton::new("changes-op-error-close", IconName::Close)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text("Dismiss"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.op_error = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .when_some(self.error.clone(), |el, e| {
                el.child(
                    div()
                        .p(px(12.))
                        .text_size(metrics::TEXT_SM)
                        .text_color(ui.text_faint.hsla())
                        .child(e),
                )
            })
            .when(self.content.is_none() && self.error.is_none(), |el| {
                el.child(
                    div()
                        .p(px(12.))
                        .text_size(metrics::TEXT_SM)
                        .text_color(ui.text_faint.hsla())
                        .child("Loading…"),
                )
            })
            .child(
                div()
                    .id("changes-list-container")
                    .key_context(contexts::CHANGES_PANEL)
                    .track_focus(&self.focus_handle)
                    .on_action(
                        cx.listener(|this, _: &git_actions::Stage, _, cx| this.toggle_selected(cx)),
                    )
                    .on_action(cx.listener(|this, _: &git_actions::Unstage, _, cx| {
                        this.unstage_selected(cx)
                    }))
                    .on_action(
                        cx.listener(|this, _: &git_actions::DiscardChanges, window, cx| {
                            this.discard_selected(window, cx)
                        }),
                    )
                    .on_action(cx.listener(|this, _: &picker_actions::SelectNext, _, cx| {
                        this.move_selection(1, cx)
                    }))
                    .on_action(cx.listener(|this, _: &picker_actions::SelectPrev, _, cx| {
                        this.move_selection(-1, cx)
                    }))
                    .on_action(
                        cx.listener(|this, _: &picker_actions::Confirm, window, cx| {
                            if let Some(Row::File(si, fi)) = this
                                .selected_row_index()
                                .and_then(|i| this.rows.get(i).cloned())
                            {
                                this.open_file_diff(si, fi, true, window, cx);
                            }
                        }),
                    )
                    .on_action(cx.listener(|this, _: &git_actions::Commit, window, cx| {
                        this.composer.update(cx, |c, cx| c.commit(window, cx));
                    }))
                    .flex_1()
                    .min_h_0()
                    .pt(px(4.))
                    .child(virtual_list(
                        "changes-list",
                        rows,
                        &self.list,
                        cx,
                        move |range, _window, cx| {
                            entity.update(cx, |this, cx| {
                                range.map(|ix| this.render_row(ix, cx)).collect()
                            })
                        },
                    )),
            )
            .children(self.menu.render())
    }
}
