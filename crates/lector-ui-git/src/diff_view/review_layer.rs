//! The v0.4 review layer of the [`DiffView`]: moved code (dimmed, with a clickable "Moved from
//! ..." label), the optional structural diff, reviewed hunks (persisted per changeset) and the
//! "why did this line change?" hover.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    anchored, deferred, div, prelude::*, AnyElement, App, Context, Corner, Entity, Hsla, Pixels,
    Point, SharedString, Task, Window,
};
use lector_app::provenance::{explanation_markdown, LineQuery};
use lector_app::review_queue::{
    hunk_fingerprints, intra_file_moves, FileMoves, HunkFingerprint, HunkReviewStore, MoveSpan,
};
use lector_git::{Changeset, Side};
use lector_theme::Theme;
use lector_ui::actions::diff as diff_actions;
use lector_ui::code_view::{
    CodeView, CodeViewEvent, HoverTarget, Pos, RowBackground, RowWidget, Selection, WidgetAnchor,
};
use lector_ui::components::{h_flex, metrics, IconButton, Tooltip};
use lector_ui::icons::{Icon, IconName, IconSize};
use lector_ui::markdown::{self, RenderOptions};
use lector_ui::scale::px;
use lector_ui::{ActiveTheme, ColorExt};
use lector_workspace::review::{DiffKind, FileDiff};
use lector_workspace::structural::{self, Budget};

use super::{DiffView, DiffViewEvent, Layout};
use crate::diff_source::{DiffColumn, DiffSource, MovedOverlay, OverlayColors, StructuralMarks};

/// Largest file (bytes, per side) the structural diff is attempted on.
const STRUCTURAL_MAX_BYTES: u64 = 600_000;

/// Owner key of the moved-code layers on a code view.
const MOVED_LAYER: &str = "review.moved";

