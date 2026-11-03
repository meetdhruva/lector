//! The read-only code view: a virtualized, monospace text view rendered by a
//! custom GPUI element ([`element::CodeElement`]) from a [`CodeSource`].
//!
//! Features: line numbers, diff gutter marks, folding, current-line
//! highlight, mouse and keyboard selection, copy, smooth pixel scrolling,
//! horizontal scroll or soft wrap, indent guides, search highlights,
//! background decorations, inline annotations (blame), sticky headers, a
//! minimap, hover and cmd/ctrl-click callbacks.

pub mod display_map;
mod element;
mod line_cache;
pub mod selection;
pub mod source;

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    div, prelude::*, px, App, ClipboardItem, Entity, EventEmitter, FocusHandle, Focusable, Global,
    Hsla, Pixels, Point, SharedString, Task, Window,
};

pub use display_map::DisplayMap;
pub use element::CodeElement;
pub use selection::{Pos, Selection};
pub use source::{
    CodeSource, DecorationColor, GutterMark, GutterMarkKind, HighlightSpan, InMemorySource,
    LexicalHighlighter, RowDecoration, SpanStyle,
};

use crate::actions::editor::*;
use crate::components::context_menu::{ContextMenu, ContextMenuState};
use crate::icons::IconName;
use crate::keymap::contexts;
use line_cache::LineCache;
use selection::LineAccess;

/// Global editor presentation settings (zoom changes `font_size`).
#[derive(Clone, Debug)]
pub struct EditorSettings {
    /// Monospace font family.
    pub font_family: SharedString,
    /// Font size.
    pub font_size: Pixels,
    /// Line height as a multiple of the font size.
    pub line_height: f32,
    /// Tab width in columns.
    pub tab_size: usize,
    /// Show the minimap by default.
    pub show_minimap: bool,
    /// Soft wrap by default.
    pub soft_wrap: bool,
    /// Show indent guides.
    pub indent_guides: bool,
    /// Show line numbers relative to the cursor line.
    pub relative_line_numbers: bool,
    /// Show sticky scope headers.
    pub sticky_headers: bool,
    /// Enable programming ligatures (`calt`).
    pub ligatures: bool,
}

impl Default for EditorSettings {
    fn default() -> Self {
        EditorSettings {
            font_family: crate::assets::MONO_FONT.into(),
            font_size: px(13.5),
            line_height: 1.6,
            tab_size: 4,
            show_minimap: true,
            soft_wrap: false,
            indent_guides: true,
            relative_line_numbers: false,
            sticky_headers: true,
            ligatures: false,
        }
    }
}

impl Global for EditorSettings {}

impl EditorSettings {
    /// The editor font (family plus ligature setting).
    pub fn font(&self) -> gpui::Font {
        let mut font = gpui::font(self.font_family.clone());
        if !self.ligatures {
            font.features = gpui::FontFeatures::disable_ligatures();
        }
        font
    }

    /// The global settings (defaults if unset).
    pub fn get(cx: &App) -> EditorSettings {
        cx.try_global::<EditorSettings>()
            .cloned()
            .unwrap_or_default()
    }
}

/// A hover request, delivered after the pointer rests on a position.
#[derive(Clone, Debug, PartialEq)]
pub struct HoverTarget {
    /// Buffer position under the pointer.
    pub pos: Pos,
    /// Window coordinates of the pointer.
    pub position: Point<Pixels>,
}

/// A background range set by the view's owner.
#[derive(Clone, Debug, PartialEq)]
pub struct BackgroundRange {
    /// Row.
    pub row: u32,
    /// Byte range within the row.
    pub range: Range<usize>,
    /// Color.
    pub color: DecorationColor,
}

/// Events emitted by [`CodeView`].
#[derive(Clone, Debug, PartialEq)]
pub enum CodeViewEvent {
    /// The selection or cursor moved.
    SelectionChanged,
    /// The view scrolled.
    Scrolled,
}

