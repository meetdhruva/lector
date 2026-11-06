//! Review comments in the diff view (v0.6, Agent FORGE): a "+" in the gutter of
//! the hovered line, inline threads rendered between rows (blank `Gap` rows
//! reserve the space, a row widget draws the card), expand / collapse, reply,
//! resolve, delete, markdown bodies, outdated / draft badges, and next / previous
//! comment navigation. Threads come from `lector_app::comments::CommentStore`
//! and are re-anchored against the revision shown each time a diff loads.

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    div, prelude::*, AnyElement, App, Entity, Focusable, FontWeight, KeyDownEvent, Pixels,
    SharedString, WeakEntity, Window,
};
use lector_app::comments::{
    self, Anchor, AnchorMatch, CommentStore, DiffSide, NewThread, Origin, Thread, ThreadFilter,
    ThreadState,
};
use lector_diff::RowKind;
use lector_diff::Segment;
use lector_ui::code_view::{
    CodeView, GutterIcon, InlineMarks, Pos, RowBackground, RowWidget, Selection, WidgetAnchor,
};
use lector_ui::components::text_area::TextArea;
use lector_ui::components::{h_flex, metrics, v_flex, Button, ButtonStyle, IconButton, Tooltip};
use lector_ui::icons::{Icon, IconName, IconSize};
use lector_ui::scale::px;
use lector_ui::views::Workspace;
use lector_ui::{ActiveTheme, ColorExt};
use lector_workspace::review::FileDiff;

use super::{DiffView, Layout};
use crate::diff_source::{DiffColumn, DisplayRow};
use crate::util::{now_secs, relative_time};

/// Owner key of the comment layers on the code views.
const LAYER: &str = "comments";

/// How comments behave in a diff (set by the review tab; pull-request reviews
/// read contents from the shadow repository and create drafts).
#[derive(Clone)]
pub struct ReviewContext {
    /// Review key threads are stored under (changeset key or `pr:…` key).
    pub key: String,
    /// Repository to load the diff from instead of the project's (PR shadow).
    pub repo: Option<lector_git::Repo>,
    /// New comments are drafts of a pending review.
    pub drafts: bool,
    /// The pull request, for replying / resolving remote threads.
    pub pr: Option<Arc<crate::pull_requests::PrSession>>,
}

impl std::fmt::Debug for ReviewContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewContext")
            .field("key", &self.key)
            .field("drafts", &self.drafts)
            .finish_non_exhaustive()
    }
}

/// Space reserved for one thread in a column's display rows.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GapInfo {
    /// First gap row (the anchored line is the row before it).
    pub first_row: u32,
    /// Gap rows.
    pub rows: u32,
    /// Index into the thread list.
    pub thread: usize,
}

/// Comment state of one diff view.
#[derive(Default)]
pub(crate) struct DiffComments {
    pub threads: Rc<Vec<Thread>>,
    /// Threads the user expanded / collapsed (overrides the state default).
    toggled: HashSet<u64>,
    /// Hovered (column, row): where the "+" is shown.
    hover: Option<(DiffColumn, u32)>,
    pub split_gaps: Rc<Vec<GapInfo>>,
    pub unified_gaps: Rc<Vec<GapInfo>>,
    /// Text width used for the last height estimate.
    width: Option<Pixels>,
    /// Thread to put the cursor on once it is laid out.
    pending_reveal: Option<u64>,
}

/// Where a display row of a column points: (side, line).
fn row_side_line(diff: &FileDiff, row: &DisplayRow, column: DiffColumn) -> Option<(DiffSide, u32)> {
    let DisplayRow::Row(ix) = row else {
        return None;
    };
    match column {
        DiffColumn::Old => diff.split.get(*ix)?.old.map(|l| (DiffSide::Old, l)),
        DiffColumn::New => diff.split.get(*ix)?.new.map(|l| (DiffSide::New, l)),
        DiffColumn::Unified => {
            let r = diff.unified.get(*ix)?;
            match r.kind {
                RowKind::Removed => r.old.map(|l| (DiffSide::Old, l)),
                _ => r.new.map(|l| (DiffSide::New, l)),
            }
        }
    }
}

/// Whether display row `row` is where `thread` hangs (its last line, on its side).
fn anchors(diff: &FileDiff, row: &DisplayRow, unified: bool, t: &Thread) -> bool {
    let DisplayRow::Row(ix) = row else {
        return false;
    };
    let end = t
        .anchor
        .end_line
        .unwrap_or(t.anchor.line)
        .max(t.anchor.line);
    let (old, new, kind) = if unified {
        match diff.unified.get(*ix) {
            Some(r) => (r.old, r.new, r.kind),
            None => return false,
        }
    } else {
        match diff.split.get(*ix) {
            Some(r) => (r.old, r.new, r.kind),
            None => return false,
        }
    };
    match t.anchor.side {
        DiffSide::New => new == Some(end) && (!unified || kind != RowKind::Removed),
        DiffSide::Old => old == Some(end) && (!unified || kind != RowKind::Added),
    }
}

impl DiffComments {
    fn is_expanded(&self, t: &Thread) -> bool {
        let default = t.state == ThreadState::Open;
        default != self.toggled.contains(&t.id)
    }

    /// Fold starts (segment row ranges) that hide an anchored line: shown expanded.
    pub(crate) fn folds_with_threads(
        &self,
        segments: &[Segment],
        diff: &FileDiff,
        unified: bool,
    ) -> HashSet<usize> {
        let mut out = HashSet::new();
        if self.threads.is_empty() {
            return out;
        }
        for seg in segments {
            if let Segment::Folded { rows } = seg {
                let hit = rows.clone().any(|ix| {
                    let row = DisplayRow::Row(ix);
                    self.threads.iter().any(|t| anchors(diff, &row, unified, t))
                });
                if hit {
                    out.insert(rows.start);
                }
            }
        }
        out
    }

