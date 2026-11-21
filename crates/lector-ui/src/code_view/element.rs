//! The custom GPUI element that lays out and paints a [`CodeView`].
//!
//! Only visible display rows are shaped (through an LRU line cache); all
//! geometry needed by mouse handlers is kept in a [`LayoutSnapshot`].

use std::collections::HashSet;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    fill, point, px, quad, relative, size, App, BorderStyle, Bounds, ContentMask, CursorStyle,
    Element, ElementId, ElementInputHandler, Entity, GlobalElementId, Hitbox, HitboxBehavior,
    IntoElement, LayoutId, Pixels, Point, ShapedLine, Style, Window,
};

use super::display_map::ExpandedLine;
use super::selection::{self, Pos};
use super::{decoration_color, CodeView, EditorSettings};
use crate::theme::{ActiveTheme, ColorExt};

mod layout;
mod mouse;
mod paint;

use mouse::register_mouse_handlers;
use paint::{
    paint_gutter, paint_gutter_annotations, paint_gutter_icons, paint_hatch, paint_indent_guides,
    paint_minimap, paint_scrollbar_marks, paint_scrollbars, paint_underlines, prepaint_row_widgets,
    shape_annotation, shape_plain,
};

const GUTTER_LEFT_PAD: f32 = 14.;
const FOLD_COLUMN: f32 = 14.;
const TEXT_LEFT_PAD: f32 = 6.;
const SCROLLBAR: f32 = 12.;
const MINIMAP_WIDTH: f32 = 96.;
const MINIMAP_ROW: f32 = 3.;
const MAX_STICKY: usize = 4;
const WRAP_MAX_LINES: u32 = 200_000;

/// The element; construct with [`CodeElement::new`] (done by `CodeView::render`).
pub struct CodeElement {
    view: Entity<CodeView>,
}

impl CodeElement {
    /// Element painting `view`.
    pub fn new(view: Entity<CodeView>) -> Self {
        CodeElement { view }
    }
}

/// Geometry of one painted display row.
pub(crate) struct RowLayout {
    pub display_row: u32,
    pub row: u32,
    pub seg_start: usize,
    pub seg_end: usize,
    pub disp_start: usize,
    pub expanded: Rc<ExpandedLine>,
    pub line: ShapedLine,
    /// Window y of the row's top.
    pub y: Pixels,
    pub first_seg: bool,
    pub last_seg: bool,
    /// Horizontal offset of continuation rows (wrap indent).
    pub x_offset: Pixels,
}

impl RowLayout {
    /// x of source byte `col` relative to the text origin.
    fn x_for_col(&self, col: usize) -> Pixels {
        let col = col.clamp(self.seg_start, self.seg_end);
        let disp = self
            .expanded
            .to_display(col)
            .saturating_sub(self.disp_start);
        self.x_offset + self.line.x_for_index(disp.min(self.line.len()))
    }

    /// Source byte closest to `x` (relative to the text origin).
    fn col_for_x(&self, x: Pixels) -> usize {
        let disp = self
            .line
            .closest_index_for_x((x - self.x_offset).max(px(0.)));
        let col = self.expanded.to_source(self.disp_start + disp);
        let col = col.clamp(self.seg_start, self.seg_end);
        if !self.last_seg && col == self.seg_end && self.seg_end > self.seg_start {
            // Clicking past a wrapped segment's end stays on this row.
            return self.seg_end.saturating_sub(1).max(self.seg_start);
        }
        col
    }
}

