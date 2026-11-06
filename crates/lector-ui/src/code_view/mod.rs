//! The code view: a virtualized, monospace text view rendered by a custom
//! GPUI element ([`element::CodeElement`]) from a [`CodeSource`], optionally
//! editable through a shared [`edit::EditBuffer`].
//!
//! Features: line numbers, diff gutter marks, folding, current-line
//! highlight, multiple carets and selections (mouse and keyboard), copy,
//! smooth pixel scrolling, horizontal scroll or soft wrap, indent guides,
//! search highlights, background decorations, inline annotations (blame),
//! sticky headers, a minimap, hover and cmd/ctrl-click callbacks, and — with
//! a buffer — text input (IME composition via `EntityInputHandler`), every
//! `editor::*` editing action, undo/redo and caret blinking.

pub mod display_map;
pub mod edit;
mod element;
pub mod layers;
mod line_cache;
pub mod selection;
pub mod source;

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    div, prelude::*, px, App, Bounds, ClipboardItem, Entity, EntityInputHandler, EventEmitter,
    FocusHandle, Focusable, Global, Hsla, Pixels, Point, SharedString, Subscription, Task,
    UTF16Selection, Window,
};
use lector_buffer::{Buffer, Transaction};
use lector_text::Rope;

pub use display_map::DisplayMap;
pub use edit::{EditBuffer, EditBufferEvent, RopeSource};
pub use element::CodeElement;
pub use layers::{RowBackground, RowWidget, WidgetAnchor};
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

/// Language features available for a file (context menu enablement).
/// Provided by [`LanguageFeatureProvider`]; all off by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LanguageFeatures {
    /// `editor::Format` has a handler for this file.
    pub format: bool,
    /// `editor::Rename` has a handler for this file.
    pub rename: bool,
    /// `editor::CodeActions` has a handler for this file.
    pub code_actions: bool,
}

/// Global hook telling the code view context menu which language features
/// are available for a path (set by the LSP integration).
#[derive(Clone)]
pub struct LanguageFeatureProvider(pub FeatureFn);

/// The provider function of [`LanguageFeatureProvider`].
pub type FeatureFn = Rc<dyn Fn(&std::path::Path, &App) -> LanguageFeatures>;

impl Global for LanguageFeatureProvider {}

impl LanguageFeatureProvider {
    /// Installs `f` as the provider.
    pub fn set(cx: &mut App, f: impl Fn(&std::path::Path, &App) -> LanguageFeatures + 'static) {
        cx.set_global(LanguageFeatureProvider(Rc::new(f)));
    }

    /// Features for `path` (all off without a provider).
    pub fn features(path: &std::path::Path, cx: &App) -> LanguageFeatures {
        cx.try_global::<LanguageFeatureProvider>()
            .map(|p| (p.0)(path, cx))
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

/// An underline under a byte range of one row (diagnostics).
#[derive(Clone, Debug, PartialEq)]
pub struct Underline {
    /// Row.
    pub row: u32,
    /// Byte range within the row (an empty range underlines one char).
    pub range: Range<usize>,
    /// Color.
    pub color: Hsla,
    /// Squiggly instead of straight.
    pub wavy: bool,
}

/// An icon in the gutter's left margin (diagnostic severity, lightbulb).
#[derive(Clone, Debug, PartialEq)]
pub struct GutterIcon {
    /// Row.
    pub row: u32,
    /// Icon.
    pub icon: IconName,
    /// Tint.
    pub color: Hsla,
    /// Clicks call [`CodeView::on_gutter_icon_click`].
    pub clickable: bool,
}

/// Language-feature marks of a view, set together with
/// [`CodeView::set_inline_marks`] (by the LSP integration).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InlineMarks {
    /// Underlines, sorted by row.
    pub underlines: Vec<Underline>,
    /// Gutter icons (one per row; later entries win).
    pub gutter_icons: Vec<GutterIcon>,
    /// Scrollbar markers: row and color.
    pub scrollbar: Vec<(u32, Hsla)>,
}

type GutterIconCallback = Rc<dyn Fn(u32, &mut Window, &mut App)>;

/// Events emitted by [`CodeView`].
#[derive(Clone, Debug, PartialEq)]
pub enum CodeViewEvent {
    /// The selection or cursor moved.
    SelectionChanged,
    /// The view scrolled.
    Scrolled,
    /// The buffer position under the pointer changed (`None`: off text).
    HoverMoved(Option<Pos>),
    /// This view changed the text (typing, paste, undo, ...).
    Edited,
    /// The view's read-only flag changed.
    ReadOnlyChanged,
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

/// Pointer rest time before [`CodeView::on_hover`] fires.
pub const HOVER_DELAY_MS: u64 = 500;

/// Caret blink half-period.
pub const BLINK_INTERVAL_MS: u64 = 530;

type HoverCallback = Rc<dyn Fn(&HoverTarget, &mut Window, &mut App)>;
type ClickCallback = Rc<dyn Fn(Pos, &mut Window, &mut App)>;

/// The code view entity. Create with [`CodeView::new`] and render it as a
/// child view; it renders [`CodeElement`] inside a focusable container with
/// key context `"CodeView"`.
pub struct CodeView {
    source: Rc<dyn CodeSource>,
    focus_handle: FocusHandle,
    /// Sorted, non-overlapping; never empty.
    selections: Vec<Selection>,
    /// Index of the primary (newest) selection.
    primary: usize,
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
    /// Owner-keyed row backgrounds and widgets (`layers`).
    layers: layers::Layers,
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
    gutter_annotations: Option<Rc<GutterAnnotations>>,
    on_gutter_annotation_click: Option<AnnotationClickCallback>,
    /// Soft-wrap extra rows per buffer row: (cols, tab, text version, counts).
    wrap_counts: Option<WrapCounts>,
    // Editing.
    buffer: Option<Entity<EditBuffer>>,
    manage_source: bool,
    read_only: bool,
    marked: Option<Range<usize>>,
    ime_transaction: bool,
    pub(crate) cursor_visible: bool,
    blink_epoch: u64,
    blink_task: Option<Task<()>>,
    _buffer_subscription: Option<Subscription>,
    inline_marks: Rc<InlineMarks>,
    on_gutter_icon_click: Option<GutterIconCallback>,
    overlay: Option<(gpui::AnyView, Point<Pixels>, gpui::Corner, Subscription)>,
}

type AnnotationClickCallback = Rc<dyn Fn(usize, &mut Window, &mut App)>;

struct WrapCounts {
    cols: usize,
    tab: usize,
    version: u64,
    counts: Rc<std::cell::RefCell<Vec<Option<u32>>>>,
}

/// One block of a left gutter annotation column (blame): consecutive rows
/// attributed to the same thing. The label is drawn on the block's first
/// visible row.
#[derive(Clone, Debug, PartialEq)]
pub struct GutterAnnotation {
    /// Rows covered (sorted, non-overlapping across blocks).
    pub rows: Range<u32>,
    /// Main text (e.g. "Alice · Fix parser").
    pub text: SharedString,
    /// Right-aligned secondary text (e.g. "3 days ago").
    pub detail: SharedString,
    /// Color of a thin bar at the left edge (e.g. age heat).
    pub bar: Option<Hsla>,
    /// Draw the block with a subtle background (alternating blocks).
    pub shaded: bool,
    /// Draw the text dimmed/italic (e.g. uncommitted lines).
    pub muted: bool,
}

/// A left gutter annotation column, see [`CodeView::set_gutter_annotations`].
#[derive(Clone, Debug, PartialEq, Default)]
pub struct GutterAnnotations {
    /// Column width in characters.
    pub width_chars: usize,
    /// Blocks sorted by row.
    pub blocks: Vec<GutterAnnotation>,
}

impl GutterAnnotations {
    /// Index of the block containing `row`.
    pub fn block_at(&self, row: u32) -> Option<usize> {
        let ix = self.blocks.partition_point(|b| b.rows.end <= row);
        self.blocks
            .get(ix)
            .filter(|b| b.rows.start <= row && row < b.rows.end)
            .map(|_| ix)
    }
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
            selections: vec![Selection::default()],
            primary: 0,
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
            layers: layers::Layers::default(),
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
            gutter_annotations: None,
            on_gutter_annotation_click: None,
            wrap_counts: None,
            buffer: None,
            manage_source: false,
            read_only: false,
            marked: None,
            ime_transaction: false,
            cursor_visible: true,
            blink_epoch: 0,
            blink_task: None,
            _buffer_subscription: None,
            inline_marks: Rc::new(InlineMarks::default()),
            on_gutter_icon_click: None,
            overlay: None,
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