    /// Inserts gap rows after anchored rows; returns the new rows and the gaps.
    pub(crate) fn insert_gaps(
        &self,
        rows: Vec<DisplayRow>,
        diff: &FileDiff,
        unified: bool,
        line_height: Pixels,
    ) -> (Vec<DisplayRow>, Vec<GapInfo>) {
        if self.threads.is_empty() {
            return (rows, Vec::new());
        }
        let width = self.width.unwrap_or(px(720.));
        let mut placed = vec![false; self.threads.len()];
        let mut out = Vec::with_capacity(rows.len() + self.threads.len() * 4);
        let mut gaps = Vec::new();
        for row in rows {
            let hits: Vec<usize> = self
                .threads
                .iter()
                .enumerate()
                .filter(|(i, t)| !placed[*i] && anchors(diff, &row, unified, t))
                .map(|(i, _)| i)
                .collect();
            out.push(row);
            for i in hits {
                placed[i] = true;
                let t = &self.threads[i];
                let n = thread_rows(t, self.is_expanded(t), line_height, width);
                gaps.push(GapInfo {
                    first_row: out.len() as u32,
                    rows: n,
                    thread: i,
                });
                out.extend((0..n).map(|_| DisplayRow::Gap(gaps.len() - 1)));
            }
        }
        // Threads whose line is not in the diff any more (outdated beyond the end)
        // hang after the last row.
        for (i, t) in self.threads.iter().enumerate() {
            if !placed[i] {
                let n = thread_rows(t, self.is_expanded(t), line_height, width);
                gaps.push(GapInfo {
                    first_row: out.len() as u32,
                    rows: n,
                    thread: i,
                });
                out.extend((0..n).map(|_| DisplayRow::Gap(gaps.len() - 1)));
            }
        }
        (out, gaps)
    }
}

/// Rows a thread card needs (estimated from its text; generous).
fn thread_rows(t: &Thread, expanded: bool, lh: Pixels, width: Pixels) -> u32 {
    let lh = f32::from(lh).max(10.);
    if !expanded {
        return ((36.0 / lh).ceil() as u32).max(1);
    }
    // The card is at most 860 px wide; ~6.6 px per character of 12 px UI text.
    let card = f32::from(width).min(900.) - 40.;
    let chars = ((card - 24.) / 6.6).max(24.) as usize;
    let mut h = 44.0; // header + padding
    if t.state == ThreadState::Outdated || t.matched == AnchorMatch::Lost {
        h += 30.;
    }
    for (k, c) in t.comments.iter().enumerate() {
        if k > 0 {
            h += 26.; // author line + separator
        }
        let mut in_code = false;
        for line in c.body.lines() {
            if line.trim_start().starts_with("```") {
                in_code = !in_code;
                h += 8.;
                continue;
            }
            let n = line.chars().count();
            let wraps = if in_code { 1 } else { n.div_ceil(chars).max(1) };
            h += 19.5 * wraps as f32;
        }
        h += 6.;
    }
    h += 40.; // actions
    (h / lh).ceil() as u32
}

/// Where comments of this view live: (store, repo workdir, review key, path, old path).
pub(crate) struct CommentQuery {
    pub store: Arc<CommentStore>,
    pub workdir: PathBuf,
    pub key: String,
}

/// Re-anchors the file's threads against the loaded revision and lists them.
pub(crate) fn load_threads(q: &CommentQuery, diff: &FileDiff) -> Vec<Thread> {
    let repo = q.workdir.as_path();
    let mut paths: Vec<(DiffSide, String, Option<String>)> = Vec::new();
    for (side, s) in [(DiffSide::New, &diff.new), (DiffSide::Old, &diff.old)] {
        if let (Some(path), Some(text)) = (&s.path, &s.text) {
            paths.push((side, path.clone(), Some(text.to_string())));
        }
    }
    for (side, path, text) in &paths {
        if let Some(text) = text {
            if let Err(err) =
                q.store
                    .reanchor_path(repo, Some(&q.key), path, *side, text, None, &|_| None)
            {
                tracing::debug!("re-anchoring comments of {path}: {err}");
            }
        }
    }
    threads_for(q, diff)
}

fn threads_for(q: &CommentQuery, diff: &FileDiff) -> Vec<Thread> {
    let new = diff.new.path.clone();
    let old = diff.old.path.clone();
    q.store
        .threads(&q.workdir, &ThreadFilter::changeset(q.key.clone()))
        .unwrap_or_default()
        .into_iter()
        .filter(|t| match t.anchor.side {
            DiffSide::New => Some(&t.anchor.path) == new.as_ref(),
            DiffSide::Old => Some(&t.anchor.path) == old.as_ref(),
        })
        .collect()
}

impl DiffView {
    /// Sets how comments behave (review key, PR context). Reloads.
    pub fn set_review_context(&mut self, ctx: Option<ReviewContext>, cx: &mut Context<Self>) {
        self.review_ctx = ctx.map(Rc::new);
        if self.target.is_some() {
            self.reload(cx);
        }
    }

    /// The review context.
    pub fn review_context(&self) -> Option<&ReviewContext> {
        self.review_ctx.as_deref()
    }

    /// Review key comments are stored under.
    pub(crate) fn review_key(&self) -> Option<String> {
        if let Some(c) = &self.review_ctx {
            return Some(c.key.clone());
        }
        if let Some(v) = &self.viewed {
            return Some(v.key.clone());
        }
        match self.target.as_ref()? {
            super::DiffTarget::Change(_) => self
                .changeset()
                .map(|cs| lector_workspace::review::changeset_key(&cs)),
            super::DiffTarget::FileVsRev { rev, .. } => Some(format!("workdir:{rev}")),
        }
    }

    pub(crate) fn comment_query(&self, cx: &App) -> Option<CommentQuery> {
        let workdir = self.store.read(cx).repo()?.workdir().to_path_buf();
        Some(CommentQuery {
            store: CommentStore::for_core(&lector_ui::commands_bridge::core(cx)),
            workdir,
            key: self.review_key()?,
        })
    }

    pub(crate) fn set_threads(&mut self, threads: Vec<Thread>) {
        self.comments.threads = Rc::new(threads);
    }