/// Everything needed to paint and hit-test the last frame.
pub(crate) struct LayoutSnapshot {
    pub bounds: Bounds<Pixels>,
    pub gutter_bounds: Bounds<Pixels>,
    pub text_bounds: Bounds<Pixels>,
    pub minimap_bounds: Option<Bounds<Pixels>>,
    pub line_height: Pixels,
    pub char_width: Pixels,
    pub font_size: Pixels,
    pub text_origin_x: Pixels,
    pub scroll: Point<Pixels>,
    pub max_scroll: Point<Pixels>,
    pub display_row_count: u32,
    pub first_display_row: u32,
    pub rows: Vec<RowLayout>,
    pub sticky: Vec<RowLayout>,
    pub wrap_cols: Option<usize>,
    pub tab_size: usize,
    pub fold_starts: HashSet<u32>,
    pub line_number_right: Pixels,
    /// Right edge of the gutter annotation (blame) column; `bounds.left()` when hidden.
    pub annotation_right: Pixels,
    pub diff_bar_x: Pixels,
    pub fold_x: Pixels,
    pub minimap_top_row: u32,
}

impl LayoutSnapshot {
    pub(crate) fn visible_rows(&self) -> usize {
        (self.bounds.size.height / self.line_height).floor() as usize
    }

    fn row_at_display(&self, d: u32) -> Option<&RowLayout> {
        let first = self.rows.first()?.display_row;
        let ix = d.checked_sub(first)? as usize;
        self.rows.get(ix).filter(|r| r.display_row == d)
    }

    fn vscroll_track(&self) -> Bounds<Pixels> {
        Bounds::new(
            point(
                self.text_bounds.right() - px(SCROLLBAR),
                self.text_bounds.top(),
            ),
            size(px(SCROLLBAR), self.text_bounds.size.height),
        )
    }

    fn hscroll_track(&self) -> Option<Bounds<Pixels>> {
        (self.max_scroll.x > px(0.)).then(|| {
            Bounds::new(
                point(
                    self.text_bounds.left(),
                    self.text_bounds.bottom() - px(SCROLLBAR),
                ),
                size(self.text_bounds.size.width - px(SCROLLBAR), px(SCROLLBAR)),
            )
        })
    }

    fn content_height(&self) -> Pixels {
        self.max_scroll.y + self.bounds.size.height
    }

    fn sticky_height(&self) -> Pixels {
        self.line_height * self.sticky.len() as f32
    }
}

/// Hitboxes registered during prepaint.
pub struct PrepaintState {
    layout: Rc<LayoutSnapshot>,
    hitbox: Hitbox,
    text_hitbox: Hitbox,
    gutter_hitbox: Hitbox,
    /// Row widgets (`layers`), laid out and prepainted above the code's hitboxes.
    widgets: Vec<gpui::AnyElement>,
}

