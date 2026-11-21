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
mod editing;
mod element;
mod ime;
pub mod layers;
mod line_cache;
mod mouse;
mod movement;
pub mod popup;
pub mod selection;
pub mod source;

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    div, prelude::*, px, App, Entity, EventEmitter, FocusHandle, Focusable, Global, Hsla, Pixels,
    Point, SharedString, Subscription, Task, Window,
};
use lector_buffer::Buffer;
use lector_text::Rope;

pub use display_map::DisplayMap;
pub use edit::{EditBuffer, EditBufferEvent, RopeSource};
pub use element::CodeElement;
pub use layers::{RowBackground, RowWidget, WidgetAnchor};
pub use popup::{place_popup, CommandHook, EditorPopup, Placement, Vertical};
pub use selection::{Pos, Selection};
pub use source::{
    CodeSource, DecorationColor, GutterMark, GutterMarkKind, HighlightSpan, InMemorySource,
    LexicalHighlighter, RowDecoration, SpanStyle,
};

use crate::components::context_menu::ContextMenuState;
use crate::icons::IconName;
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

/// A boxed action listener (see [`CodeView::command`]).
type ActionListener<A> = Box<dyn Fn(&A, &mut Window, &mut App) + 'static>;

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
    /// Someone tried to make a view allow editing whose read-only state is locked
    /// ([`CodeView::set_read_only_locked`]): the owner explains why it stays read-only.
    ReadOnlyLocked,
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
    /// A restriction that `set_read_only(false)` and the toggle cannot lift
    /// ([`CodeView::set_read_only_locked`]); the view stays read-only whatever `read_only` says.
    read_only_locked: bool,
    marked: Option<Range<usize>>,
    ime_transaction: bool,
    pub(crate) cursor_visible: bool,
    blink_epoch: u64,
    blink_task: Option<Task<()>>,
    _buffer_subscription: Option<Subscription>,
    inline_marks: Rc<InlineMarks>,
    on_gutter_icon_click: Option<GutterIconCallback>,
    overlay: Option<(gpui::AnyView, Point<Pixels>, gpui::Corner, Subscription)>,
    /// Non-focusing popups (completion, signature help), key context flags, typing hooks.
    popups: popup::PopupState,
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
            read_only_locked: false,
            marked: None,
            ime_transaction: false,
            cursor_visible: true,
            blink_epoch: 0,
            blink_task: None,
            _buffer_subscription: None,
            inline_marks: Rc::new(InlineMarks::default()),
            on_gutter_icon_click: None,
            overlay: None,
            popups: popup::PopupState::default(),
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
        self.read_only || self.read_only_locked || self.buffer.is_none()
    }

    /// Whether the read-only state is locked (the toggle only explains).
    pub fn is_read_only_locked(&self) -> bool {
        self.read_only_locked
    }

    /// Locks (or unlocks) the view read-only. While locked, [`CodeView::is_read_only`] is true
    /// whatever [`CodeView::set_read_only`] was given, so no reload, setting change or
    /// document event can make the view editable; the owner is the only one to lift it.
    pub fn set_read_only_locked(&mut self, locked: bool, cx: &mut Context<Self>) {
        if self.read_only_locked != locked {
            let before = self.is_read_only();
            self.read_only_locked = locked;
            self.marked = None;
            if before != self.is_read_only() {
                cx.emit(CodeViewEvent::ReadOnlyChanged);
            }
            cx.notify();
        }
    }

    /// Flips read-only (the toggle command, the status bar). A locked view stays read-only
    /// and emits [`CodeViewEvent::ReadOnlyLocked`] instead. Does nothing without a buffer.
    pub fn toggle_read_only(&mut self, cx: &mut Context<Self>) {
        if self.buffer.is_none() {
            return;
        }
        if self.read_only_locked {
            cx.emit(CodeViewEvent::ReadOnlyLocked);
            return;
        }
        let ro = !self.read_only;
        self.set_read_only(ro, cx);
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

    /// Text of `row` (for tests and integration).
    pub fn line_text(&self, row: u32) -> SharedString {
        self.source.line_text(row)
    }
}