    /// Re-reads the threads of the shown file (after a change) and re-lays out.
    pub(crate) fn refresh_threads(&mut self, cx: &mut Context<Self>) {
        let (Some(q), Some(loaded)) = (self.comment_query(cx), self.loaded.as_ref()) else {
            return;
        };
        let threads = threads_for(&q, &loaded.diff);
        self.comments.threads = Rc::new(threads);
        self.rebuild(cx);
    }

    /// Updates the cached text width (for card height estimates).
    pub(crate) fn note_comment_width(&mut self, cx: &App) {
        let view = match self.last_layout {
            Layout::Split => &self.new_view,
            Layout::Unified => &self.unified_view,
        };
        if let Some(w) = view.read(cx).text_width() {
            self.comments.width = Some(w);
        }
    }

    /// Re-estimates thread heights when the text area width changed (first paint,
    /// layout switch, window resize).
    pub(crate) fn relayout_comments_if_resized(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.comments.threads.is_empty() || self.loaded.is_none() {
            return;
        }
        let view = match self.last_layout {
            Layout::Split => &self.new_view,
            Layout::Unified => &self.unified_view,
        };
        let Some(w) = view.read(cx).text_width() else {
            return;
        };
        let changed = self
            .comments
            .width
            .is_none_or(|old| (f32::from(old) - f32::from(w)).abs() > 24.);
        if changed {
            self.comments.width = Some(w);
            cx.on_next_frame(window, |this, _, cx| this.rebuild(cx));
        }
    }

    /// Display rows with comment gaps (and folds around comments opened).
    pub(crate) fn rows_with_comments(
        &mut self,
        diff: &FileDiff,
        segments: &[Segment],
        unified: bool,
        line_height: Pixels,
    ) -> Vec<DisplayRow> {
        let mut expanded = self.expanded.clone();
        expanded.extend(self.comments.folds_with_threads(segments, diff, unified));
        let rows = crate::diff_source::display_rows(segments, &expanded, self.expand_all);
        let (rows, gaps) = self.comments.insert_gaps(rows, diff, unified, line_height);
        if unified {
            self.comments.unified_gaps = Rc::new(gaps);
        } else {
            self.comments.split_gaps = Rc::new(gaps);
        }
        rows
    }

    /// Installs thread cards, gap backgrounds and the "+" gutter icon on the views.
    pub(crate) fn update_comment_layers(&mut self, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        let threads = self.comments.threads.clone();
        let theme = cx.theme().clone();
        let gap_bg = theme.ui.surface.hsla();
        let lh = Self::line_height(cx);
        for (view, column) in [
            (self.old_view.clone(), DiffColumn::Old),
            (self.new_view.clone(), DiffColumn::New),
            (self.unified_view.clone(), DiffColumn::Unified),
        ] {
            let gaps = if column == DiffColumn::Unified {
                self.comments.unified_gaps.clone()
            } else {
                self.comments.split_gaps.clone()
            };
            let mut widgets = Vec::new();
            let mut backgrounds = Vec::new();
            for gap in gaps.iter() {
                let Some(t) = threads.get(gap.thread) else {
                    continue;
                };
                backgrounds.push(RowBackground {
                    rows: gap.first_row..gap.first_row + gap.rows,
                    color: gap_bg,
                    edge: None,
                });
                let on_this_column = match column {
                    DiffColumn::Unified => true,
                    DiffColumn::Old => t.anchor.side == DiffSide::Old,
                    DiffColumn::New => t.anchor.side == DiffSide::New,
                };
                if !on_this_column {
                    continue;
                }
                let card = ThreadCard {
                    thread: Rc::new(t.clone()),
                    expanded: self.comments.is_expanded(t),
                    height: lh * gap.rows as f32,
                    view: weak.clone(),
                    code_view: view.downgrade(),
                    remote: self
                        .review_ctx
                        .as_ref()
                        .and_then(|c| c.pr.clone())
                        .is_some(),
                };
                widgets.push(RowWidget {
                    row: gap.first_row,
                    anchor: WidgetAnchor::AfterText,
                    render: Rc::new(move |window, cx| card.render(window, cx)),
                });
            }
            view.update(cx, |v, cx| {
                v.set_row_widgets(LAYER, widgets, cx);
                v.set_row_backgrounds(LAYER, backgrounds, cx);
            });
        }
        self.update_comment_gutter(cx);
        if let Some(id) = self.comments.pending_reveal {
            if self.comments.threads.iter().any(|t| t.id == id) {
                self.comments.pending_reveal = None;
                self.place_cursor_on_thread(id, cx);
            }
        }
    }

    /// Shows thread `id` (expanded, cursor on its line), now or once its file loaded.
    pub fn reveal_thread(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = self.comments.threads.iter().find(|t| t.id == id) {
            if !self.comments.is_expanded(t) {
                if !self.comments.toggled.remove(&id) {
                    self.comments.toggled.insert(id);
                }
                self.rebuild(cx);
            }
            self.place_cursor_on_thread(id, cx);
            let (_, view) = self.active_column();
            let f = view.focus_handle(cx);
            window.focus(&f);
        } else {
            self.comments.pending_reveal = Some(id);
        }
    }

    fn place_cursor_on_thread(&mut self, id: u64, cx: &mut Context<Self>) {
        let (_, view) = self.active_column();
        let gaps = match self.last_layout {
            Layout::Split => self.comments.split_gaps.clone(),
            Layout::Unified => self.comments.unified_gaps.clone(),
        };
        let Some(g) = gaps.iter().find(|g| {
            self.comments
                .threads
                .get(g.thread)
                .is_some_and(|t| t.id == id)
        }) else {
            return;
        };
        let row = g.first_row.saturating_sub(1);
        view.update(cx, |v, cx| {
            v.set_selection(Selection::cursor(Pos::new(row, 0)), cx);
            v.scroll_to_row(row, true, cx);
        });
    }

    fn source_for(&self, column: DiffColumn) -> Option<Rc<crate::diff_source::DiffSource>> {
        let (o, n, u) = self.sources.as_ref()?;
        Some(match column {
            DiffColumn::Old => o.clone(),
            DiffColumn::New => n.clone(),
            DiffColumn::Unified => u.clone(),
        })
    }

