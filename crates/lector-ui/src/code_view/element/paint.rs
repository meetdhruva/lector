//! Painting passes of the [`CodeElement`](super::CodeElement): row widgets, indent guides, the
//! gutter, the minimap, underlines, gutter icons and annotations, scrollbar
//! marks and scrollbars.

use std::collections::HashSet;
use std::ops::Range;

use gpui::{
    fill, point, px, size, App, Bounds, ContentMask, Corners, Entity, FontStyle, Hsla, Pixels,
    ShapedLine, SharedString, TextRun, TransformationMatrix, Window,
};

use super::{LayoutSnapshot, FOLD_COLUMN, GUTTER_LEFT_PAD, MINIMAP_ROW, SCROLLBAR, TEXT_LEFT_PAD};
use crate::code_view::display_map::{display_column, DisplayMap};
use crate::code_view::source::{indent_width, GutterMarkKind};
use crate::code_view::{CodeView, EditorSettings};
use crate::components::scrollbar::thumb_geometry;
use crate::icons::{ui_icon_path, IconName};
use crate::theme::ColorExt;

/// Paints the left gutter annotation (blame) column.
pub(super) fn paint_gutter_annotations(
    window: &mut Window,
    cx: &mut App,
    layout: &LayoutSnapshot,
    ann: &crate::code_view::GutterAnnotations,
    settings: &EditorSettings,
    theme: &lector_theme::Theme,
) {
    let ui = &theme.ui;
    let lh = layout.line_height;
    let left = layout.bounds.left();
    let width = layout.annotation_right - left;
    if width <= px(0.) {
        return;
    }
    let font_size = layout.font_size * 0.86;
    let shade = ui.text.hsla().opacity(0.045);
    let mut last_block: Option<usize> = None;
    // Rows under the sticky header are hidden; labels go on the first row below it.
    let visible_top = layout.bounds.top() + layout.sticky_height() - lh * 0.5;
    for rl in &layout.rows {
        let Some(ix) = ann.block_at(rl.row) else {
            last_block = None;
            continue;
        };
        if rl.y < visible_top {
            continue;
        }
        let block = &ann.blocks[ix];
        let row_bounds = Bounds::new(point(left, rl.y), size(width, lh));
        if block.shaded {
            window.paint_quad(fill(row_bounds, shade));
        }
        if let Some(bar) = block.bar {
            window.paint_quad(fill(
                Bounds::new(point(left + px(3.), rl.y), size(px(2.), lh)),
                bar,
            ));
        }
        let first_visible = last_block != Some(ix);
        last_block = Some(ix);
        if !first_visible || !rl.first_seg {
            continue;
        }
        // Separator above each block.
        if rl.row == block.rows.start && rl.row > 0 {
            window.paint_quad(fill(
                Bounds::new(point(left, rl.y), size(width, px(1.))),
                ui.border_variant.hsla().opacity(0.6),
            ));
        }
        let color = if block.muted {
            ui.text_faint.hsla()
        } else {
            ui.text_muted.hsla()
        };
        let detail = shape_plain(
            window,
            &block.detail,
            font_size,
            ui.text_faint.hsla(),
            settings,
        );
        let detail_x = layout.annotation_right - px(8.) - detail.width;
        let text_x = left + px(10.);
        let max_text = (detail_x - px(8.) - text_x).max(px(0.));
        // Truncate the label to the space left of the detail.
        let mut text = block.text.to_string();
        let mut shaped = shape_plain(window, &text, font_size, color, settings);
        while shaped.width > max_text && !text.is_empty() {
            let keep = text.chars().count().saturating_sub(2);
            text = text.chars().take(keep).collect::<String>();
            let label = format!("{}…", text.trim_end());
            shaped = shape_plain(window, &label, font_size, color, settings);
            if shaped.width <= max_text {
                break;
            }
        }
        let y = rl.y;
        let _ = shaped.paint(point(text_x, y), lh, window, cx);
        let _ = detail.paint(point(detail_x, y), lh, window, cx);
    }
    // Divider between annotations and line numbers.
    window.paint_quad(fill(
        Bounds::new(
            point(layout.annotation_right - px(1.), layout.bounds.top()),
            size(px(1.), layout.bounds.size.height),
        ),
        ui.border_variant.hsla(),
    ));
}

