//! The custom GPUI element that lays out and paints a [`CodeView`].
//!
//! Only visible display rows are shaped (through an LRU line cache); all
//! geometry needed by mouse handlers is kept in a [`LayoutSnapshot`].

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    fill, point, px, quad, relative, size, App, BorderStyle, Bounds, ContentMask, Corners,
    CursorStyle, DispatchPhase, Element, ElementId, Entity, FontStyle, FontWeight, GlobalElementId,
    Hitbox, HitboxBehavior, Hsla, IntoElement, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollWheelEvent, ShapedLine, SharedString, Style,
    TextRun, TransformationMatrix, Window,
};
use smallvec::SmallVec;

use super::display_map::{
    byte_for_display_column, display_column, wrap_indent, wrap_segments, DisplayMap, ExpandedLine,
};
use super::line_cache::line_key;
use super::selection::{self, Pos};
use super::source::{indent_width, GutterMarkKind, HighlightSpan, SpanStyle};
use super::{decoration_color, CodeView, CodeViewEvent, EditorSettings, ScrollRequest};
use crate::components::scrollbar::thumb_geometry;
use crate::icons::{ui_icon_path, IconName};
use crate::theme::{ActiveTheme, ColorExt};

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

fn floor_char_boundary(s: &str, mut ix: usize) -> usize {
    ix = ix.min(s.len());
    while ix > 0 && !s.is_char_boundary(ix) {
        ix -= 1;
    }
    ix
}

struct RowText {
    expanded: Rc<ExpandedLine>,
    segments: SmallVec<[usize; 4]>,
    len: usize,
}