    /// (side, line) of display row `row` of `column`.
    pub(crate) fn comment_target(&self, column: DiffColumn, row: u32) -> Option<(DiffSide, u32)> {
        let loaded = self.loaded.as_ref()?;
        let src = self.source_for(column)?;
        row_side_line(&loaded.diff, src.rows().get(row as usize)?, column)
    }

    /// The "+" on the hovered row and scrollbar marks for threads.
    fn update_comment_gutter(&mut self, cx: &mut Context<Self>) {
        let theme = cx.theme().clone();
        let can_comment = self.can_comment(cx);
        for (view, column) in [
            (self.old_view.clone(), DiffColumn::Old),
            (self.new_view.clone(), DiffColumn::New),
            (self.unified_view.clone(), DiffColumn::Unified),
        ] {
            let mut marks = InlineMarks::default();
            if can_comment {
                if let Some((c, row)) = self.comments.hover {
                    if c == column && self.comment_target(column, row).is_some() {
                        marks.gutter_icons.push(GutterIcon {
                            row,
                            icon: IconName::Plus,
                            color: theme.ui.accent.hsla(),
                            clickable: true,
                        });
                    }
                }
            }
            let gaps = if column == DiffColumn::Unified {
                &self.comments.unified_gaps
            } else {
                &self.comments.split_gaps
            };
            for g in gaps.iter() {
                let color = match self.comments.threads.get(g.thread).map(|t| t.state) {
                    Some(ThreadState::Open) => theme.ui.accent.hsla(),
                    Some(ThreadState::Outdated) => theme.ui.warning.hsla(),
                    _ => theme.ui.text_faint.hsla(),
                };
                marks.scrollbar.push((g.first_row, color));
            }
            view.update(cx, |v, cx| {
                if *v.inline_marks().as_ref() != marks {
                    v.set_inline_marks(marks, cx)
                }
            });
        }
    }

    /// Comments need a repository and a review key.
    pub(crate) fn can_comment(&self, cx: &App) -> bool {
        self.loaded.is_some() && self.comment_query(cx).is_some()
    }

    /// Pointer moved over `row` of `column` (keeps the last row while in the gutter).
    pub(crate) fn comment_hover(&mut self, column: DiffColumn, row: u32, cx: &mut Context<Self>) {
        if self.comments.hover != Some((column, row)) {
            self.comments.hover = Some((column, row));
            self.update_comment_gutter(cx);
        }
    }

