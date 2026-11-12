//! The in-place diff of an inline agent edit (v0.7), drawn inside the file's own tab.
//!
//! This module is the UI seam that `lector-ui-agent` drives (it knows nothing about agents):
//! the owner calls [`FileView::set_inline_edit`] with an [`InlineEditUi`] describing what to show
//! and receives the user's clicks as [`InlineEditAction`]s through the handler it supplied. The
//! state machine that decides *what* to show lives in `lector_agent::inline_edit` (UI-free).
//!
//! While hunks are pending the tab shows a *merged view*: the file with every pending hunk's old
//! lines inserted above its new lines. [`InlineDiffSource`] wraps the document's source for that
//! (the code view has no virtual rows, so the wrapper adds real ones); removed lines are red and
//! prefixed by a `-` in the gutter, added lines are green and prefixed by `+`, using the same
//! decoration colors as every diff, so the meaning never rests on color alone. The tab is
//! read-only while the merged view is up (row positions no longer match the buffer); Alt+]/Alt+[
//! walk the hunks because the wrapper reports one gutter mark per hunk.

use std::ops::Range;
use std::rc::Rc;

use gpui::{div, prelude::*, AnyElement, App, Context, SharedString, Window};

use super::FileView;
use crate::code_view::{
    CodeSource, DecorationColor, GutterMark, GutterMarkKind, HighlightSpan, Pos, RowBackground,
    RowDecoration, RowWidget, WidgetAnchor,
};
use crate::components::{h_flex, metrics, v_flex, Button, ButtonStyle};
use crate::icons::{Icon, IconName, IconSize};
use crate::scale::px;
use crate::theme::{ActiveTheme, ColorExt};

/// Owner name of the layers this module sets on the code view.
pub const LAYER: &str = "inline-edit";
/// Key-context identifier set while hunks are pending (`CodeView && inline_edit`).
pub const CONTEXT_FLAG: &str = "inline_edit";

/// One pending hunk, in the coordinates of the file as the buffer now has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineHunkView {
    /// Stable id the owner gave the hunk (echoed in the actions).
    pub id: u64,
    /// Rows of the hunk's new text (empty for a pure deletion: the row the old lines precede).
    pub new_rows: Range<u32>,
    /// The old lines (without terminators) shown above the new rows.
    pub old_lines: Vec<String>,
}

/// What the user did in the in-place diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InlineEditAction {
    /// Keep one hunk.
    AcceptHunk(u64),
    /// Undo one hunk.
    RejectHunk(u64),
    /// Keep every pending hunk.
    AcceptAll,
    /// Undo every pending hunk.
    RejectAll,
    /// Stop the running edit.
    Cancel,
    /// Close the bar (a finished message or a settled review).
    Dismiss,
    /// Open the prompt box to refine the pending hunks.
    FollowUp,
    /// Open the turn's review tab (other files changed, or a conflict).
    OpenReview,
}

/// Receives the user's actions. Called from inside event handlers: defer anything that updates
/// the file view.
pub type InlineActionHandler = Rc<dyn Fn(InlineEditAction, &mut Window, &mut App)>;

/// What the bar and the code area show.
#[derive(Clone, Debug, PartialEq)]
pub enum InlineEditPhase {
    /// The agent works on `rows`; `status` streams what it says.
    Working {
        /// The rows being edited (marked with a spinner line).
        rows: Range<u32>,
        /// The agent's latest status text.
        status: SharedString,
    },
    /// Hunks wait for a decision.
    Review {
        /// Pending hunks, in file order.
        hunks: Vec<InlineHunkView>,
    },
    /// A one-line outcome ("The agent made no changes.").
    Message {
        /// The text.
        text: SharedString,
    },
}

/// An extra line in the bar, optionally with a link-style button.
#[derive(Clone, Debug, PartialEq)]
pub struct InlineNoteUi {
    /// The text ("The agent also changed 2 other files.").
    pub text: SharedString,
    /// A button after the text: its label and the action it triggers.
    pub action: Option<(SharedString, InlineEditAction)>,
}