    /// An editable view over a new buffer holding `text`; the view keeps its
    /// own [`RopeSource`] in sync (scratch buffers, tests).
    pub fn editable(
        text: &str,
        file_name: Option<&str>,
        options: lector_buffer::BufferOptions,
        cx: &mut Context<Self>,
    ) -> Self {
        let rope = Rope::from_str(text);
        let source = Rc::new(RopeSource::new(rope.clone(), file_name));
        let mut view = Self::new(source, cx);
        view.path = file_name.map(PathBuf::from);
        let buffer = cx.new(|_| EditBuffer::new(Buffer::new(rope, options)));
        view.set_buffer(Some(buffer), true, cx);
        view
    }

    /// The source.
    pub fn source(&self) -> &Rc<dyn CodeSource> {
        &self.source
    }

    /// Replaces the source (e.g. new snapshot). Selections are clipped.
    pub fn set_source(&mut self, source: Rc<dyn CodeSource>, cx: &mut Context<Self>) {
        self.source = source;
        self.display_map_key = None;
        self.fold_ranges_version = None;
        let sels = std::mem::take(&mut self.selections);
        let clipped = sels
            .into_iter()
            .map(|s| Selection {
                anchor: selection::clip(self.source.as_ref(), s.anchor),
                head: selection::clip(self.source.as_ref(), s.head),
                goal_column: s.goal_column,
            })
            .collect();
        let (sels, primary) = selection::normalize(clipped, self.primary);
        self.selections = sels;
        self.primary = primary;
        cx.notify();
    }

    /// Attaches (or detaches) the editing buffer. With `manage_source`, the
    /// view replaces its source with a [`RopeSource`] after every edit;
    /// otherwise the owner updates the source (see `editor::DocumentModel`).
    pub fn set_buffer(
        &mut self,
        buffer: Option<Entity<EditBuffer>>,
        manage_source: bool,
        cx: &mut Context<Self>,
    ) {
        self._buffer_subscription = buffer.as_ref().map(|b| {
            cx.subscribe(b, |this, buffer, event: &EditBufferEvent, cx| {
                let EditBufferEvent::Edited {
                    origin,
                    transaction,
                } = event;
                this.buffer_edited(*origin, transaction, &buffer, cx);
            })
        });
        self.buffer = buffer;
        self.manage_source = manage_source;
        self.marked = None;
        cx.notify();
    }

    /// The editing buffer, if any.
    pub fn buffer(&self) -> Option<&Entity<EditBuffer>> {
        self.buffer.as_ref()
    }

    /// Whether editing is blocked (or there is no buffer).
    pub fn is_read_only(&self) -> bool {
        self.read_only || self.buffer.is_none()
    }

    /// Whether the text can be edited here (a buffer and not read-only).
    pub fn is_editable(&self) -> bool {
        !self.is_read_only()
    }

    /// Blocks or allows editing in this view.
    pub fn set_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        if self.read_only != read_only {
            self.read_only = read_only;
            self.marked = None;
            cx.emit(CodeViewEvent::ReadOnlyChanged);
            cx.notify();
        }
    }

    /// File path shown in menus (copy path etc.).
    pub fn set_path(&mut self, path: Option<PathBuf>) {
        self.path = path;
    }

    /// File path, if any.
    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    /// The primary (newest) selection.
    pub fn selection(&self) -> Selection {
        self.selections
            .get(self.primary)
            .copied()
            .unwrap_or_default()
    }

    /// All selections, sorted by position.
    pub fn selections(&self) -> &[Selection] {
        &self.selections
    }

    /// Index of the primary selection in [`CodeView::selections`].
    pub fn primary_index(&self) -> usize {
        self.primary
    }

    /// Number of selections (carets).
    pub fn selection_count(&self) -> usize {
        self.selections.len()
    }

    /// The IME composition range (bytes), if composing.
    pub fn marked_range(&self) -> Option<Range<usize>> {
        self.marked.clone()
    }

    /// Cursor position (primary caret).
    pub fn cursor(&self) -> Pos {
        self.selection().head
    }

    /// 1-based `(line, column)` of the cursor for display (column in
    /// tab-expanded display columns).
    pub fn cursor_line_col(&self, tab_size: usize) -> (u32, usize) {
        let head = self.selection().head;
        let line = self.source.line(head.row);
        (
            head.row + 1,
            display_map::display_column(&line, head.col, tab_size) + 1,
        )
    }