/// Lays out the view's row widgets (`layers`) on their visible rows, after the
/// code's hitboxes so they receive clicks first. Rows under the sticky header
/// or folded away get none.
pub(super) fn prepaint_row_widgets(
    view: &Entity<CodeView>,
    layout: &LayoutSnapshot,
    window: &mut Window,
    cx: &mut App,
) -> Vec<gpui::AnyElement> {
    let widgets = view.read(cx).layers.widgets();
    if widgets.is_empty() {
        return Vec::new();
    }
    let lh = layout.line_height;
    let top = layout.text_bounds.top() + layout.sticky_height();
    let right = layout.text_bounds.right() - px(SCROLLBAR + 6.);
    let mut out = Vec::new();
    window.with_content_mask(
        Some(ContentMask {
            bounds: layout.text_bounds,
        }),
        |window| {
            for widget in widgets {
                let Some(rl) = layout
                    .rows
                    .iter()
                    .find(|r| r.row == widget.row && r.first_seg)
                else {
                    continue;
                };
                if rl.y < top - px(0.5) {
                    continue;
                }
                let mut element = (widget.render)(window, cx);
                let size = element.layout_as_root(gpui::AvailableSpace::min_size(), window, cx);
                let x = match widget.anchor {
                    crate::code_view::WidgetAnchor::AfterText => {
                        let end = rl.x_offset + rl.line.width;
                        (layout.text_origin_x + end + layout.char_width * 2.)
                            .min(right - size.width)
                    }
                    crate::code_view::WidgetAnchor::Right => right - size.width,
                };
                let x = x.max(layout.text_bounds.left());
                let y = rl.y + ((lh - size.height) / 2.).max(px(0.));
                element.prepaint_at(point(x, y), window, cx);
                out.push(element);
            }
        },
    );
    out
}