/// Everything the file tab shows for one inline edit.
#[derive(Clone)]
pub struct InlineEditUi {
    /// Name of the agent doing the edit.
    pub agent: SharedString,
    /// The phase.
    pub phase: InlineEditPhase,
    /// Extra lines under the bar's main line.
    pub notes: Vec<InlineNoteUi>,
    /// Where the user's clicks go.
    pub on_action: InlineActionHandler,
}

/// Library-private state of the file tab.
pub(super) struct InlineEditState {
    ui: InlineEditUi,
    /// The merged source while hunks are shown.
    merged: Option<Rc<InlineDiffSource>>,
    /// Whether the code view was read-only before the merged view took over.
    was_read_only: bool,
}

// ---------------------------------------------------------------------------------------------
// The merged source
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct MergedHunk {
    id: u64,
    /// Merged rows of the old lines.
    old: Range<u32>,
    /// Merged rows of the new lines.
    new: Range<u32>,
    /// Base rows of the new lines.
    base: Range<u32>,
    old_lines: Vec<SharedString>,
}

/// The document's source with each pending hunk's old lines inserted above its new lines.
pub struct InlineDiffSource {
    base: Rc<dyn CodeSource>,
    hunks: Vec<MergedHunk>,
    total_old: u32,
    version: u64,
    digits: usize,
}

impl InlineDiffSource {
    /// Wraps `base`. Hunks are sorted by position; overlapping or out-of-range ones are dropped
    /// (they cannot be drawn consistently).
    pub fn new(base: Rc<dyn CodeSource>, hunks: &[InlineHunkView]) -> InlineDiffSource {
        use std::sync::atomic::{AtomicU64, Ordering};
        static REV: AtomicU64 = AtomicU64::new(1);
        let line_count = base.line_count();
        let mut sorted: Vec<&InlineHunkView> = hunks.iter().collect();
        sorted.sort_by_key(|h| (h.new_rows.start, h.new_rows.end));
        let mut merged = Vec::with_capacity(sorted.len());
        let mut shift = 0u32;
        let mut floor = 0u32;
        for h in sorted {
            if h.new_rows.start < floor
                || h.new_rows.end > line_count
                || h.new_rows.start > h.new_rows.end
            {
                continue;
            }
            let old_start = h.new_rows.start + shift;
            let old_end = old_start + h.old_lines.len() as u32;
            let new_end = old_end + (h.new_rows.end - h.new_rows.start);
            merged.push(MergedHunk {
                id: h.id,
                old: old_start..old_end,
                new: old_end..new_end,
                base: h.new_rows.clone(),
                old_lines: h
                    .old_lines
                    .iter()
                    .map(|l| SharedString::from(l.clone()))
                    .collect(),
            });
            shift += h.old_lines.len() as u32;
            floor = h.new_rows.end;
        }
        let digits = line_count.max(1).to_string().len();
        let rev = REV.fetch_add(1, Ordering::Relaxed);
        InlineDiffSource {
            version: base
                .version()
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(rev),
            base,
            hunks: merged,
            total_old: shift,
            digits,
        }
    }

    /// The merged rows of every hunk (old and new rows together), with the hunk id.
    pub fn hunk_rows(&self) -> Vec<(u64, Range<u32>)> {
        self.hunks
            .iter()
            .map(|h| (h.id, h.old.start..h.new.end))
            .collect()
    }

    /// The row of the base source that merged `row` shows; for an old row, the first row of the
    /// hunk's new text.
    pub fn base_row(&self, row: u32) -> u32 {
        let mut shift = 0u32;
        for h in &self.hunks {
            if row < h.old.start {
                break;
            }
            if row < h.old.end {
                return h.base.start;
            }
            shift = h.new.start - h.base.start;
        }
        row.saturating_sub(shift)
    }