/// Where to scroll on the next layout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ScrollRequest {
    /// Keep the cursor visible with a margin.
    FitCursor,
    /// Show `row` at the top or centered.
    Row { row: u32, center: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DragMode {
    Char,
    Word,
    Line,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DragState {
    mode: DragMode,
    origin: (Pos, Pos),
}

type HoverCallback = Rc<dyn Fn(&HoverTarget, &mut Window, &mut App)>;
type ClickCallback = Rc<dyn Fn(Pos, &mut Window, &mut App)>;

/// The code view entity. Create with [`CodeView::new`] and render it as a
/// child view; it renders [`CodeElement`] inside a focusable container with
/// key context `"CodeView"`.
pub struct CodeView {
    source: Rc<dyn CodeSource>,
    focus_handle: FocusHandle,
    selection: Selection,
    pub(crate) scroll: Point<Pixels>,
    pub(crate) pending_scroll: Option<ScrollRequest>,
    folded: BTreeSet<u32>,
    fold_ranges: Rc<Vec<Range<u32>>>,
    fold_ranges_version: Option<u64>,
    pub(crate) display_map: Rc<DisplayMap>,
    display_map_key: Option<(u64, u64, Option<usize>, usize, u32)>,
    fold_generation: u64,
    soft_wrap: Option<bool>,
    show_minimap: Option<bool>,
    show_line_numbers: bool,
    search_matches: Vec<(u32, Range<usize>)>,
    active_match: Option<usize>,
    decorations: Vec<BackgroundRange>,
    annotations: BTreeMap<u32, SharedString>,
    pub(crate) line_cache: LineCache,
    pub(crate) last_layout: Option<Rc<element::LayoutSnapshot>>,
    pub(crate) drag: Option<DragState>,
    pub(crate) scrollbar_drag: Option<Pixels>,
    pub(crate) minimap_drag: bool,
    hover_pos: Option<Pos>,
    hover_task: Option<Task<()>>,
    on_hover: Option<HoverCallback>,
    on_cmd_click: Option<ClickCallback>,
    context_menu: ContextMenuState,
    path: Option<PathBuf>,
    pub(crate) gutter_hovered: bool,
}

impl EventEmitter<CodeViewEvent> for CodeView {}

impl Focusable for CodeView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl CodeView {
    /// A view over `source`.
    pub fn new(source: Rc<dyn CodeSource>, cx: &mut Context<Self>) -> Self {
        let line_count = source.line_count();
        CodeView {
            source,
            focus_handle: cx.focus_handle(),
            selection: Selection::default(),
            scroll: Point::default(),
            pending_scroll: None,
            folded: BTreeSet::new(),
            fold_ranges: Rc::new(Vec::new()),
            fold_ranges_version: None,
            display_map: Rc::new(DisplayMap::new(line_count)),
            display_map_key: None,
            fold_generation: 0,
            soft_wrap: None,
            show_minimap: None,
            show_line_numbers: true,
            search_matches: Vec::new(),
            active_match: None,
            decorations: Vec::new(),
            annotations: BTreeMap::new(),
            line_cache: LineCache::new(4096),
            last_layout: None,
            drag: None,
            scrollbar_drag: None,
            minimap_drag: false,
            hover_pos: None,
            hover_task: None,
            on_hover: None,
            on_cmd_click: None,
            context_menu: ContextMenuState::default(),
            path: None,
            gutter_hovered: false,
        }
    }

    /// Convenience: an in-memory view of `text`.
    pub fn from_text(text: &str, file_name: Option<&str>, cx: &mut Context<Self>) -> Self {
        let source: Rc<dyn CodeSource> = match file_name {
            Some(name) => Rc::new(InMemorySource::for_file(text, name)),
            None => Rc::new(InMemorySource::new(text)),
        };
        Self::new(source, cx)
    }

    /// The source.
    pub fn source(&self) -> &Rc<dyn CodeSource> {
        &self.source
    }

    /// Replaces the source (e.g. new snapshot). Selection is clipped.
    pub fn set_source(&mut self, source: Rc<dyn CodeSource>, cx: &mut Context<Self>) {
        self.source = source;
        self.display_map_key = None;
        self.fold_ranges_version = None;
        let sel = self.selection;
        self.selection = Selection {
            anchor: selection::clip(self.source.as_ref(), sel.anchor),
            head: selection::clip(self.source.as_ref(), sel.head),
            goal_column: None,
        };
        cx.notify();
    }

    /// File path shown in menus (copy path etc.).
    pub fn set_path(&mut self, path: Option<PathBuf>) {
        self.path = path;
    }

    /// File path, if any.
    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    /// Current selection.
    pub fn selection(&self) -> Selection {
        self.selection
    }

    /// Cursor position.
    pub fn cursor(&self) -> Pos {
        self.selection.head
    }

    /// 1-based `(line, column)` of the cursor for display (column in
    /// tab-expanded display columns).
    pub fn cursor_line_col(&self, tab_size: usize) -> (u32, usize) {
        let head = self.selection.head;
        let line = self.source.line(head.row);
        (
            head.row + 1,
            display_map::display_column(&line, head.col, tab_size) + 1,
        )
    }

    /// Sets the selection (clipped) and scrolls it into view.
    pub fn set_selection(&mut self, selection: Selection, cx: &mut Context<Self>) {
        self.selection = Selection {
            anchor: selection::clip(self.source.as_ref(), selection.anchor),
            head: selection::clip(self.source.as_ref(), selection.head),
            goal_column: selection.goal_column,
        };
        self.pending_scroll = Some(ScrollRequest::FitCursor);
        cx.emit(CodeViewEvent::SelectionChanged);
        cx.notify();
    }

    /// Moves the cursor to `pos` (collapsing the selection).
    pub fn set_cursor(&mut self, pos: Pos, cx: &mut Context<Self>) {
        self.set_selection(Selection::cursor(pos), cx);
    }

    /// Scrolls so `row` is at the top (or centered).
    pub fn scroll_to_row(&mut self, row: u32, center: bool, cx: &mut Context<Self>) {
        self.pending_scroll = Some(ScrollRequest::Row { row, center });
        cx.notify();
    }

    /// Current pixel scroll offset (x, y), both non-negative.
    pub fn scroll_offset(&self) -> Point<Pixels> {
        self.scroll
    }

    /// Sets the pixel scroll offset (clamped at the next layout).
    pub fn set_scroll_offset(&mut self, offset: Point<Pixels>, cx: &mut Context<Self>) {
        self.scroll = Point::new(offset.x.max(px(0.)), offset.y.max(px(0.)));
        cx.emit(CodeViewEvent::Scrolled);
        cx.notify();
    }

    /// Soft wrap state.
    pub fn soft_wrap(&self, cx: &App) -> bool {
        self.soft_wrap
            .unwrap_or_else(|| EditorSettings::get(cx).soft_wrap)
    }

    /// Enables or disables soft wrap for this view.
    pub fn set_soft_wrap(&mut self, wrap: bool, cx: &mut Context<Self>) {
        self.soft_wrap = Some(wrap);
        self.display_map_key = None;
        self.scroll.x = px(0.);
        self.pending_scroll = Some(ScrollRequest::FitCursor);
        cx.notify();
    }

    /// Minimap visibility.
    pub fn show_minimap(&self, cx: &App) -> bool {
        self.show_minimap
            .unwrap_or_else(|| EditorSettings::get(cx).show_minimap)
    }

    /// Shows or hides the minimap for this view.
    pub fn set_show_minimap(&mut self, show: bool, cx: &mut Context<Self>) {
        self.show_minimap = Some(show);
        cx.notify();
    }

    /// Shows or hides line numbers.
    pub fn set_show_line_numbers(&mut self, show: bool, cx: &mut Context<Self>) {
        self.show_line_numbers = show;
        cx.notify();
    }

    pub(crate) fn line_numbers_visible(&self) -> bool {
        self.show_line_numbers
    }

    /// Search matches to highlight; `active` is drawn emphasized.
    pub fn set_search_matches(
        &mut self,
        matches: Vec<(u32, Range<usize>)>,
        active: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.search_matches = matches;
        self.active_match = active;
        if let Some((row, _)) = active.and_then(|ix| self.search_matches.get(ix)) {
            self.pending_scroll = Some(ScrollRequest::Row {
                row: *row,
                center: true,
            });
        }
        cx.notify();
    }

    pub(crate) fn search_matches(&self) -> (&[(u32, Range<usize>)], Option<usize>) {
        (&self.search_matches, self.active_match)
    }

    /// Background ranges (e.g. diff word highlights, references).
    pub fn set_decorations(&mut self, decorations: Vec<BackgroundRange>, cx: &mut Context<Self>) {
        self.decorations = decorations;
        cx.notify();
    }

    pub(crate) fn decorations(&self) -> &[BackgroundRange] {
        &self.decorations
    }

    /// Inline end-of-line annotations (e.g. blame) keyed by row.
    pub fn set_annotations(
        &mut self,
        annotations: BTreeMap<u32, SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.annotations = annotations;
        cx.notify();
    }

    pub(crate) fn annotations(&self) -> &BTreeMap<u32, SharedString> {
        &self.annotations
    }

    /// Called after the pointer rests over text for ~300 ms.
    pub fn on_hover(&mut self, f: impl Fn(&HoverTarget, &mut Window, &mut App) + 'static) {
        self.on_hover = Some(Rc::new(f));
    }

    /// Called on cmd-click (macOS) / ctrl-click (elsewhere).
    pub fn on_cmd_click(&mut self, f: impl Fn(Pos, &mut Window, &mut App) + 'static) {
        self.on_cmd_click = Some(Rc::new(f));
    }

    pub(crate) fn cmd_click_callback(&self) -> Option<ClickCallback> {
        self.on_cmd_click.clone()
    }

    /// Fold regions from the source (cached per version).
    pub(crate) fn fold_ranges(&mut self) -> Rc<Vec<Range<u32>>> {
        let version = self.source.version();
        if self.fold_ranges_version != Some(version) {
            self.fold_ranges = Rc::new(self.source.fold_ranges());
            self.fold_ranges_version = Some(version);
        }
        self.fold_ranges.clone()
    }

    /// Whether the fold starting at `row` is folded.
    pub fn is_folded(&self, row: u32) -> bool {
        self.folded.contains(&row)
    }

    /// Folds or unfolds the region starting at `row`.
    pub fn set_folded(&mut self, row: u32, folded: bool, cx: &mut Context<Self>) {
        let changed = if folded {
            self.fold_ranges().iter().any(|r| r.start == row) && self.folded.insert(row)
        } else {
            self.folded.remove(&row)
        };
        if changed {
            self.fold_generation += 1;
            self.display_map_key = None;
            // Keep the cursor out of hidden rows.
            if folded && self.display_map_hides(self.selection.head.row) {
                self.selection = Selection::cursor(Pos::new(row, 0));
                cx.emit(CodeViewEvent::SelectionChanged);
            }
            cx.notify();
        }
    }

    fn display_map_hides(&mut self, row: u32) -> bool {
        let ranges = self.fold_ranges();
        self.folded.iter().any(|start| {
            ranges
                .iter()
                .find(|r| r.start == *start)
                .is_some_and(|r| row > r.start && row < r.end)
        })
    }

    /// Toggles the innermost fold containing the cursor (or starting on its row).
    pub fn toggle_fold_at_cursor(&mut self, cx: &mut Context<Self>) {
        let row = self.selection.head.row;
        let ranges = self.fold_ranges();
        if self.folded.contains(&row) {
            self.set_folded(row, false, cx);
            return;
        }
        let target = ranges
            .iter()
            .filter(|r| r.start == row || (r.start < row && row < r.end))
            .max_by_key(|r| r.start)
            .map(|r| r.start);
        if let Some(start) = target {
            let folded = !self.folded.contains(&start);
            self.set_folded(start, folded, cx);
        }
    }

    pub(crate) fn active_folds(&mut self) -> Vec<Range<u32>> {
        let ranges = self.fold_ranges();
        ranges
            .iter()
            .filter(|r| self.folded.contains(&r.start))
            .cloned()
            .collect()
    }

    pub(crate) fn display_map_key(&self) -> Option<(u64, u64, Option<usize>, usize, u32)> {
        self.display_map_key
    }

    pub(crate) fn set_display_map(
        &mut self,
        map: DisplayMap,
        key: (u64, u64, Option<usize>, usize, u32),
    ) {
        self.display_map = Rc::new(map);
        self.display_map_key = Some(key);
    }

    pub(crate) fn fold_generation(&self) -> u64 {
        self.fold_generation
    }

    fn page_rows(&self) -> i64 {
        self.last_layout
            .as_ref()
            .map(|l| (l.visible_rows() as i64 - 2).max(1))
            .unwrap_or(30)
    }

    fn tab_size(cx: &App) -> usize {
        EditorSettings::get(cx).tab_size
    }

    fn move_head(
        &mut self,
        extend: bool,
        cx: &mut Context<Self>,
        f: impl FnOnce(&Self, Pos) -> Pos,
    ) {
        let head = f(self, self.selection.head);
        let anchor = if extend { self.selection.anchor } else { head };
        self.set_selection(
            Selection {
                anchor,
                head,
                goal_column: None,
            },
            cx,
        );
    }

    fn move_vertical(&mut self, delta: i64, extend: bool, cx: &mut Context<Self>) {
        let tab = Self::tab_size(cx);
        let sel = self.selection;
        // Collapse to the selection edge in the direction of travel.
        let from = if !extend && !sel.is_empty() {
            let (s, e) = sel.range();
            if delta < 0 {
                s
            } else {
                e
            }
        } else {
            sel.head
        };
        let (head, goal) = selection::vertical(
            self.source.as_ref(),
            &self.display_map,
            from,
            sel.goal_column,
            delta,
            tab,
        );
        let anchor = if extend { sel.anchor } else { head };
        self.set_selection(
            Selection {
                anchor,
                head,
                goal_column: Some(goal),
            },
            cx,
        );
    }

    fn move_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(-1, false, cx);
    }
    fn move_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(1, false, cx);
    }
    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(-1, true, cx);
    }
    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(1, true, cx);
    }
    fn page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        let n = self.page_rows();
        self.scroll.y = (self.scroll.y - self.line_height_px(cx) * n as f32).max(px(0.));
        self.move_vertical(-n, false, cx);
    }
    fn page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        let n = self.page_rows();
        self.scroll.y += self.line_height_px(cx) * n as f32;
        self.move_vertical(n, false, cx);
    }
    fn select_page_up(&mut self, _: &SelectPageUp, _: &mut Window, cx: &mut Context<Self>) {
        let n = self.page_rows();
        self.move_vertical(-n, true, cx);
    }
    fn select_page_down(&mut self, _: &SelectPageDown, _: &mut Window, cx: &mut Context<Self>) {
        let n = self.page_rows();
        self.move_vertical(n, true, cx);
    }

    fn line_height_px(&self, cx: &App) -> Pixels {
        self.last_layout
            .as_ref()
            .map(|l| l.line_height)
            .unwrap_or_else(|| {
                let s = EditorSettings::get(cx);
                (s.font_size * s.line_height).round()
            })
    }

    fn move_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.is_empty() {
            let (s, _) = self.selection.range();
            self.set_cursor(s, cx);
            return;
        }
        self.move_head(false, cx, |this, p| {
            selection::left(this.source.as_ref(), p)
        });
    }
    fn move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.is_empty() {
            let (_, e) = self.selection.range();
            self.set_cursor(e, cx);
            return;
        }
        self.move_head(false, cx, |this, p| {
            selection::right(this.source.as_ref(), p)
        });
    }
    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_head(true, cx, |this, p| selection::left(this.source.as_ref(), p));
    }
    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_head(true, cx, |this, p| {
            selection::right(this.source.as_ref(), p)
        });
    }
    fn move_word_left(&mut self, _: &MoveWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_head(false, cx, |this, p| {
            selection::word_left(this.source.as_ref(), p)
        });
    }
    fn move_word_right(&mut self, _: &MoveWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_head(false, cx, |this, p| {
            selection::word_right(this.source.as_ref(), p)
        });
    }
    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_head(true, cx, |this, p| {
            selection::word_left(this.source.as_ref(), p)
        });
    }
    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_head(true, cx, |this, p| {
            selection::word_right(this.source.as_ref(), p)
        });
    }

    fn line_start(this: &Self, p: Pos) -> Pos {
        // Smart home: first non-whitespace, then column 0.
        let line = this.source.line(p.row);
        let first = line.len() - line.trim_start().len();
        Pos::new(p.row, if p.col == first { 0 } else { first })
    }

    fn move_to_line_start(&mut self, _: &MoveToLineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_head(false, cx, Self::line_start);
    }
    fn move_to_line_end(&mut self, _: &MoveToLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_head(false, cx, |this, p| {
            Pos::new(p.row, this.source.line(p.row).len())
        });
    }
    fn select_to_line_start(
        &mut self,
        _: &SelectToLineStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_head(true, cx, Self::line_start);
    }
    fn select_to_line_end(&mut self, _: &SelectToLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_head(true, cx, |this, p| {
            Pos::new(p.row, this.source.line(p.row).len())
        });
    }

    fn end_pos(&self) -> Pos {
        let last = self.source.line_count().saturating_sub(1);
        Pos::new(last, self.source.line(last).len())
    }

    fn scroll_to_top(&mut self, _: &ScrollToTop, _: &mut Window, cx: &mut Context<Self>) {
        self.set_cursor(Pos::new(0, 0), cx);
    }
    fn scroll_to_bottom(&mut self, _: &ScrollToBottom, _: &mut Window, cx: &mut Context<Self>) {
        let end = self.end_pos();
        self.set_cursor(end, cx);
    }
    fn select_to_top(&mut self, _: &SelectToTop, _: &mut Window, cx: &mut Context<Self>) {
        self.move_head(true, cx, |_, _| Pos::new(0, 0));
    }
    fn select_to_bottom(&mut self, _: &SelectToBottom, _: &mut Window, cx: &mut Context<Self>) {
        let end = self.end_pos();
        self.move_head(true, cx, move |_, _| end);
    }

    /// Selects the whole text.
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        let end = self.end_pos();
        self.selection = Selection {
            anchor: Pos::new(0, 0),
            head: end,
            goal_column: None,
        };
        cx.emit(CodeViewEvent::SelectionChanged);
        cx.notify();
    }

    fn select_all_action(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_all(cx);
    }

    fn clear_selection(&mut self, _: &ClearSelection, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.is_empty() {
            let head = self.selection.head;
            self.set_cursor(head, cx);
        } else if !self.search_matches.is_empty() {
            self.search_matches.clear();
            self.active_match = None;
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    fn find_next(&mut self, _: &FindNext, _: &mut Window, cx: &mut Context<Self>) {
        if self.step_search_match(1, false, cx).is_none() {
            cx.propagate();
        }
    }

    fn find_prev(&mut self, _: &FindPrev, _: &mut Window, cx: &mut Context<Self>) {
        if self.step_search_match(-1, false, cx).is_none() {
            cx.propagate();
        }
    }

    /// Selected text (empty if the selection is empty).
    pub fn selected_text(&self) -> String {
        selection::selected_text(self.source.as_ref(), &self.selection)
    }

    /// Copies the selection (or the current line when empty) to the clipboard.
    pub fn copy(&mut self, cx: &mut Context<Self>) {
        let text = if self.selection.is_empty() {
            let row = self.selection.head.row;
            format!("{}\n", self.source.line(row))
        } else {
            self.selected_text()
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn copy_action(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        self.copy(cx);
    }

    fn toggle_soft_wrap(&mut self, _: &ToggleSoftWrap, _: &mut Window, cx: &mut Context<Self>) {
        let wrap = !self.soft_wrap(cx);
        self.set_soft_wrap(wrap, cx);
    }

    fn toggle_minimap(&mut self, _: &ToggleMinimap, _: &mut Window, cx: &mut Context<Self>) {
        let show = !self.show_minimap(cx);
        self.set_show_minimap(show, cx);
    }

    fn toggle_fold(&mut self, _: &ToggleFold, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_fold_at_cursor(cx);
    }

    fn fold_all(&mut self, _: &FoldAll, _: &mut Window, cx: &mut Context<Self>) {
        let ranges = self.fold_ranges();
        // Fold top-level regions only (nested ones are hidden anyway).
        let mut end = 0;
        for r in ranges.iter() {
            if r.start >= end {
                self.folded.insert(r.start);
                end = r.end;
            }
        }
        self.fold_generation += 1;
        self.display_map_key = None;
        let head = self.selection.head;
        if self.display_map_hides(head.row) {
            let start = ranges
                .iter()
                .filter(|r| r.start < head.row && head.row < r.end)
                .map(|r| r.start)
                .min()
                .unwrap_or(0);
            self.selection = Selection::cursor(Pos::new(start, 0));
        }
        self.pending_scroll = Some(ScrollRequest::FitCursor);
        cx.notify();
    }

    fn unfold_all(&mut self, _: &UnfoldAll, _: &mut Window, cx: &mut Context<Self>) {
        self.folded.clear();
        self.fold_generation += 1;
        self.display_map_key = None;
        self.pending_scroll = Some(ScrollRequest::FitCursor);
        cx.notify();
    }

    fn jump_change(&mut self, forward: bool, cx: &mut Context<Self>) {
        let marks = self.source.gutter_marks(0..self.source.line_count());
        let row = self.selection.head.row;
        let target = if forward {
            marks
                .iter()
                .map(|m| m.rows.start)
                .filter(|r| *r > row)
                .min()
        } else {
            marks
                .iter()
                .map(|m| m.rows.start)
                .filter(|r| *r < row)
                .max()
        };
        if let Some(target) = target {
            self.set_cursor(Pos::new(target, 0), cx);
            self.pending_scroll = Some(ScrollRequest::Row {
                row: target,
                center: true,
            });
        }
    }

    fn next_change(&mut self, _: &NextChange, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_change(true, cx);
    }
    fn prev_change(&mut self, _: &PrevChange, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_change(false, cx);
    }

    // Mouse-driven selection (called from the element).

    pub(crate) fn begin_selection(
        &mut self,
        pos: Pos,
        click_count: usize,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        let (mode, range) = match click_count {
            2 => (
                DragMode::Word,
                selection::word_range(self.source.as_ref(), pos),
            ),
            n if n >= 3 => (DragMode::Line, self.line_range(pos.row)),
            _ => (DragMode::Char, (pos, pos)),
        };
        if extend && mode == DragMode::Char {
            self.selection.head = pos;
            self.selection.goal_column = None;
            self.drag = Some(DragState {
                mode,
                origin: (self.selection.anchor, self.selection.anchor),
            });
        } else {
            self.selection = Selection {
                anchor: range.0,
                head: range.1,
                goal_column: None,
            };
            self.drag = Some(DragState {
                mode,
                origin: range,
            });
        }
        cx.emit(CodeViewEvent::SelectionChanged);
        cx.notify();
    }

    fn line_range(&self, row: u32) -> (Pos, Pos) {
        let next = row + 1;
        if next < self.source.line_count() {
            (Pos::new(row, 0), Pos::new(next, 0))
        } else {
            (Pos::new(row, 0), Pos::new(row, self.source.line(row).len()))
        }
    }

    pub(crate) fn extend_selection(&mut self, pos: Pos, cx: &mut Context<Self>) {
        let Some(drag) = self.drag else { return };
        let range = match drag.mode {
            DragMode::Char => (pos, pos),
            DragMode::Word => selection::word_range(self.source.as_ref(), pos),
            DragMode::Line => self.line_range(pos.row),
        };
        let (o_start, o_end) = drag.origin;
        let (anchor, head) = if range.0 < o_start {
            (o_end, range.0)
        } else {
            (o_start, range.1.max(o_end))
        };
        let new = Selection {
            anchor,
            head,
            goal_column: None,
        };
        if new != self.selection {
            self.selection = new;
            cx.emit(CodeViewEvent::SelectionChanged);
            cx.notify();
        }
    }

    pub(crate) fn end_selection(&mut self) {
        self.drag = None;
    }

    pub(crate) fn is_selecting(&self) -> bool {
        self.drag.is_some()
    }

    pub(crate) fn hover_moved(
        &mut self,
        pos: Option<Pos>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.hover_pos == pos {
            return;
        }
        self.hover_pos = pos;
        self.hover_task = None;
        let (Some(pos), Some(callback)) = (pos, self.on_hover.clone()) else {
            return;
        };
        self.hover_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.hover_pos == Some(pos) {
                    callback(&HoverTarget { pos, position }, window, cx);
                }
            });
        }));
    }

    pub(crate) fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = self.focus_handle.clone();
        let has_selection = !self.selection.is_empty();
        let has_path = self.path.is_some();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let menu = menu.context(focus);
            let menu = if has_selection {
                menu.icon_action("Copy", IconName::Copy, Box::new(Copy))
            } else {
                menu.icon_action("Copy Line", IconName::Copy, Box::new(Copy))
            };
            let menu = menu
                .action("Select All", Box::new(SelectAll))
                .separator()
                .action("Go to Definition", Box::new(GoToDefinition))
                .action("Find References", Box::new(FindReferences))
                .action("Go to Line…", Box::new(GoToLine))
                .separator();
            let menu = if has_path {
                menu.icon_action("Copy Path", IconName::Link, Box::new(CopyPath))
                    .action("Copy Relative Path", Box::new(CopyRelativePath))
                    .icon_action(
                        "Copy Permalink",
                        IconName::ExternalLink,
                        Box::new(CopyPermalink),
                    )
                    .separator()
                    .icon_action(
                        "Show File History",
                        IconName::History,
                        Box::new(ShowFileHistory),
                    )
                    .action("Show Line History", Box::new(ShowLineHistory))
                    .action("Toggle Blame", Box::new(ToggleBlame))
                    .separator()
            } else {
                menu
            };
            menu.icon_action(
                "Toggle Soft Wrap",
                IconName::WrapText,
                Box::new(ToggleSoftWrap),
            )
            .icon_action("Toggle Minimap", IconName::Map, Box::new(ToggleMinimap))
            .action("Fold / Unfold", Box::new(ToggleFold))
        });
        self.context_menu
            .deploy(menu, position, window, cx, |this: &mut Self| {
                &mut this.context_menu
            });
        cx.notify();
    }

    /// Text of `row` (for tests and integration).
    pub fn line_text(&self, row: u32) -> SharedString {
        self.source.line_text(row)
    }
}

