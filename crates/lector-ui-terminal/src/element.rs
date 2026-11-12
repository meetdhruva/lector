//! The terminal element: layout (grid size → `resize`), the per-frame snapshot, row shaping
//! through the [`RowCache`](crate::grid::RowCache), painting and mouse handling.
//!
//! Painting order: background → merged cell backgrounds → selection / search highlights →
//! failed-command marks → text → cursor → hovered link → IME preedit → scrollbar.

use std::hash::{Hash, Hasher};
use std::rc::Rc;

use gpui::{
    fill, outline, point, px, size, App, BorderStyle, Bounds, ContentMask, CursorStyle,
    DispatchPhase, Element, ElementId, ElementInputHandler, Entity, Font, FontStyle, FontWeight,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, IntoElement, LayoutId, ModifiersChangedEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollWheelEvent,
    ShapedLine, SharedString, StrikethroughStyle, Style, TextRun as GpuiRun, UnderlineStyle,
    Window,
};
use lector_terminal::{
    grid_to_viewport, Attrs, CellWidth, CursorShape, Damage, GridPoint, MouseEventKind,
    ScrollAmount, SelectionKind, Side, SnapshotRow, TermSize, UnderlineStyle as TermUnderline,
};
use lector_theme::Color;
use lector_ui::code_view::EditorSettings;
use lector_ui::theme::{ActiveTheme, ColorExt};

use crate::colors::TerminalPalette;
use crate::config::font_overrides;
use crate::grid::{cell_at, grid_size, merge_backgrounds, PADDING_X, PADDING_Y};
use crate::view::{terminal_settings, LayoutInfo, TerminalView};

/// Shaped text and merged backgrounds of one row (cached per row identity).
pub struct ShapedRow {
    /// Text pieces positioned at their first column.
    pub texts: Vec<(u32, ShapedLine)>,
    /// Backgrounds `(start_col, end_col, color)`, end exclusive.
    pub backgrounds: Vec<(u32, u32, Hsla)>,
}

/// Font and cell metrics of a terminal.
#[derive(Clone, Debug)]
pub struct CellMetrics {
    /// Regular font (ligatures off: cells must stay aligned).
    pub font: Font,
    /// Font size.
    pub font_size: Pixels,
    /// Cell width (the font's em advance).
    pub cell_width: Pixels,
    /// Line height.
    pub line_height: Pixels,
}

impl CellMetrics {
    /// Metrics from the terminal settings, inheriting the code font (family, size).
    pub fn resolve(window: &mut Window, cx: &App) -> Self {
        let editor = EditorSettings::get(cx);
        let settings = terminal_settings(cx);
        let (family, font_size, line_height) = font_overrides(&settings);
        let mut font = editor.font();
        if let Some(family) = family {
            font.family = family.into();
        }
        font.features = gpui::FontFeatures::disable_ligatures();
        let font_size = font_size.map(px).unwrap_or(editor.font_size);
        let line_height = (font_size * line_height).round().max(px(4.));
        let text_system = window.text_system().clone();
        let font_id = text_system.resolve_font(&font);
        let cell_width = text_system
            .em_advance(font_id, font_size)
            .unwrap_or(font_size * 0.6)
            .max(px(1.));
        CellMetrics {
            font,
            font_size,
            cell_width,
            line_height,
        }
    }

    fn generation(&self, theme: &str) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        theme.hash(&mut h);
        self.font.family.hash(&mut h);
        f32::from(self.font_size).to_bits().hash(&mut h);
        f32::from(self.cell_width).to_bits().hash(&mut h);
        h.finish()
    }
}

/// Top-left corner of a viewport cell.
pub(crate) fn cell_origin(layout: &LayoutInfo, row: u32, col: u32) -> gpui::Point<Pixels> {
    point(
        layout.bounds.left() + px(PADDING_X) + layout.cell_width * col as f32,
        layout.bounds.top() + px(PADDING_Y) + layout.line_height * row as f32,
    )
}

fn hsla(c: Color) -> Hsla {
    c.hsla()
}