    fn clip_selection(&self, s: Selection) -> Selection {
        Selection {
            anchor: selection::clip(self.source.as_ref(), s.anchor),
            head: selection::clip(self.source.as_ref(), s.head),
            goal_column: s.goal_column,
        }
    }

    /// Sets a single selection (clipped) and scrolls it into view.
    pub fn set_selection(&mut self, selection: Selection, cx: &mut Context<Self>) {
        self.selections = vec![self.clip_selection(selection)];
        self.primary = 0;
        self.selections_changed(true, cx);
    }

    /// Sets several selections (clipped, merged); the last one becomes the
    /// primary. Scrolls the primary into view.
    pub fn set_selections(&mut self, selections: Vec<Selection>, cx: &mut Context<Self>) {
        if selections.is_empty() {
            return;
        }
        let primary = selections.len() - 1;
        let clipped: Vec<Selection> = selections
            .into_iter()
            .map(|s| self.clip_selection(s))
            .collect();
        let (sels, primary) = selection::normalize(clipped, primary);
        self.selections = sels;
        self.primary = primary;
        self.selections_changed(true, cx);
    }

    fn selections_changed(&mut self, scroll: bool, cx: &mut Context<Self>) {
        if scroll {
            self.pending_scroll = Some(ScrollRequest::FitCursor);
        }
        self.marked = None;
        self.reset_blink();
        cx.emit(CodeViewEvent::SelectionChanged);
        cx.notify();
    }

    /// Moves the cursor to `pos` (collapsing all selections).
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

    /// Replaces the language-feature marks (underlines, gutter icons,
    /// scrollbar markers).
    pub fn set_inline_marks(&mut self, marks: InlineMarks, cx: &mut Context<Self>) {
        if *self.inline_marks != marks {
            self.inline_marks = Rc::new(marks);
            cx.notify();
        }
    }

    /// The language-feature marks.
    pub fn inline_marks(&self) -> &Rc<InlineMarks> {
        &self.inline_marks
    }

    /// Called with the row when a clickable [`GutterIcon`] is clicked.
    pub fn on_gutter_icon_click(&mut self, f: impl Fn(u32, &mut Window, &mut App) + 'static) {
        self.on_gutter_icon_click = Some(Rc::new(f));
    }

    pub(crate) fn gutter_icon_click_callback(&self) -> Option<GutterIconCallback> {
        self.on_gutter_icon_click.clone()
    }

    /// Shows a floating view (code actions menu, rename box) anchored at
    /// `position` (window coordinates), focused; it closes when it emits
    /// `DismissEvent` (focus returns to what had it) or on
    /// [`CodeView::close_overlay`].
    pub fn show_overlay<V: gpui::ManagedView>(
        &mut self,
        view: Entity<V>,
        position: Point<Pixels>,
        anchor: gpui::Corner,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous_focus = window.focused(cx);
        let sub = cx.subscribe_in(
            &view,
            window,
            move |this, _, _: &gpui::DismissEvent, window, cx| {
                this.overlay = None;
                if let Some(focus) = previous_focus.as_ref() {
                    window.focus(focus);
                }
                cx.notify();
            },
        );
        window.focus(&view.focus_handle(cx));
        self.overlay = Some((view.into(), position, anchor, sub));
        // A pointer hover popover would cover the overlay: dismiss it.
        self.hover_pos = None;
        self.hover_task = None;
        cx.emit(CodeViewEvent::HoverMoved(None));
        cx.notify();
    }

    /// Closes the floating view, if any.
    pub fn close_overlay(&mut self, cx: &mut Context<Self>) {
        if self.overlay.take().is_some() {
            cx.notify();
        }
    }

    /// The floating view shown by [`CodeView::show_overlay`], if any.
    pub fn overlay(&self) -> Option<&gpui::AnyView> {
        self.overlay.as_ref().map(|(v, ..)| v)
    }