    /// Installs the gutter-click handler on a code view.
    pub(crate) fn wire_comment_gutter(
        view: &Entity<CodeView>,
        column: DiffColumn,
        weak: WeakEntity<DiffView>,
        cx: &mut App,
    ) {
        view.update(cx, |v, _| {
            v.on_gutter_icon_click(move |row, window, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.start_comment_at(column, row, row, window, cx)
                });
            });
        });
    }

    fn active_column(&self) -> (DiffColumn, Entity<CodeView>) {
        match self.last_layout {
            Layout::Split => (DiffColumn::New, self.new_view.clone()),
            Layout::Unified => (DiffColumn::Unified, self.unified_view.clone()),
        }
    }

    /// `review::AddComment`: comments on the selection (or cursor line) of the
    /// focused column.
    pub(crate) fn add_comment_at_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (mut column, mut view) = self.active_column();
        if self.last_layout == Layout::Split && self.old_view.focus_handle(cx).is_focused(window) {
            column = DiffColumn::Old;
            view = self.old_view.clone();
        }
        let sel = view.read(cx).selection();
        let (a, b) = if sel.head.row <= sel.anchor.row {
            (sel.head.row, sel.anchor.row)
        } else {
            (sel.anchor.row, sel.head.row)
        };
        self.start_comment_at(column, a, b, window, cx);
    }

    /// Opens the composer for rows `first..=last` of `column`.
    pub(crate) fn start_comment_at(
        &mut self,
        column: DiffColumn,
        first: u32,
        last: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(q) = self.comment_query(cx) else {
            self.flash_comment_error("Comments need a git repository", cx);
            return;
        };
        let Some((side, end)) = self
            .comment_target(column, last)
            .or_else(|| self.comment_target(column, first))
        else {
            self.flash_comment_error("Put the cursor on a line of the diff to comment on it", cx);
            return;
        };
        let start = self
            .comment_target(column, first)
            .filter(|(s, l)| *s == side && *l < end)
            .map(|(_, l)| l);
        let Some(loaded) = self.loaded.as_ref() else {
            return;
        };
        let s = match side {
            DiffSide::New => &loaded.diff.new,
            DiffSide::Old => &loaded.diff.old,
        };
        let (Some(path), Some(text)) = (s.path.clone(), s.text.as_ref().map(|t| t.to_string()))
        else {
            return;
        };
        let line = start.unwrap_or(end);
        let end_line = start.map(|_| end);
        let Some(anchor) = Anchor::capture(&path, side, &text, line, end_line, &q.key, None) else {
            return;
        };
        q.store.remember_text(&text);
        let drafts = self.review_ctx.as_ref().is_some_and(|c| c.drafts);
        let title: SharedString = match end_line {
            Some(e) => format!("Comment on {path}:{}-{}", line + 1, e + 1),
            None => format!("Comment on {path}:{}", line + 1),
        }
        .into();
        let snippet: SharedString = anchor.text.trim().to_string().into();
        let weak = cx.entity().downgrade();
        let workdir = q.workdir.clone();
        let store = q.store.clone();
        self.open_composer(
            ComposerSpec {
                title,
                snippet: Some(snippet),
                drafts,
                placeholder: "Leave a comment (Markdown). Ctrl+Enter to save".into(),
            },
            window,
            cx,
            move |body, draft, _window, cx| {
                let author = comments::local_author(&workdir);
                let result = store.add(NewThread {
                    repo: workdir.clone(),
                    anchor,
                    body,
                    author,
                    draft,
                    ..Default::default()
                });
                let _ = weak.update(cx, |this, cx| match result {
                    Ok(_) => this.refresh_threads(cx),
                    Err(e) => this.flash_comment_error(&e.to_string(), cx),
                });
            },
        );
    }

    pub(crate) fn flash_comment_error(&mut self, msg: &str, cx: &mut Context<Self>) {
        self.op_error = Some(SharedString::from(msg.to_string()));
        cx.notify();
    }

    fn open_composer(
        &mut self,
        spec: ComposerSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_submit: impl FnOnce(String, bool, &mut Window, &mut App) + 'static,
    ) {
        let Some(ws) = self.store.read(cx).workspace.upgrade() else {
            return;
        };
        show_composer(&ws, spec, window, cx, on_submit);
    }

    fn toggle_thread(&mut self, id: u64, cx: &mut Context<Self>) {
        if !self.comments.toggled.insert(id) {
            self.comments.toggled.remove(&id);
        }
        self.rebuild(cx);
    }

    fn thread_by_id(&self, id: u64) -> Option<Thread> {
        self.comments.threads.iter().find(|t| t.id == id).cloned()
    }

    fn reply_to(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(q), Some(t)) = (self.comment_query(cx), self.thread_by_id(id)) else {
            return;
        };
        let pr = self.review_ctx.as_ref().and_then(|c| c.pr.clone());
        let remote = t.remote_id.clone().filter(|_| t.origin != Origin::Local);
        // Replies to published threads of a pull request are posted right away;
        // replies to local or draft threads stay local / drafts.
        let drafts = self.review_ctx.as_ref().is_some_and(|c| c.drafts) && remote.is_none();
        let weak = cx.entity().downgrade();
        self.open_composer(
            ComposerSpec {
                title: format!("Reply on {}", t.location()).into(),
                snippet: Some(t.anchor.text.trim().to_string().into()),
                drafts,
                placeholder: if remote.is_some() {
                    "Reply (posted to the pull request). Ctrl+Enter to send".into()
                } else {
                    "Reply (Markdown). Ctrl+Enter to save".into()
                },
            },
            window,
            cx,
            move |body, draft, window, cx| {
                let workdir = q.workdir.clone();
                let store = q.store.clone();
                match (pr, remote) {
                    (Some(pr), Some(remote)) => {
                        let task = cx.background_spawn(async move {
                            let c = pr.reply(&remote, &body)?;
                            store
                                .add_remote_comment(
                                    &workdir,
                                    id,
                                    lector_app::comments::Comment {
                                        author: c.author,
                                        body: c.body,
                                        created: c.created,
                                        updated: c.created,
                                        remote_id: Some(c.id),
                                        ..Default::default()
                                    },
                                )
                                .map_err(|e| e.to_string())?;
                            Ok::<_, String>(())
                        });
                        window
                            .spawn(cx, async move |cx| {
                                let r = task.await;
                                let _ = weak.update(cx, |this, cx| match r {
                                    Ok(()) => this.refresh_threads(cx),
                                    Err(e) => this.flash_comment_error(&e, cx),
                                });
                            })
                            .detach();
                    }
                    _ => {
                        let author = comments::local_author(&workdir);
                        let r = store.reply(&workdir, id, &body, &author, draft);
                        let _ = weak.update(cx, |this, cx| match r {
                            Ok(_) => this.refresh_threads(cx),
                            Err(e) => this.flash_comment_error(&e.to_string(), cx),
                        });
                    }
                }
            },
        );
    }

    fn resolve_thread(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(q), Some(t)) = (self.comment_query(cx), self.thread_by_id(id)) else {
            return;
        };
        let resolved = t.state != ThreadState::Resolved;
        let pr = self.review_ctx.as_ref().and_then(|c| c.pr.clone());
        let remote = t.remote_id.clone().filter(|_| t.origin != Origin::Local);
        let weak = cx.entity().downgrade();
        let task = cx.background_spawn(async move {
            if let (Some(pr), Some(remote)) = (pr, remote) {
                pr.resolve(&remote, resolved)?;
            }
            q.store
                .set_resolved(&q.workdir, id, resolved)
                .map_err(|e| e.to_string())?;
            Ok::<_, String>(())
        });
        window
            .spawn(cx, async move |cx| {
                let r = task.await;
                let _ = weak.update(cx, |this, cx| match r {
                    Ok(()) => this.refresh_threads(cx),
                    Err(e) => this.flash_comment_error(&e, cx),
                });
            })
            .detach();
    }

    fn delete_thread(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(q) = self.comment_query(cx) else {
            return;
        };
        if let Err(e) = q.store.delete(&q.workdir, id) {
            self.flash_comment_error(&e.to_string(), cx);
        }
        self.refresh_threads(cx);
    }

    /// `review::NextComment` / `PrevComment`: moves the cursor to the next
    /// thread's line (expanding it).
    pub(crate) fn go_to_comment(
        &mut self,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (_, view) = self.active_column();
        let gaps = match self.last_layout {
            Layout::Split => self.comments.split_gaps.clone(),
            Layout::Unified => self.comments.unified_gaps.clone(),
        };
        let mut anchors: Vec<(u32, usize)> = gaps
            .iter()
            .map(|g| (g.first_row.saturating_sub(1), g.thread))
            .collect();
        anchors.sort();
        anchors.dedup_by_key(|a| a.0);
        if anchors.is_empty() {
            return false;
        }
        let cur = view.read(cx).cursor().row;
        let pick = if delta > 0 {
            anchors.iter().find(|(r, _)| *r > cur).or(anchors.first())
        } else {
            anchors
                .iter()
                .rev()
                .find(|(r, _)| *r < cur)
                .or(anchors.last())
        };
        let Some(&(row, thread)) = pick else {
            return false;
        };
        if let Some(t) = self.comments.threads.get(thread) {
            if !self.comments.is_expanded(t) {
                let id = t.id;
                self.toggle_thread(id, cx);
            }
        }
        view.update(cx, |v, cx| {
            v.set_selection(Selection::cursor(Pos::new(row, 0)), cx);
            v.scroll_to_row(row, true, cx);
        });
        let f = view.focus_handle(cx);
        window.focus(&f);
        true
    }
}