/// Result of the structural diff of a loaded file.
#[derive(Clone, Debug)]
pub(super) enum StructuralOutcome {
    /// Not requested or not applicable (added / deleted file, no text).
    Off,
    /// Marks to paint.
    Marks(Arc<StructuralMarks>),
    /// Marks to paint, but part of the file was too large for the syntax-aware diff and keeps
    /// the line diff (`StructuralDiff::skipped` is not empty).
    PartialMarks(Arc<StructuralMarks>),
    /// Requested but not possible; the message tells why (line diff is shown).
    Fallback(&'static str),
}

/// Review data computed next to the diff on the background thread.
#[derive(Clone, Debug)]
pub(super) struct Extras {
    pub moves: Option<Arc<FileMoves>>,
    pub hunks: Arc<Vec<HunkFingerprint>>,
    pub structural: StructuralOutcome,
}

impl Extras {
    /// Computes the review data of `diff` (blocking).
    pub(super) fn compute(diff: &FileDiff, detect_moved: bool, structural_diff: bool) -> Extras {
        Extras {
            moves: detect_moved.then(|| Arc::new(intra_file_moves(diff))),
            hunks: Arc::new(hunk_fingerprints(diff)),
            structural: if structural_diff {
                structural_marks(diff)
            } else {
                StructuralOutcome::Off
            },
        }
    }
}

fn structural_marks(diff: &FileDiff) -> StructuralOutcome {
    if diff.kind != DiffKind::Text || !diff.old.exists || !diff.new.exists {
        return StructuralOutcome::Off;
    }
    let (Some(old_syntax), Some(new_syntax)) = (&diff.old.syntax, &diff.new.syntax) else {
        return StructuralOutcome::Fallback("no syntax support for this language");
    };
    let (Some(old_text), Some(new_text)) = (&diff.old.text, &diff.new.text) else {
        return StructuralOutcome::Off;
    };
    if diff.old.size > STRUCTURAL_MAX_BYTES || diff.new.size > STRUCTURAL_MAX_BYTES {
        return StructuralOutcome::Fallback("the file is too large");
    }
    match structural::diff(
        old_syntax,
        old_text,
        new_syntax,
        new_text,
        &Budget::default(),
    ) {
        Some(result) => {
            let marks = Arc::new(StructuralMarks::from_diff(
                Some(old_text),
                Some(new_text),
                &result,
            ));
            if result.skipped.is_empty() {
                StructuralOutcome::Marks(marks)
            } else {
                StructuralOutcome::PartialMarks(marks)
            }
        }
        None => StructuralOutcome::Fallback("the change is too complex"),
    }
}

/// The review layer state of a [`DiffView`].
pub(super) struct ReviewState {
    /// Show moved code dimmed (setting `review.detect_moved`, toggled by `diff::ToggleMovedCode`).
    pub show_moved: bool,
    /// Structural diff on (setting `review.structural`, toggled by `diff::ToggleStructural`).
    pub structural: bool,
    /// Moves computed inside the file when it loaded.
    pub intra_moves: Option<Arc<FileMoves>>,
    /// Moves of the whole changeset (from the review tab), covering cross-file moves.
    pub changeset_moves: Option<Arc<FileMoves>>,
    pub structural_marks: Option<Arc<StructuralMarks>>,
    /// Why the structural diff is not shown, when it was requested.
    pub notice: Option<SharedString>,
    pub hunks: Arc<Vec<HunkFingerprint>>,
    pub reviewed: Vec<bool>,
    pub hover: Option<ProvenanceHover>,
    pub hover_task: Option<Task<()>>,
    /// A line to show once the diff has loaded (after following a cross-file "moved" link).
    pub pending_line: Option<(bool, u32)>,
}

impl ReviewState {
    pub(super) fn new(settings: &lector_config::ReviewSettings) -> Self {
        ReviewState {
            show_moved: settings.detect_moved,
            structural: settings.structural,
            intra_moves: None,
            changeset_moves: None,
            structural_marks: None,
            notice: None,
            hunks: Arc::new(Vec::new()),
            reviewed: Vec::new(),
            hover: None,
            hover_task: None,
            pending_line: None,
        }
    }
}

/// The open "why" popover.
pub(super) struct ProvenanceHover {
    column: DiffColumn,
    row: u32,
    blocks: Arc<Vec<markdown::Block>>,
    position: Point<Pixels>,
    hovered: bool,
}

/// Colors of the moved / structural overlays derived from `theme`.
pub fn overlay_colors(theme: &Theme) -> OverlayColors {
    let accent = theme.git.renamed;
    OverlayColors {
        moved_bg: accent.with_alpha(0.10),
        moved_text: theme.ui.text_muted,
        moved_word: accent.with_alpha(0.30),
    }
}

impl DiffView {
    /// Wires the hover callback of one code view (called once per view).
    pub(super) fn wire_provenance_hover(
        view: &Entity<CodeView>,
        column: DiffColumn,
        weak: gpui::WeakEntity<DiffView>,
        cx: &mut Context<DiffView>,
    ) {
        view.update(cx, |v, _| {
            v.on_hover(move |target, window, cx| {
                // Called while the code view is being updated: defer.
                let target = target.clone();
                let weak = weak.clone();
                window.defer(cx, move |_, cx| {
                    let _ = weak.update(cx, |this, cx| this.show_provenance(column, target, cx));
                });
            });
        });
    }

    /// Handles code view events of the review layer; returns true when consumed.
    pub(super) fn review_code_event(&mut self, event: &CodeViewEvent, cx: &mut Context<Self>) {
        match event {
            CodeViewEvent::HoverMoved(pos) => self.hover_moved(*pos, cx),
            CodeViewEvent::Scrolled if self.review.hover.as_ref().is_some_and(|h| !h.hovered) => {
                self.review.hover = None;
                cx.notify();
            }
            _ => {}
        }
    }

    /// Moved spans that apply: the changeset's (with cross-file moves) or the file's own.
    fn active_moves(&self) -> Option<Arc<FileMoves>> {
        if !self.review.show_moved {
            return None;
        }
        self.review
            .changeset_moves
            .clone()
            .or_else(|| self.review.intra_moves.clone())
    }

    /// Builds one column's source with the review overlays applied.
    pub(super) fn review_source(&self, source: DiffSource, cx: &App) -> DiffSource {
        let colors = overlay_colors(cx.theme());
        let moved = self
            .active_moves()
            .map(|moves| MovedOverlay { moves, colors });
        source
            .with_moved(moved)
            .with_structural(self.review.structural_marks.clone(), colors)
    }