impl Render for CodeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("code-view")
            .key_context(contexts::CODE_VIEW)
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_hidden()
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::move_left))
            .on_action(cx.listener(Self::move_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::select_page_up))
            .on_action(cx.listener(Self::select_page_down))
            .on_action(cx.listener(Self::move_word_left))
            .on_action(cx.listener(Self::move_word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::move_to_line_start))
            .on_action(cx.listener(Self::move_to_line_end))
            .on_action(cx.listener(Self::select_to_line_start))
            .on_action(cx.listener(Self::select_to_line_end))
            .on_action(cx.listener(Self::scroll_to_top))
            .on_action(cx.listener(Self::scroll_to_bottom))
            .on_action(cx.listener(Self::select_to_top))
            .on_action(cx.listener(Self::select_to_bottom))
            .on_action(cx.listener(Self::select_all_action))
            .on_action(cx.listener(Self::clear_selection))
            .on_action(cx.listener(Self::copy_action))
            .on_action(cx.listener(Self::toggle_soft_wrap))
            .on_action(cx.listener(Self::toggle_minimap))
            .on_action(cx.listener(Self::toggle_fold))
            .on_action(cx.listener(Self::fold_all))
            .on_action(cx.listener(Self::unfold_all))
            .on_action(cx.listener(Self::find_next))
            .on_action(cx.listener(Self::find_prev))
            .on_action(cx.listener(Self::next_change))
            .on_action(cx.listener(Self::prev_change))
            .child(CodeElement::new(cx.entity()))
            .children(self.context_menu.render())
    }
}

/// Theme color for a decoration.
pub(crate) fn decoration_color(color: DecorationColor, theme: &lector_theme::Theme) -> Hsla {
    use crate::theme::ColorExt;
    match color {
        DecorationColor::AddedLine => theme.diff.added_line_bg.hsla(),
        DecorationColor::RemovedLine => theme.diff.removed_line_bg.hsla(),
        DecorationColor::AddedWord => theme.diff.added_word_bg.hsla(),
        DecorationColor::RemovedWord => theme.diff.removed_word_bg.hsla(),
        DecorationColor::Filler => theme.diff.filler.hsla(),
        DecorationColor::HunkHeader => theme.diff.hunk_header_bg.hsla(),
        DecorationColor::SearchMatch => theme.ui.search_match.hsla(),
        DecorationColor::WordHighlight => theme.ui.word_highlight.hsla(),
        DecorationColor::BracketMatch => theme.ui.bracket_match.hsla(),
        DecorationColor::Custom(c) => c.hsla(),
    }
}

/// Creates a code view entity for `text`.
pub fn code_view_for_text(text: &str, file_name: Option<&str>, cx: &mut App) -> Entity<CodeView> {
    cx.new(|cx| CodeView::from_text(text, file_name, cx))
}