/// One thread card (row widget).
struct ThreadCard {
    thread: Rc<Thread>,
    expanded: bool,
    height: Pixels,
    view: WeakEntity<DiffView>,
    code_view: WeakEntity<CodeView>,
    remote: bool,
}

fn badge(label: &str, fg: gpui::Hsla, bg: gpui::Hsla) -> impl IntoElement {
    div()
        .px(px(6.))
        .py(px(1.))
        .rounded(px(9.))
        .bg(bg)
        .text_size(metrics::text_xs())
        .font_weight(FontWeight::MEDIUM)
        .text_color(fg)
        .child(label.to_string())
}

impl ThreadCard {
    fn render(&self, window: &mut Window, cx: &mut App) -> AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let t = &self.thread;
        let id = t.id;
        let width = self
            .code_view
            .upgrade()
            .and_then(|v| v.read(cx).text_width())
            .map(|w| (w - px(40.)).min(px(860.)).max(px(260.)))
            .unwrap_or(px(640.));
        let now = now_secs();
        let state_color = match t.state {
            ThreadState::Open => ui.accent.hsla(),
            ThreadState::Resolved => ui.success.hsla(),
            ThreadState::Outdated => ui.warning.hsla(),
        };
        let first = t.comments.first();
        let mut header = h_flex()
            .gap(px(6.))
            .h(px(24.))
            .child(
                Icon::new(match t.state {
                    ThreadState::Resolved => IconName::CircleCheck,
                    ThreadState::Outdated => IconName::AlertTriangle,
                    ThreadState::Open => IconName::CircleDot,
                })
                .size(IconSize::Small)
                .color(state_color),
            )
            .child(
                div()
                    .text_size(metrics::text_sm())
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui.text.hsla())
                    .child(first.map(|c| display_author(&c.author)).unwrap_or_default()),
            );
        if !self.expanded {
            header = header.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(metrics::text_sm())
                    .text_color(ui.text_muted.hsla())
                    .child(t.summary()),
            );
            if t.comments.len() > 1 {
                header = header.child(
                    div()
                        .text_size(metrics::text_xs())
                        .text_color(ui.text_faint.hsla())
                        .child(format!("{} replies", t.comments.len() - 1)),
                );
            }
        } else {
            header = header.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(metrics::text_xs())
                    .text_color(ui.text_faint.hsla())
                    .child(format!(
                        "on {} · {}",
                        t.location(),
                        first
                            .map(|c| relative_time(c.created, now))
                            .unwrap_or_default()
                    )),
            );
        }
        let faint_bg = ui.hover.hsla();
        if t.draft_count() > 0 {
            header = header.child(badge(
                "Pending",
                ui.accent_foreground.hsla(),
                ui.accent.hsla(),
            ));
        }
        match t.state {
            ThreadState::Outdated => {
                header = header.child(badge("Outdated", ui.warning.hsla(), faint_bg))
            }
            ThreadState::Resolved => {
                header = header.child(badge("Resolved", ui.success.hsla(), faint_bg))
            }
            ThreadState::Open => {}
        }
        if t.matched == AnchorMatch::Fuzzy {
            header = header.child(
                div()
                    .id(SharedString::from(format!("thread-moved-{id}")))
                    .child(badge(
                        &format!("Re-anchored {:.0}%", t.confidence * 100.),
                        ui.text_muted.hsla(),
                        faint_bg,
                    ))
                    .tooltip(Tooltip::text(
                        "The code changed since this comment; Lector found the most similar line.",
                    )),
            );
        }
        match t.origin {
            Origin::Github => {
                header = header.child(badge("GitHub", ui.text_muted.hsla(), faint_bg))
            }
            Origin::Gitlab => {
                header = header.child(badge("GitLab", ui.text_muted.hsla(), faint_bg))
            }
            Origin::Local => {}
        }
        let view = self.view.clone();
        header = header.child(
            IconButton::new(
                SharedString::from(format!("thread-toggle-{id}")),
                if self.expanded {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                },
            )
            .icon_size(IconSize::Small)
            .tooltip(Tooltip::text(if self.expanded {
                "Collapse"
            } else {
                "Expand"
            }))
            .on_click(move |_, _, cx| {
                let _ = view.update(cx, |this, cx| this.toggle_thread(id, cx));
            }),
        );

        let mut card = v_flex()
            .id(SharedString::from(format!("thread-{id}")))
            .w(width)
            .h(self.height - px(6.))
            .mt(px(3.))
            .px(px(10.))
            .py(px(4.))
            .gap(px(4.))
            .overflow_hidden()
            .rounded(metrics::radius_lg())
            .border_1()
            .border_color(ui.border.hsla())
            .border_l_2()
            .bg(ui.elevated.hsla())
            .shadow_sm()
            .cursor_default()
            .child(header);
        if !self.expanded {
            let view = self.view.clone();
            return card
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.toggle_thread(id, cx));
                })
                .border_color(state_color.opacity(0.6))
                .into_any_element();
        }
        if t.state == ThreadState::Outdated || t.matched == AnchorMatch::Lost {
            card = card.child(
                h_flex()
                    .gap(px(6.))
                    .px(px(6.))
                    .py(px(2.))
                    .rounded(metrics::radius())
                    .bg(ui.surface.hsla())
                    .text_size(metrics::text_xs())
                    .text_color(ui.text_faint.hsla())
                    .child(format!("Line {} was:", t.original.line + 1))
                    .child(
                        div()
                            .font_family(lector_ui::code_view::EditorSettings::get(cx).font_family)
                            .text_color(ui.text_muted.hsla())
                            .line_through()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(t.original.text.trim().to_string()),
                    ),
            );
        }
        for (k, c) in t.comments.iter().enumerate() {
            let blocks = lector_ui::markdown::parse(&c.body);
            let opts = lector_ui::markdown::RenderOptions {
                base_dir: None,
                on_link: Some(Rc::new(
                    |target: &str, _window: &mut Window, cx: &mut App| {
                        if target.contains("://") {
                            cx.open_url(target);
                        }
                    },
                )),
                compact: true,
                id: format!("thread-{id}-c{k}").into(),
            };
            let body = lector_ui::markdown::render_blocks(&blocks, &opts, window, cx);
            card = card.child(
                v_flex()
                    .gap(px(1.))
                    .when(k > 0, |el| {
                        el.pt(px(4.))
                            .border_t_1()
                            .border_color(ui.border_variant.hsla())
                    })
                    .when(k > 0, |el| {
                        el.child(
                            h_flex()
                                .gap(px(6.))
                                .text_size(metrics::text_xs())
                                .when(k > 0, |el| {
                                    el.child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(ui.text.hsla())
                                            .child(display_author(&c.author)),
                                    )
                                })
                                .child(
                                    div()
                                        .text_color(ui.text_faint.hsla())
                                        .child(relative_time(c.created, now)),
                                )
                                .when(c.draft, |el| {
                                    el.child(div().text_color(ui.accent.hsla()).child("draft"))
                                }),
                        )
                    })
                    .child(
                        v_flex()
                            .text_size(metrics::text_sm())
                            .text_color(ui.text.hsla())
                            .children(body),
                    ),
            );
        }
        let (v1, v2, v3) = (self.view.clone(), self.view.clone(), self.view.clone());
        let resolved = t.state == ThreadState::Resolved;
        let deletable = t.origin == Origin::Local || t.is_draft();
        card = card.child(div().flex_1()).child(
            h_flex()
                .gap(px(6.))
                .pb(px(2.))
                .child(
                    Button::new(SharedString::from(format!("thread-reply-{id}")), "Reply")
                        .icon(IconName::ArrowLeft)
                        .style(ButtonStyle::Outlined)
                        .on_click(move |_, window, cx| {
                            let _ = v1.update(cx, |this, cx| this.reply_to(id, window, cx));
                        }),
                )
                .child(
                    Button::new(
                        SharedString::from(format!("thread-resolve-{id}")),
                        if resolved { "Reopen" } else { "Resolve" },
                    )
                    .icon(if resolved {
                        IconName::Circle
                    } else {
                        IconName::Check
                    })
                    .style(ButtonStyle::Subtle)
                    .on_click(move |_, window, cx| {
                        let _ = v2.update(cx, |this, cx| this.resolve_thread(id, window, cx));
                    }),
                )
                .child(div().flex_1())
                .when(deletable, |el| {
                    el.child(
                        IconButton::new(
                            SharedString::from(format!("thread-delete-{id}")),
                            IconName::Trash,
                        )
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text(if self.remote && !t.is_draft() {
                            "Delete locally"
                        } else {
                            "Delete thread"
                        }))
                        .on_click(move |_, _, cx| {
                            let _ = v3.update(cx, |this, cx| this.delete_thread(id, cx));
                        }),
                    )
                }),
        );
        card.border_color(state_color.opacity(0.7))
            .into_any_element()
    }
}