impl IntoElement for CodeElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for CodeElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> PrepaintState {
        let started = std::time::Instant::now();
        let layout = self
            .view
            .update(cx, |view, cx| view.layout(bounds, window, cx));
        tracing::trace!(target: "lector_ui::perf", rows = layout.rows.len(), elapsed_us = started.elapsed().as_micros() as u64, "code view layout");
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let text_hitbox = window.insert_hitbox(layout.text_bounds, HitboxBehavior::Normal);
        let gutter_hitbox = window.insert_hitbox(layout.gutter_bounds, HitboxBehavior::Normal);
        let widgets = prepaint_row_widgets(&self.view, &layout, window, cx);
        PrepaintState {
            layout,
            hitbox,
            text_hitbox,
            gutter_hitbox,
            widgets,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        state: &mut PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let started = std::time::Instant::now();
        let layout = state.layout.clone();
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let view = self.view.read(cx);
        let focused = view.focus_handle.is_focused(window);
        let selection = view.selection();
        let selections: Vec<super::Selection> = view.selections().to_vec();
        let single_caret = selections.len() == 1;
        let cursor_visible = view.cursor_visible;
        let editable = view.is_editable();
        let focus_handle = view.focus_handle.clone();
        let marked: Option<(Pos, Pos)> = view.marked_range().and_then(|r| {
            let text = view.buffer()?.read(cx).text().clone();
            Some((
                super::edit::offset_to_pos(&text, r.start),
                super::edit::offset_to_pos(&text, r.end),
            ))
        });
        let (search_matches, active_match) = view.search_matches();
        let search_matches: Vec<(u32, Range<usize>)> = search_matches.to_vec();
        let decorations = view.decorations().to_vec();
        let layer_backgrounds = view.layers.backgrounds();
        let annotations = view.annotations().clone();
        let gutter_annotations = view.gutter_annotations().cloned();
        let inline_marks = view.inline_marks().clone();
        let source = view.source().clone();
        let gutter_hovered = view.gutter_hovered;
        let folded: HashSet<u32> = layout
            .fold_starts
            .iter()
            .copied()
            .filter(|r| view.is_folded(*r))
            .collect();
        let settings = EditorSettings::get(cx);
        let lh = layout.line_height;
        let display_map = view.display_map.clone();
        let scrollbar_dragging = view.scrollbar_drag.is_some();

        window.set_cursor_style(CursorStyle::IBeam, &state.text_hitbox);
        window.set_cursor_style(CursorStyle::Arrow, &state.gutter_hitbox);

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.paint_quad(fill(bounds, ui.background.hsla()));

            let first_row = layout.rows.first().map(|r| r.row).unwrap_or(0);
            let last_row = layout.rows.last().map(|r| r.row).unwrap_or(0);
            // Rows holding an empty caret get the current-line background.
            let caret_rows: HashSet<u32> = selections
                .iter()
                .filter(|s| s.is_empty() && s.head.row >= first_row && s.head.row <= last_row)
                .map(|s| s.head.row)
                .collect();
            // Selections intersecting the visible rows (sorted, non-overlapping).
            let visible_sels: &[super::Selection] = {
                let lo = selections.partition_point(|s| s.range().1 < Pos::new(first_row, 0));
                let hi =
                    selections.partition_point(|s| s.range().0 <= Pos::new(last_row, usize::MAX));
                &selections[lo..hi.max(lo)]
            };

            // Current line (across gutter and text).
            for rl in layout.rows.iter().filter(|r| caret_rows.contains(&r.row)) {
                window.paint_quad(fill(
                    Bounds::new(
                        point(bounds.left(), rl.y),
                        size(layout.text_bounds.right() - bounds.left(), lh),
                    ),
                    ui.active_line.hsla(),
                ));
            }

            // Row decorations from the source (diff backgrounds).
            let row_decorations = source.row_decorations(first_row..last_row + 1);
            let text_right = layout.text_bounds.right();
            window.with_content_mask(
                Some(ContentMask {
                    bounds: layout.text_bounds,
                }),
                |window| {
                    let text_x = layout.text_origin_x;
                    let range_rect = |rl: &RowLayout,
                                      range: &Range<usize>|
                     -> Option<Bounds<Pixels>> {
                        if range.end < rl.seg_start
                            || range.start > rl.seg_end
                            || (range.start == range.end && range.start != rl.seg_start)
                        {
                            return None;
                        }
                        let x0 = rl.x_for_col(range.start.max(rl.seg_start));
                        let x1 = rl.x_for_col(range.end.min(rl.seg_end));
                        (x1 > x0).then(|| Bounds::new(point(text_x + x0, rl.y), size(x1 - x0, lh)))
                    };

                    for deco in &row_decorations {
                        for rl in layout.rows.iter().filter(|r| r.row == deco.row) {
                            if let Some(bg) = deco.background {
                                let color = decoration_color(bg, &theme);
                                if matches!(bg, super::DecorationColor::Filler) {
                                    paint_hatch(
                                        window,
                                        Bounds::new(
                                            point(layout.text_bounds.left(), rl.y),
                                            size(layout.text_bounds.size.width, lh),
                                        ),
                                        color,
                                    );
                                } else {
                                    window.paint_quad(fill(
                                        Bounds::new(
                                            point(layout.text_bounds.left(), rl.y),
                                            size(layout.text_bounds.size.width, lh),
                                        ),
                                        color,
                                    ));
                                }
                            }
                            for (range, color) in &deco.ranges {
                                if let Some(r) = range_rect(rl, range) {
                                    window.paint_quad(
                                        fill(r, decoration_color(*color, &theme))
                                            .corner_radii(px(2.)),
                                    );
                                }
                            }
                        }
                    }

                    // Owner row backgrounds (`layers`).
                    for bg in layer_backgrounds.iter().flat_map(|l| l.iter()) {
                        for rl in layout.rows.iter().filter(|r| bg.rows.contains(&r.row)) {
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(layout.text_bounds.left(), rl.y),
                                    size(layout.text_bounds.size.width, lh),
                                ),
                                bg.color,
                            ));
                            if let Some(edge) = bg.edge {
                                window.paint_quad(fill(
                                    Bounds::new(
                                        point(layout.text_bounds.left(), rl.y),
                                        size(px(2.), lh),
                                    ),
                                    edge,
                                ));
                            }
                        }
                    }