fn font_for(base: &Font, attrs: Attrs) -> Font {
    let mut font = base.clone();
    if attrs.contains(Attrs::BOLD) {
        font.weight = FontWeight::BOLD;
    }
    if attrs.contains(Attrs::ITALIC) {
        font.style = FontStyle::Italic;
    }
    font
}

/// Shapes one row: one shaped line per styled run, or per cell where glyph advances would
/// break the grid (wide characters, fallback fonts).
pub(crate) fn shape_row(
    row: &SnapshotRow,
    palette: &TerminalPalette,
    metrics: &CellMetrics,
    window: &Window,
) -> ShapedRow {
    let text_system = window.text_system();
    let cw = metrics.cell_width;
    let mut cell_bg: Vec<Option<Color>> = vec![None; row.cells.len()];
    let mut texts = Vec::new();
    for run in row.runs() {
        let attrs = run.style.attrs;
        let (fg, bg) = palette.cell_colors(
            run.style.fg,
            run.style.bg,
            attrs.contains(Attrs::INVERSE),
            attrs.contains(Attrs::DIM),
        );
        if let Some(bg) = bg {
            let end = (run.col + run.cols).min(row.cells.len() as u32);
            for slot in &mut cell_bg[run.col as usize..end as usize] {
                *slot = Some(bg);
            }
        }
        if attrs.contains(Attrs::HIDDEN) {
            continue;
        }
        let underline = match run.style.underline {
            TermUnderline::None => None,
            style => Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(hsla(
                    run.style
                        .underline_color
                        .map(|c| palette.resolve(c))
                        .unwrap_or(fg),
                )),
                wavy: matches!(style, TermUnderline::Curly),
            }),
        };
        let strikethrough = attrs
            .contains(Attrs::STRIKETHROUGH)
            .then_some(StrikethroughStyle {
                thickness: px(1.),
                color: Some(hsla(fg)),
            });
        let decorated = underline.is_some() || strikethrough.is_some();
        let text = if decorated {
            run.text.as_str()
        } else {
            run.text.trim_end_matches(' ')
        };
        if text.is_empty() || (!decorated && text.chars().all(|c| c == ' ')) {
            continue;
        }
        let font = font_for(&metrics.font, attrs);
        let gpui_run = |len: usize| GpuiRun {
            len,
            font: font.clone(),
            color: hsla(fg),
            background_color: None,
            underline,
            strikethrough,
        };
        let shaped = text_system.shape_line(
            SharedString::from(text.to_string()),
            metrics.font_size,
            &[gpui_run(text.len())],
            None,
        );
        let expected = cw * text.chars().filter(|c| !is_zero_width(*c)).count() as f32;
        let aligned = !run.has_wide && (shaped.width - expected).abs() < px(0.5);
        if aligned {
            texts.push((run.col, shaped));
            continue;
        }
        // Per cell: every glyph lands on its own column.
        let end = (run.col + run.cols).min(row.cells.len() as u32);
        for col in run.col..end {
            let cell = &row.cells[col as usize];
            if matches!(cell.width, CellWidth::WideSpacer | CellWidth::LeadingSpacer) {
                continue;
            }
            if cell.ch == ' ' && cell.zerowidth.is_none() && !decorated {
                continue;
            }
            let mut s = String::new();
            s.push(cell.ch);
            if let Some(z) = &cell.zerowidth {
                s.extend(z.iter());
            }
            let cols = if cell.width == CellWidth::Wide {
                2.
            } else {
                1.
            };
            let shaped = text_system.shape_line(
                SharedString::from(s.clone()),
                metrics.font_size,
                &[gpui_run(s.len())],
                Some(cw * cols),
            );
            texts.push((col, shaped));
        }
    }
    let backgrounds = merge_backgrounds(&cell_bg)
        .into_iter()
        .map(|(s, e, c)| (s, e, hsla(c)))
        .collect();
    ShapedRow { texts, backgrounds }
}

fn is_zero_width(c: char) -> bool {
    matches!(c, '\u{300}'..='\u{36f}' | '\u{200b}'..='\u{200f}' | '\u{fe00}'..='\u{fe0f}')
}

/// The element drawing a [`TerminalView`].
pub struct TerminalElement {
    view: Entity<TerminalView>,
}