    /// The merged row of base row `row`.
    pub fn merged_row(&self, row: u32) -> u32 {
        let shift: u32 = self
            .hunks
            .iter()
            .filter(|h| h.base.start <= row)
            .map(|h| h.old.end - h.old.start)
            .sum();
        row + shift
    }

    fn old_line(&self, row: u32) -> Option<&SharedString> {
        let ix = self.hunks.partition_point(|h| h.old.start <= row);
        let h = self.hunks.get(ix.checked_sub(1)?)?;
        (row < h.old.end).then(|| &h.old_lines[(row - h.old.start) as usize])
    }

    fn is_new_row(&self, row: u32) -> bool {
        self.hunks.iter().any(|h| h.new.contains(&row))
    }
}

impl CodeSource for InlineDiffSource {
    fn line_count(&self) -> u32 {
        self.base.line_count() + self.total_old
    }

    fn line(&self, row: u32) -> SharedString {
        match self.old_line(row) {
            Some(l) => l.clone(),
            None => self.base.line(self.base_row(row)),
        }
    }

    fn version(&self) -> u64 {
        self.version
    }

    fn max_line_len(&self) -> Option<usize> {
        let old = self
            .hunks
            .iter()
            .flat_map(|h| h.old_lines.iter().map(|l| l.len()))
            .max()
            .unwrap_or(0);
        self.base.max_line_len().map(|m| m.max(old))
    }

    fn highlights(&self, rows: Range<u32>) -> Vec<HighlightSpan> {
        let lo = rows.start.saturating_sub(self.total_old);
        let hi = rows.end.min(self.base.line_count());
        if lo >= hi {
            return Vec::new();
        }
        self.base
            .highlights(lo..hi)
            .into_iter()
            .filter_map(|mut span| {
                let merged = self.merged_row(span.row);
                (rows.contains(&merged)).then(|| {
                    span.row = merged;
                    span
                })
            })
            .collect()
    }

    fn gutter_marks(&self, rows: Range<u32>) -> Vec<GutterMark> {
        self.hunks
            .iter()
            .filter_map(|h| {
                let span = h.old.start..h.new.end;
                if span.is_empty() || span.end <= rows.start || span.start >= rows.end {
                    return None;
                }
                let kind = if h.old.is_empty() {
                    GutterMarkKind::Added
                } else {
                    GutterMarkKind::Modified
                };
                Some(GutterMark { rows: span, kind })
            })
            .collect()
    }

    fn line_number(&self, row: u32) -> Option<u32> {
        if self.old_line(row).is_some() {
            None
        } else {
            Some(self.base_row(row))
        }
    }

    fn line_number_label(&self, row: u32) -> Option<SharedString> {
        let d = self.digits;
        Some(if self.old_line(row).is_some() {
            format!("- {:>d$}", "").into()
        } else {
            let sign = if self.is_new_row(row) { '+' } else { ' ' };
            format!("{sign} {:>d$}", self.base_row(row) + 1).into()
        })
    }

    fn line_number_chars(&self) -> Option<usize> {
        Some(self.digits + 2)
    }

    fn row_decorations(&self, rows: Range<u32>) -> Vec<RowDecoration> {
        let mut out = Vec::new();
        for h in &self.hunks {
            for (range, color) in [
                (&h.old, DecorationColor::RemovedLine),
                (&h.new, DecorationColor::AddedLine),
            ] {
                for row in range.start.max(rows.start)..range.end.min(rows.end) {
                    out.push(RowDecoration {
                        row,
                        background: Some(color),
                        ranges: Vec::new(),
                    });
                }
            }
        }
        out
    }

    fn language_name(&self) -> Option<SharedString> {
        self.base.language_name()
    }
}

// ---------------------------------------------------------------------------------------------
// The file tab side
// ---------------------------------------------------------------------------------------------