impl Render for CodeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_blink(window, cx);
        self.ensure_popup_hooks(window, cx);
        let popups = self.render_popups(window, cx);
        div()
            .id("code-view")
            .key_context(self.key_context())
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_hidden()
            .on_action(Self::command(cx, Self::move_up))
            .on_action(Self::command(cx, Self::move_down))
            .on_action(Self::command(cx, Self::move_left))
            .on_action(Self::command(cx, Self::move_right))
            .on_action(Self::command(cx, Self::select_up))
            .on_action(Self::command(cx, Self::select_down))
            .on_action(Self::command(cx, Self::select_left))
            .on_action(Self::command(cx, Self::select_right))
            .on_action(Self::command(cx, Self::page_up))
            .on_action(Self::command(cx, Self::page_down))
            .on_action(Self::command(cx, Self::select_page_up))
            .on_action(Self::command(cx, Self::select_page_down))
            .on_action(Self::command(cx, Self::move_word_left))
            .on_action(Self::command(cx, Self::move_word_right))
            .on_action(Self::command(cx, Self::select_word_left))
            .on_action(Self::command(cx, Self::select_word_right))
            .on_action(Self::command(cx, Self::move_to_line_start))
            .on_action(Self::command(cx, Self::move_to_line_end))
            .on_action(Self::command(cx, Self::select_to_line_start))
            .on_action(Self::command(cx, Self::select_to_line_end))
            .on_action(Self::command(cx, Self::scroll_to_top))
            .on_action(Self::command(cx, Self::scroll_to_bottom))
            .on_action(Self::command(cx, Self::select_to_top))
            .on_action(Self::command(cx, Self::select_to_bottom))
            .on_action(Self::command(cx, Self::select_all_action))
            .on_action(Self::command(cx, Self::clear_selection))
            .on_action(Self::command(cx, Self::copy_action))
            .on_action(Self::command(cx, Self::toggle_soft_wrap))
            .on_action(Self::command(cx, Self::toggle_minimap))
            .on_action(Self::command(cx, Self::toggle_fold))
            .on_action(Self::command(cx, Self::fold_all))
            .on_action(Self::command(cx, Self::unfold_all))
            .on_action(Self::command(cx, Self::find_next))
            .on_action(Self::command(cx, Self::find_prev))
            .on_action(Self::command(cx, Self::next_change))
            .on_action(Self::command(cx, Self::prev_change))
            .on_action(Self::command(cx, Self::select_line))
            .on_action(Self::command(cx, Self::select_next_occurrence))
            .on_action(Self::command(cx, Self::select_all_occurrences))
            .on_action(Self::command(cx, Self::add_cursor_above))
            .on_action(Self::command(cx, Self::add_cursor_below))
            .on_action(Self::command(cx, Self::split_selection_into_lines))
            .on_action(Self::command(cx, Self::show_context_menu))
            .when(self.buffer.is_some(), |el| {
                el.on_action(Self::command(cx, Self::undo))
                    .on_action(Self::command(cx, Self::redo))
                    .on_action(Self::command(cx, Self::cut))
                    .on_action(Self::command(cx, Self::paste))
                    .on_action(Self::command(cx, Self::newline))
                    .on_action(Self::command(cx, Self::newline_below))
                    .on_action(Self::command(cx, Self::newline_above))
                    .on_action(Self::command(cx, Self::backspace))
                    .on_action(Self::command(cx, Self::delete))
                    .on_action(Self::command(cx, Self::delete_word_left))
                    .on_action(Self::command(cx, Self::delete_word_right))
                    .on_action(Self::command(cx, Self::delete_to_line_start))
                    .on_action(Self::command(cx, Self::delete_line))
                    .on_action(Self::command(cx, Self::tab))
                    .on_action(Self::command(cx, Self::indent))
                    .on_action(Self::command(cx, Self::outdent))
                    .on_action(Self::command(cx, Self::toggle_comment))
                    .on_action(Self::command(cx, Self::duplicate_line))
                    .on_action(Self::command(cx, Self::move_line_up))
                    .on_action(Self::command(cx, Self::move_line_down))
                    .on_action(Self::command(cx, Self::toggle_read_only_action))
            })
            .child(CodeElement::new(cx.entity()))
            .children(self.context_menu.render())
            .children(self.render_overlay())
            .children(popups)
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