/// "you" for an empty author.
pub(crate) fn display_author(a: &str) -> String {
    if a.is_empty() {
        "you".into()
    } else {
        a.to_string()
    }
}

/// What the composer shows.
pub(crate) struct ComposerSpec {
    pub title: SharedString,
    pub snippet: Option<SharedString>,
    /// Offer "Add to review" (draft) as the primary action.
    pub drafts: bool,
    pub placeholder: SharedString,
}

type SubmitFn = Box<dyn FnOnce(String, bool, &mut Window, &mut App)>;

/// Modal comment editor (markdown, Ctrl/Cmd+Enter saves, Escape cancels).
pub struct CommentComposer {
    spec: ComposerSpec,
    editor: Entity<TextArea>,
    on_submit: Option<SubmitFn>,
    focus_handle: gpui::FocusHandle,
}

impl gpui::EventEmitter<gpui::DismissEvent> for CommentComposer {}

impl gpui::Focusable for CommentComposer {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl CommentComposer {
    fn submit(&mut self, draft: bool, window: &mut Window, cx: &mut Context<Self>) {
        let body = self.editor.read(cx).text().trim().to_string();
        if body.is_empty() {
            return;
        }
        let f = self.on_submit.take();
        cx.emit(gpui::DismissEvent);
        if let Some(f) = f {
            f(body, draft, window, cx);
        }
    }
}

impl Render for CommentComposer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let drafts = self.spec.drafts;
        let empty = self.editor.read(cx).is_empty();
        let mono = lector_ui::code_view::EditorSettings::get(cx).font_family;
        v_flex()
            .id("comment-composer")
            .key_context("CommentComposer")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(move |this, e: &KeyDownEvent, window, cx| {
                let k = &e.keystroke;
                if k.key == "enter" && (k.modifiers.control || k.modifiers.platform) {
                    this.submit(drafts, window, cx);
                    cx.stop_propagation();
                } else if k.key == "escape" {
                    this.on_submit = None;
                    cx.emit(gpui::DismissEvent);
                    cx.stop_propagation();
                }
            }))
            .w(px(560.))
            .p(px(14.))
            .gap(px(10.))
            .rounded(metrics::radius_lg())
            .border_1()
            .border_color(ui.border.hsla())
            .bg(ui.elevated.hsla())
            .shadow_lg()
            .child(
                div()
                    .text_size(metrics::text())
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui.text.hsla())
                    .child(self.spec.title.clone()),
            )
            .when_some(self.spec.snippet.clone(), |el, s| {
                el.child(
                    div()
                        .px(px(8.))
                        .py(px(4.))
                        .rounded(metrics::radius())
                        .bg(ui.surface.hsla())
                        .font_family(mono)
                        .text_size(metrics::text_sm())
                        .text_color(ui.text_muted.hsla())
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(s),
                )
            })
            .child(
                div()
                    .min_h(px(96.))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(metrics::radius())
                    .border_1()
                    .border_color(ui.border_focused.hsla())
                    .bg(ui.background.hsla())
                    .child(self.editor.clone()),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(metrics::text_xs())
                            .text_color(ui.text_faint.hsla())
                            .child(if drafts {
                                "Drafts are published when you submit the review."
                            } else {
                                "Markdown supported. Stored by Lector, not in your repository."
                            }),
                    )
                    .child(
                        Button::new("composer-cancel", "Cancel")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.on_submit = None;
                                cx.emit(gpui::DismissEvent);
                            })),
                    )
                    .child(
                        Button::new(
                            "composer-save",
                            if drafts { "Add to review" } else { "Comment" },
                        )
                        .style(ButtonStyle::Accent)
                        .disabled(empty)
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.submit(drafts, window, cx)),
                        ),
                    ),
            )
    }
}

