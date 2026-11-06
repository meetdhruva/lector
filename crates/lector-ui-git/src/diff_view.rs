//! The diff view: side-by-side (two synchronized code views) or unified
//! (one code view with old/new line numbers), folded context, word-level
//! highlights, hunk navigation, binary/image summaries.

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    div, img, prelude::*, AnyView, App, ClickEvent, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Hsla, ImageSource, MouseButton, MouseDownEvent, Pixels, SharedString, Subscription,
    Task, WeakEntity, Window,
};
use lector_config::DiffLayout;
use lector_diff::{DiffOptions, DiffStats, Whitespace};
use lector_git::{ChangeKind, Changeset, FileChange, Side};
use lector_ui::actions::diff as diff_actions;
use lector_ui::actions::editor as editor_actions;
use lector_ui::actions::git as git_actions;
use lector_ui::code_view::{CodeSource, CodeView, CodeViewEvent, EditorSettings, Pos, Selection};
use lector_ui::components::context_menu::{ContextMenu, ContextMenuState};
use lector_ui::components::{h_flex, metrics, v_flex, IconButton, Tooltip};
use lector_ui::icons::{FileIcon, Icon, IconName, IconSize};
use lector_ui::keymap::contexts;
use lector_ui::scale::px;
use lector_ui::views::pane::{PaneItem, TabIcon};
use lector_ui::views::Workspace;
use lector_ui::{ActiveTheme, ColorExt};
use lector_workspace::review::{
    self, diff_options, DiffKind, FileDiff, HunkAction, LineSelection, ReviewFile,
};

use crate::confirm::{confirm, ConfirmSpec};
use crate::diff_source::{hunk_display_starts, DiffColumn, DiffMeta, DiffSource};
use crate::staging::{run_op, unstage_paths_for, GitOp};
use crate::store::{GitStore, GitStoreEvent};
use crate::util::{format_size, kind_color, kind_label, split_path};

/// What a [`DiffView`] shows.
#[derive(Clone, Debug, PartialEq)]
pub enum DiffTarget {
    /// A change from a changeset of the project's repository.
    Change(FileChange),
    /// The working copy of a repo-relative path against a revision.
    FileVsRev {
        /// Repo-relative path.
        path: String,
        /// Revision (branch, tag, sha).
        rev: String,
    },
}

impl DiffTarget {
    /// The repo-relative path shown.
    pub fn path(&self) -> &str {
        match self {
            DiffTarget::Change(c) => c.path(),
            DiffTarget::FileVsRev { path, .. } => path,
        }
    }
}

/// The changeset a diff belongs to, for the "Viewed" checkbox.
#[derive(Clone, Debug)]
pub struct ViewedTarget {
    /// Changeset key.
    pub key: String,
    /// The file (with content hash and current state).
    pub file: ReviewFile,
}

/// Events from a [`DiffView`].
#[derive(Clone, Debug, PartialEq)]
pub enum DiffViewEvent {
    /// The user toggled "Viewed" (path, new state).
    ViewedToggled(String, bool),
    /// The diff finished loading.
    Loaded,
    /// Reviewed hunks of a file changed (path, reviewed flag per hunk).
    HunksReviewed(String, Vec<bool>),
    /// `review::NextUnreviewed` and this file has no unreviewed hunk after the cursor.
    NextUnreviewed,
    /// The user followed a "Moved from ..." label into another file.
    GoToMove {
        /// Repository path of the other file.
        path: String,
        /// 0-based line there.
        line: u32,
        /// The line is on the old side of that file's diff.
        old_side: bool,
    },
}

/// Effective layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Side by side.
    Split,
    /// Single column.
    Unified,
}

/// Old / new images of an image diff.
type ImagePair = (Option<Arc<gpui::Image>>, Option<Arc<gpui::Image>>);

struct LoadedDiff {
    diff: Arc<FileDiff>,
    meta: Arc<DiffMeta>,
    images: Option<ImagePair>,
    /// Moved code, structural marks and hunk fingerprints (v0.4 review layer).
    extras: review_layer::Extras,
    /// Review comment threads of this file (re-anchored to this revision).
    threads: Vec<lector_app::comments::Thread>,
}

mod comments;
pub(crate) use comments::display_author;
pub use comments::{CommentComposer, ReviewContext};
mod review_layer;
pub use review_layer::overlay_colors;

/// The diff view entity.
pub struct DiffView {
    store: Entity<GitStore>,
    target: Option<DiffTarget>,
    title_override: Option<SharedString>,
    loaded: Option<LoadedDiff>,
    error: Option<SharedString>,
    loading: bool,
    load_task: Option<Task<()>>,
    layout_override: Option<Layout>,
    ignore_whitespace: bool,
    expanded: HashSet<usize>,
    expand_all: bool,
    sources: Option<(Rc<DiffSource>, Rc<DiffSource>, Rc<DiffSource>)>,
    hunk_rows: (Rc<Vec<u32>>, Rc<Vec<u32>>),
    old_view: Entity<CodeView>,
    new_view: Entity<CodeView>,
    unified_view: Entity<CodeView>,
    current_hunk: Option<usize>,
    viewed: Option<ViewedTarget>,
    last_layout: Layout,
    /// Hunk to put the cursor on after the next load (after staging one).
    pending_hunk: Option<usize>,
    /// Last failed stage / unstage / discard (shown in a banner).
    op_error: Option<SharedString>,
    op_task: Option<Task<()>>,
    menu: ContextMenuState,
    focus_handle: FocusHandle,
    /// Moved code, structural diff, reviewed hunks and provenance (v0.4 review layer).
    review: review_layer::ReviewState,
    /// Review key / PR context of comments (v0.6).
    review_ctx: Option<Rc<ReviewContext>>,
    /// Inline comment threads (v0.6).
    comments: comments::DiffComments,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DiffViewEvent> for DiffView {}

impl DiffView {
    /// Whether the body shows the code views (a text diff with changes), as
    /// opposed to a message ("No changes", binary, image, error, loading).
    fn shows_code_views(&self) -> bool {
        self.error.is_none()
            && self
                .loaded
                .as_ref()
                .is_some_and(|l| l.diff.kind == DiffKind::Text && !l.diff.is_identical())
    }
}

impl Focusable for DiffView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // A code view that is not rendered cannot hold the focus: actions and
        // key bindings would no longer reach the workspace.
        if !self.shows_code_views() {
            return self.focus_handle.clone();
        }
        match self.last_layout {
            Layout::Split => self.new_view.focus_handle(cx),
            Layout::Unified => self.unified_view.focus_handle(cx),
        }
    }
}

fn empty_view(cx: &mut Context<DiffView>) -> Entity<CodeView> {
    cx.new(|cx| {
        let mut v = CodeView::from_text("", None, cx);
        v.set_soft_wrap(false, cx);
        v.set_show_minimap(false, cx);
        v
    })
}

fn to_gpui_format(f: lector_fs::ImageFormat) -> Option<gpui::ImageFormat> {
    use lector_fs::ImageFormat as F;
    Some(match f {
        F::Png => gpui::ImageFormat::Png,
        F::Jpeg => gpui::ImageFormat::Jpeg,
        F::Gif => gpui::ImageFormat::Gif,
        F::Webp => gpui::ImageFormat::Webp,
        F::Svg => gpui::ImageFormat::Svg,
        F::Bmp => gpui::ImageFormat::Bmp,
        F::Ico => return None,
    })
}