pub(super) fn shape_plain(
    window: &mut Window,
    text: &str,
    font_size: Pixels,
    color: Hsla,
    settings: &EditorSettings,
) -> ShapedLine {
    let run = TextRun {
        len: text.len(),
        font: settings.font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window.text_system().shape_line(
        SharedString::from(text.to_string()),
        font_size,
        &[run],
        None,
    )
}

pub(super) fn shape_annotation(
    window: &mut Window,
    text: &str,
    font_size: Pixels,
    color: Hsla,
    settings: &EditorSettings,
) -> ShapedLine {
    let mut font = settings.font();
    font.style = FontStyle::Italic;
    let run = TextRun {
        len: text.len(),
        font,
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window.text_system().shape_line(
        SharedString::from(text.to_string()),
        font_size * 0.95,
        &[run],
        None,
    )
}

pub(super) fn paint_hatch(window: &mut Window, bounds: Bounds<Pixels>, color: Hsla) {
    // Diagonal stripes approximated with short vertical slivers per row of
    // pixels would be costly; draw evenly spaced thin diagonal bands using
    // stepped quads (cheap and crisp at typical row heights).
    let step = px(8.);
    let h = bounds.size.height;
    let mut x = bounds.left() - h;
    while x < bounds.right() {
        let mut y = px(0.);
        while y < h {
            let seg = px(2.);
            let bx = x + y;
            if bx + px(1.5) >= bounds.left() && bx <= bounds.right() {
                window.paint_quad(fill(
                    Bounds::new(point(bx, bounds.top() + y), size(px(1.5), seg)),
                    color,
                ));
            }
            y += seg;
        }
        x += step;
    }
}

pub(super) fn paint_indent_guides(
    window: &mut Window,
    layout: &LayoutSnapshot,
    source: &dyn crate::code_view::CodeSource,
    cursor_row: u32,
    ui: &lector_theme::UiColors,
) {
    let tab = layout.tab_size;
    let indents: Vec<Option<usize>> = layout
        .rows
        .iter()
        .map(|rl| {
            let line = &rl.expanded.text;
            (!line.trim().is_empty()).then(|| indent_width(line, tab))
        })
        .collect();
    // Blank rows inherit the smaller of the surrounding indents.
    let n = indents.len();
    let mut resolved = vec![0usize; n];
    let mut prev: Option<usize> = None;
    let mut next_nonblank: Vec<Option<usize>> = vec![None; n];
    let mut next: Option<usize> = None;
    for i in (0..n).rev() {
        if indents[i].is_some() {
            next = indents[i];
        }
        next_nonblank[i] = next;
    }
    for i in 0..n {
        resolved[i] = match indents[i] {
            Some(v) => {
                prev = Some(v);
                v
            }
            None => prev.unwrap_or(0).min(next_nonblank[i].unwrap_or(0)),
        };
    }
    // Wrapped continuation rows: same as their first segment.
    for (i, rl) in layout.rows.iter().enumerate() {
        if !rl.first_seg && i > 0 {
            resolved[i] = resolved[i - 1];
        }
    }
    let cursor_level = layout
        .rows
        .iter()
        .position(|r| r.row == cursor_row)
        .map(|i| resolved[i])
        .or_else(|| {
            let line = source.line(cursor_row);
            Some(indent_width(&line, tab))
        })
        .unwrap_or(0);
    // The active guide is the innermost one enclosing the cursor line.
    let active_col = (cursor_level / tab).checked_sub(1).map(|l| l * tab);
    let cursor_ix = layout.rows.iter().position(|r| r.row == cursor_row);
    let (mut active_lo, mut active_hi) = (usize::MAX, 0usize);
    if let (Some(col), Some(ci)) = (active_col, cursor_ix) {
        let mut lo = ci;
        while lo > 0 && resolved[lo - 1] > col {
            lo -= 1;
        }
        let mut hi = ci;
        while hi + 1 < n && resolved[hi + 1] > col {
            hi += 1;
        }
        active_lo = lo;
        active_hi = hi;
    }
    let lh = layout.line_height;
    for (i, rl) in layout.rows.iter().enumerate() {
        let levels = resolved[i] / tab;
        for level in 0..levels {
            let col = level * tab;
            let x = layout.text_origin_x + layout.char_width * col as f32;
            if x < layout.text_bounds.left() + px(TEXT_LEFT_PAD) - px(1.) {
                continue;
            }
            let active = Some(col) == active_col && i >= active_lo && i <= active_hi;
            let color = if active {
                ui.indent_guide_active.hsla()
            } else {
                ui.indent_guide.hsla()
            };
            window.paint_quad(fill(
                Bounds::new(point(x.round(), rl.y), size(px(1.), lh)),
                color,
            ));
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn paint_gutter(
    window: &mut Window,
    cx: &mut App,
    layout: &LayoutSnapshot,
    source: &dyn crate::code_view::CodeSource,
    cursor_row: u32,
    folded: &HashSet<u32>,
    gutter_hovered: bool,
    settings: &EditorSettings,
    theme: &lector_theme::Theme,
) {
    let ui = &theme.ui;
    let lh = layout.line_height;
    let first = layout.rows.first().map(|r| r.row).unwrap_or(0);
    let last = layout.rows.last().map(|r| r.row).unwrap_or(0);
    let marks = source.gutter_marks(first..last + 1);

    for rl in &layout.rows {
        // Line numbers.
        if rl.first_seg && layout.line_number_right > layout.bounds.left() + px(GUTTER_LEFT_PAD) {
            let custom = source
                .line_number_chars()
                .and_then(|_| source.line_number_label(rl.row));
            if let Some(label) = custom {
                let color = if rl.row == cursor_row {
                    ui.line_number_active.hsla()
                } else {
                    ui.line_number.hsla()
                };
                let shaped = shape_plain(window, &label, layout.font_size * 0.93, color, settings);
                let _ = shaped.paint(
                    point(layout.line_number_right - shaped.width, rl.y),
                    lh,
                    window,
                    cx,
                );
            } else if let Some(n) = source.line_number(rl.row) {
                let is_cursor = rl.row == cursor_row;
                let label = if settings.relative_line_numbers && !is_cursor {
                    rl.row.abs_diff(cursor_row).to_string()
                } else {
                    (n + 1).to_string()
                };
                let color = if is_cursor {
                    ui.line_number_active.hsla()
                } else {
                    ui.line_number.hsla()
                };
                let shaped = shape_plain(window, &label, layout.font_size * 0.93, color, settings);
                let _ = shaped.paint(
                    point(layout.line_number_right - shaped.width, rl.y),
                    lh,
                    window,
                    cx,
                );
            }
        }
        // Diff bars.
        for mark in &marks {
            let color = match mark.kind {
                GutterMarkKind::Added => theme.diff.added_gutter.hsla(),
                GutterMarkKind::Modified => theme.diff.modified_gutter.hsla(),
                GutterMarkKind::Deleted => theme.diff.removed_gutter.hsla(),
            };
            match mark.kind {
                GutterMarkKind::Deleted => {
                    if rl.first_seg && mark.rows.start == rl.row {
                        window.paint_quad(
                            fill(
                                Bounds::new(
                                    point(layout.diff_bar_x - px(1.), rl.y - px(3.)),
                                    size(px(7.), px(6.)),
                                ),
                                color,
                            )
                            .corner_radii(Corners::all(px(1.5))),
                        );
                    }
                }
                _ => {
                    if mark.rows.start <= rl.row && rl.row < mark.rows.end {
                        let top_round = rl.row == mark.rows.start && rl.first_seg;
                        let bottom_round = rl.row + 1 == mark.rows.end && rl.last_seg;
                        let radii = Corners {
                            top_left: if top_round { px(1.5) } else { px(0.) },
                            top_right: if top_round { px(1.5) } else { px(0.) },
                            bottom_left: if bottom_round { px(1.5) } else { px(0.) },
                            bottom_right: if bottom_round { px(1.5) } else { px(0.) },
                        };
                        window.paint_quad(
                            fill(
                                Bounds::new(point(layout.diff_bar_x, rl.y), size(px(3.), lh)),
                                color,
                            )
                            .corner_radii(radii),
                        );
                    }
                }
            }
        }
        // Fold markers.
        if rl.first_seg && layout.fold_starts.contains(&rl.row) {
            let is_folded = folded.contains(&rl.row);
            if is_folded || gutter_hovered {
                let icon = if is_folded {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                };
                let color = if is_folded {
                    ui.text_muted.hsla()
                } else {
                    ui.fold_marker.hsla()
                };
                let s = px(12.);
                let b = Bounds::new(
                    point(
                        layout.fold_x + (px(FOLD_COLUMN) - s) / 2.,
                        rl.y + (lh - s) / 2.,
                    ),
                    size(s, s),
                );
                let _ = window.paint_svg(
                    b,
                    ui_icon_path(icon),
                    TransformationMatrix::unit(),
                    color,
                    cx,
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn paint_minimap(
    window: &mut Window,
    layout: &LayoutSnapshot,
    display_map: &DisplayMap,
    mb: Bounds<Pixels>,
    source: &dyn crate::code_view::CodeSource,
    search_matches: &[(u32, Range<usize>)],
    theme: &lector_theme::Theme,
    hovered: bool,
) {
    let ui = &theme.ui;
    window.paint_quad(fill(mb, ui.minimap_bg.hsla()));
    window.paint_quad(fill(
        Bounds::new(mb.origin, size(px(1.), mb.size.height)),
        ui.border_variant.hsla(),
    ));
    let row_h = px(MINIMAP_ROW);
    let capacity = (mb.size.height / row_h).floor() as u32;
    let top = layout.minimap_top_row;
    let end = (top + capacity).min(layout.display_row_count);
    let left = mb.left() + px(8.);
    let max_w = mb.size.width - px(14.);
    let text = ui.minimap_text.hsla();
    let comment = theme
        .syntax
        .style_for("comment")
        .map(|s| s.color.hsla().opacity(0.45))
        .unwrap_or(text);
    let mut last_row = u32::MAX;
    let mut rows_in_view: Vec<(u32, u32)> = Vec::new();
    for d in top..end {
        let (row, _) = display_map.display_to_row(d);
        if row == last_row {
            continue;
        }
        last_row = row;
        rows_in_view.push((d, row));
    }
    let y_for = |d: u32| mb.top() + row_h * (d - top) as f32;
    for &(d, row) in &rows_in_view {
        let line = source.line(row);
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let indent = indent_width(&line, layout.tab_size) as f32;
        let width = display_column(&line, line.len(), layout.tab_size) as f32;
        let x0 = left + px(indent.min(80.));
        let x1 = (left + px(width.min(f32::from(max_w)))).min(left + max_w);
        if x1 <= x0 {
            continue;
        }
        let color = if trimmed.starts_with("//") || trimmed.starts_with('#') {
            comment
        } else {
            text
        };
        window.paint_quad(fill(
            Bounds::new(point(x0, y_for(d)), size(x1 - x0, px(2.))),
            color,
        ));
    }
    // Diff marks.
    let marks = source.gutter_marks(
        rows_in_view.first().map(|r| r.1).unwrap_or(0)
            ..rows_in_view.last().map(|r| r.1 + 1).unwrap_or(0),
    );
    for &(d, row) in &rows_in_view {
        for m in &marks {
            if m.rows.start <= row && row < m.rows.end.max(m.rows.start + 1) {
                let color = match m.kind {
                    GutterMarkKind::Added => theme.diff.added_gutter.hsla(),
                    GutterMarkKind::Modified => theme.diff.modified_gutter.hsla(),
                    GutterMarkKind::Deleted => theme.diff.removed_gutter.hsla(),
                };
                window.paint_quad(fill(
                    Bounds::new(point(mb.left() + px(2.), y_for(d)), size(px(3.), row_h)),
                    color,
                ));
            }
        }
    }
    // Search matches.
    let rows: HashSet<u32> = rows_in_view.iter().map(|r| r.1).collect();
    for &(d, row) in &rows_in_view {
        if rows.contains(&row)
            && search_matches
                .binary_search_by_key(&row, |(r, _)| *r)
                .is_ok()
        {
            window.paint_quad(fill(
                Bounds::new(point(left, y_for(d) - px(0.5)), size(max_w, px(3.))),
                ui.search_match_active.hsla(),
            ));
        }
    }
    // Viewport slider.
    let first = layout.first_display_row.max(top);
    let visible = (layout.bounds.size.height / layout.line_height).ceil() as u32;
    let slider_top = mb.top() + row_h * (first.saturating_sub(top)) as f32;
    let slider_h = (row_h * visible as f32).min(mb.bottom() - slider_top);
    if slider_h > px(0.) {
        let color = if hovered {
            ui.minimap_slider_hover.hsla()
        } else {
            ui.minimap_slider.hsla()
        };
        window.paint_quad(fill(
            Bounds::new(
                point(mb.left() + px(1.), slider_top),
                size(mb.size.width - px(1.), slider_h),
            ),
            color,
        ));
    }
}

/// Underlines (diagnostics) under byte ranges of visible rows.
pub(super) fn paint_underlines(
    window: &mut Window,
    layout: &LayoutSnapshot,
    underlines: &[crate::code_view::Underline],
    text_x: Pixels,
) {
    if underlines.is_empty() {
        return;
    }
    let (Some(first), Some(last)) = (layout.rows.first(), layout.rows.last()) else {
        return;
    };
    let lo = underlines.partition_point(|u| u.row < first.row);
    for u in underlines[lo..].iter().take_while(|u| u.row <= last.row) {
        for rl in layout.rows.iter().filter(|r| r.row == u.row) {
            let start = u.range.start.max(rl.seg_start);
            let mut end = u.range.end.min(rl.seg_end);
            if start > rl.seg_end || u.range.end < rl.seg_start {
                continue;
            }
            let x0 = rl.x_for_col(start);
            let mut x1 = rl.x_for_col(end);
            if x1 <= x0 {
                // Empty range or at the end of the line: one char wide.
                end = start;
                x1 = rl.x_for_col(end) + layout.char_width;
            }
            let thickness = px(1.);
            window.paint_underline(
                point(text_x + x0, rl.y + layout.line_height - px(3.5)),
                x1 - x0,
                &gpui::UnderlineStyle {
                    thickness,
                    color: Some(u.color),
                    wavy: u.wavy,
                },
            );
        }
    }
}

/// Icons in the gutter's left margin (diagnostic severity, lightbulb).
pub(super) fn paint_gutter_icons(
    window: &mut Window,
    cx: &mut App,
    layout: &LayoutSnapshot,
    icons: &[crate::code_view::GutterIcon],
) {
    if icons.is_empty() {
        return;
    }
    let lh = layout.line_height;
    let s = px(12.);
    for rl in layout.rows.iter().filter(|r| r.first_seg) {
        let Some(icon) = icons.iter().rev().find(|i| i.row == rl.row) else {
            continue;
        };
        let b = Bounds::new(
            point(layout.annotation_right + px(2.), rl.y + (lh - s) / 2.),
            size(s, s),
        );
        let _ = window.paint_svg(
            b,
            ui_icon_path(icon.icon),
            TransformationMatrix::unit(),
            icon.color,
            cx,
        );
    }
}

/// Scrollbar markers (diagnostics), in the middle of the track.
pub(super) fn paint_scrollbar_marks(
    window: &mut Window,
    layout: &LayoutSnapshot,
    marks: &[(u32, Hsla)],
) {
    if marks.is_empty() || layout.max_scroll.y <= px(0.) {
        return;
    }
    let track = layout.vscroll_track();
    let total = layout.display_row_count.max(1) as f32;
    let mut last: Option<(Pixels, Hsla)> = None;
    for (row, color) in marks.iter().take(4000) {
        let d = layout.display_row_count.min(*row) as f32;
        let y = track.top() + track.size.height * (d / total);
        if last.is_some_and(|(ly, lc)| y - ly < px(2.) && lc == *color) {
            continue;
        }
        last = Some((y, *color));
        window.paint_quad(fill(
            Bounds::new(point(track.left() + px(3.), y), size(px(6.), px(2.))),
            *color,
        ));
    }
}

pub(super) fn paint_scrollbars(
    window: &mut Window,
    layout: &LayoutSnapshot,
    source: &dyn crate::code_view::CodeSource,
    search_matches: &[(u32, Range<usize>)],
    theme: &lector_theme::Theme,
    dragging: bool,
) {
    let ui = &theme.ui;
    let track = layout.vscroll_track();
    let total = layout.display_row_count.max(1) as f32;
    let hovered = track.contains(&window.mouse_position()) || dragging;
    // Markers: diff marks (left half) and search matches (right half).
    if layout.max_scroll.y > px(0.) {
        let marks = source.gutter_marks(0..source.line_count());
        for m in marks.iter().take(4000) {
            let color = match m.kind {
                GutterMarkKind::Added => theme.diff.added_gutter.hsla(),
                GutterMarkKind::Modified => theme.diff.modified_gutter.hsla(),
                GutterMarkKind::Deleted => theme.diff.removed_gutter.hsla(),
            };
            let y0 = track.top() + track.size.height * (m.rows.start as f32 / total);
            let h = (track.size.height
                * ((m.rows.end.saturating_sub(m.rows.start)) as f32 / total))
                .max(px(2.));
            window.paint_quad(fill(
                Bounds::new(point(track.left() + px(2.), y0), size(px(4.), h)),
                color.opacity(0.85),
            ));
        }
        let mut last_y = px(-10.);
        for (row, _) in search_matches {
            let y = track.top() + track.size.height * (*row as f32 / total);
            if y - last_y < px(2.) {
                continue;
            }
            last_y = y;
            window.paint_quad(fill(
                Bounds::new(point(track.right() - px(6.), y), size(px(4.), px(2.))),
                ui.search_match_active_border.hsla(),
            ));
        }
    }
    if let Some((start, len)) = thumb_geometry(
        f32::from(track.size.height),
        f32::from(layout.bounds.size.height),
        f32::from(layout.content_height()),
        f32::from(layout.scroll.y),
    ) {
        let w = if hovered { px(8.) } else { px(6.) };
        let color = if hovered {
            ui.scrollbar_thumb_hover.hsla()
        } else {
            ui.scrollbar_thumb.hsla()
        };
        let b = Bounds::new(
            point(
                track.right() - w - (px(SCROLLBAR) - w) / 2.,
                track.top() + px(start),
            ),
            size(w, px(len)),
        );
        window.paint_quad(fill(b, color).corner_radii(Corners::all(w / 2.)));
    }
    if let Some(htrack) = layout.hscroll_track() {
        let content_w = layout.max_scroll.x + layout.text_bounds.size.width;
        if let Some((start, len)) = thumb_geometry(
            f32::from(htrack.size.width),
            f32::from(layout.text_bounds.size.width),
            f32::from(content_w),
            f32::from(layout.scroll.x),
        ) {
            let h = px(6.);
            let b = Bounds::new(
                point(htrack.left() + px(start), htrack.bottom() - h - px(3.)),
                size(px(len), h),
            );
            window
                .paint_quad(fill(b, ui.scrollbar_thumb.hsla()).corner_radii(Corners::all(h / 2.)));
        }
    }
}