impl CodeView {
    /// Lays out the visible rows for `bounds`; called from prepaint.
    pub(crate) fn layout(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Rc<LayoutSnapshot> {
        let settings = EditorSettings::get(cx);
        let theme = cx.theme().clone();
        let tab = settings.tab_size.max(1);
        let font = settings.font();
        let font_size = settings.font_size;
        let line_height = (font_size * settings.line_height).round();
        let text_system = window.text_system().clone();
        let font_id = text_system.resolve_font(&font);
        let char_width = text_system
            .em_advance(font_id, font_size)
            .unwrap_or(font_size * 0.6);
        let source = self.source.clone();
        let line_count = source.line_count().max(1);

        // Gutter: [pad][line numbers][gap][diff bar][fold column][pad].
        let digits = source
            .line_number_chars()
            .unwrap_or_else(|| (line_count.max(1) as f64).log10().floor() as usize + 1);
        let ln_width = if self.line_numbers_visible() {
            char_width * digits.max(3) as f32
        } else {
            px(0.)
        };
        let annotation_width = self
            .gutter_annotations()
            .map(|a| (char_width * 0.9 * a.width_chars as f32 + px(18.)).round())
            .unwrap_or(px(0.));
        let annotation_right = bounds.left() + annotation_width;
        let line_number_right = annotation_right + px(GUTTER_LEFT_PAD) + ln_width;
        let diff_bar_x = line_number_right + px(8.);
        let fold_x = diff_bar_x + px(5.);
        let gutter_width = (fold_x + px(FOLD_COLUMN) + px(4.)) - bounds.left();
        let minimap_width = if self.show_minimap(cx) && bounds.size.width > px(640.) {
            px(MINIMAP_WIDTH)
        } else {
            px(0.)
        };
        let gutter_bounds = Bounds::new(bounds.origin, size(gutter_width, bounds.size.height));
        let text_bounds = Bounds::from_corners(
            point(bounds.left() + gutter_width, bounds.top()),
            point(bounds.right() - minimap_width, bounds.bottom()),
        );
        let minimap_bounds = (minimap_width > px(0.)).then(|| {
            Bounds::from_corners(
                point(text_bounds.right(), bounds.top()),
                bounds.bottom_right(),
            )
        });

        // Display map (folds + wrap).
        let wrap_cols = (self.soft_wrap(cx) && line_count <= WRAP_MAX_LINES).then(|| {
            let usable = text_bounds.size.width - px(TEXT_LEFT_PAD) - px(SCROLLBAR) - char_width;
            ((usable / char_width).floor() as usize).max(20)
        });
        let key = (
            source.version(),
            self.fold_generation(),
            wrap_cols,
            tab,
            line_count,
        );
        if self.display_map_key() != Some(key) {
            let mut map = DisplayMap::new(line_count);
            map.set_folds(self.active_folds());
            if wrap_cols.is_some() {
                map.set_wrap(wrap_cols, tab, |r| source.line(r).to_string());
            }
            self.set_display_map(map, key);
        }
        let display_rows = self.display_map.display_row_count();

        // Scroll limits and requests.
        let viewport_h = bounds.size.height;
        let content_h = line_height * display_rows as f32;
        let max_y = if content_h > viewport_h {
            content_h - viewport_h + (viewport_h * 0.4).round()
        } else {
            px(0.)
        };
        let content_width = {
            let longest = source.max_line_len().unwrap_or(0) as f32;
            char_width * longest + px(TEXT_LEFT_PAD) + char_width * 8.
        };
        let max_x = if wrap_cols.is_some() {
            px(0.)
        } else {
            (content_width - text_bounds.size.width + px(SCROLLBAR)).max(px(0.))
        };
        if let Some(request) = self.pending_scroll.take() {
            let margin = line_height * 3.;
            match request {
                ScrollRequest::FitCursor => {
                    let head = self.selection().head;
                    let line = source.line(head.row);
                    let seg = match wrap_cols {
                        Some(cols) => {
                            let segs = wrap_segments(&line, cols, tab);
                            segs.partition_point(|&s| s <= head.col).saturating_sub(1)
                        }
                        None => 0,
                    };
                    let d = self.display_map.row_to_display(head.row) + seg as u32;
                    let y = line_height * d as f32;
                    if y < self.scroll.y + margin {
                        self.scroll.y = y - margin;
                    } else if y + line_height > self.scroll.y + viewport_h - margin {
                        self.scroll.y = y + line_height - viewport_h + margin;
                    }
                    if wrap_cols.is_none() {
                        let x = char_width * display_column(&line, head.col, tab) as f32;
                        let visible_w = text_bounds.size.width - px(TEXT_LEFT_PAD) - px(SCROLLBAR);
                        let hmargin = char_width * 4.;
                        if x < self.scroll.x + hmargin {
                            self.scroll.x = x - hmargin;
                        } else if x > self.scroll.x + visible_w - hmargin {
                            self.scroll.x = x - visible_w + hmargin;
                        }
                    }
                }
                ScrollRequest::Row { row, center } => {
                    let d = self.display_map.row_to_display(row.min(line_count - 1));
                    let y = line_height * d as f32;
                    self.scroll.y = if center {
                        y - (viewport_h - line_height) / 2.
                    } else {
                        y
                    };
                }
            }
        }
        self.scroll.y = self.scroll.y.clamp(px(0.), max_y);
        self.scroll.x = self.scroll.x.clamp(px(0.), max_x);
        let scroll = self.scroll;

        let first_display_row = (scroll.y / line_height).floor() as u32;
        let visible = (viewport_h / line_height).ceil() as u32 + 1;
        let last_display_row = (first_display_row + visible).min(display_rows);

        // Gather row texts and highlights for the buffer rows involved.
        let mut row_texts: HashMap<u32, RowText> = HashMap::new();
        let mut get_row_text = |row: u32| -> (Rc<ExpandedLine>, SmallVec<[usize; 4]>, usize) {
            let entry = row_texts.entry(row).or_insert_with(|| {
                let text = source.line(row);
                let segments = match wrap_cols {
                    Some(cols) => wrap_segments(&text, cols, tab),
                    None => SmallVec::from_slice(&[0]),
                };
                RowText {
                    expanded: Rc::new(ExpandedLine::new(&text, tab)),
                    segments,
                    len: text.len(),
                }
            });
            (entry.expanded.clone(), entry.segments.clone(), entry.len)
        };

        let mut targets: Vec<(u32, u32, usize)> = Vec::new();
        for d in first_display_row..last_display_row {
            let (row, seg) = self.display_map.display_to_row(d);
            targets.push((d, row, seg));
        }
        let first_row = targets.first().map(|t| t.1).unwrap_or(0);
        let last_row = targets.last().map(|t| t.1).unwrap_or(0);

        // Sticky headers for the first visible row.
        let mut sticky_rows: Vec<u32> = Vec::new();
        if settings.sticky_headers && scroll.y > px(0.) {
            let top_row = first_row;
            let mut rows: Vec<u32> = source
                .sticky_rows(top_row)
                .into_iter()
                .filter(|r| *r < top_row)
                .collect();
            if rows.len() > MAX_STICKY {
                rows.drain(..rows.len() - MAX_STICKY);
            }
            let n = rows.len() as u32;
            if n > 0 {
                let (below, _) = self.display_map.display_to_row(first_display_row + n);
                let mut refined: Vec<u32> = source
                    .sticky_rows(below)
                    .into_iter()
                    .filter(|r| {
                        self.display_map.row_to_display(*r)
                            < first_display_row + n.min(MAX_STICKY as u32)
                    })
                    .collect();
                if refined.len() > MAX_STICKY {
                    refined.drain(..refined.len() - MAX_STICKY);
                }
                sticky_rows = refined;
            }
        }

        let hl_start = first_row.min(sticky_rows.first().copied().unwrap_or(first_row));
        let mut highlights: HashMap<u32, Vec<HighlightSpan>> = HashMap::new();
        let mut hl_rows: Vec<Range<u32>> = Vec::with_capacity(1 + sticky_rows.len());
        hl_rows.push(first_row..last_row + 1);
        if hl_start < first_row {
            for r in &sticky_rows {
                hl_rows.push(*r..*r + 1);
            }
        }
        for range in hl_rows {
            for span in source.highlights(range) {
                highlights.entry(span.row).or_default().push(span);
            }
        }

        let mut style_cache: HashMap<&'static str, Option<lector_theme::TextStyle>> =
            HashMap::new();
        let default_color = theme.ui.text.hsla();
        let base_font = font.clone();
        let mut shape_segment = |this: &mut CodeView,
                                 row: u32,
                                 seg_start: usize,
                                 seg_end: usize,
                                 expanded: &ExpandedLine|
         -> (ShapedLine, usize) {
            let ds = expanded.to_display(seg_start);
            let de = if seg_end >= expanded.to_source(expanded.text.len()) {
                expanded.text.len()
            } else {
                expanded.to_display(seg_end)
            };
            let ds = floor_char_boundary(&expanded.text, ds);
            let de = floor_char_boundary(&expanded.text, de.max(ds));
            let text = &expanded.text[ds..de];
            let mut runs: Vec<TextRun> = Vec::new();
            let mut cursor = 0usize;
            let push_run =
                |runs: &mut Vec<TextRun>, len: usize, color: Hsla, bold: bool, italic: bool| {
                    if len == 0 {
                        return;
                    }
                    let mut font = base_font.clone();
                    if bold {
                        font.weight = FontWeight::BOLD;
                    }
                    if italic {
                        font.style = FontStyle::Italic;
                    }
                    runs.push(TextRun {
                        len,
                        font,
                        color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    });
                };
            if let Some(spans) = highlights.get(&row) {
                for span in spans {
                    let style = match &span.style {
                        SpanStyle::Capture(name) => *style_cache
                            .entry(name)
                            .or_insert_with(|| theme.syntax.style_for(name)),
                        SpanStyle::Custom(style) => Some(*style),
                    };
                    let Some(style) = style else { continue };
                    let s = expanded.to_display(span.range.start).max(ds);
                    let e = expanded.to_display(span.range.end).min(de);
                    if e <= s || s < ds + cursor {
                        continue;
                    }
                    if !text.is_char_boundary(s - ds) || !text.is_char_boundary(e - ds) {
                        continue;
                    }
                    let (s, e) = (s - ds, e - ds);
                    push_run(&mut runs, s - cursor, default_color, false, false);
                    push_run(
                        &mut runs,
                        e - s,
                        style.color.hsla(),
                        style.bold,
                        style.italic,
                    );
                    cursor = e;
                }
            }
            push_run(&mut runs, text.len() - cursor, default_color, false, false);
            let key = line_key(text, &runs, font_size);
            let shaped = this.line_cache.get_or_shape(key, text, || {
                text_system.shape_line(SharedString::from(text.to_string()), font_size, &runs, None)
            });
            (shaped, ds)
        };

        let text_origin_x = text_bounds.left() + px(TEXT_LEFT_PAD) - scroll.x;
        let mut rows = Vec::with_capacity(targets.len());
        for (d, row, seg) in targets {
            let (expanded, segments, len) = get_row_text(row);
            let seg_start = segments.get(seg).copied().unwrap_or(0);
            let seg_end = segments.get(seg + 1).copied().unwrap_or(len);
            let (line, disp_start) = shape_segment(self, row, seg_start, seg_end, &expanded);
            let x_offset = match wrap_cols {
                Some(cols) if seg > 0 => char_width * wrap_indent(&expanded.text, cols, tab) as f32,
                _ => px(0.),
            };
            rows.push(RowLayout {
                x_offset,
                display_row: d,
                row,
                seg_start,
                seg_end,
                disp_start,
                expanded,
                line,
                y: bounds.top() + line_height * d as f32 - scroll.y,
                first_seg: seg == 0,
                last_seg: seg + 1 >= segments.len(),
            });
        }
        let mut sticky = Vec::with_capacity(sticky_rows.len());
        for (i, row) in sticky_rows.into_iter().enumerate() {
            let (expanded, segments, len) = get_row_text(row);
            let seg_end = segments.get(1).copied().unwrap_or(len);
            let (line, disp_start) = shape_segment(self, row, 0, seg_end, &expanded);
            sticky.push(RowLayout {
                display_row: self.display_map.row_to_display(row),
                row,
                seg_start: 0,
                seg_end,
                disp_start,
                expanded,
                line,
                y: bounds.top() + line_height * i as f32,
                first_seg: true,
                last_seg: segments.len() <= 1,
                x_offset: px(0.),
            });
        }

        let fold_starts: HashSet<u32> = self.fold_ranges().iter().map(|r| r.start).collect();
        let minimap_top_row = minimap_bounds
            .map(|mb| {
                let capacity = (mb.size.height / px(MINIMAP_ROW)).floor() as u32;
                if display_rows <= capacity || max_y <= px(0.) {
                    0
                } else {
                    let ratio = (scroll.y / max_y).clamp(0., 1.);
                    ((display_rows - capacity) as f32 * ratio).round() as u32
                }
            })
            .unwrap_or(0);

        let snapshot = Rc::new(LayoutSnapshot {
            bounds,
            gutter_bounds,
            text_bounds,
            minimap_bounds,
            line_height,
            char_width,
            font_size,
            text_origin_x,
            scroll,
            max_scroll: point(max_x, max_y),
            display_row_count: display_rows,
            first_display_row,
            rows,
            sticky,
            wrap_cols,
            tab_size: tab,
            fold_starts,
            line_number_right,
            annotation_right,
            diff_bar_x,
            fold_x,
            minimap_top_row,
        });
        self.last_layout = Some(snapshot.clone());
        snapshot
    }

    /// Window point (top-left of the glyph's line box) of `pos`, if it was
    /// painted in the last frame. Used to anchor popovers.
    pub fn point_for_pos(&self, pos: Pos) -> Option<Point<Pixels>> {
        let l = self.last_layout.as_ref()?;
        let rl = l
            .rows
            .iter()
            .filter(|r| r.row == pos.row && pos.col >= r.seg_start)
            .find(|r| pos.col <= r.seg_end || r.last_seg)?;
        Some(point(l.text_origin_x + rl.x_for_col(pos.col), rl.y))
    }

    /// Line height of the last layout.
    pub fn line_height(&self) -> Option<Pixels> {
        self.last_layout.as_ref().map(|l| l.line_height)
    }

    /// First buffer row visible in the last frame (0 before the first paint).
    pub fn top_visible_row(&self) -> u32 {
        self.last_layout
            .as_ref()
            .and_then(|l| l.rows.first())
            .map(|r| r.row)
            .unwrap_or(0)
    }

    /// Buffer position for a window point, using the last layout.
    pub(crate) fn pos_for_point(&self, p: Point<Pixels>) -> Option<Pos> {
        let l = self.last_layout.as_ref()?;
        let y = p.y - l.bounds.top() + l.scroll.y;
        let max_d = l.display_row_count.saturating_sub(1) as f32;
        let d = (y / l.line_height).floor().clamp(0., max_d) as u32;
        let x = p.x - l.text_origin_x;
        if let Some(rl) = l.row_at_display(d) {
            return Some(Pos::new(rl.row, rl.col_for_x(x)));
        }
        let (row, seg) = self.display_map.display_to_row(d);
        let line = self.source.line(row);
        let seg_start = match l.wrap_cols {
            Some(cols) => wrap_segments(&line, cols, l.tab_size)
                .get(seg)
                .copied()
                .unwrap_or(0),
            None => 0,
        };
        let base = display_column(&line, seg_start, l.tab_size);
        let indent = match l.wrap_cols {
            Some(cols) if seg > 0 => wrap_indent(&line, cols, l.tab_size),
            _ => 0,
        };
        let cols = ((x / l.char_width).round().max(0.) as usize).saturating_sub(indent);
        Some(Pos::new(
            row,
            byte_for_display_column(&line, base + cols, l.tab_size),
        ))
    }

    fn scroll_by(&mut self, delta: Point<Pixels>, cx: &mut gpui::Context<Self>) {
        let Some(l) = self.last_layout.clone() else {
            return;
        };
        let new = point(
            (self.scroll.x - delta.x).clamp(px(0.), l.max_scroll.x),
            (self.scroll.y - delta.y).clamp(px(0.), l.max_scroll.y),
        );
        if new != self.scroll {
            self.scroll = new;
            cx.emit(CodeViewEvent::Scrolled);
            cx.notify();
        }
    }
}

/// Hitboxes registered during prepaint.
pub struct PrepaintState {
    layout: Rc<LayoutSnapshot>,
    hitbox: Hitbox,
    text_hitbox: Hitbox,
    gutter_hitbox: Hitbox,
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
        PrepaintState {
            layout,
            hitbox,
            text_hitbox,
            gutter_hitbox,
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
        let (search_matches, active_match) = view.search_matches();
        let search_matches: Vec<(u32, Range<usize>)> = search_matches.to_vec();
        let decorations = view.decorations().to_vec();
        let annotations = view.annotations().clone();
        let gutter_annotations = view.gutter_annotations().cloned();
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
            let (sel_start, sel_end) = selection.range();
            let cursor_row = selection.head.row;

            // Current line (across gutter and text).
            if selection.is_empty() {
                for rl in layout.rows.iter().filter(|r| r.row == cursor_row) {
                    window.paint_quad(fill(
                        Bounds::new(
                            point(bounds.left(), rl.y),
                            size(layout.text_bounds.right() - bounds.left(), lh),
                        ),
                        ui.active_line.hsla(),
                    ));
                }
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
                    if selection.is_empty() && focused {
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

                    // Selection.
                    if !selection.is_empty() {
                        let color = if focused {
                            ui.selection.hsla()
                        } else {
                            ui.selection_inactive.hsla()
                        };
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

                    // Cursor.
                    if focused {
                        let head = selection.head;
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
                                    size(px(2.), lh - px(4.)),
                                ),
                                ui.cursor.hsla(),
                            ));
                        }
                    }
                },
            );

            // Gutter (painted over horizontally scrolled text).
            window.paint_quad(fill(layout.gutter_bounds, ui.background.hsla()));
            if selection.is_empty() {
                for rl in layout.rows.iter().filter(|r| r.row == cursor_row) {
                    window.paint_quad(fill(
                        Bounds::new(
                            point(bounds.left(), rl.y),
                            size(layout.gutter_bounds.size.width, lh),
                        ),
                        ui.active_line.hsla(),
                    ));
                }
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
        });

        register_mouse_handlers(window, &self.view, &layout, &state.hitbox);
        tracing::trace!(target: "lector_ui::perf", elapsed_us = started.elapsed().as_micros() as u64, "code view paint");
    }
}