/// Shows the composer in `workspace`'s modal layer.
pub(crate) fn show_composer(
    workspace: &Entity<Workspace>,
    spec: ComposerSpec,
    window: &mut Window,
    cx: &mut App,
    on_submit: impl FnOnce(String, bool, &mut Window, &mut App) + 'static,
) {
    let layer = workspace.read(cx).modal_layer().clone();
    let placeholder = spec.placeholder.clone();
    let composer = cx.new(|cx| {
        let editor = cx.new(|cx| {
            let mut a = TextArea::new(placeholder, cx);
            a.set_line_limits(4, 14, cx);
            a
        });
        cx.subscribe(
            &editor,
            |_, _, _: &lector_ui::components::text_area::TextAreaEvent, cx| cx.notify(),
        )
        .detach();
        CommentComposer {
            spec,
            editor,
            on_submit: Some(Box::new(on_submit)),
            focus_handle: cx.focus_handle(),
        }
    });
    layer.update(cx, |layer, cx| {
        layer.show_modal(composer.clone(), window, cx)
    });
    let f = composer.read(cx).editor.read(cx).focus_handle(cx);
    window.focus(&f);
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use lector_diff::DiffOptions;
    use lector_git::{ChangeKind, FileChange, Side};
    use lector_syntax::LanguageRegistry;
    use lector_workspace::review::diff_bytes;

    use super::*;

    fn diff(old: &str, new: &str) -> FileDiff {
        let change = FileChange {
            old_path: Some("a.rs".into()),
            new_path: Some("a.rs".into()),
            kind: ChangeKind::Modified,
            old: Side::None,
            new: Side::None,
            binary: None,
        };
        diff_bytes(
            change,
            Some(old.as_bytes().to_vec()),
            Some(new.as_bytes().to_vec()),
            &LanguageRegistry::new(),
            &DiffOptions::default(),
        )
    }

    fn thread(side: DiffSide, line: u32, state: ThreadState) -> Thread {
        let anchor = Anchor {
            path: "a.rs".into(),
            side,
            line,
            ..Default::default()
        };
        Thread {
            id: line as u64 + 1,
            anchor: anchor.clone(),
            original: anchor,
            state,
            comments: vec![lector_app::comments::Comment {
                body: "Why?".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn text(n: usize) -> String {
        (0..n).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn gaps_follow_anchored_rows_in_both_layouts() {
        let old = text(40);
        let new = old.replace("line 20\n", "line twenty\n");
        let d = diff(&old, &new);
        let comments = DiffComments {
            threads: Rc::new(vec![
                thread(DiffSide::New, 20, ThreadState::Open),
                thread(DiffSide::Old, 20, ThreadState::Resolved),
                thread(DiffSide::New, 2, ThreadState::Open),
            ]),
            ..Default::default()
        };
        for unified in [false, true] {
            let segments = if unified {
                d.unified_segments(3)
            } else {
                d.split_segments(3)
            };
            // Line 2 is folded away: its fold opens.
            let open = comments.folds_with_threads(&segments, &d, unified);
            assert_eq!(open.len(), 1, "unified={unified}");
            let rows = crate::diff_source::display_rows(&segments, &open, false);
            let (rows, gaps) = comments.insert_gaps(rows, &d, unified, px(20.));
            assert_eq!(gaps.len(), 3);
            for g in &gaps {
                // Stacked threads of one row: walk back over gap rows.
                let anchor_row = rows[..g.first_row as usize]
                    .iter()
                    .rev()
                    .find(|r| !matches!(r, DisplayRow::Gap(_)))
                    .expect("a row before the gap");
                let t = &comments.threads[g.thread];
                let (side, line) = row_side_line(
                    &d,
                    anchor_row,
                    match (unified, t.anchor.side) {
                        (true, _) => DiffColumn::Unified,
                        (false, DiffSide::Old) => DiffColumn::Old,
                        (false, DiffSide::New) => DiffColumn::New,
                    },
                )
                .expect("anchored on a line");
                assert_eq!((side, line), (t.anchor.side, t.anchor.line));
                assert!(rows[g.first_row as usize..(g.first_row + g.rows) as usize]
                    .iter()
                    .all(|r| matches!(r, DisplayRow::Gap(_))));
            }
            // Collapsed (resolved) threads take less room than open ones.
            let resolved = gaps.iter().find(|g| g.thread == 1).map(|g| g.rows);
            let open_rows = gaps.iter().find(|g| g.thread == 0).map(|g| g.rows);
            assert!(resolved < open_rows);
            // Hunk starts ignore gaps.
            let starts = crate::diff_source::hunk_display_starts(&d, &rows, unified);
            assert_eq!(starts.len(), 1);
        }
        let none = DiffComments::default();
        let segments = d.split_segments(3);
        let rows = crate::diff_source::display_rows(&segments, &HashSet::new(), false);
        let n = rows.len();
        assert_eq!(none.insert_gaps(rows, &d, false, px(20.)).0.len(), n);
    }

    #[test]
    fn long_bodies_need_more_rows() {
        let mut t = thread(DiffSide::New, 1, ThreadState::Open);
        let short = thread_rows(&t, true, px(20.), px(600.));
        t.comments[0].body = "word ".repeat(400);
        assert!(thread_rows(&t, true, px(20.), px(600.)) > short + 5);
        assert!(thread_rows(&t, false, px(20.), px(600.)) <= 2);
    }
}