impl FileView {
    /// Shows (or, with `None`, removes) the in-place diff of an inline agent edit. Calling it
    /// again replaces what is shown (new status, fewer hunks...). See the module docs.
    pub fn set_inline_edit(&mut self, ui: Option<InlineEditUi>, cx: &mut Context<Self>) {
        let cv = self.code_view.clone();
        let Some(ui) = ui else {
            let Some(state) = self.inline_edit.take() else {
                return;
            };
            let restore_row = state.merged.as_ref().map(|m| {
                let row = cv.read(cx).cursor().row;
                m.base_row(row)
            });
            if state.merged.is_some() {
                if let Some(source) = self.model.read(cx).source() {
                    cv.update(cx, |v, cx| v.set_source(source, cx));
                }
            }
            cv.update(cx, |v, cx| {
                v.set_row_backgrounds(LAYER, Vec::new(), cx);
                v.set_row_widgets(LAYER, Vec::new(), cx);
                v.set_context_flag(CONTEXT_FLAG, false, cx);
                if state.merged.is_some() {
                    v.set_read_only(state.was_read_only, cx);
                    if let Some(row) = restore_row {
                        v.set_cursor(Pos { row, col: 0 }, cx);
                    }
                }
            });
            cx.notify();
            return;
        };
        let was_read_only = match &self.inline_edit {
            Some(state) if state.merged.is_some() => state.was_read_only,
            _ => cv.read(cx).is_read_only(),
        };
        let handler = ui.on_action.clone();
        let mut new_merged = None;
        let mut widgets: Vec<RowWidget> = Vec::new();
        let mut backgrounds: Vec<RowBackground> = Vec::new();
        match &ui.phase {
            InlineEditPhase::Review { hunks } => {
                if let Some(base) = self.model.read(cx).source() {
                    let source = Rc::new(InlineDiffSource::new(base, hunks));
                    for (n, (id, rows)) in source.hunk_rows().into_iter().enumerate() {
                        let handler = handler.clone();
                        let count = hunks.len();
                        widgets.push(RowWidget {
                            row: rows.start,
                            anchor: WidgetAnchor::Right,
                            render: Rc::new(move |_window, cx| {
                                hunk_buttons(id, n, count, &handler, cx)
                            }),
                        });
                    }
                    new_merged = Some(source);
                }
            }
            InlineEditPhase::Working { rows, .. } => {
                let ui_colors = cx.theme().ui.clone();
                backgrounds.push(RowBackground {
                    rows: rows.clone(),
                    color: ui_colors.accent.hsla().opacity(0.10),
                    edge: Some(ui_colors.accent.hsla()),
                });
                let row = rows.start;
                widgets.push(RowWidget {
                    row,
                    anchor: WidgetAnchor::AfterText,
                    render: Rc::new(move |_window, cx| working_marker(cx)),
                });
            }
            InlineEditPhase::Message { .. } => {}
        }
        let was_merged = self
            .inline_edit
            .as_ref()
            .is_some_and(|s| s.merged.is_some());
        let flag = new_merged.is_some();
        let cursor_row = cv.read(cx).cursor().row;
        cv.update(cx, |v, cx| {
            match &new_merged {
                Some(m) => {
                    let source: Rc<dyn CodeSource> = m.clone();
                    v.set_source(source, cx);
                    if !was_merged {
                        v.set_read_only(true, cx);
                        // Land on the first hunk.
                        if let Some((_, rows)) = m.hunk_rows().first() {
                            v.set_cursor(
                                Pos {
                                    row: rows.start,
                                    col: 0,
                                },
                                cx,
                            );
                        }
                    }
                }
                None if was_merged => {
                    // Back to the plain text (the last hunk was decided): the plain source and
                    // the cursor near where it was.
                    v.set_read_only(was_read_only, cx);
                }
                None => {}
            }
            v.set_row_backgrounds(LAYER, backgrounds, cx);
            v.set_row_widgets(LAYER, widgets, cx);
            v.set_context_flag(CONTEXT_FLAG, flag, cx);
        });
        if new_merged.is_none() && was_merged {
            if let Some(source) = self.model.read(cx).source() {
                let row = self
                    .inline_edit
                    .as_ref()
                    .and_then(|s| s.merged.as_ref())
                    .map(|m| m.base_row(cursor_row))
                    .unwrap_or(0);
                cv.update(cx, |v, cx| {
                    v.set_source(source, cx);
                    v.set_cursor(Pos { row, col: 0 }, cx);
                });
            }
        }
        self.inline_edit = Some(InlineEditState {
            ui,
            merged: new_merged,
            was_read_only,
        });
        cx.notify();
    }