    /// Takes the review data of a freshly loaded diff.
    pub(super) fn apply_extras(&mut self, extras: Extras, cx: &mut Context<Self>) {
        self.review.intra_moves = extras.moves;
        self.review.hunks = extras.hunks;
        match extras.structural {
            StructuralOutcome::Off => {
                self.review.structural_marks = None;
                self.review.notice = None;
            }
            StructuralOutcome::Marks(marks) => {
                self.review.structural_marks = Some(marks);
                self.review.notice = None;
            }
            StructuralOutcome::PartialMarks(marks) => {
                self.review.structural_marks = Some(marks);
                self.review.notice = Some(
                    "Syntax-aware diff skipped for part of this file (too large); showing line diff there"
                        .into(),
                );
            }
            StructuralOutcome::Fallback(why) => {
                self.review.structural_marks = None;
                self.review.notice = Some(
                    format!("Structural diff unavailable: {why}. Showing the line diff.").into(),
                );
            }
        }
        self.review.hover = None;
        self.load_reviewed_marks(cx);
    }

    /// Reads the persisted reviewed hunks for the loaded diff.
    fn load_reviewed_marks(&mut self, cx: &mut Context<Self>) {
        let n = self.review.hunks.len();
        let Some(viewed) = &self.viewed else {
            self.review.reviewed = vec![false; n];
            return;
        };
        if viewed.file.viewed {
            self.review.reviewed = vec![true; n];
            return;
        }
        let store = self.store.read(cx);
        let Some(repo) = store.repo() else {
            self.review.reviewed = vec![false; n];
            return;
        };
        let new_text = self
            .loaded
            .as_ref()
            .and_then(|l| l.diff.new.text.as_ref())
            .filter(|t| t.len() < 1_000_000)
            .map(|t| t.to_string());
        let db = lector_ui::commands_bridge::core(cx).db().clone();
        self.review.reviewed = HunkReviewStore::new(&db).reviewed(
            repo.workdir(),
            &viewed.key,
            viewed.file.path(),
            &self.review.hunks,
            new_text.as_deref(),
        );
    }

    fn persist_reviewed(&self, cx: &App) {
        let (Some(viewed), Some(repo)) = (&self.viewed, self.store.read(cx).repo()) else {
            return;
        };
        let db = lector_ui::commands_bridge::core(cx).db().clone();
        HunkReviewStore::new(&db).store_reviewed(
            repo.workdir(),
            &viewed.key,
            viewed.file.path(),
            &self.review.hunks,
            &self.review.reviewed,
        );
    }

    /// Reviewed flag per hunk of the loaded diff.
    pub fn reviewed_hunks(&self) -> &[bool] {
        &self.review.reviewed
    }