impl DiffView {
    /// A diff view; call [`DiffView::set_target`] to load something.
    pub fn new(store: Entity<GitStore>, _standalone: bool, cx: &mut Context<Self>) -> Self {
        let old_view = empty_view(cx);
        let new_view = empty_view(cx);
        let unified_view = empty_view(cx);
        let ignore_whitespace = store.read(cx).settings(cx).diff.ignore_whitespace;
        let mut subscriptions = vec![cx.subscribe(&store, |this, _, _: &GitStoreEvent, cx| {
            // Index / working-tree sides change with every stage, unstage or edit.
            if this.tracks_worktree() && this.op_task.is_none() {
                this.reload(cx);
            }
        })];
        for (view, column) in [
            (&old_view, DiffColumn::Old),
            (&new_view, DiffColumn::New),
            (&unified_view, DiffColumn::Unified),
        ] {
            subscriptions.push(cx.subscribe(view, move |this, view, event, cx| {
                this.on_code_view_event(column, &view, event, cx)
            }));
            Self::wire_comment_gutter(view, column, cx.entity().downgrade(), cx);
            Self::wire_provenance_hover(view, column, cx.entity().downgrade(), cx);
        }
        let review = review_layer::ReviewState::new(&store.read(cx).settings(cx).review);
        DiffView {
            review,
            store,
            target: None,
            title_override: None,
            loaded: None,
            error: None,
            loading: false,
            load_task: None,
            layout_override: None,
            ignore_whitespace,
            expanded: HashSet::new(),
            expand_all: false,
            sources: None,
            hunk_rows: Default::default(),
            old_view,
            new_view,
            unified_view,
            current_hunk: None,
            viewed: None,
            last_layout: Layout::Split,
            pending_hunk: None,
            op_error: None,
            op_task: None,
            menu: ContextMenuState::default(),
            focus_handle: cx.focus_handle(),
            review_ctx: None,
            comments: Default::default(),
            _subscriptions: subscriptions,
        }
    }

    /// Shows `target` (reloads; keeps the layout and options).
    pub fn set_target(
        &mut self,
        target: DiffTarget,
        viewed: Option<ViewedTarget>,
        cx: &mut Context<Self>,
    ) {
        let same = self.target.as_ref() == Some(&target);
        self.target = Some(target);
        self.viewed = viewed;
        if !same {
            self.expanded.clear();
            self.expand_all = false;
            self.current_hunk = None;
            self.loaded = None;
            self.sources = None;
        }
        self.reload(cx);
    }

    /// Overrides the header title (e.g. "file.rs vs main").
    pub fn set_title(&mut self, title: Option<SharedString>) {
        self.title_override = title;
    }

    /// The target.
    pub fn target(&self) -> Option<&DiffTarget> {
        self.target.as_ref()
    }

    /// Updates the viewed state without reloading.
    pub fn set_viewed(&mut self, viewed: Option<ViewedTarget>, cx: &mut Context<Self>) {
        let was = self.viewed.as_ref().map(|v| v.file.viewed);
        let now = viewed.as_ref().map(|v| v.file.viewed);
        self.viewed = viewed;
        self.follow_viewed_change(was, now, cx);
        cx.notify();
    }

    /// Line stats of the loaded diff.
    pub fn stats(&self) -> Option<DiffStats> {
        self.loaded.as_ref().map(|l| l.diff.stats())
    }

    fn options(&self) -> DiffOptions {
        DiffOptions {
            whitespace: if self.ignore_whitespace {
                Whitespace::IgnoreAll
            } else {
                Whitespace::Exact
            },
            ..DiffOptions::default()
        }
    }