    /// Whether an inline edit is shown in this tab.
    pub fn has_inline_edit(&self) -> bool {
        self.inline_edit.is_some()
    }

    /// The id of the pending hunk the cursor is in, or the next one below it (else the last).
    pub fn inline_hunk_at_cursor(&self, cx: &App) -> Option<u64> {
        let merged = self.inline_edit.as_ref()?.merged.as_ref()?;
        let row = self.code_view.read(cx).cursor().row;
        let rows = merged.hunk_rows();
        rows.iter()
            .find(|(_, r)| r.contains(&row))
            .or_else(|| rows.iter().find(|(_, r)| r.start > row))
            .or_else(|| rows.last())
            .map(|(id, _)| *id)
    }

    /// Wraps a new document source for the merged view when one is showing (called when the
    /// model replaces its source).
    pub(super) fn wrap_inline_source(
        &mut self,
        source: Rc<dyn CodeSource>,
        _cx: &App,
    ) -> Rc<dyn CodeSource> {
        let Some(state) = self.inline_edit.as_mut() else {
            return source;
        };
        let Some(old) = state.merged.as_ref() else {
            return source;
        };
        let hunks: Vec<InlineHunkView> = old
            .hunks
            .iter()
            .map(|h| InlineHunkView {
                id: h.id,
                new_rows: h.base.clone(),
                old_lines: h.old_lines.iter().map(|l| l.to_string()).collect(),
            })
            .collect();
        let merged = Rc::new(InlineDiffSource::new(source, &hunks));
        state.merged = Some(merged.clone());
        merged
    }