impl TerminalElement {
    /// Element for `view`.
    pub fn new(view: Entity<TerminalView>) -> Self {
        Self { view }
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

struct CursorPaint {
    bounds: Bounds<Pixels>,
    shape: CursorShape,
    color: Hsla,
    glyph: Option<ShapedLine>,
}

/// State carried from prepaint to paint.
pub struct PrepaintState {
    hitbox: Hitbox,
    layout: LayoutInfo,
    background: Hsla,
    rows: Vec<Option<Rc<ShapedRow>>>,
    highlights: Vec<(Bounds<Pixels>, Hsla)>,
    marks: Vec<(Bounds<Pixels>, Hsla)>,
    cursor: Option<CursorPaint>,
    link_lines: Vec<(Bounds<Pixels>, Hsla)>,
    ime: Option<(gpui::Point<Pixels>, ShapedLine, Hsla)>,
    scrollbar: Option<(Bounds<Pixels>, Hsla)>,
    link_hovered: bool,
}

/// Rectangles covering grid span `start..=end` (inclusive columns) on the visible rows.
fn span_rects(
    layout: &LayoutInfo,
    start: GridPoint,
    end: GridPoint,
    display_offset: u32,
    color: Hsla,
    out: &mut Vec<(Bounds<Pixels>, Hsla)>,
) {
    let cols = u32::from(layout.cols);
    for line in start.line..=end.line {
        let Some(row) = grid_to_viewport(GridPoint::new(line, 0), display_offset, layout.rows)
        else {
            continue;
        };
        let c0 = if line == start.line { start.col } else { 0 };
        let c1 = if line == end.line { end.col + 1 } else { cols };
        if c1 <= c0 {
            continue;
        }
        let origin = cell_origin(layout, row, c0);
        out.push((
            Bounds::new(
                origin,
                size(layout.cell_width * (c1 - c0) as f32, layout.line_height),
            ),
            color,
        ));
    }
}

impl Element for TerminalElement {
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
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = gpui::relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> PrepaintState {
        let metrics = CellMetrics::resolve(window, cx);
        let theme = cx.theme().clone();
        let palette = TerminalPalette::for_theme(&theme);
        let generation = metrics.generation(&theme.name);
        let (cols, rows) = grid_size(
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
            f32::from(metrics.cell_width),
            f32::from(metrics.line_height),
        );
        let layout = LayoutInfo {
            bounds,
            cell_width: metrics.cell_width,
            line_height: metrics.line_height,
            cols,
            rows,
        };
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let started = std::time::Instant::now();
        let view = self.view.clone();
        let reduce_motion = lector_ui::a11y::reduce_motion(cx);
        let focused_now = self.view.read(cx).focus_handle.is_focused(window);
        let state = view.update(cx, |v, cx| {
            // Focus as GPUI sees it this frame (focus events can be missed while the window
            // is inactive).
            if v.focused != focused_now {
                v.set_focused(focused_now, cx);
            }
            v.layout = Some(layout);
            v.request_size(
                TermSize {
                    cols,
                    rows,
                    cell_width: f32::from(metrics.cell_width).round() as u16,
                    cell_height: f32::from(metrics.line_height).round() as u16,
                },
                cx,
            );
            if v.palette_theme.as_deref() != Some(theme.name.as_str()) {
                if let Some(s) = &v.session {
                    s.lock().set_query_colors(palette.query_colors());
                    v.palette_theme = Some(theme.name.clone());
                }
            }
            let fresh = v.refresh_snapshot();
            v.mark_seen();
            v.frames += 1;
            let mut state = PrepaintState {
                hitbox,
                layout,
                background: hsla(palette.background),
                rows: Vec::new(),
                highlights: Vec::new(),
                marks: Vec::new(),
                cursor: None,
                link_lines: Vec::new(),
                ime: None,
                scrollbar: None,
                link_hovered: v.hovered_link.is_some(),
            };
            let Some(snapshot) = v.snapshot.clone() else {
                return state;
            };
            let shaped = v
                .cache
                .sync(&snapshot.rows, &snapshot.damage, generation, |_, row| {
                    Rc::new(shape_row(row, &palette, &metrics, window))
                });
            if fresh {
                tracing::trace!(
                    target: "lector_ui::perf",
                    shaped,
                    rows = snapshot.rows.len(),
                    full = matches!(snapshot.damage, Damage::Full),
                    "terminal frame"
                );
            }
            state.rows = (0..snapshot.rows.len())
                .map(|i| v.cache.get(i).cloned())
                .collect();
            let off = snapshot.display_offset;
            // Selection.
            let selection = hsla(palette.selection);
            for row in 0..snapshot.rows.len() as u32 {
                if let Some(cols) = snapshot.selected_cols(row) {
                    if cols.end > cols.start {
                        let origin = cell_origin(&layout, row, cols.start);
                        state.highlights.push((
                            Bounds::new(
                                origin,
                                size(
                                    layout.cell_width * (cols.end - cols.start) as f32,
                                    layout.line_height,
                                ),
                            ),
                            selection,
                        ));
                    }
                }
            }
            if let Some(session) = v.session.clone() {
                let mut em = session.lock();
                // Search matches.
                if let Some(find) = &v.find {
                    let active = find.active;
                    for m in em.visible_matches() {
                        let color = if Some(m) == active {
                            palette.search_match_active
                        } else {
                            palette.search_match
                        };
                        span_rects(
                            &layout,
                            m.start,
                            m.end,
                            off,
                            hsla(color),
                            &mut state.highlights,
                        );
                    }
                }
                // Failed commands: a bar on the left edge over the block.
                for block in em.command_blocks().iter().filter(|b| b.failed()) {
                    let end = block.end_line.unwrap_or(block.prompt_line);
                    for line in block.prompt_line..=end {
                        if let Some(row) =
                            grid_to_viewport(GridPoint::new(line, 0), off, layout.rows)
                        {
                            state.marks.push((
                                Bounds::new(
                                    point(bounds.left() + px(2.), cell_origin(&layout, row, 0).y),
                                    size(px(3.), layout.line_height),
                                ),
                                hsla(palette.error),
                            ));
                        }
                    }
                }
                drop(em);
            }
            // Hovered link.
            if let Some(link) = &v.hovered_link {
                let mut rects = Vec::new();
                span_rects(
                    &layout,
                    link.start,
                    link.end,
                    off,
                    hsla(palette.link),
                    &mut rects,
                );
                for (b, c) in rects {
                    state.link_lines.push((
                        Bounds::new(
                            point(b.left(), b.bottom() - px(2.)),
                            size(b.size.width, px(1.)),
                        ),
                        c,
                    ));
                }
            }
            // Cursor.
            let c = snapshot.cursor;
            let blink_hidden = v.focused && !v.blink_on && !reduce_motion;
            if c.visible && v.exited.is_none() && !blink_hidden && v.marked_text.is_none() {
                let mut shape = v.cursor_shape(c.shape, cx);
                if !v.focused {
                    shape = CursorShape::HollowBlock;
                }
                let width = layout.cell_width * if c.wide { 2. } else { 1. };
                let origin = cell_origin(&layout, c.row, c.col);
                let glyph = (shape == CursorShape::Block)
                    .then(|| {
                        let cell = snapshot
                            .rows
                            .get(c.row as usize)?
                            .cells
                            .get(c.col as usize)?;
                        if cell.ch == ' ' || cell.style.attrs.contains(Attrs::HIDDEN) {
                            return None;
                        }
                        let mut s = String::new();
                        s.push(cell.ch);
                        if let Some(z) = &cell.zerowidth {
                            s.extend(z.iter());
                        }
                        Some(window.text_system().shape_line(
                            SharedString::from(s.clone()),
                            metrics.font_size,
                            &[GpuiRun {
                                len: s.len(),
                                font: font_for(&metrics.font, cell.style.attrs),
                                color: hsla(palette.cursor_text),
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }],
                            Some(width),
                        ))
                    })
                    .flatten();
                state.cursor = Some(CursorPaint {
                    bounds: Bounds::new(origin, size(width, layout.line_height)),
                    shape,
                    color: hsla(palette.cursor),
                    glyph,
                });
            }
            // IME preedit at the cursor.
            if let Some(marked) = v.marked_text.clone() {
                let origin = cell_origin(&layout, c.row, c.col);
                let line = window.text_system().shape_line(
                    SharedString::from(marked.clone()),
                    metrics.font_size,
                    &[GpuiRun {
                        len: marked.len(),
                        font: metrics.font.clone(),
                        color: hsla(palette.foreground),
                        background_color: None,
                        underline: Some(UnderlineStyle {
                            thickness: px(1.),
                            color: Some(hsla(palette.cursor)),
                            wavy: false,
                        }),
                        strikethrough: None,
                    }],
                    None,
                );
                state.ime = Some((origin, line, hsla(palette.background)));
            }
            // Scrollbar (only with scrollback).
            if snapshot.history_size > 0 && !snapshot.modes.alt_screen {
                let total = (snapshot.history_size + u32::from(layout.rows)) as f32;
                let h = f32::from(bounds.size.height);
                let thumb = (f32::from(layout.rows) / total * h).max(16.).min(h);
                let top =
                    (snapshot.history_size - off.min(snapshot.history_size)) as f32 / total * h;
                let top = top.min(h - thumb);
                let mut color = theme.ui.scrollbar_thumb;
                if off == 0 {
                    color = color.with_alpha(color.alpha() * 0.5);
                }
                state.scrollbar = Some((
                    Bounds::new(
                        point(bounds.right() - px(6.), bounds.top() + px(top)),
                        size(px(4.), px(thumb)),
                    ),
                    hsla(color),
                ));
            }
            state
        });
        tracing::trace!(
            target: "lector_ui::perf",
            elapsed_us = started.elapsed().as_micros() as u64,
            "terminal prepaint"
        );
        state
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        state: &mut PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout = state.layout;
        window.set_cursor_style(
            if state.link_hovered {
                CursorStyle::PointingHand
            } else {
                CursorStyle::IBeam
            },
            &state.hitbox,
        );
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.paint_quad(fill(bounds, state.background));
            for (i, row) in state.rows.iter().enumerate() {
                let Some(row) = row else { continue };
                for (s, e, color) in &row.backgrounds {
                    let origin = cell_origin(&layout, i as u32, *s);
                    window.paint_quad(fill(
                        Bounds::new(
                            origin,
                            size(layout.cell_width * (e - s) as f32, layout.line_height),
                        ),
                        *color,
                    ));
                }
            }
            for (b, c) in &state.highlights {
                window.paint_quad(fill(*b, *c));
            }
            for (b, c) in &state.marks {
                window.paint_quad(fill(*b, *c));
            }
            if let Some(cursor) = &state.cursor {
                if cursor.shape == CursorShape::Block {
                    window.paint_quad(fill(cursor.bounds, cursor.color));
                }
            }
            for (i, row) in state.rows.iter().enumerate() {
                let Some(row) = row else { continue };
                for (col, line) in &row.texts {
                    let origin = cell_origin(&layout, i as u32, *col);
                    let _ = line.paint(origin, layout.line_height, window, cx);
                }
            }
            if let Some(cursor) = &state.cursor {
                let b = cursor.bounds;
                match cursor.shape {
                    CursorShape::Block => {
                        if let Some(glyph) = &cursor.glyph {
                            let _ = glyph.paint(b.origin, layout.line_height, window, cx);
                        }
                    }
                    CursorShape::Beam => window.paint_quad(fill(
                        Bounds::new(b.origin, size(px(2.), b.size.height)),
                        cursor.color,
                    )),
                    CursorShape::Underline => window.paint_quad(fill(
                        Bounds::new(
                            point(b.left(), b.bottom() - px(2.)),
                            size(b.size.width, px(2.)),
                        ),
                        cursor.color,
                    )),
                    CursorShape::HollowBlock => {
                        window.paint_quad(outline(b, cursor.color, BorderStyle::Solid))
                    }
                }
            }
            for (b, c) in &state.link_lines {
                window.paint_quad(fill(*b, *c));
            }
            if let Some((origin, line, bg)) = &state.ime {
                window.paint_quad(fill(
                    Bounds::new(*origin, size(line.width, layout.line_height)),
                    *bg,
                ));
                let _ = line.paint(*origin, layout.line_height, window, cx);
            }
            if let Some((b, c)) = &state.scrollbar {
                window.paint_quad(fill(*b, *c));
            }
        });

        let focus = self.view.read(cx).focus_handle.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );
        register_mouse(&self.view, &state.hitbox, layout, window);
    }
}