    /// Reloads from the repository (e.g. after the file changed).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let Some(project) = self.store.read(cx).project().cloned() else {
            self.error = Some("Not a git repository".into());
            cx.notify();
            return;
        };
        let opts = self.options();
        let (want_moves, want_structural) = (self.review.show_moved, self.review.structural);
        self.loading = true;
        let repo_override = self.review_ctx.as_ref().and_then(|c| c.repo.clone());
        let comment_query = self.comment_query(cx);
        let work = cx.background_spawn(async move {
            let repo = match &repo_override {
                Some(r) => r,
                None => project
                    .repo()
                    .ok_or_else(|| "Not a git repository".to_string())?,
            };
            let languages = project.languages().clone();
            let diff = match &target {
                DiffTarget::Change(change) => {
                    review::load_file_diff_with(repo, &languages, change, &opts)
                }
                DiffTarget::FileVsRev { path, rev } => {
                    review::file_vs_revision_with(repo, &languages, path, rev, &opts)
                }
            }
            .map_err(|e| e.to_string())?;
            let meta = DiffMeta::compute(&diff);
            let images = match diff.kind {
                DiffKind::Image(f) => to_gpui_format(f).map(|fmt| {
                    let make = |side: &review::DiffSide| {
                        side.bytes
                            .as_ref()
                            .map(|b| Arc::new(gpui::Image::from_bytes(fmt, b.as_ref().clone())))
                    };
                    (make(&diff.old), make(&diff.new))
                }),
                _ => None,
            };
            let threads = comment_query
                .map(|q| comments::load_threads(&q, &diff))
                .unwrap_or_default();
            let extras = review_layer::Extras::compute(&diff, want_moves, want_structural);
            Ok::<_, String>(LoadedDiff {
                diff: Arc::new(diff),
                meta: Arc::new(meta),
                images,
                extras,
                threads,
            })
        });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(mut loaded) => {
                        this.error = None;
                        this.set_threads(std::mem::take(&mut loaded.threads));
                        let extras = loaded.extras.clone();
                        this.loaded = Some(loaded);
                        this.apply_extras(extras, cx);
                        this.note_comment_width(cx);
                        this.rebuild(cx);
                        if let Some(k) = this.pending_hunk.take() {
                            this.select_hunk(k, cx);
                        }
                        this.take_pending_line(cx);
                        cx.emit(DiffViewEvent::Loaded);
                    }
                    Err(e) => this.error = Some(e.into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let Some(loaded) = &self.loaded else {
            return;
        };
        let settings = self.store.read(cx).settings(cx);
        let context = settings.diff.context_lines;
        let word = settings.diff.word_diff;
        let diff = loaded.diff.clone();
        let loaded_diff = diff.clone();
        let meta = loaded.meta.clone();
        let lh = Self::line_height(cx);
        let split =
            Rc::new(self.rows_with_comments(&diff, &diff.split_segments(context), false, lh));
        let unified =
            Rc::new(self.rows_with_comments(&diff, &diff.unified_segments(context), true, lh));
        let old = Rc::new(self.review_source(
            DiffSource::new(
                diff.clone(),
                meta.clone(),
                split.clone(),
                DiffColumn::Old,
                word,
            ),
            cx,
        ));
        let new = Rc::new(self.review_source(
            DiffSource::new(diff.clone(), meta.clone(), split, DiffColumn::New, word),
            cx,
        ));
        let uni = Rc::new(self.review_source(
            DiffSource::new(diff, meta, unified, DiffColumn::Unified, word),
            cx,
        ));
        let path = self.absolute_path(cx);
        for (view, src) in [
            (&self.old_view, old.clone()),
            (&self.new_view, new.clone()),
            (&self.unified_view, uni.clone()),
        ] {
            let path = path.clone();
            view.update(cx, |v, cx| {
                let src: Rc<dyn CodeSource> = src;
                v.set_source(src, cx);
                v.set_path(path);
            });
        }
        self.hunk_rows = (
            Rc::new(hunk_display_starts(&loaded_diff, new.rows(), false)),
            Rc::new(hunk_display_starts(&loaded_diff, uni.rows(), true)),
        );
        self.sources = Some((old, new, uni));
        self.update_comment_layers(cx);
        self.apply_review_layers(cx);
        cx.notify();
    }

    fn absolute_path(&self, cx: &App) -> Option<PathBuf> {
        let repo = self.store.read(cx).repo()?;
        let target = self.target.as_ref()?;
        Some(repo.workdir().join(target.path()))
    }

    fn layout(&self, window: &Window, cx: &App) -> Layout {
        // One-sided diffs (added / deleted files) read best in one column.
        if self
            .loaded
            .as_ref()
            .is_some_and(|l| !l.diff.old.exists || !l.diff.new.exists)
        {
            return Layout::Unified;
        }
        if let Some(l) = self.layout_override {
            return l;
        }
        match self.store.read(cx).settings(cx).diff.layout {
            DiffLayout::Split => Layout::Split,
            DiffLayout::Unified => Layout::Unified,
            DiffLayout::Auto => {
                if window.viewport_size().width >= px(1280.) {
                    Layout::Split
                } else {
                    Layout::Unified
                }
            }
        }
    }

    fn on_code_view_event(
        &mut self,
        column: DiffColumn,
        view: &Entity<CodeView>,
        event: &CodeViewEvent,
        cx: &mut Context<Self>,
    ) {
        self.review_code_event(event, cx);
        match event {
            CodeViewEvent::Scrolled if column != DiffColumn::Unified => {
                if !self.hunk_actions().is_empty() {
                    cx.notify(); // keeps the hunk toolbars next to their hunks
                }
                let offset = view.read(cx).scroll_offset();
                let other = if column == DiffColumn::Old {
                    &self.new_view
                } else {
                    &self.old_view
                };
                if other.read(cx).scroll_offset() != offset {
                    other.update(cx, |o, cx| o.set_scroll_offset(offset, cx));
                }
            }
            CodeViewEvent::Scrolled => {
                if !self.hunk_actions().is_empty() {
                    cx.notify();
                }
            }
            CodeViewEvent::HoverMoved(Some(pos)) => self.comment_hover(column, pos.row, cx),
            CodeViewEvent::SelectionChanged => {
                let sel = view.read(cx).selection();
                if !sel.is_empty() {
                    return;
                }
                let row = sel.head.row;
                let fold = self.sources.as_ref().and_then(|(o, n, u)| {
                    match column {
                        DiffColumn::Old => o,
                        DiffColumn::New => n,
                        DiffColumn::Unified => u,
                    }
                    .fold_at(row)
                });
                if let Some(fold) = fold {
                    self.expanded.insert(fold.start);
                    self.rebuild(cx);
                } else if column != DiffColumn::Unified {
                    // Mirror the cursor row on the other column (line highlight).
                    let other = if column == DiffColumn::Old {
                        &self.new_view
                    } else {
                        &self.old_view
                    };
                    if other.read(cx).cursor().row != row {
                        other.update(cx, |o, cx| {
                            let offset = o.scroll_offset();
                            o.set_selection(Selection::cursor(Pos::new(row, 0)), cx);
                            o.set_scroll_offset(offset, cx);
                        });
                    }
                }
            }
            _ => {}
        }
    }

    fn active_view(&self) -> &Entity<CodeView> {
        match self.last_layout {
            Layout::Split => &self.new_view,
            Layout::Unified => &self.unified_view,
        }
    }

    fn line_height(cx: &App) -> Pixels {
        let s = EditorSettings::get(cx);
        (s.font_size * s.line_height).round()
    }

    fn hunk_starts(&self) -> Rc<Vec<u32>> {
        match self.last_layout {
            Layout::Split => self.hunk_rows.0.clone(),
            Layout::Unified => self.hunk_rows.1.clone(),
        }
    }

    /// Jumps to the next (`delta = 1`) or previous (`-1`) hunk. Returns
    /// false when there is none in that direction.
    pub fn go_to_hunk(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let starts = self.hunk_starts();
        if starts.is_empty() {
            return false;
        }
        let lh = Self::line_height(cx);
        let top = (self.active_view().read(cx).scroll_offset().y / lh).floor() as u32;
        let next = match self.current_hunk {
            Some(i) if i < starts.len() => {
                let n = i as isize + delta;
                if n < 0 || n as usize >= starts.len() {
                    return false;
                }
                n as usize
            }
            _ => {
                let anchor = top + 3;
                if delta > 0 {
                    match starts.iter().position(|&s| s > anchor) {
                        Some(i) => i,
                        None => return false,
                    }
                } else {
                    match starts.iter().rposition(|&s| s < anchor) {
                        Some(i) => i,
                        None => return false,
                    }
                }
            }
        };
        self.select_hunk(next, cx);
        true
    }

    /// Puts the cursor on hunk `k` (clamped to the last one) and scrolls to it.
    fn select_hunk(&mut self, k: usize, cx: &mut Context<Self>) {
        let starts = self.hunk_starts();
        let Some(last) = starts.len().checked_sub(1) else {
            return;
        };
        let next = k.min(last);
        self.current_hunk = Some(next);
        let row = starts[next];
        let views: Vec<_> = match self.last_layout {
            Layout::Split => vec![self.old_view.clone(), self.new_view.clone()],
            Layout::Unified => vec![self.unified_view.clone()],
        };
        for v in views {
            v.update(cx, |v, cx| {
                v.set_selection(Selection::cursor(Pos::new(row, 0)), cx);
                v.scroll_to_row(row.saturating_sub(4), false, cx);
            });
        }
        cx.notify();
    }

    fn toggle_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.layout(window, cx);
        self.set_layout(
            match current {
                Layout::Split => Layout::Unified,
                Layout::Unified => Layout::Split,
            },
            window,
            cx,
        );
    }

    fn set_layout(&mut self, layout: Layout, window: &mut Window, cx: &mut Context<Self>) {
        // Keep roughly the same position: map the top row's new line.
        self.layout_override = Some(layout);
        self.current_hunk = None;
        let focus = match layout {
            Layout::Split => self.new_view.focus_handle(cx),
            Layout::Unified => self.unified_view.focus_handle(cx),
        };
        window.focus(&focus);
        cx.notify();
    }

    fn toggle_ignore_whitespace(&mut self, cx: &mut Context<Self>) {
        self.ignore_whitespace = !self.ignore_whitespace;
        self.expanded.clear();
        self.current_hunk = None;
        self.reload(cx);
    }

    fn toggle_expand_all(&mut self, cx: &mut Context<Self>) {
        self.expand_all = !self.expand_all;
        self.expanded.clear();
        self.rebuild(cx);
    }

    /// The new-side line at the active cursor (for "open in editor").
    fn cursor_new_line(&self, cx: &App) -> Option<u32> {
        let loaded = self.loaded.as_ref()?;
        let (_, new, uni) = self.sources.as_ref()?;
        let (src, view, unified) = match self.last_layout {
            Layout::Split => (new, &self.new_view, false),
            Layout::Unified => (uni, &self.unified_view, true),
        };
        let row = view.read(cx).cursor().row as usize;
        let ix = match src.rows().get(row) {
            Some(crate::diff_source::DisplayRow::Row(ix)) => *ix,
            Some(crate::diff_source::DisplayRow::Fold(r)) => r.start,
            Some(crate::diff_source::DisplayRow::Gap(_)) | None => 0,
        };
        Some(if unified {
            loaded.diff.new_line_near_unified_row(ix)
        } else {
            loaded.diff.new_line_near_split_row(ix)
        })
    }

    fn open_in_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.absolute_path(cx) else {
            return;
        };
        if !path.exists() {
            return;
        }
        let line = self.cursor_new_line(cx).unwrap_or(0);
        let ws = self.store.read(cx).workspace.clone();
        window.defer(cx, move |window, cx| {
            let _ = ws.update(cx, |ws, cx| {
                ws.open_path_at(path, Some(line + 1), None, window, cx);
            });
        });
    }

    /// Toggles "viewed" (standalone views persist it themselves).
    pub fn toggle_viewed(&mut self, cx: &mut Context<Self>) {
        let Some(target) = &self.viewed else {
            return;
        };
        let new_state = !target.file.viewed;
        // Every hunk follows the file (marking a file viewed reviews all its hunks).
        self.set_viewed_state(new_state, true, cx);
    }

    // Staging (v0.2).

    /// Whether either side is the index or the working tree (changes on git writes).
    fn tracks_worktree(&self) -> bool {
        match &self.target {
            Some(DiffTarget::Change(c)) => [&c.old, &c.new]
                .iter()
                .any(|s| matches!(s, Side::Index(_) | Side::Workdir(_))),
            Some(DiffTarget::FileVsRev { .. }) => true,
            None => false,
        }
    }

    /// The changeset this diff belongs to, when it is `Unstaged` or `Staged`.
    pub fn changeset(&self) -> Option<Changeset> {
        let key = &self.viewed.as_ref()?.key;
        match review::parse_changeset_key(key)? {
            cs @ (Changeset::Unstaged | Changeset::Staged) => Some(cs),
            _ => None,
        }
    }

    /// Hunk actions offered for this diff (Unstaged: stage + discard; Staged: unstage).
    pub fn hunk_actions(&self) -> &'static [HunkAction] {
        let text = self
            .loaded
            .as_ref()
            .is_some_and(|l| l.diff.kind == DiffKind::Text && !l.diff.is_identical());
        match (text, self.changeset()) {
            (true, Some(cs)) => HunkAction::available_for(&cs),
            _ => &[],
        }
    }

    /// The hunk at the cursor (or the next one below it, else the last one).
    fn hunk_at_cursor(&self, cx: &App) -> Option<usize> {
        let loaded = self.loaded.as_ref()?;
        let (src, view, unified) = self.active_source_in(cx)?;
        let row = view.read(cx).cursor().row;
        if let Some(crate::diff_source::DisplayRow::Row(ix)) = src.rows().get(row as usize) {
            let hunk = if unified {
                loaded.diff.unified.get(*ix).and_then(|r| r.hunk)
            } else {
                loaded.diff.split.get(*ix).and_then(|r| r.hunk)
            };
            if hunk.is_some() {
                return hunk;
            }
        }
        let starts = self.hunk_starts();
        starts
            .iter()
            .position(|s| *s >= row)
            .or_else(|| starts.len().checked_sub(1))
    }

    fn active_source_in(&self, cx: &App) -> Option<(Rc<DiffSource>, Entity<CodeView>, bool)> {
        let (old, new, uni) = self.sources.as_ref()?;
        Some(match self.last_layout {
            Layout::Unified => (uni.clone(), self.unified_view.clone(), true),
            Layout::Split => {
                let old_sel = !self.old_view.read(cx).selection().is_empty();
                let new_sel = !self.new_view.read(cx).selection().is_empty();
                if old_sel && !new_sel {
                    (old.clone(), self.old_view.clone(), false)
                } else {
                    (new.clone(), self.new_view.clone(), false)
                }
            }
        })
    }

    /// Diff rows covered by the selection (old and new line numbers of changed rows).
    fn selected_lines(&self, cx: &App) -> Option<LineSelection> {
        let (src, view, _) = self.active_source_in(cx)?;
        let sel = view.read(cx).selection();
        if sel.is_empty() {
            return None;
        }
        let (a, b) = sel.range();
        let end = if b.col == 0 && b.row > a.row {
            b.row
        } else {
            b.row + 1
        };
        let mut out = LineSelection::default();
        for row in a.row..end {
            let Some((old, new, kind)) = src.lines_at(row) else {
                continue;
            };
            if kind == lector_diff::RowKind::Context {
                continue;
            }
            if let Some(o) = old {
                out.old.push(o..o + 1);
            }
            if let Some(n) = new {
                out.new.push(n..n + 1);
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// Stages / unstages / discards hunk `hunk`, or the selected lines when `lines`.
    pub fn hunk_action(
        &mut self,
        action: HunkAction,
        hunk: Option<usize>,
        lines: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.hunk_actions().contains(&action) {
            return;
        }
        let (Some(changeset), Some(loaded), Some(DiffTarget::Change(change))) =
            (self.changeset(), self.loaded.as_ref(), self.target.clone())
        else {
            return;
        };
        let k = hunk.or_else(|| self.hunk_at_cursor(cx));
        let selection = if lines {
            self.selected_lines(cx)
        } else {
            k.and_then(|k| loaded.diff.hunk_selection(k))
        };
        let Some(selection) = selection else {
            return;
        };
        let op = GitOp::Hunk {
            changeset,
            change: change.clone(),
            action,
            selection,
            content_id: loaded.diff.content_id().to_string(),
        };
        if action == HunkAction::Discard && self.store.read(cx).settings(cx).git.confirm_discard {
            let Some(ws) = self.store.read(cx).workspace.upgrade() else {
                return;
            };
            let what = if lines {
                "the selected lines"
            } else {
                "this hunk"
            };
            let spec = ConfirmSpec {
                title: format!("Discard {what}?").into(),
                message: vec![
                    format!(
                        "The working-tree changes of {what} in {} are reverted to the staged \
                         version. Other changes in the file are kept.",
                        change.path()
                    )
                    .into(),
                    "This cannot be undone.".into(),
                ],
                items: Vec::new(),
                confirm_label: "Discard".into(),
            };
            let this = cx.entity().downgrade();
            confirm(&ws, spec, window, cx, move |_, cx| {
                let _ = this.update(cx, |v, cx| v.run_hunk_op(op, k, cx));
            });
            return;
        }
        self.run_hunk_op(op, k, cx);
    }

    fn run_hunk_op(&mut self, op: GitOp, hunk: Option<usize>, cx: &mut Context<Self>) {
        let task = run_op(&self.store, op, cx);
        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.op_task = None;
                match result {
                    Ok(_) => {
                        this.op_error = None;
                        // The hunk now at the same index is the next one.
                        this.pending_hunk = hunk;
                        this.reload(cx);
                    }
                    Err(e) => this.op_error = Some(e.into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Stages / unstages / discards the whole file of this diff.
    fn file_action(&mut self, action: HunkAction, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(cs), Some(DiffTarget::Change(change))) = (self.changeset(), self.target.clone())
        else {
            cx.propagate();
            return;
        };
        let op = match (action, cs) {
            (HunkAction::Stage, Changeset::Unstaged) => {
                GitOp::Stage(vec![change.path().to_string()])
            }
            (HunkAction::Unstage, Changeset::Staged) => GitOp::Unstage(unstage_paths_for(&change)),
            (HunkAction::Discard, Changeset::Unstaged) => {
                let Some(views) = self.store.read(cx).views.clone() else {
                    return;
                };
                let path = change.path().to_string();
                views.changes.update(cx, |p, cx| {
                    p.request_discard(vec![path], Vec::new(), window, cx)
                });
                return;
            }
            _ => return,
        };
        self.run_hunk_op(op, None, cx);
    }

    fn deploy_menu(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let actions = self.hunk_actions();
        let has_lines = self.selected_lines(cx).is_some();
        let focus = self
            .active_source_in(cx)
            .map(|(_, v, _)| v.focus_handle(cx))
            .unwrap_or_else(|| self.focus_handle.clone());
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let mut menu = menu.context(focus);
            let entry = |menu: ContextMenu,
                         label: &'static str,
                         icon: IconName,
                         action: HunkAction,
                         lines: bool| {
                let t = this.clone();
                menu.icon_entry(label, icon, move |window, cx| {
                    let _ = t.update(cx, |v, cx| v.hunk_action(action, None, lines, window, cx));
                })
            };
            for action in actions {
                menu = match (action, has_lines) {
                    (HunkAction::Stage, true) => {
                        entry(menu, "Stage Selected Lines", IconName::Plus, *action, true)
                    }
                    (HunkAction::Unstage, true) => entry(
                        menu,
                        "Unstage Selected Lines",
                        IconName::Minus,
                        *action,
                        true,
                    ),
                    (HunkAction::Discard, true) => entry(
                        menu,
                        "Discard Selected Lines…",
                        IconName::Undo,
                        *action,
                        true,
                    ),
                    _ => menu,
                };
            }
            for action in actions {
                menu = match action {
                    HunkAction::Stage => entry(menu, "Stage Hunk", IconName::Plus, *action, false),
                    HunkAction::Unstage => {
                        entry(menu, "Unstage Hunk", IconName::Minus, *action, false)
                    }
                    HunkAction::Discard => {
                        entry(menu, "Discard Hunk…", IconName::Undo, *action, false)
                    }
                };
            }
            menu.separator()
                .icon_action("Copy", IconName::Copy, Box::new(editor_actions::Copy))
                .icon_action(
                    "Open File",
                    IconName::FileCode,
                    Box::new(diff_actions::OpenInEditor),
                )
        });
        self.menu
            .deploy(menu, position, window, cx, |this: &mut Self| &mut this.menu);
        cx.notify();
    }

    /// Floating Stage / Unstage / Discard buttons next to each visible hunk.
    fn render_hunk_toolbars(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let actions = self.hunk_actions();
        let reviewable = self.hunk_review_enabled();
        if actions.is_empty() && !reviewable {
            return Vec::new();
        }
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let lh = Self::line_height(cx);
        let view = match self.last_layout {
            Layout::Split => &self.new_view,
            Layout::Unified => &self.unified_view,
        };
        let scroll = view.read(cx).scroll_offset().y;
        let viewport = window.viewport_size().height;
        let busy = self.op_task.is_some();
        let current = self.current_hunk.or_else(|| self.hunk_at_cursor(cx));
        let mut out = Vec::new();
        for (k, start) in self.hunk_starts().iter().enumerate() {
            let y = lh * *start as f32 - scroll;
            if y < -lh || y > viewport {
                continue;
            }
            let top = (y - lh + (lh - px(20.)) / 2.).max(px(0.));
            let mut bar = h_flex()
                .absolute()
                .top(top)
                .right(px(18.))
                .h(px(20.))
                .px(px(2.))
                .gap(px(1.))
                .rounded(px(5.))
                .border_1()
                .border_color(if current == Some(k) {
                    ui.border_focused.hsla()
                } else {
                    ui.border.hsla()
                })
                .bg(ui.elevated.hsla())
                .shadow_sm()
                .text_size(metrics::text_xs());
            if reviewable {
                bar = bar.child(self.reviewed_chip(k, cx));
            }
            for action in actions {
                let (label, icon, tip): (&str, IconName, &str) = match action {
                    HunkAction::Stage => ("Stage", IconName::Plus, "Stage Hunk (s)"),
                    HunkAction::Unstage => ("Unstage", IconName::Minus, "Unstage Hunk (u)"),
                    HunkAction::Discard => ("Discard", IconName::Undo, "Discard Hunk…"),
                };
                let action = *action;
                bar = bar.child(
                    h_flex()
                        .id(("hunk-action", k * 4 + action as usize))
                        .h(px(16.))
                        .px(px(5.))
                        .gap(px(3.))
                        .rounded(px(3.))
                        .text_color(if busy {
                            ui.text_faint.hsla()
                        } else {
                            ui.text_muted.hsla()
                        })
                        .when(!busy, |el| {
                            el.cursor_pointer()
                                .hover(|s| s.bg(ui.hover.hsla()).text_color(ui.text.hsla()))
                        })
                        .child(
                            Icon::new(icon)
                                .size(IconSize::XSmall)
                                .color(ui.text_muted.hsla()),
                        )
                        .child(label)
                        .tab_index(0)
                        .focus(lector_ui::a11y::ring_focus(cx))
                        .tooltip(Tooltip::text(tip))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            if this.op_task.is_none() {
                                this.hunk_action(action, Some(k), false, window, cx);
                            }
                        })),
                );
            }
            out.push(bar.into_any_element());
        }
        out
    }

    // Rendering.

    fn render_header(&self, layout: Layout, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let path: SharedString = match (&self.title_override, &self.loaded, &self.target) {
            (Some(t), _, _) => t.clone(),
            (None, Some(l), _) => l.diff.display_path().into(),
            (None, None, Some(t)) => t.path().to_string().into(),
            _ => SharedString::default(),
        };
        let file_name = self
            .target
            .as_ref()
            .map(|t| split_path(t.path()).0.to_string())
            .unwrap_or_default();
        let (name, dir) = if self.title_override.is_some() || path.contains(" → ") {
            (path.to_string(), String::new())
        } else {
            let (n, d) = split_path(&path);
            (n.to_string(), d.to_string())
        };
        let kind = match (&self.loaded, &self.target) {
            (Some(l), _) => Some(l.diff.change.kind),
            (None, Some(DiffTarget::Change(c))) => Some(c.kind),
            _ => None,
        };
        let stats = self
            .loaded
            .as_ref()
            .filter(|l| l.diff.kind == DiffKind::Text)
            .map(|l| l.diff.stats())
            .filter(|s| s.added + s.removed > 0);
        let viewed = self.viewed.as_ref().map(|v| v.file.viewed);
        let hunks = self.hunk_starts().len();
        let split = layout == Layout::Split;
        let review_controls = self.render_review_controls(cx);
        let seg =
            |id: &'static str, label: &'static str, icon: IconName, selected: bool, l: Layout| {
                let accent = ui.text.hsla();
                let faint = ui.text_muted.hsla();
                h_flex()
                    .id(id)
                    .h(px(22.))
                    .px(px(8.))
                    .gap(px(5.))
                    .rounded(px(4.))
                    .text_size(metrics::text_sm())
                    .cursor_pointer()
                    .text_color(if selected { accent } else { faint })
                    .when(selected, |el| el.bg(ui.tab_active_bg.hsla()).shadow_sm())
                    .when(!selected, |el| el.hover(|s| s.text_color(accent)))
                    .child(Icon::new(icon).size(IconSize::XSmall).color(if selected {
                        accent
                    } else {
                        faint
                    }))
                    .tab_index(0)
                    .focus(lector_ui::a11y::ring_focus(cx))
                    .child(label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.set_layout(l, window, cx)
                    }))
            };
        h_flex()
            .flex_none()
            .h(px(40.))
            .px(px(12.))
            .gap(px(8.))
            .border_b_1()
            .border_color(ui.border.hsla())
            .bg(ui.tab_bar_bg.hsla())
            .child(FileIcon::file(file_name).size(px(16.)))
            .child(
                h_flex()
                    .min_w_0()
                    .flex_shrink()
                    .gap(px(6.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui.text.hsla())
                            .child(name),
                    )
                    .when(!dir.is_empty(), |el| {
                        el.child(
                            div()
                                .text_size(metrics::text_sm())
                                .text_color(ui.text_faint.hsla())
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(dir),
                        )
                    }),
            )
            .when_some(kind, |el, kind| {
                let color = kind_color(kind, cx);
                el.child(
                    div()
                        .flex_none()
                        .px(px(6.))
                        .py(px(1.))
                        .rounded(px(4.))
                        .text_size(metrics::text_xs())
                        .font_weight(FontWeight::MEDIUM)
                        .bg(color.opacity(0.14))
                        .text_color(color)
                        .child(kind_label(kind)),
                )
            })
            .when_some(stats, |el, s| el.child(stats_label(s, &theme)))
            .child(div().flex_1())
            .when(self.loading, |el| {
                el.child(
                    div()
                        .text_size(metrics::text_sm())
                        .text_color(ui.text_faint.hsla())
                        .child("Loading…"),
                )
            })
            .when(hunks > 0, |el| {
                el.child(
                    h_flex()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(metrics::text_xs())
                                .text_color(ui.text_faint.hsla())
                                .mr(px(2.))
                                .child(format!(
                                    "{hunks} hunk{}",
                                    if hunks == 1 { "" } else { "s" }
                                )),
                        )
                        .child(
                            IconButton::new("prev-hunk", IconName::ArrowUp)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::for_action(
                                    "Previous Hunk",
                                    &diff_actions::PrevHunk,
                                ))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.go_to_hunk(-1, cx);
                                })),
                        )
                        .child(
                            IconButton::new("next-hunk", IconName::ArrowDown)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::for_action("Next Hunk", &diff_actions::NextHunk))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.go_to_hunk(1, cx);
                                })),
                        ),
                )
            })
            .children(review_controls)
            .child(
                IconButton::new(
                    "expand-all",
                    if self.expand_all {
                        IconName::ListCollapse
                    } else {
                        IconName::ChevronsUpDown
                    },
                )
                .icon_size(IconSize::Small)
                .selected(self.expand_all)
                .tooltip(Tooltip::for_action(
                    if self.expand_all {
                        "Collapse Unchanged Lines"
                    } else {
                        "Expand All Lines"
                    },
                    &diff_actions::ExpandAllContext,
                ))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_expand_all(cx))),
            )
            .child(
                IconButton::new("ignore-ws", IconName::WholeWord)
                    .icon_size(IconSize::Small)
                    .selected(self.ignore_whitespace)
                    .tooltip(Tooltip::for_action(
                        if self.ignore_whitespace {
                            "Show Whitespace Changes"
                        } else {
                            "Ignore Whitespace"
                        },
                        &diff_actions::ToggleIgnoreWhitespace,
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_ignore_whitespace(cx))),
            )
            .child(
                h_flex()
                    .p(px(2.))
                    .gap(px(2.))
                    .rounded(px(6.))
                    .bg(ui.hover.hsla())
                    .child(seg(
                        "layout-split",
                        "Split",
                        IconName::Columns,
                        split,
                        Layout::Split,
                    ))
                    .child(seg(
                        "layout-unified",
                        "Unified",
                        IconName::AlignJustify,
                        !split,
                        Layout::Unified,
                    )),
            )
            .child(
                IconButton::new("open-file", IconName::FileCode)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::for_action(
                        "Open File",
                        &diff_actions::OpenInEditor,
                    ))
                    .on_click(cx.listener(|this, _, window, cx| this.open_in_editor(window, cx))),
            )
            .when_some(viewed, |el, viewed| {
                el.child(
                    viewed_checkbox("viewed-toggle", viewed, cx)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_viewed(cx))),
                )
            })
    }

    fn render_body(&self, layout: Layout, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let message = |icon: IconName, title: String, detail: Option<String>| {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .child(
                    Icon::new(icon)
                        .size(IconSize::Custom(px(32.)))
                        .color(ui.text_faint.hsla()),
                )
                .child(div().text_color(ui.text_muted.hsla()).child(title))
                .when_some(detail, |el, d| {
                    el.child(
                        div()
                            .text_size(metrics::text_sm())
                            .text_color(ui.text_faint.hsla())
                            .child(d),
                    )
                })
                .into_any_element()
        };
        if let Some(err) = &self.error {
            return message(
                IconName::AlertTriangle,
                "Could not load diff".into(),
                Some(err.to_string()),
            );
        }
        let Some(loaded) = &self.loaded else {
            return div().size_full().into_any_element();
        };
        let diff = &loaded.diff;
        let sizes = format!(
            "{} → {}",
            if diff.old.exists {
                format_size(diff.old.size)
            } else {
                "—".into()
            },
            if diff.new.exists {
                format_size(diff.new.size)
            } else {
                "—".into()
            }
        );
        match diff.kind {
            DiffKind::Binary => {
                return message(
                    IconName::Binary,
                    match diff.change.kind {
                        ChangeKind::Added => "Binary file added".into(),
                        ChangeKind::Deleted => "Binary file deleted".into(),
                        _ => "Binary file changed".into(),
                    },
                    Some(sizes),
                )
            }
            DiffKind::TooLarge => {
                return message(
                    IconName::FileText,
                    "File too large to diff".into(),
                    Some(sizes),
                )
            }
            DiffKind::Image(_) => {
                let side = |label: &'static str,
                            image: Option<Arc<gpui::Image>>,
                            size: u64,
                            bg: Hsla| {
                    v_flex()
                        .flex_1()
                        .h_full()
                        .min_w_0()
                        .items_center()
                        .gap(px(8.))
                        .p(px(16.))
                        .child(
                            div()
                                .text_size(metrics::text_sm())
                                .text_color(ui.text_muted.hsla())
                                .child(match &image {
                                    Some(_) => format!("{label} · {}", format_size(size)),
                                    None => format!("{label} · none"),
                                }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .w_full()
                                .min_h_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(6.))
                                .border_1()
                                .border_color(bg)
                                .bg(ui.surface.hsla())
                                .when_some(image, |el, image| {
                                    el.child(
                                        img(ImageSource::Image(image)).max_w_full().max_h_full(),
                                    )
                                }),
                        )
                };
                let (old_img, new_img) = loaded.images.clone().unwrap_or((None, None));
                return h_flex()
                    .size_full()
                    .items_start()
                    .child(side(
                        "Before",
                        old_img,
                        diff.old.size,
                        theme.diff.removed_gutter.hsla().opacity(0.6),
                    ))
                    .child(side(
                        "After",
                        new_img,
                        diff.new.size,
                        theme.diff.added_gutter.hsla().opacity(0.6),
                    ))
                    .into_any_element();
            }
            DiffKind::Text => {}
        }
        if diff.is_identical() {
            let detail = if diff.change.kind == ChangeKind::Renamed {
                Some(diff.display_path())
            } else if self.ignore_whitespace {
                Some("Only whitespace changed".into())
            } else {
                None
            };
            let title = match diff.change.kind {
                ChangeKind::Renamed => "File renamed without changes",
                ChangeKind::Added => "Empty file added",
                ChangeKind::Deleted => "Empty file deleted",
                _ => "No changes",
            };
            return message(IconName::CircleCheck, title.into(), detail);
        }
        match layout {
            Layout::Split => h_flex()
                .size_full()
                .items_start()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(self.old_view.clone()),
                )
                .child(div().w(px(1.)).h_full().bg(ui.border.hsla()))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(self.new_view.clone()),
                )
                .into_any_element(),
            Layout::Unified => div()
                .size_full()
                .child(self.unified_view.clone())
                .into_any_element(),
        }
    }
}

