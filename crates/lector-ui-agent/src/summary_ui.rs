//! The turn summary card (v0.7): the headline of `lector_agent::summary::TurnSummary` and its
//! files in review-queue risk order, drawn in the Agent panel (under a finished turn) and as the
//! header of the turn's review tab. No language model: the text comes from the diff and the
//! review queue's risk analysis.

use std::rc::Rc;

use gpui::{div, prelude::*, AnyElement, App, Context, Entity, FontWeight, WeakEntity, Window};
use lector_agent::summary::{NavTarget, SummaryFile, TurnSummary};
use lector_agent::TurnId;
use lector_git::ChangeKind;
use lector_ui::components::{h_flex, metrics, v_flex, Button, ButtonStyle, Tooltip};
use lector_ui::icons::IconName;
use lector_ui::scale::px;
use lector_ui::{ActiveTheme, ColorExt};

use crate::panel::AgentPanel;

/// Files listed in the panel's card; the rest is a count.
pub const PANEL_FILES: usize = 6;
/// Files listed in the review tab's header.
pub const HEADER_FILES: usize = 4;

/// What a click on a file row does.
pub type OpenFile = Rc<dyn Fn(NavTarget, &mut Window, &mut App)>;

/// The card: headline, then up to `max_files` file rows (attention files first, low-value files
/// last under their own label).
pub fn summary_card(
    id_base: usize,
    summary: &TurnSummary,
    max_files: usize,
    open_file: OpenFile,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme().clone();
    let ui = &theme.ui;
    let mut rows: Vec<AnyElement> = Vec::new();
    let mut shown = 0usize;
    let mut low_label_done = false;
    for (ix, f) in summary.files.iter().enumerate() {
        if shown >= max_files {
            break;
        }
        if f.low_value && !low_label_done {
            low_label_done = true;
            rows.push(
                div()
                    .px(px(4.))
                    .pt(px(2.))
                    .text_size(metrics::text_xs())
                    .text_color(ui.text_faint.hsla())
                    .child("Low value: generated, lock and negligible files")
                    .into_any_element(),
            );
        }
        shown += 1;
        rows.push(file_row(id_base + ix, f, open_file.clone(), cx));
    }
    let hidden = summary.files.len().saturating_sub(shown) + summary.unanalyzed;
    v_flex()
        .gap(px(3.))
        .px(px(8.))
        .py(px(6.))
        .rounded(metrics::radius())
        .bg(ui.surface.hsla())
        .child(
            div()
                .text_size(metrics::text_sm())
                .text_color(ui.text.hsla())
                .font_weight(FontWeight::MEDIUM)
                .child(summary.headline()),
        )
        .children(rows)
        .when(hidden > 0, |el| {
            el.child(
                div()
                    .px(px(4.))
                    .text_size(metrics::text_xs())
                    .text_color(ui.text_faint.hsla())
                    .child(format!(
                        "and {hidden} more file{} in the review",
                        if hidden == 1 { "" } else { "s" }
                    )),
            )
        })
        .into_any_element()
}

fn file_row(ix: usize, f: &SummaryFile, open_file: OpenFile, cx: &App) -> AnyElement {
    let theme = cx.theme().clone();
    let ui = &theme.ui;
    let (letter, color, name) = match f.kind {
        ChangeKind::Added => ("A", theme.git.added.hsla(), "Added"),
        ChangeKind::Deleted => ("D", theme.git.deleted.hsla(), "Deleted"),
        _ => ("M", theme.git.modified.hsla(), "Modified"),
    };
    let target = f.target.clone();
    let mut detail = String::new();
    if let Some(reason) = &f.reason {
        detail.push_str(reason);
        if let Some(ev) = &f.evidence {
            detail.push_str(&format!(" ({})", ev.label()));
        }
    }
    let counts = if f.binary {
        "binary".to_string()
    } else {
        format!("+{} \u{2212}{}", f.added, f.removed)
    };
    let tip = format!(
        "{name}: {}. Click to open the diff at the first point of interest.",
        f.path
    );
    h_flex()
        .id(("summary-file", ix))
        .gap(px(6.))
        .px(px(4.))
        .rounded(metrics::radius())
        .cursor_pointer()
        .tab_index(0)
        .hover(|s| s.bg(ui.hover.hsla()))
        .text_size(metrics::text_xs())
        .tooltip(Tooltip::text(tip))
        .child(
            div()
                .w(px(10.))
                .text_color(color)
                .font_weight(FontWeight::BOLD)
                .child(letter),
        )
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .text_color(ui.text.hsla())
                .child(f.path.clone()),
        )
        .child(
            div()
                .flex_none()
                .text_color(ui.text_faint.hsla())
                .child(counts),
        )
        .when(!detail.is_empty(), |el| {
            el.child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_color(ui.text_muted.hsla())
                    .child(detail),
            )
        })
        .on_click(move |_, window, cx| open_file(target.clone(), window, cx))
        .into_any_element()
}

/// The header of a turn's review tab: the card plus an "Undo turn" button. A view of its own so
/// the review tab can hold it (`ReviewView::set_header_view`).
pub struct SummaryHeader {
    summary: Rc<TurnSummary>,
    turn: TurnId,
    panel: WeakEntity<AgentPanel>,
    review: WeakEntity<lector_ui_git::review_tab::ReviewView>,
    undone: bool,
}

impl SummaryHeader {
    /// A header for the review tab `review` of `turn`.
    pub fn new(
        summary: Rc<TurnSummary>,
        turn: TurnId,
        panel: WeakEntity<AgentPanel>,
        review: &Entity<lector_ui_git::review_tab::ReviewView>,
    ) -> Self {
        SummaryHeader {
            summary,
            turn,
            panel,
            review: review.downgrade(),
            undone: false,
        }
    }
}

impl Render for SummaryHeader {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let review = self.review.clone();
        let open: OpenFile = Rc::new(move |target, window, cx| {
            let review = review.clone();
            window.defer(cx, move |_, cx| {
                let _ = review.update(cx, |r, cx| {
                    r.show_file_at(&target.path, target.old_side, target.line, cx)
                });
            });
        });
        let (panel, turn) = (self.panel.clone(), self.turn);
        let me = cx.entity().downgrade();
        let undone = self.undone;
        let ui = cx.theme().ui.clone();
        v_flex()
            .flex_none()
            .p(px(6.))
            .gap(px(4.))
            .border_b_1()
            .border_color(ui.border_variant.hsla())
            .child(summary_card(10_000, &self.summary, HEADER_FILES, open, cx))
            .child(
                h_flex().gap(px(6.)).px(px(4.)).child(
                    Button::new(
                        "summary-undo-turn",
                        if undone { "Undone" } else { "Undo turn" },
                    )
                    .style(ButtonStyle::Subtle)
                    .icon(IconName::Undo)
                    .compact()
                    .disabled(undone)
                    .tooltip(Tooltip::text(
                        "Restore the files this turn changed (undoable from the Activity Log)",
                    ))
                    .on_click(move |_, _, cx| {
                        let _ = panel.update(cx, |p, cx| p.undo_turn(turn, false, cx));
                        let _ = me.update(cx, |h, cx| {
                            h.undone = true;
                            cx.notify();
                        });
                    }),
                ),
            )
    }
}