    /// The bar above the code: who works, what is pending, and the buttons.
    pub(super) fn render_inline_edit_bar(&self, cx: &App) -> Option<AnyElement> {
        let state = self.inline_edit.as_ref()?;
        let ui = &state.ui;
        let colors = cx.theme().ui.clone();
        let handler = ui.on_action.clone();
        let act = move |action: InlineEditAction| {
            let handler = handler.clone();
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                handler(action.clone(), window, cx)
            }
        };
        let (icon, text, buttons): (IconName, SharedString, Vec<AnyElement>) = match &ui.phase {
            InlineEditPhase::Working { status, .. } => {
                let text = if status.is_empty() {
                    format!("{} is working\u{2026}", ui.agent)
                } else {
                    format!("{} is working\u{2026} {}", ui.agent, status)
                };
                (
                    IconName::Loader,
                    text.into(),
                    vec![Button::new("inline-cancel", "Cancel")
                        .style(ButtonStyle::Subtle)
                        .compact()
                        .on_click(act(InlineEditAction::Cancel))
                        .into_any_element()],
                )
            }
            InlineEditPhase::Review { hunks } => {
                let n = hunks.len();
                (
                    IconName::Sparkles,
                    format!(
                        "{}: {n} change{} to review (Alt+] next, Alt+[ previous)",
                        ui.agent,
                        if n == 1 { "" } else { "s" }
                    )
                    .into(),
                    vec![
                        Button::new("inline-follow-up", "Refine\u{2026}")
                            .style(ButtonStyle::Ghost)
                            .compact()
                            .on_click(act(InlineEditAction::FollowUp))
                            .into_any_element(),
                        Button::new("inline-keep-all", "Keep all")
                            .style(ButtonStyle::Accent)
                            .compact()
                            .icon(IconName::Check)
                            .on_click(act(InlineEditAction::AcceptAll))
                            .into_any_element(),
                        Button::new("inline-undo-all", "Undo all")
                            .style(ButtonStyle::Subtle)
                            .compact()
                            .icon(IconName::Close)
                            .on_click(act(InlineEditAction::RejectAll))
                            .into_any_element(),
                    ],
                )
            }
            InlineEditPhase::Message { text } => (
                IconName::Info,
                text.clone(),
                vec![Button::new("inline-dismiss", "Dismiss")
                    .style(ButtonStyle::Subtle)
                    .compact()
                    .on_click(act(InlineEditAction::Dismiss))
                    .into_any_element()],
            ),
        };
        let notes: Vec<AnyElement> = ui
            .notes
            .iter()
            .enumerate()
            .map(|(ix, note)| {
                h_flex()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_color(colors.text_muted.hsla())
                            .child(note.text.clone()),
                    )
                    .when_some(note.action.clone(), |el, (label, action)| {
                        el.child(
                            Button::new(("inline-note", ix), label)
                                .style(ButtonStyle::Ghost)
                                .compact()
                                .on_click(act(action)),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        Some(
            v_flex()
                .id("inline-edit-bar")
                .flex_none()
                .px(px(10.))
                .py(px(4.))
                .gap(px(2.))
                .bg(colors.elevated.hsla())
                .border_b_1()
                .border_color(colors.border_variant.hsla())
                .text_size(metrics::text_sm())
                .text_color(colors.text.hsla())
                .child(
                    h_flex()
                        .gap(px(8.))
                        .child(
                            Icon::new(icon)
                                .size(IconSize::Small)
                                .color(colors.accent.hsla()),
                        )
                        .child(div().flex_1().min_w_0().overflow_hidden().child(text))
                        .children(buttons),
                )
                .children(notes)
                .into_any_element(),
        )
    }
}

/// "Keep | Undo" next to a hunk's first row.
fn hunk_buttons(
    id: u64,
    n: usize,
    count: usize,
    handler: &InlineActionHandler,
    cx: &mut App,
) -> AnyElement {
    let colors = cx.theme().ui.clone();
    let (keep, undo) = (handler.clone(), handler.clone());
    h_flex()
        .gap(px(2.))
        .px(px(4.))
        .rounded(metrics::radius_lg())
        .bg(colors.elevated.hsla())
        .border_1()
        .border_color(colors.border.hsla())
        .text_size(metrics::text_sm())
        .child(
            div()
                .px(px(4.))
                .text_color(colors.text_muted.hsla())
                .child(format!("{}/{}", n + 1, count)),
        )
        .child(
            Button::new(("inline-keep", id as usize), "Keep")
                .style(ButtonStyle::Ghost)
                .compact()
                .icon(IconName::Check)
                .on_click(move |_, window, cx| keep(InlineEditAction::AcceptHunk(id), window, cx)),
        )
        .child(
            Button::new(("inline-undo", id as usize), "Undo")
                .style(ButtonStyle::Ghost)
                .compact()
                .icon(IconName::Close)
                .on_click(move |_, window, cx| undo(InlineEditAction::RejectHunk(id), window, cx)),
        )
        .into_any_element()
}

/// The marker after the first edited row while the agent works.
fn working_marker(cx: &mut App) -> AnyElement {
    let colors = cx.theme().ui.clone();
    h_flex()
        .gap(px(4.))
        .px(px(6.))
        .text_size(metrics::text_sm())
        .text_color(colors.accent.hsla())
        .child(
            Icon::new(IconName::Loader)
                .size(IconSize::XSmall)
                .color(colors.accent.hsla()),
        )
        .child("Working\u{2026}")
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_view::InMemorySource;

    fn hunk(id: u64, new: Range<u32>, old: &[&str]) -> InlineHunkView {
        InlineHunkView {
            id,
            new_rows: new,
            old_lines: old.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn lines(s: &dyn CodeSource) -> Vec<String> {
        (0..s.line_count()).map(|r| s.line(r).to_string()).collect()
    }

    #[test]
    fn old_lines_sit_above_the_new_ones() {
        let base = Rc::new(InMemorySource::new("a\nB\nC\nd\ne"));
        // Rows 1..3 ("B", "C") replaced "b"; and "x" was deleted before row 4 ("e").
        let s = InlineDiffSource::new(base, &[hunk(1, 1..3, &["b"]), hunk(2, 4..4, &["x"])]);
        assert_eq!(lines(&s), vec!["a", "b", "B", "C", "d", "x", "e"]);
        assert_eq!(s.line_count(), 7);
        assert_eq!(s.hunk_rows(), vec![(1, 1..4), (2, 5..6)]);
        // Row mapping both ways.
        assert_eq!(s.base_row(0), 0);
        assert_eq!(s.base_row(1), 1); // old row -> start of the new text
        assert_eq!(s.base_row(2), 1);
        assert_eq!(s.base_row(4), 3);
        assert_eq!(s.base_row(5), 4);
        assert_eq!(s.base_row(6), 4);
        assert_eq!(s.merged_row(0), 0);
        assert_eq!(s.merged_row(1), 2);
        assert_eq!(s.merged_row(4), 6);
    }

    #[test]
    fn gutter_glyphs_and_colors_mark_removed_and_added_lines() {
        let base = Rc::new(InMemorySource::new("a\nB\nc"));
        let s = InlineDiffSource::new(base, &[hunk(1, 1..2, &["b"])]);
        let labels: Vec<String> = (0..s.line_count())
            .map(|r| s.line_number_label(r).unwrap().to_string())
            .collect();
        assert_eq!(labels, vec!["  1", "-  ", "+ 2", "  3"]);
        assert_eq!(s.line_number_chars(), Some(3));
        assert_eq!(s.line_number(1), None);
        assert_eq!(s.line_number(2), Some(1));
        let deco = s.row_decorations(0..4);
        let colors: Vec<(u32, DecorationColor)> = deco
            .iter()
            .filter_map(|d| Some((d.row, d.background?)))
            .collect();
        assert_eq!(
            colors,
            vec![
                (1, DecorationColor::RemovedLine),
                (2, DecorationColor::AddedLine)
            ]
        );
        // One gutter mark per hunk (what Alt+] walks), covering old and new rows.
        let marks = s.gutter_marks(0..4);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].rows, 1..3);
        assert!(s.gutter_marks(3..4).is_empty());
    }

    #[test]
    fn a_pure_insertion_and_a_pure_deletion() {
        let base = Rc::new(InMemorySource::new("a\nnew\nb"));
        let ins = InlineDiffSource::new(base, &[hunk(1, 1..2, &[])]);
        assert_eq!(lines(&ins), vec!["a", "new", "b"]);
        assert_eq!(ins.gutter_marks(0..3)[0].kind, GutterMarkKind::Added);
        let base = Rc::new(InMemorySource::new("a\nb"));
        let del = InlineDiffSource::new(base, &[hunk(1, 1..1, &["gone"])]);
        assert_eq!(lines(&del), vec!["a", "gone", "b"]);
        assert_eq!(del.hunk_rows(), vec![(1, 1..2)]);
    }

    #[test]
    fn bad_hunks_are_dropped_not_drawn() {
        let base = Rc::new(InMemorySource::new("a\nb\nc"));
        // Overlapping and out-of-range hunks.
        let s = InlineDiffSource::new(
            base,
            &[
                hunk(1, 0..2, &["x"]),
                hunk(2, 1..3, &["y"]),
                hunk(3, 2..9, &["z"]),
            ],
        );
        assert_eq!(s.hunk_rows().len(), 1);
        assert_eq!(s.hunk_rows()[0].0, 1);
    }

    #[test]
    fn versions_change_with_every_wrapper() {
        let base: Rc<dyn CodeSource> = Rc::new(InMemorySource::new("a\nb"));
        let h = [hunk(1, 1..2, &["x"])];
        let one = InlineDiffSource::new(base.clone(), &h);
        let two = InlineDiffSource::new(base, &h);
        assert_ne!(one.version(), two.version());
    }
}