/// "+12 −3" with diff colors.
pub fn stats_label(s: DiffStats, theme: &lector_theme::Theme) -> impl IntoElement {
    h_flex()
        .flex_none()
        .gap(px(6.))
        .text_size(metrics::text_sm())
        .font_weight(FontWeight::MEDIUM)
        .child(
            div()
                .text_color(theme.diff.added_text.hsla())
                .child(format!("+{}", s.added)),
        )
        .child(
            div()
                .text_color(theme.diff.removed_text.hsla())
                .child(format!("−{}", s.removed)),
        )
}

/// A "Viewed" checkbox pill.
pub fn viewed_checkbox(id: &'static str, viewed: bool, cx: &App) -> gpui::Stateful<gpui::Div> {
    let ui = &cx.theme().ui;
    let accent = ui.accent.hsla();
    h_flex()
        .id(id)
        .flex_none()
        .h(px(24.))
        .px(px(8.))
        .gap(px(6.))
        .rounded(px(5.))
        .border_1()
        .border_color(if viewed {
            accent.opacity(0.5)
        } else {
            ui.border.hsla()
        })
        .when(viewed, |el| el.bg(accent.opacity(0.12)))
        .text_size(metrics::text_sm())
        .text_color(if viewed {
            ui.text.hsla()
        } else {
            ui.text_muted.hsla()
        })
        .cursor_pointer()
        .hover(|s| s.bg(ui.hover.hsla()))
        .tooltip(Tooltip::for_action(
            "Toggle Viewed",
            &diff_actions::ToggleViewed,
        ))
        .child(
            Icon::new(if viewed {
                IconName::CheckSquare
            } else {
                IconName::Square
            })
            .size(IconSize::Small)
            .color(if viewed { accent } else { ui.text_faint.hsla() }),
        )
        .child("Viewed")
}

