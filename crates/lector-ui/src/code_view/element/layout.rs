//! Row layout of the [`CodeView`] for the element: shapes the visible display
//! rows into a [`LayoutSnapshot`] and maps between positions and points.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    point, px, size, Bounds, FontStyle, FontWeight, Hsla, Pixels, Point, ShapedLine, SharedString,
    TextRun, Window,
};
use smallvec::SmallVec;

use super::{
    LayoutSnapshot, RowLayout, FOLD_COLUMN, GUTTER_LEFT_PAD, MAX_STICKY, MINIMAP_ROW,
    MINIMAP_WIDTH, SCROLLBAR, TEXT_LEFT_PAD, WRAP_MAX_LINES,
};
use crate::code_view::display_map::{
    byte_for_display_column, display_column, wrap_indent, wrap_segments, DisplayMap, ExpandedLine,
};
use crate::code_view::line_cache::line_key;
use crate::code_view::selection::Pos;
use crate::code_view::source::{HighlightSpan, SpanStyle};
use crate::code_view::{CodeView, CodeViewEvent, EditorSettings, ScrollRequest};
use crate::theme::{ActiveTheme, ColorExt};

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
            if let Some(cols) = wrap_cols {
                // Per-row wrap counts survive edits (only edited rows are
                // re-measured), so typing in a wrapped file stays cheap.
                let counts = self.wrap_counts(cols, tab, line_count, source.version());
                let mut counts = counts.borrow_mut();
                for row in 0..line_count as usize {
                    if counts[row].is_none() {
                        let text = source.line(row as u32);
                        counts[row] = Some(wrap_segments(&text, cols, tab).len() as u32 - 1);
                    }
                }
                map.set_wrap_counts(cols, |r| {
                    counts.get(r as usize).copied().flatten().unwrap_or(0)
                });
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

    pub(super) fn scroll_by(&mut self, delta: Point<Pixels>, cx: &mut gpui::Context<Self>) {
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
