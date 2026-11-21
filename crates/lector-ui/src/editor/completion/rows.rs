//! Model-level data for painting the completion list (UI-free apart from
//! `Pixels`/`HighlightStyle` values, tested without a window): row views,
//! kind badges, label highlight runs, list width and scrolling.

use std::ops::Range;

use gpui::{HighlightStyle, Hsla, Pixels};
use lector_lsp::{CompletionKind, CompletionSession};
use lector_syntax::SymbolKind;

use super::WIDTH_SAMPLE_ROWS;
use crate::scale::px;

/// What one row of the list shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowView {
    /// Row index in the filtered list.
    pub row: usize,
    /// The label.
    pub label: String,
    /// Byte ranges of the label that match the typed text.
    pub highlights: Vec<Range<usize>>,
    /// Dimmed text right after the label (`labelDetails.detail`).
    pub label_detail: Option<String>,
    /// Dimmed text at the right edge (`labelDetails.description`, else the item's detail).
    pub description: Option<String>,
    /// Item kind (badge).
    pub kind: CompletionKind,
    /// Drawn struck through.
    pub deprecated: bool,
    /// The selected row.
    pub selected: bool,
}

/// The rows `top..top + count` of the session's filtered list: all the popup
/// needs to paint its first page. Cost depends on `count`, not on the list size.
pub fn row_views(session: &CompletionSession, top: usize, count: usize) -> Vec<RowView> {
    let selected = session.selected();
    let matches = session.matches();
    (top..matches.len().min(top.saturating_add(count)))
        .map(|row| {
            let item = &session.items()[matches[row].item];
            let label_detail = item
                .label_details
                .as_ref()
                .and_then(|d| d.detail.clone())
                .filter(|d| !d.is_empty());
            let description = item
                .label_details
                .as_ref()
                .and_then(|d| d.description.clone())
                .or_else(|| item.detail.as_ref().map(|d| first_line(d)))
                .filter(|d| !d.is_empty());
            RowView {
                row,
                label: item.label.clone(),
                highlights: session.label_highlights(row),
                label_detail,
                description,
                kind: item.kind,
                deprecated: item.deprecated,
                selected: selected == Some(row),
            }
        })
        .collect()
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    match line.char_indices().nth(120) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}

/// How a kind is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    /// The outline's symbol badge (colored from the syntax theme).
    Symbol(SymbolKind),
    /// A letter badge.
    Letter(&'static str),
}

/// The badge of a completion kind: the outline's symbol badge where there is
/// a matching symbol kind, a letter otherwise.
pub fn badge_for(kind: CompletionKind) -> Badge {
    use CompletionKind as K;
    match kind {
        K::Method => Badge::Symbol(SymbolKind::Method),
        K::Function | K::Constructor => Badge::Symbol(SymbolKind::Function),
        K::Field | K::Property => Badge::Symbol(SymbolKind::Field),
        K::Variable | K::Value => Badge::Symbol(SymbolKind::Variable),
        K::Class => Badge::Symbol(SymbolKind::Class),
        K::Interface => Badge::Symbol(SymbolKind::Interface),
        K::Module => Badge::Symbol(SymbolKind::Module),
        K::Enum | K::EnumMember => Badge::Symbol(SymbolKind::Enum),
        K::Struct => Badge::Symbol(SymbolKind::Struct),
        K::Constant => Badge::Symbol(SymbolKind::Constant),
        K::TypeParameter => Badge::Symbol(SymbolKind::Type),
        K::Keyword => Badge::Letter("k"),
        K::Snippet => Badge::Letter("s"),
        K::Text => Badge::Letter("t"),
        K::Unit => Badge::Letter("u"),
        K::Color => Badge::Letter("c"),
        K::File => Badge::Letter("f"),
        K::Folder => Badge::Letter("d"),
        K::Reference => Badge::Letter("r"),
        K::Event => Badge::Letter("e"),
        K::Operator => Badge::Letter("o"),
        K::Other => Badge::Letter("·"),
    }
}

/// Text styles for a label: the matched ranges accented, the whole label
/// struck through when deprecated. Ranges are sorted, disjoint, on char boundaries.
pub fn label_runs(
    label: &str,
    matches: &[Range<usize>],
    deprecated: bool,
    accent: Hsla,
    strike: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let strikethrough = deprecated.then_some(gpui::StrikethroughStyle {
        thickness: px(1.),
        color: Some(strike),
    });
    let mut runs = Vec::new();
    let mut at = 0usize;
    let valid = |r: &Range<usize>, at: usize| {
        r.start >= at
            && r.start < r.end
            && r.end <= label.len()
            && label.is_char_boundary(r.start)
            && label.is_char_boundary(r.end)
    };
    for r in matches.iter().filter(|r| valid(r, 0)) {
        if !valid(r, at) {
            continue;
        }
        if deprecated && r.start > at {
            runs.push((
                at..r.start,
                HighlightStyle {
                    strikethrough,
                    ..Default::default()
                },
            ));
        }
        runs.push((
            r.clone(),
            HighlightStyle {
                color: Some(accent),
                font_weight: Some(gpui::FontWeight::SEMIBOLD),
                strikethrough,
                ..Default::default()
            },
        ));
        at = r.end;
    }
    if deprecated && at < label.len() {
        runs.push((
            at..label.len(),
            HighlightStyle {
                strikethrough,
                ..Default::default()
            },
        ));
    }
    runs
}

/// Width of the list part: wide enough for the first rows' text, within bounds.
pub fn list_width(rows: &[RowView], char_width: Pixels) -> Pixels {
    let chars = rows
        .iter()
        .take(WIDTH_SAMPLE_ROWS)
        .map(|r| {
            r.label.chars().count()
                + r.label_detail.as_ref().map_or(0, |d| d.chars().count())
                + r.description
                    .as_ref()
                    .map_or(0, |d| d.chars().count().min(28) + 2)
        })
        .max()
        .unwrap_or(0);
    (char_width * chars as f32 + px(64.)).clamp(px(280.), px(560.))
}

/// Offset in the list so that `selected` is in view with `top` as the current
/// first row; the list never scrolls past its last row.
pub fn scroll_to_show(top: usize, selected: usize, total: usize, visible: usize) -> usize {
    let visible = visible.max(1);
    let mut top = top;
    if selected < top {
        top = selected;
    } else if selected >= top + visible {
        top = selected + 1 - visible;
    }
    top.min(total.saturating_sub(visible))
}