fn term_mods(m: &gpui::Modifiers) -> lector_terminal::Modifiers {
    lector_terminal::Modifiers {
        shift: m.shift,
        alt: m.alt,
        ctrl: m.control,
        meta: false,
    }
}

/// Ctrl+click (Cmd+click on macOS) opens links.
fn link_modifier(m: &gpui::Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        m.platform
    } else {
        m.control
    }
}

fn local_cell(layout: &LayoutInfo, position: gpui::Point<Pixels>) -> (u32, u32, Side) {
    let (row, col, right) = cell_at(
        f32::from(position.x - layout.bounds.left()),
        f32::from(position.y - layout.bounds.top()),
        f32::from(layout.cell_width),
        f32::from(layout.line_height),
        layout.cols,
        layout.rows,
    );
    (row, col, if right { Side::Right } else { Side::Left })
}

fn mouse_event(
    kind: MouseEventKind,
    row: u32,
    col: u32,
    mods: &gpui::Modifiers,
) -> lector_terminal::MouseEvent {
    lector_terminal::MouseEvent {
        kind,
        row,
        col,
        mods: term_mods(mods),
    }
}

fn wants_mouse(view: &TerminalView, shift: bool) -> bool {
    !shift
        && view
            .snapshot
            .as_ref()
            .is_some_and(|s| s.modes.wants_mouse())
}

