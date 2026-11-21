//! The Changes sidebar panel: working tree sections (Conflicts / Staged /
//! Unstaged / Untracked) or any other changeset, with per-file stats.

use lector_ui::background::{background, view_key, JobKind, Priority};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    prelude::*, App, Entity, FocusHandle, Focusable, SharedString, Subscription, Task, Window,
};
use lector_app::review_queue::QueueGroup;
use lector_git::Changeset;
use lector_review::review::{ChangesetView, ReviewFile};
use lector_ui::actions::workspace as ws_actions;
use lector_ui::components::context_menu::ContextMenuState;
use lector_ui::components::list::VirtualListState;
use lector_ui::views::WorkspaceEvent;

mod load;
mod menus;
mod render;
use load::conflict_section;

use crate::commit_composer::{CommitComposer, CommitComposerEvent};
use crate::confirm::{confirm, discard_spec};
use crate::diff_view::{open_diff, settings_diff_options, DiffTarget, ViewedTarget};
use crate::operation_banner::OperationBanner;
use crate::review_model::{ChangesAnalysis, FileRisk, ReviewModelEvent};
use crate::staging::{run_op, unstage_paths_for, GitOp};
use crate::store::{GitStore, GitStoreEvent};

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
    /// Header of the collapsed "Low-risk & generated" group of a section (file count).
    LowHeader(usize),
    Empty,
}

/// The Changes panel.
pub struct ChangesPanel {
    store: Entity<GitStore>,
    mode: ChangesMode,
    content: Option<Arc<Content>>,
    rows: Rc<Vec<Row>>,
    collapsed: HashSet<Section>,
    /// Risk of the working-tree files (v0.4 review queue), once analyzed.
    risk: Option<Arc<ChangesAnalysis>>,
    /// Order files by risk (setting `review.risk_order`).
    risk_order: bool,
    /// The "Low-risk & generated" group is collapsed.
    low_collapsed: bool,
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
    sync_bar: Option<Entity<crate::sync::SyncPanelBar>>,
    menu: ContextMenuState,
    /// The current branch's default base, for the mode menu (computed off the UI thread).
    base: load::BaseCache,
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
        let model = store.read(cx).review_model.clone();
        subs.push(
            cx.subscribe(&model, |this, model, event: &ReviewModelEvent, cx| {
                if *event == ReviewModelEvent::Analysis {
                    this.risk = model.read(cx).working().cloned();
                    this.rebuild_rows();
                    cx.notify();
                }
            }),
        );
        let risk = model.read(cx).working().cloned();
        let risk_order = store.read(cx).settings(cx).review.risk_order;
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
        let sync_bar = store
            .read(cx)
            .sync
            .clone()
            .map(|c| cx.new(|cx| crate::sync::SyncPanelBar::new(c, cx)));
        let mut panel = ChangesPanel {
            store,
            sync_bar,
            mode: ChangesMode::WorkingTree,
            content: None,
            rows: Rc::new(Vec::new()),
            collapsed: HashSet::new(),
            risk,
            risk_order,
            low_collapsed: true,
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
            base: load::BaseCache::default(),
            focus_handle: cx.focus_handle().tab_stop(true),
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
        self.refresh_default_base(cx);
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
        let review = lector_ui::commands_bridge::core(cx).review();
        let opts = settings_diff_options(store, cx);
        let status = store.status.clone();
        let mode = self.mode.clone();
        let key = view_key(JobKind::GitChangeset, cx.entity_id());
        let work = background(cx, key, Priority::Visible, move |cancel| {
            let load = |cs: &Changeset| {
                review
                    .changeset(&repo, cs, &opts, cancel)
                    .map_err(|e| e.to_string())
            };
            let mut sections = Vec::new();
            match mode {
                ChangesMode::WorkingTree => {
                    let conflicted: HashSet<&str> = status
                        .iter()
                        .filter(|e| e.conflicted)
                        .map(|e| e.path.as_str())
                        .collect();
                    let staged = load(&Changeset::Staged)?;
                    let mut unstaged = load(&Changeset::Unstaged)?;
                    let untracked = load(&Changeset::Untracked)?;
                    if !conflicted.is_empty() {
                        let conflicts = conflict_section(&mut unstaged, &conflicted);
                        sections.push((Section::Conflicts, conflicts));
                    }
                    sections.push((Section::Staged, staged));
                    sections.push((Section::Unstaged, unstaged));
                    sections.push((Section::Untracked, untracked));
                }
                ChangesMode::Changeset(cs) => {
                    sections.push((Section::Files, load(&cs)?));
                }
            }
            Ok::<_, String>(Content { sections })
        });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = work.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.load_task = None;
                match result {
                    Ok(content) => {
                        this.error = None;
                        this.content = Some(Arc::new(content));
                        let store = this.store.read(cx);
                        this.risk_order = store.settings(cx).review.risk_order;
                        this.risk = store.review_model.read(cx).working().cloned();
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
                    let (attention, low) = self.queue_split(*section, view);
                    rows.extend(attention.into_iter().map(|fi| Row::File(si, fi)));
                    if !low.is_empty() {
                        rows.push(Row::LowHeader(low.len()));
                        if !self.low_collapsed {
                            rows.extend(low.into_iter().map(|fi| Row::File(si, fi)));
                        }
                    }
                }
            }
            if rows.is_empty() {
                rows.push(Row::Empty);
            }
        }
        self.rows = Rc::new(rows);
    }

    /// The files of a section: those that need attention by descending risk, then the
    /// low-risk and generated ones (only in the working-tree list, with `review.risk_order`).
    fn queue_split(&self, section: Section, view: &ChangesetView) -> (Vec<usize>, Vec<usize>) {
        let all: Vec<usize> = (0..view.files.len()).collect();
        let analysis = match (&self.risk, self.mode == ChangesMode::WorkingTree) {
            (Some(a), true) if self.risk_order && section != Section::Conflicts => a,
            _ => return (all, Vec::new()),
        };
        let (mut attention, low): (Vec<usize>, Vec<usize>) = all.into_iter().partition(|&fi| {
            analysis
                .get(view.files[fi].path())
                .is_none_or(|f| f.risk.group != QueueGroup::LowRisk)
        });
        let score = |fi: &usize| {
            analysis
                .get(view.files[*fi].path())
                .map_or(0.0, |f| f.risk.score)
        };
        attention.sort_by(|a, b| score(b).total_cmp(&score(a)));
        (attention, low)
    }

    fn file_risk(&self, file: &ReviewFile) -> Option<&FileRisk> {
        if self.mode != ChangesMode::WorkingTree || !self.risk_order {
            return None;
        }
        self.risk.as_ref()?.get(file.path()).map(|f| &f.risk)
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
}