    fn render_overlay(&self) -> Option<impl IntoElement> {
        let (view, position, anchor, _) = self.overlay.as_ref()?;
        Some(
            gpui::deferred(
                gpui::anchored()
                    .position(*position)
                    .anchor(*anchor)
                    .snap_to_window_with_margin(px(8.))
                    .child(div().occlude().child(view.clone())),
            )
            .with_priority(2),
        )
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

    /// Shows (or hides with `None`) a left gutter annotation column, e.g.
    /// blame, before the line numbers.
    pub fn set_gutter_annotations(
        &mut self,
        annotations: Option<GutterAnnotations>,
        cx: &mut Context<Self>,
    ) {
        self.gutter_annotations = annotations.map(Rc::new);
        cx.notify();
    }

    /// The gutter annotation column, if shown.
    pub fn gutter_annotations(&self) -> Option<&Rc<GutterAnnotations>> {
        self.gutter_annotations.as_ref()
    }

    /// Called with the block index when a gutter annotation is clicked.
    pub fn on_gutter_annotation_click(
        &mut self,
        f: impl Fn(usize, &mut Window, &mut App) + 'static,
    ) {
        self.on_gutter_annotation_click = Some(Rc::new(f));
    }

    pub(crate) fn gutter_annotation_click_callback(&self) -> Option<AnnotationClickCallback> {
        self.on_gutter_annotation_click.clone()
    }

    /// Called after the pointer rests over text for [`HOVER_DELAY_MS`].
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
            // Keep carets out of hidden rows.
            if folded {
                let hidden: Vec<bool> = self
                    .selections
                    .clone()
                    .iter()
                    .map(|s| self.display_map_hides(s.head.row))
                    .collect();
                if hidden.iter().any(|h| *h) {
                    self.selections = vec![Selection::cursor(Pos::new(row, 0))];
                    self.primary = 0;
                    cx.emit(CodeViewEvent::SelectionChanged);
                }
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
        let row = self.selection().head.row;
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

    /// Wrap counts for the current text, reused across edits.
    pub(crate) fn wrap_counts(
        &mut self,
        cols: usize,
        tab: usize,
        line_count: u32,
        version: u64,
    ) -> Rc<std::cell::RefCell<Vec<Option<u32>>>> {
        let reusable = self.wrap_counts.as_ref().is_some_and(|w| {
            w.cols == cols
                && w.tab == tab
                && w.version == version
                && w.counts.borrow().len() == line_count as usize
        });
        if !reusable {
            self.wrap_counts = Some(WrapCounts {
                cols,
                tab,
                version,
                counts: Rc::new(std::cell::RefCell::new(vec![None; line_count as usize])),
            });
        }
        self.wrap_counts
            .as_ref()
            .map(|w| w.counts.clone())
            .unwrap_or_default()
    }

    /// Moves cached wrap counts through an edit (rows inside it are
    /// re-measured at the next layout).
    fn splice_wrap_counts(&mut self, deltas: &[lector_buffer::EditDelta], new_version: u64) {
        let Some(w) = &mut self.wrap_counts else {
            return;
        };
        {
            let mut counts = w.counts.borrow_mut();
            for d in deltas {
                let start = d.start.row as usize;
                let old_end = (d.old_end.row as usize + 1).min(counts.len());
                if start > counts.len() {
                    continue;
                }
                let inserted = (d.new_end.row - d.start.row + 1) as usize;
                counts.splice(
                    start..old_end.max(start),
                    std::iter::repeat_n(None, inserted),
                );
            }
        }
        w.version = new_version;
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

    // Movement (every caret).

    /// Applies `f` to every selection: it returns the new head (and goal);
    /// with `extend` the anchor stays, otherwise the selection collapses.
    fn move_selections(
        &mut self,
        extend: bool,
        cx: &mut Context<Self>,
        f: impl Fn(&Self, &Selection) -> (Pos, Option<usize>),
    ) {
        let moved: Vec<Selection> = self
            .selections
            .iter()
            .map(|sel| {
                let (head, goal) = f(self, sel);
                Selection {
                    anchor: if extend { sel.anchor } else { head },
                    head,
                    goal_column: goal,
                }
            })
            .collect();
        let (sels, primary) = selection::normalize(moved, self.primary);
        self.selections = sels;
        self.primary = primary;
        self.selections_changed(true, cx);
    }

    fn move_head(&mut self, extend: bool, cx: &mut Context<Self>, f: impl Fn(&Self, Pos) -> Pos) {
        self.move_selections(extend, cx, |this, sel| (f(this, sel.head), None));
    }

    fn move_vertical(&mut self, delta: i64, extend: bool, cx: &mut Context<Self>) {
        let tab = Self::tab_size(cx);
        self.move_selections(extend, cx, |this, sel| {
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
                this.source.as_ref(),
                &this.display_map,
                from,
                sel.goal_column,
                delta,
                tab,
            );
            (head, Some(goal))
        });
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
        self.move_selections(false, cx, |this, sel| {
            if sel.is_empty() {
                (selection::left(this.source.as_ref(), sel.head), None)
            } else {
                (sel.range().0, None)
            }
        });
    }
    fn move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(false, cx, |this, sel| {
            if sel.is_empty() {
                (selection::right(this.source.as_ref(), sel.head), None)
            } else {
                (sel.range().1, None)
            }
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
        let sel = self.selection();
        self.set_selection(
            Selection {
                anchor: sel.anchor,
                head: Pos::new(0, 0),
                goal_column: None,
            },
            cx,
        );
    }
    fn select_to_bottom(&mut self, _: &SelectToBottom, _: &mut Window, cx: &mut Context<Self>) {
        let end = self.end_pos();
        let sel = self.selection();
        self.set_selection(
            Selection {
                anchor: sel.anchor,
                head: end,
                goal_column: None,
            },
            cx,
        );
    }

    /// Selects the whole text.
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        let end = self.end_pos();
        self.selections = vec![Selection {
            anchor: Pos::new(0, 0),
            head: end,
            goal_column: None,
        }];
        self.primary = 0;
        self.selections_changed(false, cx);
    }

    fn select_all_action(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_all(cx);
    }

    fn clear_selection(&mut self, _: &ClearSelection, _: &mut Window, cx: &mut Context<Self>) {
        if self.selections.len() > 1 {
            let keep = self.selection();
            self.selections = vec![keep];
            self.primary = 0;
            self.selections_changed(true, cx);
        } else if !self.selection().is_empty() {
            let head = self.selection().head;
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

    /// Selected text of the primary selection (empty if it is empty).
    pub fn selected_text(&self) -> String {
        selection::selected_text(self.source.as_ref(), &self.selection())
    }

    /// Text of every selection (empty selections give the empty string).
    pub fn selected_texts(&self) -> Vec<String> {
        self.selections
            .iter()
            .map(|s| selection::selected_text(self.source.as_ref(), s))
            .collect()
    }

    /// Text copied for the current selections: selected text joined by
    /// newlines; empty selections copy their whole line.
    fn copy_text(&mut self, cx: &mut Context<Self>) -> String {
        if let Some(buffer) = self.buffer.clone() {
            let sels = self.selections.clone();
            let primary = self.primary;
            return buffer.update(cx, |b, cx| {
                let text = b.text().clone();
                let bsels = edit::to_buffer_selections(&text, &sels, primary);
                b.update_buffer(
                    |buf| {
                        buf.set_selections(bsels);
                        buf.copy()
                    },
                    cx,
                )
            });
        }
        if self.selections.iter().all(|s| s.is_empty()) {
            let mut rows: Vec<u32> = self.selections.iter().map(|s| s.head.row).collect();
            rows.dedup();
            return rows
                .iter()
                .map(|row| format!("{}\n", self.source.line(*row)))
                .collect();
        }
        self.selected_texts()
            .into_iter()
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Copies the selections (or the current lines when empty) to the clipboard.
    pub fn copy(&mut self, cx: &mut Context<Self>) {
        let text = self.copy_text(cx);
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
        let head = self.selection().head;
        if self.display_map_hides(head.row) {
            let start = ranges
                .iter()
                .filter(|r| r.start < head.row && head.row < r.end)
                .map(|r| r.start)
                .min()
                .unwrap_or(0);
            self.selections = vec![Selection::cursor(Pos::new(start, 0))];
            self.primary = 0;
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
        let row = self.selection().head.row;
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

    // Editing.

    /// Runs a mutating buffer operation with this view's selections, then
    /// takes the buffer's resulting selections. `None` when the view is not
    /// editable.
    pub fn edit<R>(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Buffer) -> (R, Transaction),
    ) -> Option<R> {
        if self.read_only {
            return None;
        }
        let buffer = self.buffer.clone()?;
        let origin = cx.entity_id();
        let sels = self.selections.clone();
        let primary = self.primary;
        let started = std::time::Instant::now();
        let (result, new_sels, changed, rope) = buffer.update(cx, |b, cx| {
            let text = b.text().clone();
            let before = text.version();
            let bsels = edit::to_buffer_selections(&text, &sels, primary);
            let r = b.edit(Some(origin), Some(bsels), f, cx)?;
            let rope = b.text().clone();
            let changed = rope.version() != before;
            let sels = edit::from_buffer_selections(&rope, b.buffer().selections());
            Some((r, sels, changed, rope))
        })?;
        if changed && self.manage_source {
            let name = self
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string());
            self.source = Rc::new(RopeSource::new(rope, name.as_deref()));
            self.display_map_key = None;
            self.fold_ranges_version = None;
        }
        let (sels, primary) = selection::normalize(new_sels.0, new_sels.1);
        self.selections = sels;
        self.primary = primary;
        if changed {
            self.drag = None;
            cx.emit(CodeViewEvent::Edited);
        }
        self.selections_changed(true, cx);
        tracing::trace!(target: "lector_ui::perf", elapsed_us = started.elapsed().as_micros() as u64, "code view edit");
        Some(result)
    }

    /// [`CodeView::edit`] for operations returning only a transaction.
    pub fn edit_buffer(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Buffer) -> Transaction,
    ) -> bool {
        self.edit(cx, |b| ((), f(b))).is_some()
    }

    /// Runs a selection-only buffer operation (works while read-only).
    fn select_with(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Buffer)) -> bool {
        let Some(buffer) = self.buffer.clone() else {
            return false;
        };
        let sels = self.selections.clone();
        let primary = self.primary;
        let new_sels = buffer.update(cx, |b, cx| {
            b.catch_up(cx);
            let text = b.text().clone();
            let bsels = edit::to_buffer_selections(&text, &sels, primary);
            b.update_buffer(
                |buf| {
                    buf.set_selections(bsels);
                    f(buf);
                    edit::from_buffer_selections(&text, buf.selections())
                },
                cx,
            )
        });
        let (sels, primary) = selection::normalize(new_sels.0, new_sels.1);
        self.selections = sels;
        self.primary = primary;
        self.selections_changed(true, cx);
        true
    }

    /// Another view (or the owner) changed the shared buffer: map carets,
    /// folds and search matches through the edit.
    fn buffer_edited(
        &mut self,
        origin: Option<gpui::EntityId>,
        transaction: &Transaction,
        buffer: &Entity<EditBuffer>,
        cx: &mut Context<Self>,
    ) {
        let deltas = &transaction.deltas;
        let version = buffer.read(cx).text().version();
        self.splice_wrap_counts(deltas, version);
        if !self.folded.is_empty() {
            let folded: BTreeSet<u32> = self
                .folded
                .iter()
                .filter(|row| {
                    // Drop folds whose header row was replaced.
                    !deltas.iter().any(|d| {
                        d.start.row < **row && **row <= d.old_end.row
                            || (d.start.row == **row && d.old_end.row > **row)
                    })
                })
                .map(|row| edit::map_row_through(*row, deltas))
                .collect();
            self.folded = folded;
            self.fold_generation += 1;
            self.display_map_key = None;
        }
        if !self.search_matches.is_empty() {
            let edited: Vec<(u32, u32)> = deltas
                .iter()
                .map(|d| (d.start.row, d.new_end.row))
                .collect();
            self.search_matches = self
                .search_matches
                .drain(..)
                .filter_map(|(row, range)| {
                    let start = edit::map_pos_through(Pos::new(row, range.start), deltas);
                    let end = edit::map_pos_through(Pos::new(row, range.end), deltas);
                    let touched = edited
                        .iter()
                        .any(|(s, e)| start.row >= *s && start.row <= *e);
                    (!touched && start.row == end.row).then_some((start.row, start.col..end.col))
                })
                .collect();
            self.active_match = None;
        }
        if origin == Some(cx.entity_id()) {
            return;
        }
        let mapped: Vec<Selection> = self
            .selections
            .iter()
            .map(|s| Selection {
                anchor: edit::map_pos_through(s.anchor, deltas),
                head: edit::map_pos_through(s.head, deltas),
                goal_column: s.goal_column,
            })
            .collect();
        let (sels, primary) = selection::normalize(mapped, self.primary);
        self.selections = sels;
        self.primary = primary;
        self.marked = None;
        if self.manage_source {
            let rope = buffer.read(cx).text().clone();
            self.source = Rc::new(RopeSource::new(rope, None));
            self.display_map_key = None;
        }
        cx.emit(CodeViewEvent::SelectionChanged);
        cx.notify();
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.undo()) {
            cx.propagate();
        }
    }
    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.redo()) {
            cx.propagate();
        }
    }
    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_read_only() {
            // Read-only: behave like copy.
            self.copy(cx);
            return;
        }
        if let Some(text) = self.edit(cx, |b| b.cut()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }
    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else {
            return;
        };
        if !self.edit_buffer(cx, |b| b.paste(&text)) {
            cx.propagate();
        }
    }
    fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.newline()) {
            cx.propagate();
        }
    }
    fn newline_below(&mut self, _: &NewlineBelow, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.newline_below()) {
            cx.propagate();
        }
    }
    fn newline_above(&mut self, _: &NewlineAbove, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.newline_above()) {
            cx.propagate();
        }
    }
    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.backspace()) {
            cx.propagate();
        }
    }
    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.delete()) {
            cx.propagate();
        }
    }
    fn delete_word_left(&mut self, _: &DeleteWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.delete_word_left()) {
            cx.propagate();
        }
    }
    fn delete_word_right(&mut self, _: &DeleteWordRight, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.delete_word_right()) {
            cx.propagate();
        }
    }
    fn delete_to_line_start(
        &mut self,
        _: &DeleteToLineStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.edit_buffer(cx, |b| b.delete_to_line_start()) {
            cx.propagate();
        }
    }
    fn delete_line(&mut self, _: &DeleteLine, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.delete_line()) {
            cx.propagate();
        }
    }
    fn tab(&mut self, _: &Tab, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.tab()) {
            cx.propagate();
        }
    }
    fn indent(&mut self, _: &Indent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.indent()) {
            cx.propagate();
        }
    }
    fn outdent(&mut self, _: &Outdent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.outdent()) {
            cx.propagate();
        }
    }
    fn toggle_comment(&mut self, _: &ToggleComment, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.toggle_comment()) {
            cx.propagate();
        }
    }
    fn duplicate_line(&mut self, _: &DuplicateLine, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.duplicate_line()) {
            cx.propagate();
        }
    }
    fn move_line_up(&mut self, _: &MoveLineUp, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.move_lines(true)) {
            cx.propagate();
        }
    }
    fn move_line_down(&mut self, _: &MoveLineDown, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.move_lines(false)) {
            cx.propagate();
        }
    }

    // Selection operations (buffer-backed when available).

    fn select_line(&mut self, _: &SelectLine, _: &mut Window, cx: &mut Context<Self>) {
        if self.select_with(cx, |b| b.select_line()) {
            return;
        }
        let moved: Vec<Selection> = self
            .selections
            .iter()
            .map(|s| {
                let (start, end) = s.range();
                let (a, _) = self.line_range(start.row);
                let end_row = if end.col == 0 && end.row > start.row && !s.is_empty() {
                    end.row
                } else {
                    end.row + 1
                };
                let b = if end_row < self.source.line_count() {
                    Pos::new(end_row, 0)
                } else {
                    self.end_pos()
                };
                Selection {
                    anchor: a,
                    head: b,
                    goal_column: None,
                }
            })
            .collect();
        self.set_selections_keep_primary(moved, cx);
    }

    fn set_selections_keep_primary(&mut self, sels: Vec<Selection>, cx: &mut Context<Self>) {
        let (sels, primary) = selection::normalize(sels, self.primary);
        self.selections = sels;
        self.primary = primary;
        self.selections_changed(true, cx);
    }

    fn select_next_occurrence(
        &mut self,
        _: &SelectNextOccurrence,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.select_with(cx, |b| {
            b.select_next_occurrence();
        }) {
            return;
        }
        self.select_occurrences_fallback(false, cx);
    }

    fn select_all_occurrences(
        &mut self,
        _: &SelectAllOccurrences,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.select_with(cx, |b| b.select_all_occurrences()) {
            return;
        }
        self.select_occurrences_fallback(true, cx);
    }

    /// Occurrence selection for views without a buffer (diffs, large files).
    fn select_occurrences_fallback(&mut self, all: bool, cx: &mut Context<Self>) {
        let primary = self.selection();
        if primary.is_empty() {
            if let Some((s, e, _)) = selection::word_at(self.source.as_ref(), primary.head) {
                self.set_selection(
                    Selection {
                        anchor: s,
                        head: e,
                        goal_column: None,
                    },
                    cx,
                );
            }
            if !all {
                return;
            }
        }
        let needle = self.selected_text();
        if needle.is_empty() || needle.contains('\n') {
            return;
        }
        let mut found: Vec<Selection> = Vec::new();
        let n = self.source.line_count();
        for row in 0..n {
            let line = self.source.line(row);
            for (ix, _) in line.match_indices(needle.as_str()) {
                found.push(Selection {
                    anchor: Pos::new(row, ix),
                    head: Pos::new(row, ix + needle.len()),
                    goal_column: None,
                });
            }
            if found.len() > 10_000 {
                break;
            }
        }
        if all {
            if !found.is_empty() {
                self.set_selections(found, cx);
            }
            return;
        }
        let last_end = self
            .selections
            .iter()
            .map(|s| s.range().1)
            .max()
            .unwrap_or_default();
        let next = found
            .iter()
            .find(|f| f.anchor >= last_end)
            .or_else(|| found.first())
            .copied();
        if let Some(next) = next {
            let mut sels = self.selections.clone();
            sels.push(next);
            let primary = sels.len() - 1;
            let (sels, primary) = selection::normalize(sels, primary);
            self.selections = sels;
            self.primary = primary;
            self.selections_changed(true, cx);
        }
    }

    fn add_cursor_vertical(&mut self, delta: i64, cx: &mut Context<Self>) {
        // Add a caret above the topmost / below the bottommost caret, keeping
        // the display column (soft wrap and folds aware).
        let tab = Self::tab_size(cx);
        let edge = if delta < 0 {
            self.selections.first().copied()
        } else {
            self.selections.last().copied()
        };
        let Some(edge) = edge else { return };
        let (head, goal) = selection::vertical(
            self.source.as_ref(),
            &self.display_map,
            edge.head,
            edge.goal_column,
            delta,
            tab,
        );
        if head.row == edge.head.row {
            return;
        }
        let mut sels = self.selections.clone();
        sels.push(Selection {
            anchor: head,
            head,
            goal_column: Some(goal),
        });
        let primary = sels.len() - 1;
        let (sels, primary) = selection::normalize(sels, primary);
        self.selections = sels;
        self.primary = primary;
        self.selections_changed(true, cx);
    }

    fn add_cursor_above(&mut self, _: &AddCursorAbove, _: &mut Window, cx: &mut Context<Self>) {
        self.add_cursor_vertical(-1, cx);
    }
    fn add_cursor_below(&mut self, _: &AddCursorBelow, _: &mut Window, cx: &mut Context<Self>) {
        self.add_cursor_vertical(1, cx);
    }

    fn split_selection_into_lines(
        &mut self,
        _: &SplitSelectionIntoLines,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.select_with(cx, |b| b.split_selection_into_lines()) {
            return;
        }
        let mut out = Vec::new();
        for s in &self.selections {
            let (start, end) = s.range();
            if start.row == end.row {
                out.push(*s);
                continue;
            }
            for row in start.row..=end.row {
                let len = self.source.line(row).len();
                let a = if row == start.row { start.col } else { 0 };
                let b = if row == end.row { end.col } else { len };
                if row == end.row && b == 0 {
                    continue;
                }
                out.push(Selection {
                    anchor: Pos::new(row, a),
                    head: Pos::new(row, b),
                    goal_column: None,
                });
            }
        }
        if !out.is_empty() {
            self.set_selections(out, cx);
        }
    }

    /// Shift-F10 / the Menu key: the context menu at the caret (below its line).
    fn show_context_menu(
        &mut self,
        _: &crate::actions::workspace::ShowContextMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let head = self.selection().head;
        let below = self.last_layout.as_ref().map(|l| l.line_height);
        let anchor = self
            .point_for_pos(head)
            .map(|p| gpui::point(p.x, p.y + below.unwrap_or(px(20.))))
            .unwrap_or_else(|| gpui::point(px(120.), px(120.)));
        self.deploy_context_menu(anchor, window, cx);
    }

    fn toggle_read_only(&mut self, _: &ToggleReadOnly, _: &mut Window, cx: &mut Context<Self>) {
        if self.buffer.is_none() {
            cx.propagate();
            return;
        }
        let ro = !self.read_only;
        self.set_read_only(ro, cx);
    }

    // Caret blink.

    fn reset_blink(&mut self) {
        self.cursor_visible = true;
        self.blink_epoch = self.blink_epoch.wrapping_add(1);
        self.blink_task = None;
    }

    /// Whether the caret blinks: `cursor_blink` is on and motion is not reduced
    /// (`ui.reduce_motion`, or the OS preference).
    fn blink_enabled(cx: &App) -> bool {
        crate::settings::AppSettings::get(cx).cursor_blink && !crate::a11y::reduce_motion(cx)
    }

    /// Whether a blink timer is running (tests).
    pub fn is_blinking(&self) -> bool {
        self.blink_task.is_some()
    }

    fn ensure_blink(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !Self::blink_enabled(cx) {
            // Reduced motion (or blink off): a steady caret, and stop a timer that is running.
            self.blink_task = None;
            self.cursor_visible = true;
            return;
        }
        if self.blink_task.is_some() || !self.focus_handle.is_focused(window) {
            return;
        }
        let epoch = self.blink_epoch;
        self.blink_task = Some(cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(BLINK_INTERVAL_MS))
                .await;
            let go_on = this
                .update_in(cx, |v, window, cx| {
                    if v.blink_epoch != epoch
                        || !v.focus_handle.is_focused(window)
                        || !Self::blink_enabled(cx)
                    {
                        v.cursor_visible = true;
                        if v.blink_epoch == epoch {
                            v.blink_task = None;
                        }
                        cx.notify();
                        return false;
                    }
                    v.cursor_visible = !v.cursor_visible;
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !go_on {
                break;
            }
        }));
    }

    // Mouse-driven selection (called from the element).

    pub(crate) fn begin_selection(
        &mut self,
        pos: Pos,
        click_count: usize,
        extend: bool,
        add: bool,
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
            let primary = self.selection();
            self.selections = vec![Selection {
                anchor: primary.anchor,
                head: pos,
                goal_column: None,
            }];
            self.primary = 0;
            self.drag = Some(DragState {
                mode,
                origin: (primary.anchor, primary.anchor),
            });
        } else {
            let new = Selection {
                anchor: range.0,
                head: range.1,
                goal_column: None,
            };
            if add {
                if click_count == 1 && self.selections.len() > 1 {
                    // Alt-click on an existing caret removes it.
                    if let Some(ix) = self
                        .selections
                        .iter()
                        .position(|s| s.is_empty() && s.head == pos)
                    {
                        self.selections.remove(ix);
                        self.primary = self.selections.len() - 1;
                        self.drag = None;
                        self.selections_changed(false, cx);
                        return;
                    }
                }
                self.selections.push(new);
                let primary = self.selections.len() - 1;
                let (sels, primary) =
                    selection::normalize(std::mem::take(&mut self.selections), primary);
                self.selections = sels;
                self.primary = primary;
            } else {
                self.selections = vec![new];
                self.primary = 0;
            }
            self.drag = Some(DragState {
                mode,
                origin: range,
            });
        }
        self.selections_changed(false, cx);
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
        if self.selections.get(self.primary) != Some(&new) {
            if let Some(slot) = self.selections.get_mut(self.primary) {
                *slot = new;
            }
            // Merge with carets the drag ran over (keeps the list valid).
            let (sels, primary) =
                selection::normalize(std::mem::take(&mut self.selections), self.primary);
            self.selections = sels;
            self.primary = primary;
            self.reset_blink();
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
        cx.emit(CodeViewEvent::HoverMoved(pos));
        let (Some(pos), Some(callback)) = (pos, self.on_hover.clone()) else {
            return;
        };
        self.hover_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(HOVER_DELAY_MS))
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
        let has_selection = self.selections.iter().any(|s| !s.is_empty());
        let path = self.path.clone();
        let has_buffer = self.buffer.is_some();
        let editable = self.is_editable();
        let read_only = self.read_only;
        let (can_undo, can_redo) = self
            .buffer
            .as_ref()
            .map(|b| {
                let b = b.read(cx).buffer();
                (b.can_undo(), b.can_redo())
            })
            .unwrap_or((false, false));
        let features = path
            .as_deref()
            .map(|p| LanguageFeatureProvider::features(p, cx))
            .unwrap_or_default();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let item = |menu: ContextMenu,
                        enabled: bool,
                        label: &'static str,
                        icon: Option<IconName>,
                        action: Box<dyn gpui::Action>| {
                match (enabled, icon) {
                    (true, Some(icon)) => menu.icon_action(label, icon, action),
                    (true, None) => menu.action(label, action),
                    (false, _) => menu.disabled_action(label, action),
                }
            };
            let mut menu = menu.context(focus);
            if has_buffer {
                menu = item(menu, editable && can_undo, "Undo", None, Box::new(Undo));
                menu = item(menu, editable && can_redo, "Redo", None, Box::new(Redo));
                menu = menu.separator();
                menu = item(menu, editable, "Cut", None, Box::new(Cut));
            }
            menu = item(
                menu,
                true,
                if has_selection { "Copy" } else { "Copy Line" },
                Some(IconName::Copy),
                Box::new(Copy),
            );
            if has_buffer {
                menu = item(menu, editable, "Paste", None, Box::new(Paste));
            }
            menu = menu.action("Select All", Box::new(SelectAll));
            if has_buffer {
                menu = menu.separator();
                menu = item(
                    menu,
                    editable,
                    "Toggle Comment",
                    None,
                    Box::new(ToggleComment),
                );
                menu = item(
                    menu,
                    editable && features.format,
                    "Format Document",
                    None,
                    Box::new(Format),
                );
                menu = item(
                    menu,
                    editable && features.rename,
                    "Rename Symbol…",
                    None,
                    Box::new(Rename),
                );
                menu = item(
                    menu,
                    editable && features.code_actions,
                    "Code Actions…",
                    Some(IconName::Lightbulb),
                    Box::new(CodeActions),
                );
            }
            menu = menu
                .separator()
                .action("Go to Definition", Box::new(GoToDefinition))
                .action("Find References", Box::new(FindReferences));
            if path.is_some() {
                menu = menu
                    .separator()
                    .icon_action("Copy Path", IconName::Link, Box::new(CopyPath))
                    .action("Copy Relative Path", Box::new(CopyRelativePath))
                    .icon_action(
                        "Copy Permalink",
                        IconName::ExternalLink,
                        Box::new(CopyPermalink),
                    )
                    .separator()
                    .action("Toggle Blame", Box::new(ToggleBlame))
                    .icon_action("File History", IconName::History, Box::new(ShowFileHistory))
                    .action(
                        "Reveal in Files",
                        Box::new(crate::actions::workspace::RevealActiveFile),
                    );
            }
            if has_buffer {
                menu = menu.separator().icon_action(
                    if read_only {
                        "Allow Editing"
                    } else {
                        "Make Read-Only"
                    },
                    if read_only {
                        IconName::LockOpen
                    } else {
                        IconName::Lock
                    },
                    Box::new(ToggleReadOnly),
                );
            }
            menu
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

    // IME helpers.

    fn buffer_text(&self, cx: &App) -> Option<Rope> {
        self.buffer.as_ref().map(|b| b.read(cx).text().clone())
    }

    /// Replaces `range` (bytes) with `text` as an external edit (no
    /// auto-close), leaving the caret at `caret` (bytes, absolute) or after
    /// the inserted text.
    fn replace_range_raw(
        &mut self,
        range: Range<usize>,
        text: &str,
        caret: Option<Range<usize>>,
        cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let text_owned = text.to_string();
        let inserted = self.edit(cx, move |b| {
            let tx = b.apply_edits(vec![lector_buffer::TextEdit {
                range: range.clone(),
                new_text: text_owned.clone(),
            }]);
            let end = range.start + text_owned.len();
            let caret = caret.unwrap_or(end..end);
            let id = b.selections().iter().map(|s| s.id).max().unwrap_or(0) + 1;
            b.set_selections(vec![lector_buffer::Selection {
                id,
                anchor: caret.start,
                head: caret.end,
                goal: None,
            }]);
            (range.start..end, tx)
        })?;
        Some(inserted)
    }

    fn end_ime_transaction(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.ime_transaction) {
            if let Some(buffer) = &self.buffer {
                buffer.update(cx, |b, cx| b.update_buffer(|b| b.end_transaction(), cx));
            }
        }
    }
}