fn to_term_button(b: MouseButton) -> Option<lector_terminal::MouseButton> {
    match b {
        MouseButton::Left => Some(lector_terminal::MouseButton::Left),
        MouseButton::Middle => Some(lector_terminal::MouseButton::Middle),
        MouseButton::Right => Some(lector_terminal::MouseButton::Right),
        _ => None,
    }
}

fn register_mouse(
    view: &Entity<TerminalView>,
    hitbox: &Hitbox,
    layout: LayoutInfo,
    window: &mut Window,
) {
    let v = view.clone();
    let hb = hitbox.clone();
    window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
        if phase != DispatchPhase::Bubble || !hb.is_hovered(window) {
            return;
        }
        let (row, col, side) = local_cell(&layout, e.position);
        v.update(cx, |view, cx| {
            let focus = view.focus_handle.clone();
            window.focus(&focus);
            let Some(session) = view.session.clone() else {
                return;
            };
            let button = to_term_button(e.button);
            if e.button == MouseButton::Left && link_modifier(&e.modifiers) {
                if let Some(link) = view.link_at(row, col) {
                    view.open_link(link, window, cx);
                    cx.stop_propagation();
                    return;
                }
            }
            if let Some(b) = button {
                if wants_mouse(view, e.modifiers.shift)
                    && session.send_mouse(mouse_event(
                        MouseEventKind::Press(b),
                        row,
                        col,
                        &e.modifiers,
                    ))
                {
                    view.mouse_forwarded = Some(b);
                    cx.stop_propagation();
                    return;
                }
            }
            match e.button {
                MouseButton::Left => {
                    let kind = if e.modifiers.alt {
                        SelectionKind::Block
                    } else {
                        match e.click_count {
                            0 | 1 => SelectionKind::Simple,
                            2 => SelectionKind::Word,
                            _ => SelectionKind::Line,
                        }
                    };
                    let point = session.lock().viewport_point(row, col);
                    let has_selection = session.lock().selection_span().is_some();
                    if e.modifiers.shift && has_selection && e.click_count <= 1 {
                        view.update_selection(point, side, cx);
                        view.selecting = true;
                    } else {
                        view.start_selection(kind, point, side, cx);
                    }
                }
                MouseButton::Right => {
                    let link = view.link_at(row, col);
                    view.deploy_context_menu(e.position, link, window, cx);
                }
                _ => {}
            }
            cx.stop_propagation();
        });
    });

    let v = view.clone();
    let hb = hitbox.clone();
    window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
        if phase != DispatchPhase::Bubble {
            return;
        }
        let hovered = hb.is_hovered(window);
        let (row, col, side) = local_cell(&layout, e.position);
        v.update(cx, |view, cx| {
            let Some(session) = view.session.clone() else {
                return;
            };
            if let Some(b) = view.mouse_forwarded {
                session.send_mouse(mouse_event(
                    MouseEventKind::Move(Some(b)),
                    row,
                    col,
                    &e.modifiers,
                ));
                return;
            }
            if view.selecting && e.pressed_button == Some(MouseButton::Left) {
                // Dragging past the top / bottom edge scrolls the scrollback.
                if e.position.y < layout.bounds.top() {
                    session.scroll(ScrollAmount::Lines(1));
                } else if e.position.y > layout.bounds.bottom() {
                    session.scroll(ScrollAmount::Lines(-1));
                }
                let point = session.lock().viewport_point(row, col);
                view.update_selection(point, side, cx);
                return;
            }
            if !hovered {
                if view.hovered_link.take().is_some() {
                    cx.notify();
                }
                return;
            }
            if e.pressed_button.is_none()
                && view
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.modes.mouse == lector_terminal::MouseMode::Motion)
                && !e.modifiers.shift
            {
                session.send_mouse(mouse_event(
                    MouseEventKind::Move(None),
                    row,
                    col,
                    &e.modifiers,
                ));
            }
            let link = link_modifier(&e.modifiers)
                .then(|| view.link_at(row, col))
                .flatten();
            if link != view.hovered_link {
                view.hovered_link = link;
                cx.notify();
            }
        });
    });

    let v = view.clone();
    window.on_mouse_event(move |e: &MouseUpEvent, phase, _window, cx| {
        if phase != DispatchPhase::Bubble {
            return;
        }
        let (row, col, _) = local_cell(&layout, e.position);
        v.update(cx, |view, cx| {
            if let (Some(b), Some(session)) = (view.mouse_forwarded.take(), view.session.clone()) {
                session.send_mouse(mouse_event(
                    MouseEventKind::Release(b),
                    row,
                    col,
                    &e.modifiers,
                ));
                return;
            }
            if e.button == MouseButton::Left {
                view.finish_selection(cx);
            }
        });
    });

    let v = view.clone();
    let hb = hitbox.clone();
    window.on_mouse_event(move |e: &ScrollWheelEvent, phase, window, cx| {
        if phase != DispatchPhase::Bubble || !hb.is_hovered(window) {
            return;
        }
        let (row, col, _) = local_cell(&layout, e.position);
        let dy = f32::from(e.delta.pixel_delta(layout.line_height).y);
        v.update(cx, |view, cx| {
            let Some(session) = view.session.clone() else {
                return;
            };
            let lines = dy / f32::from(layout.line_height).max(1.) + view.scroll_remainder;
            let whole = lines.trunc();
            view.scroll_remainder = lines - whole;
            let n = whole as i32;
            if n == 0 {
                cx.stop_propagation();
                return;
            }
            let shift = e.modifiers.shift;
            let kind = if n > 0 {
                MouseEventKind::WheelUp
            } else {
                MouseEventKind::WheelDown
            };
            let forwarded = !shift
                && (0..n.unsigned_abs().min(20))
                    .all(|_| session.send_mouse(mouse_event(kind, row, col, &e.modifiers)));
            if !forwarded {
                view.scroll(ScrollAmount::Lines(n), cx);
            }
            cx.stop_propagation();
        });
    });

    let v = view.clone();
    let hb = hitbox.clone();
    window.on_modifiers_changed(move |e: &ModifiersChangedEvent, window, cx| {
        let position = window.mouse_position();
        let hovered = hb.is_hovered(window);
        v.update(cx, |view, cx| {
            let link = (hovered && link_modifier(&e.modifiers))
                .then(|| {
                    let (row, col, _) = local_cell(&layout, position);
                    view.link_at(row, col)
                })
                .flatten();
            if link != view.hovered_link {
                view.hovered_link = link;
                cx.notify();
            }
        });
    });
}