                    // Owner decorations.
                    for deco in decorations
                        .iter()
                        .filter(|d| d.row >= first_row && d.row <= last_row)
                    {
                        for rl in layout.rows.iter().filter(|r| r.row == deco.row) {
                            if let Some(r) = range_rect(rl, &deco.range) {
                                window.paint_quad(
                                    fill(r, decoration_color(deco.color, &theme))
                                        .corner_radii(px(2.)),
                                );
                            }
                        }
                    }

                    // Occurrences of the word under the cursor.
                    if single_caret && selection.is_empty() && focused {
                        if let Some((_, _, word)) =
                            selection::word_at(source.as_ref(), selection.head)
                        {
                            for rl in &layout.rows {
                                let line = source.line(rl.row);
                                for (ix, _) in line.match_indices(word.as_str()) {
                                    let end = ix + word.len();
                                    let before = line[..ix].chars().next_back();
                                    let after = line[end..].chars().next();
                                    let is_word = |c: Option<char>| {
                                        c.is_some_and(|c| c.is_alphanumeric() || c == '_')
                                    };
                                    if is_word(before) || is_word(after) {
                                        continue;
                                    }
                                    if let Some(r) = range_rect(rl, &(ix..end)) {
                                        window.paint_quad(
                                            fill(r, ui.word_highlight.hsla()).corner_radii(px(2.)),
                                        );
                                    }
                                }
                            }
                        }
                    }

                    // Search matches.
                    let lo = search_matches.partition_point(|(row, _)| *row < first_row);
                    for (ix, (row, range)) in search_matches.iter().enumerate().skip(lo) {
                        if *row > last_row {
                            break;
                        }
                        let active = active_match == Some(ix);
                        for rl in layout.rows.iter().filter(|r| r.row == *row) {
                            if let Some(r) = range_rect(rl, range) {
                                if active {
                                    window.paint_quad(quad(
                                        r,
                                        px(2.),
                                        ui.search_match_active.hsla(),
                                        px(1.),
                                        ui.search_match_active_border.hsla(),
                                        BorderStyle::Solid,
                                    ));
                                } else {
                                    window.paint_quad(
                                        fill(r, ui.search_match.hsla()).corner_radii(px(2.)),
                                    );
                                }
                            }
                        }
                    }

                    // Selections.
                    let color = if focused {
                        ui.selection.hsla()
                    } else {
                        ui.selection_inactive.hsla()
                    };
                    for sel in visible_sels.iter().filter(|s| !s.is_empty()) {
                        let (sel_start, sel_end) = sel.range();
                        for rl in &layout.rows {
                            let pos_start = Pos::new(rl.row, rl.seg_start);
                            let pos_end = Pos::new(rl.row, rl.seg_end);
                            if pos_end < sel_start || pos_start > sel_end {
                                continue;
                            }
                            let s = if sel_start.row == rl.row {
                                sel_start.col.max(rl.seg_start)
                            } else {
                                rl.seg_start
                            };
                            let e = if sel_end.row == rl.row {
                                sel_end.col.min(rl.seg_end)
                            } else {
                                rl.seg_end
                            };
                            if sel_start.row == rl.row && sel_start.col > rl.seg_end {
                                continue;
                            }
                            let x0 = rl.x_for_col(s);
                            let mut x1 = rl.x_for_col(e);
                            // Include the newline when the selection continues past this row.
                            if rl.last_seg && sel_end.row > rl.row {
                                x1 += layout.char_width * 0.6;
                            }
                            if x1 > x0 {
                                window.paint_quad(fill(
                                    Bounds::new(point(text_x + x0, rl.y), size(x1 - x0, lh)),
                                    color,
                                ));
                            }
                        }
                    }