    /// Gives the view the moves of the whole changeset (including moves from other files).
    pub fn set_changeset_moves(&mut self, moves: Option<Arc<FileMoves>>, cx: &mut Context<Self>) {
        let same = match (&self.review.changeset_moves, &moves) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b) || **a == **b,
            (None, None) => true,
            _ => false,
        };
        self.review.changeset_moves = moves;
        if !same && self.loaded.is_some() {
            self.rebuild(cx);
        }
    }

    /// Shows `line` (0-based, old or new side) once the diff has loaded.
    pub fn show_line_when_loaded(&mut self, old_side: bool, line: u32, cx: &mut Context<Self>) {
        if self.loaded.is_some() && !self.loading {
            self.scroll_to_line(old_side, line, cx);
        } else {
            self.review.pending_line = Some((old_side, line));
        }
    }

    /// Scrolls to and selects the row showing `line` of the old or new side.
    pub(super) fn scroll_to_line(&mut self, old_side: bool, line: u32, cx: &mut Context<Self>) {
        let Some((old, new, uni)) = self.sources.clone() else {
            return;
        };
        let (src, views): (&Rc<DiffSource>, Vec<Entity<CodeView>>) = match self.last_layout {
            Layout::Split => {
                let src = if old_side { &old } else { &new };
                (src, vec![self.old_view.clone(), self.new_view.clone()])
            }
            Layout::Unified => (&uni, vec![self.unified_view.clone()]),
        };
        let Some(row) = src.row_of_line(old_side, line) else {
            return;
        };
        for v in views {
            v.update(cx, |v, cx| {
                v.set_selection(Selection::cursor(Pos::new(row, 0)), cx);
                v.scroll_to_row(row.saturating_sub(4), false, cx);
            });
        }
        cx.notify();
    }

    /// Puts the cursor on hunk `k` once the diff has loaded.
    pub fn show_hunk_when_loaded(&mut self, k: usize, cx: &mut Context<Self>) {
        if self.loaded.is_some() && !self.loading {
            self.select_hunk(k, cx);
        } else {
            self.pending_hunk = Some(k);
        }
    }

    pub(super) fn take_pending_line(&mut self, cx: &mut Context<Self>) {
        if let Some((old_side, line)) = self.review.pending_line.take() {
            self.scroll_to_line(old_side, line, cx);
        }
    }

    /// Paints the stripes and "Moved ..." labels of every column.
    pub(super) fn apply_review_layers(&mut self, cx: &mut Context<Self>) {
        let Some((old, new, uni)) = self.sources.clone() else {
            return;
        };
        let colors = overlay_colors(cx.theme());
        let accent: Hsla = colors.moved_word.with_alpha(1.0).hsla();
        let weak = cx.entity().downgrade();
        for (view, src) in [
            (self.old_view.clone(), old),
            (self.new_view.clone(), new),
            (self.unified_view.clone(), uni),
        ] {
            let blocks = src.moved_blocks();
            let backgrounds: Vec<RowBackground> = blocks
                .iter()
                .map(|b| RowBackground {
                    rows: b.rows.clone(),
                    color: Hsla { a: 0.0, ..accent },
                    edge: Some(accent),
                })
                .collect();
            let widgets: Vec<RowWidget> = blocks
                .iter()
                .map(|b| {
                    let label: SharedString = b.label().into();
                    let (span, removed_side) = (b.span.clone(), b.removed_side);
                    let weak = weak.clone();
                    let id = b.rows.start as usize;
                    RowWidget {
                        row: b.rows.start,
                        anchor: WidgetAnchor::Right,
                        render: Rc::new(move |_, cx| {
                            let (span, weak) = (span.clone(), weak.clone());
                            let ui = cx.theme().ui.clone();
                            div()
                                .id(("moved-label", id))
                                .px(px(7.))
                                .mr(px(10.))
                                .rounded(px(9.))
                                .bg(ui.elevated.hsla())
                                .border_1()
                                .border_color(accent.opacity(0.5))
                                .text_size(metrics::text_xs())
                                .text_color(accent)
                                .cursor_pointer()
                                .hover(|s| s.underline())
                                .child(label.clone())
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    let (span, weak) = (span.clone(), weak.clone());
                                    let _ = weak.update(cx, |this, cx| {
                                        this.follow_move(&span, removed_side, cx)
                                    });
                                })
                                .into_any_element()
                        }),
                    }
                })
                .collect();
            view.update(cx, |v, cx| {
                v.set_row_backgrounds(MOVED_LAYER, backgrounds, cx);
                v.set_row_widgets(MOVED_LAYER, widgets, cx);
            });
        }
    }

    /// Jumps to the other end of a moved block.
    fn follow_move(&mut self, span: &MoveSpan, removed_side: bool, cx: &mut Context<Self>) {
        let line = span.other_lines.start;
        if span.same_file {
            // A block removed here was added at `other_lines` on the new side, and vice versa.
            self.scroll_to_line(!removed_side, line, cx);
            return;
        }
        cx.emit(DiffViewEvent::GoToMove {
            path: span.other_path.clone(),
            line,
            old_side: !removed_side,
        });
    }

    // Toggles.

    /// `diff::ToggleMovedCode`.
    pub(super) fn toggle_moved_code(&mut self, cx: &mut Context<Self>) {
        self.review.show_moved = !self.review.show_moved;
        if self.review.show_moved && self.review.intra_moves.is_none() {
            self.reload(cx); // the moves were not computed while the setting was off
        } else {
            self.rebuild(cx);
        }
    }

    /// `diff::ToggleStructural`.
    pub(super) fn toggle_structural(&mut self, cx: &mut Context<Self>) {
        self.review.structural = !self.review.structural;
        if !self.review.structural {
            self.review.structural_marks = None;
            self.review.notice = None;
            self.rebuild(cx);
        } else {
            self.reload(cx);
        }
    }

    // Reviewed hunks.

    /// Whether hunks can be marked reviewed (a file of a changeset with text hunks).
    pub(super) fn hunk_review_enabled(&self) -> bool {
        self.viewed.is_some() && !self.review.reviewed.is_empty()
    }

    fn sync_file_viewed(&mut self, cx: &mut Context<Self>) {
        let Some(viewed) = self.viewed.as_ref().map(|v| v.file.viewed) else {
            return;
        };
        let all = !self.review.reviewed.is_empty() && self.review.reviewed.iter().all(|r| *r);
        if all && !viewed {
            self.set_viewed_state(true, false, cx);
        } else if !all && viewed {
            self.set_viewed_state(false, false, cx);
        }
    }

    /// Marks hunk `k` reviewed or not (persisted), keeping the file's "viewed" state in sync.
    pub fn set_hunk_reviewed(&mut self, k: usize, reviewed: bool, cx: &mut Context<Self>) {
        let Some(slot) = self.review.reviewed.get_mut(k) else {
            return;
        };
        if *slot == reviewed {
            return;
        }
        *slot = reviewed;
        self.persist_reviewed(cx);
        self.sync_file_viewed(cx);
        self.emit_hunks_reviewed(cx);
        cx.notify();
    }

    fn emit_hunks_reviewed(&self, cx: &mut Context<Self>) {
        if let Some(viewed) = &self.viewed {
            cx.emit(DiffViewEvent::HunksReviewed(
                viewed.file.path().to_string(),
                self.review.reviewed.clone(),
            ));
        }
    }

    /// `review::ToggleHunkReviewed`: toggles the hunk at the cursor and moves on to the next
    /// unreviewed one.
    pub(super) fn toggle_hunk_reviewed_at_cursor(&mut self, cx: &mut Context<Self>) {
        if !self.hunk_review_enabled() {
            cx.propagate();
            return;
        }
        let Some(k) = self.current_hunk.or_else(|| self.hunk_at_cursor(cx)) else {
            return;
        };
        let now = !self.review.reviewed.get(k).copied().unwrap_or(false);
        self.set_hunk_reviewed(k, now, cx);
        if now {
            self.go_to_next_unreviewed_hunk(cx);
        }
    }

    /// Moves to the next unreviewed hunk of this file after the current one (wrapping around).
    /// Returns false when every hunk is reviewed.
    pub fn go_to_next_unreviewed_hunk(&mut self, cx: &mut Context<Self>) -> bool {
        let n = self.review.reviewed.len();
        let current = self
            .current_hunk
            .or_else(|| self.hunk_at_cursor(cx))
            .map_or(0, |k| k + 1);
        let next = (current..n)
            .chain(0..current.min(n))
            .find(|&k| !self.review.reviewed[k]);
        match next {
            Some(k) => {
                self.select_hunk(k, cx);
                true
            }
            None => false,
        }
    }

    /// `review::NextUnreviewed`: the next unreviewed hunk of this file, else (in a review tab)
    /// of the next file in the queue.
    pub(super) fn next_unreviewed_action(&mut self, cx: &mut Context<Self>) {
        let n = self.review.reviewed.len();
        let current = self
            .current_hunk
            .or_else(|| self.hunk_at_cursor(cx))
            .map_or(0, |k| k + 1);
        if let Some(k) = (current..n).find(|&k| !self.review.reviewed[k]) {
            self.select_hunk(k, cx);
        } else {
            cx.emit(DiffViewEvent::NextUnreviewed);
        }
    }

    /// Sets the file's viewed state, persisting it; with `sync_hunks` every hunk follows.
    pub(super) fn set_viewed_state(
        &mut self,
        new_state: bool,
        sync_hunks: bool,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.read(cx);
        let Some(target) = &mut self.viewed else {
            return;
        };
        if let Some(repo) = store.repo() {
            let root = repo.workdir().to_path_buf();
            if let Err(e) = lector_workspace::review::set_viewed_with(
                &store.review,
                &root,
                &target.key,
                &mut target.file,
                new_state,
            ) {
                tracing::warn!("could not save viewed state: {e}");
            }
        }
        let path = target.file.path().to_string();
        if sync_hunks && !self.review.reviewed.is_empty() {
            self.review.reviewed = vec![new_state; self.review.reviewed.len()];
            self.persist_reviewed(cx);
            self.emit_hunks_reviewed(cx);
        }
        cx.emit(DiffViewEvent::ViewedToggled(path, new_state));
        cx.notify();
    }

    /// Applies a viewed state set from outside (the review list's checkbox): every hunk follows.
    pub(super) fn follow_viewed_change(
        &mut self,
        was: Option<bool>,
        now: Option<bool>,
        cx: &mut Context<Self>,
    ) {
        if self.review.reviewed.is_empty() {
            return;
        }
        let state = match (was, now) {
            (Some(true), Some(false)) => false,
            (w, Some(true)) if w != Some(true) => true,
            _ => return,
        };
        self.review.reviewed = vec![state; self.review.reviewed.len()];
        self.persist_reviewed(cx);
        cx.notify();
    }

    // Provenance hover.

    fn provenance_query(&self, column: DiffColumn, row: u32, cx: &App) -> Option<LineQuery> {
        let (old, new, kind) = {
            let (o, n, u) = self.sources.as_ref()?;
            match column {
                DiffColumn::Old => o,
                DiffColumn::New => n,
                DiffColumn::Unified => u,
            }
            .lines_at(row)?
        };
        let _ = old;
        // Only lines that exist on the new side and changed.
        if kind == lector_diff::RowKind::Context || kind == lector_diff::RowKind::Removed {
            return None;
        }
        if column == DiffColumn::Old {
            return None;
        }
        let line = new?;
        let repo = self.store.read(cx).repo()?.workdir().to_path_buf();
        let loaded = self.loaded.as_ref()?;
        let path = loaded.diff.change.new_path.clone()?;
        let rev = match (&loaded.diff.change.new, self.viewed.as_ref()) {
            (Side::Workdir(_) | Side::Index(_), _) => None,
            (_, Some(v)) => match lector_workspace::review::parse_changeset_key(&v.key)? {
                Changeset::Commit(oid) => Some(oid.to_string()),
                Changeset::Range { to, .. } if to != lector_workspace::review::WORKDIR_REV => {
                    Some(to)
                }
                Changeset::BranchVsBase { branch, .. } => Some(branch),
                _ => None,
            },
            _ => None,
        };
        Some(LineQuery {
            repo,
            path,
            line,
            rev,
        })
    }

    fn show_provenance(&mut self, column: DiffColumn, target: HoverTarget, cx: &mut Context<Self>) {
        let row = target.pos.row;
        if self
            .review
            .hover
            .as_ref()
            .is_some_and(|h| h.column == column && h.row == row)
        {
            return;
        }
        let Some(query) = self.provenance_query(column, row, cx) else {
            return;
        };
        let core = lector_ui::commands_bridge::core(cx);
        let position = target.position;
        self.review.hover_task = Some(cx.spawn(async move |this, cx| {
            let md = cx
                .background_executor()
                .spawn(async move {
                    let items = core.provenance().explain_all(&query);
                    let md = explanation_markdown(&items);
                    (!md.is_empty()).then(|| Arc::new(markdown::parse(&md)))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.review.hover = md.map(|blocks| ProvenanceHover {
                    column,
                    row,
                    blocks,
                    position,
                    hovered: false,
                });
                cx.notify();
            });
        }));
    }

    fn hover_moved(&mut self, pos: Option<Pos>, cx: &mut Context<Self>) {
        let Some(hover) = &self.review.hover else {
            self.review.hover_task = None;
            return;
        };
        if hover.hovered {
            return;
        }
        if pos.map(|p| p.row) != Some(hover.row) {
            self.review.hover = None;
            self.review.hover_task = None;
            cx.notify();
        }
    }

    /// The "why" popover, when a changed line is hovered.
    pub(super) fn render_provenance(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let hover = self.review.hover.as_ref()?;
        let ui = cx.theme().ui.clone();
        let opts = RenderOptions {
            base_dir: None,
            on_link: Some(Rc::new(|target: &str, _w: &mut Window, cx: &mut App| {
                if target.contains("://") {
                    cx.open_url(target);
                }
            })),
            compact: true,
            id: "provenance-md".into(),
        };
        let elements = markdown::render_blocks(&hover.blocks, &opts, window, cx);
        let weak = cx.entity().downgrade();
        Some(
            deferred(
                anchored()
                    .position(hover.position)
                    .anchor(Corner::BottomLeft)
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        div()
                            .id("provenance-popover")
                            .occlude()
                            .mb(px(4.))
                            .max_w(px(560.))
                            .max_h(px(320.))
                            .overflow_y_scroll()
                            .px(px(12.))
                            .pt(px(10.))
                            .pb(px(4.))
                            .bg(ui.elevated.hsla())
                            .border_1()
                            .border_color(ui.border.hsla())
                            .rounded(metrics::radius_lg())
                            .shadow_lg()
                            .text_size(metrics::text())
                            .text_color(ui.text.hsla())
                            .on_hover(move |hovered, _, cx| {
                                let hovered = *hovered;
                                let _ = weak.update(cx, |this, cx| {
                                    if let Some(h) = &mut this.review.hover {
                                        h.hovered = hovered;
                                        if !hovered {
                                            this.review.hover = None;
                                        }
                                        cx.notify();
                                    }
                                });
                            })
                            .children(elements),
                    ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    // Header controls.

    /// Toggle buttons and the review progress of this file, for the header.
    pub(super) fn render_review_controls(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let ui = cx.theme().ui.clone();
        let mut out: Vec<AnyElement> = Vec::new();
        let total = self.review.reviewed.len();
        if total > 0 && self.viewed.is_some() {
            let done = self.review.reviewed.iter().filter(|r| **r).count();
            out.push(
                div()
                    .text_size(metrics::text_xs())
                    .text_color(if done == total {
                        ui.text_accent.hsla()
                    } else {
                        ui.text_faint.hsla()
                    })
                    .child(format!("{done} of {total} reviewed"))
                    .into_any_element(),
            );
        }
        let has_moves = self
            .active_moves()
            .or_else(|| self.review.intra_moves.clone())
            .is_some_and(|m| !m.is_empty());
        if self.loaded.is_some() && (has_moves || self.review.show_moved) {
            out.push(
                IconButton::new("toggle-moved", IconName::Layers)
                    .icon_size(IconSize::Small)
                    .selected(self.review.show_moved)
                    .tooltip(Tooltip::for_action(
                        if self.review.show_moved {
                            "Show Moved Code as Changes"
                        } else {
                            "Dim Moved Code"
                        },
                        &diff_actions::ToggleMovedCode,
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_moved_code(cx)))
                    .into_any_element(),
            );
        }
        if self.loaded.is_some() {
            out.push(
                IconButton::new("toggle-structural", IconName::Braces)
                    .icon_size(IconSize::Small)
                    .selected(self.review.structural)
                    .tooltip(Tooltip::for_action(
                        if self.review.structural {
                            "Line Diff"
                        } else {
                            "Structural Diff (ignores reformatting)"
                        },
                        &diff_actions::ToggleStructural,
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_structural(cx)))
                    .into_any_element(),
            );
        }
        out
    }

    /// The subtle "structural diff unavailable" line under the header.
    pub(super) fn render_structural_notice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let notice = self.review.notice.clone()?;
        let ui = cx.theme().ui.clone();
        Some(
            h_flex()
                .flex_none()
                .gap(px(6.))
                .px(px(12.))
                .py(px(3.))
                .border_b_1()
                .border_color(ui.border_variant.hsla())
                .text_size(metrics::text_xs())
                .text_color(ui.text_faint.hsla())
                .child(
                    Icon::new(IconName::Info)
                        .size(IconSize::XSmall)
                        .color(ui.text_faint.hsla()),
                )
                .child(notice)
                .into_any_element(),
        )
    }

    /// The "Reviewed" checkbox of hunk `k`'s toolbar.
    pub(super) fn reviewed_chip(&self, k: usize, cx: &mut Context<Self>) -> AnyElement {
        let ui = cx.theme().ui.clone();
        let reviewed = self.review.reviewed.get(k).copied().unwrap_or(false);
        h_flex()
            .id(("hunk-reviewed", k))
            .h(px(16.))
            .px(px(5.))
            .gap(px(3.))
            .rounded(px(3.))
            .cursor_pointer()
            .text_color(if reviewed {
                ui.text_accent.hsla()
            } else {
                ui.text_muted.hsla()
            })
            .hover(|s| s.bg(ui.hover.hsla()).text_color(ui.text.hsla()))
            .child(
                Icon::new(if reviewed {
                    IconName::CheckSquare
                } else {
                    IconName::Square
                })
                .size(IconSize::XSmall)
                .color(if reviewed {
                    ui.text_accent.hsla()
                } else {
                    ui.text_muted.hsla()
                }),
            )
            .child("Reviewed")
            .tab_index(0)
            .focus(lector_ui::a11y::ring_focus(cx))
            .tooltip(Tooltip::text("Mark Hunk Reviewed (r)"))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                let now = !this.review.reviewed.get(k).copied().unwrap_or(false);
                this.set_hunk_reviewed(k, now, cx);
            }))
            .into_any_element()
    }
}