fn shape_plain(
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

fn shape_annotation(
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

fn paint_hatch(window: &mut Window, bounds: Bounds<Pixels>, color: Hsla) {
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

fn paint_indent_guides(
    window: &mut Window,
    layout: &LayoutSnapshot,
    source: &dyn super::CodeSource,
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
fn paint_gutter(
    window: &mut Window,
    cx: &mut App,
    layout: &LayoutSnapshot,
    source: &dyn super::CodeSource,
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
fn paint_minimap(
    window: &mut Window,
    layout: &LayoutSnapshot,
    display_map: &DisplayMap,
    mb: Bounds<Pixels>,
    source: &dyn super::CodeSource,
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

fn paint_scrollbars(
    window: &mut Window,
    layout: &LayoutSnapshot,
    source: &dyn super::CodeSource,
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

fn register_mouse_handlers(
    window: &mut Window,
    view: &Entity<CodeView>,
    layout: &Rc<LayoutSnapshot>,
    hitbox: &Hitbox,
) {
    // Scroll wheel.
    {
        let view = view.clone();
        let hitbox = hitbox.clone();
        let lh = layout.line_height;
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || !hitbox.should_handle_scroll(window) {
                return;
            }
            let mut delta = event.delta.pixel_delta(lh);
            if event.modifiers.shift && delta.x == px(0.) {
                delta = point(delta.y, px(0.));
            }
            view.update(cx, |v, cx| v.scroll_by(delta, cx));
            cx.stop_propagation();
        });
    }
    // Mouse down.
    {
        let view = view.clone();
        let hitbox = hitbox.clone();
        let layout = layout.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                return;
            }
            let p = event.position;
            let l = &layout;
            view.update(cx, |v, cx| {
                // Minimap.
                if let Some(mb) = l.minimap_bounds {
                    if mb.contains(&p) && event.button == MouseButton::Left {
                        v.minimap_drag = true;
                        minimap_scroll_to(v, l, mb, p.y, cx);
                        cx.stop_propagation();
                        return;
                    }
                }
                // Vertical scrollbar.
                let track = l.vscroll_track();
                if track.contains(&p)
                    && l.max_scroll.y > px(0.)
                    && event.button == MouseButton::Left
                {
                    if let Some((start, len)) = thumb_geometry(
                        f32::from(track.size.height),
                        f32::from(l.bounds.size.height),
                        f32::from(l.content_height()),
                        f32::from(l.scroll.y),
                    ) {
                        let rel = p.y - track.top();
                        let grab = if rel >= px(start) && rel <= px(start + len) {
                            rel - px(start)
                        } else {
                            px(len / 2.)
                        };
                        v.scrollbar_drag = Some(grab);
                        scrollbar_scroll_to(v, l, p.y, grab, len, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                // Sticky header click: jump to that row.
                if !l.sticky.is_empty()
                    && p.y < l.bounds.top() + l.sticky_height()
                    && p.x < l.text_bounds.right()
                {
                    let ix = ((p.y - l.bounds.top()) / l.line_height).floor() as usize;
                    if let Some(rl) = l.sticky.get(ix) {
                        let row = rl.row;
                        v.set_cursor(Pos::new(row, 0), cx);
                        v.scroll_to_row(row, false, cx);
                        window.focus(&v.focus_handle);
                    }
                    cx.stop_propagation();
                    return;
                }
                let Some(pos) = v.pos_for_point(p) else {
                    return;
                };
                match event.button {
                    MouseButton::Left => {
                        if l.gutter_bounds.contains(&p) {
                            let annotation_cb = (p.x < l.annotation_right)
                                .then(|| v.gutter_annotation_click_callback())
                                .flatten()
                                .and_then(|cb| {
                                    let ix = v.gutter_annotations()?.block_at(pos.row)?;
                                    Some((cb, ix))
                                });
                            if let Some((cb, ix)) = annotation_cb {
                                cx.defer_in(window, move |_, window, cx| cb(ix, window, cx));
                            } else if p.x >= l.fold_x && l.fold_starts.contains(&pos.row) {
                                let folded = v.is_folded(pos.row);
                                v.set_folded(pos.row, !folded, cx);
                            } else {
                                v.begin_selection(Pos::new(pos.row, 0), 3, false, window, cx);
                            }
                        } else if event.modifiers.secondary() {
                            if let Some(cb) = v.cmd_click_callback() {
                                window.focus(&v.focus_handle);
                                v.set_cursor(pos, cx);
                                cx.defer_in(window, move |_, window, cx| cb(pos, window, cx));
                            }
                        } else {
                            v.begin_selection(
                                pos,
                                event.click_count,
                                event.modifiers.shift,
                                window,
                                cx,
                            );
                        }
                        cx.stop_propagation();
                    }
                    MouseButton::Right => {
                        window.focus(&v.focus_handle);
                        let (s, e) = v.selection().range();
                        if pos < s || pos > e {
                            v.set_cursor(pos, cx);
                        }
                        v.deploy_context_menu(p, window, cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            });
        });
    }
    // Mouse move: drag selection, scrollbar/minimap drags, hover.
    {
        let view = view.clone();
        let hitbox = hitbox.clone();
        let layout = layout.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let p = event.position;
            let l = &layout;
            view.update(cx, |v, cx| {
                let left_down = event.pressed_button == Some(MouseButton::Left);
                if let Some(grab) = v.scrollbar_drag {
                    if left_down {
                        let track = l.vscroll_track();
                        if let Some((_, len)) = thumb_geometry(
                            f32::from(track.size.height),
                            f32::from(l.bounds.size.height),
                            f32::from(l.content_height()),
                            f32::from(l.scroll.y),
                        ) {
                            scrollbar_scroll_to(v, l, p.y, grab, len, cx);
                        }
                    } else {
                        v.scrollbar_drag = None;
                    }
                    return;
                }
                if v.minimap_drag {
                    if left_down {
                        if let Some(mb) = l.minimap_bounds {
                            minimap_scroll_to(v, l, mb, p.y, cx);
                        }
                    } else {
                        v.minimap_drag = false;
                    }
                    return;
                }
                if v.is_selecting() {
                    if !left_down {
                        v.end_selection();
                        return;
                    }
                    // Autoscroll when dragging outside the text area.
                    let tb = l.text_bounds;
                    let mut delta = point(px(0.), px(0.));
                    if p.y < tb.top() {
                        delta.y = (tb.top() - p.y).min(l.line_height * 3.);
                    } else if p.y > tb.bottom() {
                        delta.y = -(p.y - tb.bottom()).min(l.line_height * 3.);
                    }
                    if l.wrap_cols.is_none() {
                        if p.x < tb.left() {
                            delta.x = (tb.left() - p.x).min(l.char_width * 4.);
                        } else if p.x > tb.right() {
                            delta.x = -(p.x - tb.right()).min(l.char_width * 4.);
                        }
                    }
                    if delta != point(px(0.), px(0.)) {
                        v.scroll_by(delta, cx);
                    }
                    let clamped = point(p.x, p.y.clamp(tb.top(), tb.bottom() - px(1.)));
                    if let Some(pos) = v.pos_for_point(clamped) {
                        v.extend_selection(pos, cx);
                    }
                    return;
                }
                let in_gutter = hitbox.is_hovered(window) && l.gutter_bounds.contains(&p);
                if in_gutter != v.gutter_hovered {
                    v.gutter_hovered = in_gutter;
                    cx.notify();
                }
                let over_text = hitbox.is_hovered(window)
                    && l.text_bounds.contains(&p)
                    && !l.vscroll_track().contains(&p);
                let pos = if over_text { v.pos_for_point(p) } else { None };
                v.hover_moved(pos, p, window, cx);
            });
        });
    }
    // Mouse up.
    {
        let view = view.clone();
        window.on_mouse_event(move |_: &MouseUpEvent, phase, _window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            view.update(cx, |v, cx| {
                if v.is_selecting() || v.scrollbar_drag.is_some() || v.minimap_drag {
                    v.end_selection();
                    v.scrollbar_drag = None;
                    v.minimap_drag = false;
                    cx.notify();
                }
            });
        });
    }
}

fn scrollbar_scroll_to(
    v: &mut CodeView,
    l: &LayoutSnapshot,
    y: Pixels,
    grab: Pixels,
    thumb_len: f32,
    cx: &mut gpui::Context<CodeView>,
) {
    let track = l.vscroll_track();
    let travel = track.size.height - px(thumb_len);
    if travel <= px(0.) {
        return;
    }
    let ratio = ((y - grab - track.top()) / travel).clamp(0., 1.);
    v.scroll.y = l.max_scroll.y * ratio;
    cx.emit(CodeViewEvent::Scrolled);
    cx.notify();
}

fn minimap_scroll_to(
    v: &mut CodeView,
    l: &LayoutSnapshot,
    mb: Bounds<Pixels>,
    y: Pixels,
    cx: &mut gpui::Context<CodeView>,
) {
    let d = l.minimap_top_row as f32 + ((y - mb.top()) / px(MINIMAP_ROW)).max(0.);
    let target = l.line_height * d - l.bounds.size.height / 2.;
    v.scroll.y = target.clamp(px(0.), l.max_scroll.y);
    cx.emit(CodeViewEvent::Scrolled);
    cx.notify();
}

/// Paints the left gutter annotation (blame) column.
fn paint_gutter_annotations(
    window: &mut Window,
    cx: &mut App,
    layout: &LayoutSnapshot,
    ann: &super::GutterAnnotations,
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