                    // Indent guides.
                    if settings.indent_guides {
                        paint_indent_guides(
                            window,
                            &layout,
                            source.as_ref(),
                            selection.head.row,
                            ui,
                        );
                    }

                    // Text.
                    for rl in &layout.rows {
                        let _ = rl
                            .line
                            .paint(point(text_x + rl.x_offset, rl.y), lh, window, cx);
                        // Folded placeholder.
                        if rl.last_seg && folded.contains(&rl.row) {
                            let x = text_x + rl.x_offset + rl.line.width + layout.char_width;
                            let badge = Bounds::new(
                                point(x, rl.y + px(3.)),
                                size(layout.char_width * 3., lh - px(6.)),
                            );
                            window.paint_quad(quad(
                                badge,
                                px(3.),
                                ui.hover.hsla(),
                                px(1.),
                                ui.border.hsla(),
                                BorderStyle::Solid,
                            ));
                            let dots = shape_plain(
                                window,
                                "⋯",
                                layout.font_size,
                                ui.fold_marker.hsla(),
                                &settings,
                            );
                            let _ = dots.paint(
                                point(x + (badge.size.width - dots.width) / 2., rl.y),
                                lh,
                                window,
                                cx,
                            );
                        }
                        // Inline annotation (blame).
                        if rl.last_seg {
                            if let Some(note) = annotations.get(&rl.row) {
                                let x =
                                    text_x + rl.x_offset + rl.line.width + layout.char_width * 4.;
                                let shaped = shape_annotation(
                                    window,
                                    note,
                                    layout.font_size,
                                    ui.inline_annotation.hsla(),
                                    &settings,
                                );
                                let _ = shaped.paint(point(x, rl.y), lh, window, cx);
                            }
                        }
                    }

                    // Language-feature underlines (diagnostics).
                    paint_underlines(window, &layout, &inline_marks.underlines, text_x);

                    // IME composition: underline the marked text.
                    if let Some((m_start, m_end)) = marked {
                        for rl in &layout.rows {
                            if rl.row < m_start.row || rl.row > m_end.row {
                                continue;
                            }
                            let s = if rl.row == m_start.row {
                                m_start.col
                            } else {
                                rl.seg_start
                            };
                            let e = if rl.row == m_end.row {
                                m_end.col
                            } else {
                                rl.seg_end
                            };
                            if e < rl.seg_start || s > rl.seg_end {
                                continue;
                            }
                            let x0 = rl.x_for_col(s.max(rl.seg_start));
                            let x1 = rl.x_for_col(e.min(rl.seg_end));
                            if x1 > x0 {
                                window.paint_quad(fill(
                                    Bounds::new(
                                        point(text_x + x0, rl.y + lh - px(3.)),
                                        size(x1 - x0, px(1.5)),
                                    ),
                                    ui.text.hsla(),
                                ));
                            }
                        }
                    }