impl Render for DiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.relayout_comments_if_resized(window, cx);
        // Focused while there was nothing to show (loading, "No changes"): hand
        // the focus to the code view once the diff appears.
        if self.focus_handle.is_focused(window) && self.shows_code_views() {
            let handle = Focusable::focus_handle(self, cx);
            window.focus(&handle);
        }
        let layout = self.layout(window, cx);
        if layout != self.last_layout {
            self.last_layout = layout;
            self.current_hunk = None;
        }
        let ui = cx.theme().ui.clone();
        let has_actions = !self.hunk_actions().is_empty();
        let toolbars = self.render_hunk_toolbars(window, cx);
        v_flex()
            .id("diff-view")
            .key_context(contexts::DIFF_VIEW)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(ui.background.hsla())
            .on_action(
                cx.listener(|this, _: &diff_actions::ToggleLayout, window, cx| {
                    this.toggle_layout(window, cx)
                }),
            )
            .on_action(cx.listener(
                |this, _: &lector_ui::actions::review::AddComment, window, cx| {
                    this.add_comment_at_cursor(window, cx)
                },
            ))
            .on_action(cx.listener(
                |this, _: &lector_ui::actions::review::NextComment, window, cx| {
                    if !this.go_to_comment(1, window, cx) {
                        cx.propagate();
                    }
                },
            ))
            .on_action(cx.listener(
                |this, _: &lector_ui::actions::review::PrevComment, window, cx| {
                    if !this.go_to_comment(-1, window, cx) {
                        cx.propagate();
                    }
                },
            ))
            .on_action(cx.listener(|this, _: &diff_actions::NextHunk, _, cx| {
                if !this.go_to_hunk(1, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &diff_actions::PrevHunk, _, cx| {
                if !this.go_to_hunk(-1, cx) {
                    cx.propagate();
                }
            }))
            .on_action(
                cx.listener(|this, _: &diff_actions::ToggleIgnoreWhitespace, _, cx| {
                    this.toggle_ignore_whitespace(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &diff_actions::ExpandAllContext, _, cx| {
                    this.toggle_expand_all(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &diff_actions::OpenInEditor, window, cx| {
                    this.open_in_editor(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &diff_actions::ToggleMovedCode, _, cx| {
                    this.toggle_moved_code(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &diff_actions::ToggleStructural, _, cx| {
                    this.toggle_structural(cx)
                }),
            )
            .on_action(cx.listener(
                |this, _: &lector_ui::actions::review::ToggleHunkReviewed, _, cx| {
                    this.toggle_hunk_reviewed_at_cursor(cx)
                },
            ))
            .on_action(cx.listener(
                |this, _: &lector_ui::actions::review::NextUnreviewed, _, cx| {
                    this.next_unreviewed_action(cx)
                },
            ))
            .on_action(cx.listener(|this, _: &diff_actions::ToggleViewed, _, cx| {
                if this.viewed.is_some() {
                    this.toggle_viewed(cx)
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &git_actions::StageHunk, window, cx| {
                if this.hunk_actions().contains(&HunkAction::Stage) {
                    this.hunk_action(HunkAction::Stage, None, false, window, cx)
                } else {
                    cx.propagate();
                }
            }))
            .on_action(
                cx.listener(|this, _: &git_actions::UnstageHunk, window, cx| {
                    if this.hunk_actions().contains(&HunkAction::Unstage) {
                        this.hunk_action(HunkAction::Unstage, None, false, window, cx)
                    } else {
                        cx.propagate();
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &git_actions::DiscardHunk, window, cx| {
                    if this.hunk_actions().contains(&HunkAction::Discard) {
                        this.hunk_action(HunkAction::Discard, None, false, window, cx)
                    } else {
                        cx.propagate();
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &git_actions::StageSelectedLines, window, cx| {
                    if this.hunk_actions().contains(&HunkAction::Stage) {
                        this.hunk_action(HunkAction::Stage, None, true, window, cx)
                    } else {
                        cx.propagate();
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &git_actions::UnstageSelectedLines, window, cx| {
                    if this.hunk_actions().contains(&HunkAction::Unstage) {
                        this.hunk_action(HunkAction::Unstage, None, true, window, cx)
                    } else {
                        cx.propagate();
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &git_actions::Stage, window, cx| {
                this.file_action(HunkAction::Stage, window, cx)
            }))
            .on_action(cx.listener(|this, _: &git_actions::Unstage, window, cx| {
                this.file_action(HunkAction::Unstage, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &git_actions::DiscardChanges, window, cx| {
                    this.file_action(HunkAction::Discard, window, cx)
                }),
            )
            .child(self.render_header(layout, cx))
            .children(self.render_structural_notice(cx))
            .when_some(self.op_error.clone(), |el, e| {
                el.child(
                    h_flex()
                        .flex_none()
                        .items_start()
                        .gap(px(8.))
                        .px(px(12.))
                        .py(px(6.))
                        .border_b_1()
                        .border_color(ui.error.hsla().opacity(0.6))
                        .bg(ui.error.hsla().opacity(0.08))
                        .child(
                            Icon::new(IconName::CircleX)
                                .size(IconSize::Small)
                                .color(ui.error.hsla()),
                        )
                        .child(
                            div()
                                .id("diff-op-error")
                                .flex_1()
                                .max_h(px(120.))
                                .overflow_y_scroll()
                                .text_size(metrics::text_sm())
                                .text_color(ui.text.hsla())
                                .child(e),
                        )
                        .child(
                            IconButton::new("diff-op-error-close", IconName::Close)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Dismiss"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.op_error = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .when(has_actions, |el| {
                        el.capture_any_mouse_down(cx.listener(
                            |this, e: &MouseDownEvent, window, cx| {
                                if e.button == MouseButton::Right {
                                    cx.stop_propagation();
                                    this.deploy_menu(e.position, window, cx);
                                }
                            },
                        ))
                    })
                    .child(self.render_body(layout, cx))
                    .children(toolbars),
            )
            .children(self.menu.render())
            .children(self.render_provenance(window, cx))
    }
}

/// A diff in a pane tab.
pub struct DiffItem {
    /// The view.
    pub view: Entity<DiffView>,
    key: SharedString,
}

impl DiffItem {
    /// Wraps `view`; `key` identifies the tab (dedupe).
    pub fn new(view: Entity<DiffView>, key: impl Into<SharedString>) -> Self {
        DiffItem {
            view,
            key: key.into(),
        }
    }
}

impl PaneItem for DiffItem {
    fn title(&self, cx: &App) -> SharedString {
        let v = self.view.read(cx);
        if let Some(t) = &v.title_override {
            return t.clone();
        }
        let name = v
            .target
            .as_ref()
            .map(|t| split_path(t.path()).0.to_string())
            .unwrap_or_else(|| "Diff".into());
        format!("{name} (diff)").into()
    }

    fn icon(&self, _: &App) -> TabIcon {
        TabIcon::Ui(IconName::Diff)
    }

    fn path(&self, _: &App) -> Option<PathBuf> {
        None
    }

    fn item_key(&self, _: &App) -> Option<SharedString> {
        Some(self.key.clone())
    }

    fn tab_kind(&self, cx: &App) -> Option<lector_config::TabKind> {
        let v = self.view.read(cx);
        match (v.target.as_ref()?, &v.viewed) {
            (DiffTarget::FileVsRev { path, rev }, _) => Some(lector_config::TabKind::Other {
                label: format!("vs:{rev}:{path}"),
            }),
            (DiffTarget::Change(change), Some(viewed)) => Some(lector_config::TabKind::Diff {
                changeset_key: viewed.key.clone(),
                path: change.path().to_string(),
            }),
            (DiffTarget::Change(_), None) => None,
        }
    }

    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.read(cx).focus_handle(cx)
    }

    fn clone_item(&self, _window: &mut Window, cx: &mut App) -> Option<Box<dyn PaneItem>> {
        let (store, target, viewed, title) = {
            let v = self.view.read(cx);
            (
                v.store.clone(),
                v.target.clone()?,
                v.viewed.clone(),
                v.title_override.clone(),
            )
        };
        let view = cx.new(|cx| {
            let mut v = DiffView::new(store, true, cx);
            v.set_title(title);
            v.set_target(target, viewed, cx);
            v
        });
        Some(Box::new(DiffItem {
            view,
            key: self.key.clone(),
        }))
    }
}

/// Opens a diff tab for `target` in `workspace` (activating an existing one).
#[allow(clippy::too_many_arguments)]
pub fn open_diff(
    workspace: &WeakEntity<Workspace>,
    store: &Entity<GitStore>,
    target: DiffTarget,
    viewed: Option<ViewedTarget>,
    title: Option<SharedString>,
    key: String,
    preview: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(ws) = workspace.upgrade() else {
        return;
    };
    let pane = ws.read(cx).active_pane().clone();
    let key: SharedString = format!("diff:{key}").into();
    if let Some(ix) = pane.read(cx).index_for_key(&key, cx) {
        pane.update(cx, |p, cx| {
            if !preview {
                p.pin(ix, cx);
            }
            p.activate(ix, true, window, cx);
        });
        return;
    }
    let store = store.clone();
    let view = cx.new(|cx| {
        let mut v = DiffView::new(store, true, cx);
        v.set_title(title);
        v.set_target(target, viewed, cx);
        v
    });
    let item = Box::new(DiffItem::new(view, key));
    ws.update(cx, |ws, cx| ws.add_item(item, preview, window, cx));
}

/// Recreates a diff tab from the session (the change is looked up again).
pub fn restore_item(
    store: &Entity<GitStore>,
    kind: &lector_config::TabKind,
    cx: &mut App,
) -> Option<Box<dyn PaneItem>> {
    match kind {
        lector_config::TabKind::Other { label } => {
            let (rev, path) = label.strip_prefix("vs:")?.split_once(':')?;
            let (rev, path) = (rev.to_string(), path.to_string());
            let name = split_path(&path).0.to_string();
            let short = if rev.len() == 40 {
                rev[..7].to_string()
            } else {
                rev.clone()
            };
            let title: SharedString = format!("{name} vs {short}").into();
            let key = format!("diff:vs:{rev}:{path}");
            let store = store.clone();
            let view = cx.new(|cx| {
                let mut v = DiffView::new(store, true, cx);
                v.set_title(Some(title));
                v.set_target(DiffTarget::FileVsRev { path, rev }, None, cx);
                v
            });
            Some(Box::new(DiffItem::new(view, key)))
        }
        lector_config::TabKind::Diff {
            changeset_key,
            path,
        } => {
            let cs = review::parse_changeset_key(changeset_key)?;
            let repo = store.read(cx).repo()?.clone();
            let opts = settings_diff_options(store.read(cx), cx);
            let key = format!("diff:{changeset_key}:{path}");
            let (ck, path) = (changeset_key.clone(), path.clone());
            let store = store.clone();
            let view = cx.new(|cx| {
                let task = cx.background_spawn(async move {
                    let change = repo
                        .changes(&cs)
                        .ok()?
                        .into_iter()
                        .find(|c| c.path() == path)?;
                    let (stats, binary, hash) = review::file_summary(&repo, &change, &opts).ok()?;
                    Some((change, stats, binary, hash))
                });
                cx.spawn(async move |this, cx| {
                    let Some((change, stats, binary, hash)) = task.await else {
                        return;
                    };
                    let _ = this.update(cx, |v: &mut DiffView, cx| {
                        let viewed = {
                            let s = v.store.read(cx);
                            s.repo()
                                .map(|r| s.review.is_viewed(r.workdir(), &ck, change.path(), &hash))
                        }
                        .unwrap_or(false);
                        let file = ReviewFile {
                            display_path: change.path().to_string(),
                            change: change.clone(),
                            stats,
                            viewed,
                            content_hash: hash,
                            binary,
                        };
                        v.set_target(
                            DiffTarget::Change(change),
                            Some(ViewedTarget {
                                key: ck.clone(),
                                file,
                            }),
                            cx,
                        );
                    });
                })
                .detach();
                DiffView::new(store, true, cx)
            });
            Some(Box::new(DiffItem::new(view, key)))
        }
        _ => None,
    }
}

/// Default diff options from settings (used by the panels for stats).
pub fn settings_diff_options(store: &GitStore, cx: &App) -> DiffOptions {
    diff_options(&store.settings(cx).diff)
}