impl EntityInputHandler for CodeView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.buffer_text(cx)?;
        let range = edit::range_from_utf16(&text, &range_utf16);
        adjusted_range.replace(edit::range_to_utf16(&text, &range));
        Some(text.text_for_range(range))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let text = self.buffer_text(cx)?;
        let sel = self.selection();
        let (start, end) = sel.range();
        let range = edit::pos_to_offset(&text, start)..edit::pos_to_offset(&text, end);
        Some(UTF16Selection {
            range: edit::range_to_utf16(&text, &range),
            reversed: sel.head < sel.anchor,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let marked = self.marked.clone()?;
        let text = self.buffer_text(cx)?;
        Some(edit::range_to_utf16(&text, &marked))
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        self.end_ime_transaction(cx);
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_editable() {
            return;
        }
        let Some(rope) = self.buffer_text(cx) else {
            return;
        };
        let explicit = range_utf16.map(|r| edit::range_from_utf16(&rope, &r));
        match (explicit, self.marked.clone()) {
            (Some(range), _) | (None, Some(range)) => {
                // Committing a composition, or the platform replacing a range
                // (accent menu, character palette): one exact replacement.
                self.replace_range_raw(range, text, None, cx);
            }
            (None, None) => {
                // Plain typing: every caret, with auto-close/type-over.
                let text = text.to_string();
                self.edit_buffer(cx, move |b| b.insert(&text));
            }
        }
        self.marked = None;
        self.end_ime_transaction(cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_editable() {
            return;
        }
        let Some(rope) = self.buffer_text(cx) else {
            return;
        };
        let range = range_utf16
            .map(|r| edit::range_from_utf16(&rope, &r))
            .or(self.marked.clone())
            .unwrap_or_else(|| {
                let (s, e) = self.selection().range();
                edit::pos_to_offset(&rope, s)..edit::pos_to_offset(&rope, e)
            });
        if !self.ime_transaction {
            if let Some(buffer) = &self.buffer {
                buffer.update(cx, |b, cx| b.update_buffer(|b| b.begin_transaction(), cx));
                self.ime_transaction = true;
            }
        }
        let start = range.start;
        let caret = new_selected_range_utf16.map(|sel| {
            // Relative to the new text, in UTF-16.
            let to_bytes = |u: usize| {
                let mut units = 0;
                let mut bytes = 0;
                for c in new_text.chars() {
                    if units >= u {
                        break;
                    }
                    units += c.len_utf16();
                    bytes += c.len_utf8();
                }
                bytes
            };
            start + to_bytes(sel.start)..start + to_bytes(sel.end)
        });
        let inserted = self.replace_range_raw(range, new_text, caret, cx);
        self.marked = inserted.filter(|r| !r.is_empty());
        if self.marked.is_none() {
            self.end_ime_transaction(cx);
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let text = self.buffer_text(cx)?;
        let range = edit::range_from_utf16(&text, &range_utf16);
        let start = edit::offset_to_pos(&text, range.start);
        let origin = self.point_for_pos(start)?;
        let line_height = self.line_height()?;
        let end = edit::offset_to_pos(&text, range.end);
        let width = match self.point_for_pos(end) {
            Some(p) if end.row == start.row && p.x > origin.x => p.x - origin.x,
            _ => px(1.),
        };
        Some(Bounds::new(origin, gpui::size(width, line_height)))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let pos = self.pos_for_point(point)?;
        let text = self.buffer_text(cx)?;
        Some(edit::offset_to_utf16(
            &text,
            edit::pos_to_offset(&text, pos),
        ))
    }
}

impl Render for CodeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_blink(window, cx);
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
            .on_action(cx.listener(Self::select_line))
            .on_action(cx.listener(Self::select_next_occurrence))
            .on_action(cx.listener(Self::select_all_occurrences))
            .on_action(cx.listener(Self::add_cursor_above))
            .on_action(cx.listener(Self::add_cursor_below))
            .on_action(cx.listener(Self::split_selection_into_lines))
            .on_action(cx.listener(Self::show_context_menu))
            .when(self.buffer.is_some(), |el| {
                el.on_action(cx.listener(Self::undo))
                    .on_action(cx.listener(Self::redo))
                    .on_action(cx.listener(Self::cut))
                    .on_action(cx.listener(Self::paste))
                    .on_action(cx.listener(Self::newline))
                    .on_action(cx.listener(Self::newline_below))
                    .on_action(cx.listener(Self::newline_above))
                    .on_action(cx.listener(Self::backspace))
                    .on_action(cx.listener(Self::delete))
                    .on_action(cx.listener(Self::delete_word_left))
                    .on_action(cx.listener(Self::delete_word_right))
                    .on_action(cx.listener(Self::delete_to_line_start))
                    .on_action(cx.listener(Self::delete_line))
                    .on_action(cx.listener(Self::tab))
                    .on_action(cx.listener(Self::indent))
                    .on_action(cx.listener(Self::outdent))
                    .on_action(cx.listener(Self::toggle_comment))
                    .on_action(cx.listener(Self::duplicate_line))
                    .on_action(cx.listener(Self::move_line_up))
                    .on_action(cx.listener(Self::move_line_down))
                    .on_action(cx.listener(Self::toggle_read_only))
            })
            .child(CodeElement::new(cx.entity()))
            .children(self.context_menu.render())
            .children(self.render_overlay())
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