                    // Carets (every selection head; the primary is drawn last).
                    if focused && cursor_visible {
                        let caret_color = ui.cursor.hsla();
                        let paint_caret = |window: &mut Window, head: Pos, width: Pixels| {
                            if let Some(rl) = layout.rows.iter().find(|r| {
                                r.row == head.row
                                    && head.col >= r.seg_start
                                    && (head.col < r.seg_end
                                        || r.last_seg
                                        || head.col == r.seg_end && r.seg_end == r.seg_start)
                            }) {
                                let x = text_x + rl.x_for_col(head.col);
                                window.paint_quad(fill(
                                    Bounds::new(
                                        point(x - px(1.), rl.y + px(2.)),
                                        size(width, lh - px(4.)),
                                    ),
                                    caret_color,
                                ));
                            }
                        };
                        for sel in visible_sels {
                            if sel.head != selection.head {
                                paint_caret(window, sel.head, px(2.));
                            }
                        }
                        paint_caret(window, selection.head, px(2.));
                    }
                },
            );

            // Gutter (painted over horizontally scrolled text).
            window.paint_quad(fill(layout.gutter_bounds, ui.background.hsla()));
            for rl in layout.rows.iter().filter(|r| caret_rows.contains(&r.row)) {
                window.paint_quad(fill(
                    Bounds::new(
                        point(bounds.left(), rl.y),
                        size(layout.gutter_bounds.size.width, lh),
                    ),
                    ui.active_line.hsla(),
                ));
            }
            paint_gutter(
                window,
                cx,
                &layout,
                source.as_ref(),
                selection.head.row,
                &folded,
                gutter_hovered,
                &settings,
                &theme,
            );
            if let Some(ann) = &gutter_annotations {
                paint_gutter_annotations(window, cx, &layout, ann, &settings, &theme);
            }
            paint_gutter_icons(window, cx, &layout, &inline_marks.gutter_icons);

            // Sticky header.
            if !layout.sticky.is_empty() {
                let h = layout.sticky_height();
                let area = Bounds::new(bounds.origin, size(text_right - bounds.left(), h));
                window.paint_quad(fill(area, ui.sticky_header_bg.hsla()));
                window.with_content_mask(
                    Some(ContentMask {
                        bounds: layout.text_bounds,
                    }),
                    |window| {
                        for rl in &layout.sticky {
                            let _ =
                                rl.line
                                    .paint(point(layout.text_origin_x, rl.y), lh, window, cx);
                        }
                    },
                );
                for rl in &layout.sticky {
                    if let Some(n) = source.line_number(rl.row) {
                        let label = shape_plain(
                            window,
                            &(n + 1).to_string(),
                            layout.font_size,
                            ui.line_number.hsla(),
                            &settings,
                        );
                        let _ = label.paint(
                            point(layout.line_number_right - label.width, rl.y),
                            lh,
                            window,
                            cx,
                        );
                    }
                }
                window.paint_quad(fill(
                    Bounds::new(
                        point(bounds.left(), bounds.top() + h - px(1.)),
                        size(area.size.width, px(1.)),
                    ),
                    ui.sticky_header_border.hsla(),
                ));
                // Soft shadow under the header.
                for i in 0..4 {
                    let alpha = 0.10 - i as f32 * 0.025;
                    window.paint_quad(fill(
                        Bounds::new(
                            point(bounds.left(), bounds.top() + h + px(i as f32)),
                            size(area.size.width, px(1.)),
                        ),
                        ui.shadow.hsla().opacity(alpha.max(0.)),
                    ));
                }
            }

            if let Some(mb) = layout.minimap_bounds {
                paint_minimap(
                    window,
                    &layout,
                    &display_map,
                    mb,
                    source.as_ref(),
                    &search_matches,
                    &theme,
                    state.hitbox.is_hovered(window),
                );
            }
            paint_scrollbars(
                window,
                &layout,
                source.as_ref(),
                &search_matches,
                &theme,
                scrollbar_dragging,
            );
            paint_scrollbar_marks(window, &layout, &inline_marks.scrollbar);
        });

        if focused && editable {
            window.handle_input(
                &focus_handle,
                ElementInputHandler::new(bounds, self.view.clone()),
                cx,
            );
        }
        register_mouse_handlers(window, &self.view, &layout, &state.hitbox);
        if !state.widgets.is_empty() {
            window.with_content_mask(
                Some(ContentMask {
                    bounds: layout.text_bounds,
                }),
                |window| {
                    for widget in &mut state.widgets {
                        widget.paint(window, cx);
                    }
                },
            );
        }
        tracing::trace!(target: "lector_ui::perf", elapsed_us = started.elapsed().as_micros() as u64, "code view paint");
    }
}
