//! File tabs backed by `lector_workspace::Document`: the code view with
//! breadcrumbs, banners, hover popovers, and image / binary previews.

pub mod image_view;
pub mod source;

use std::cell::Cell;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use futures::channel::mpsc;
use futures::StreamExt;
use gpui::{
    anchored, deferred, div, prelude::*, px, AnyElement, AnyView, App, Corner, Entity,
    EventEmitter, FocusHandle, Focusable, FontWeight, Pixels, Point, SharedString, Subscription,
    Task, WeakEntity, Window,
};
use lector_fs::{FileKind, ImageFormat};
use lector_syntax::{OutlineItem, SymbolKind};
use lector_workspace::{Document, Indentation, LineEnding, Project};

pub use source::{DocumentSource, SourceState, HIGHLIGHT_CHUNK};

use crate::actions::editor::{ClearSelection, GoToDefinition, Hover};
use crate::code_view::{CodeView, CodeViewEvent, HoverTarget, InMemorySource, Pos, Selection};
use crate::components::{h_flex, metrics, v_flex};
use crate::icons::{FileIcon, Icon, IconName, IconSize};
use crate::markdown::{self, Block, RenderOptions};
use crate::settings::AppSettings;
use crate::theme::{ActiveTheme, ColorExt};
use crate::views::pane::{PaneItem, TabIcon};
use image_view::{ImageInfo, ImageView};

/// What a file tab currently shows.
pub enum Content {
    /// Loading in the background.
    Loading,
    /// Text in the code view.
    Text,
    /// An image preview (SVG can toggle to its source).
    Image(Entity<ImageView>),
    /// A binary file panel.
    Binary,
    /// Could not be read.
    Error(SharedString),
}

/// A location to reveal once the document is loaded.
#[derive(Clone, Debug, PartialEq)]
pub struct Reveal {
    /// Row (0-based).
    pub row: u32,
    /// Byte column or selected byte range within the row.
    pub cols: Range<usize>,
    /// Center the row vertically.
    pub center: bool,
}

/// Events emitted by [`FileView`].
#[derive(Clone, Debug, PartialEq)]
pub enum FileViewEvent {
    /// The document (re)loaded.
    Loaded,
    /// The cursor moved (breadcrumbs / outline follow it).
    CursorMoved,
}

struct HoverPopover {
    word: Range<usize>,
    blocks: Arc<Vec<Block>>,
    position: Point<Pixels>,
    hovered: bool,
}

/// Status-bar facts about the document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocumentMeta {
    /// "UTF-8", ...
    pub encoding: SharedString,
    /// Decoding replaced invalid bytes.
    pub lossy: bool,
    /// "LF" / "CRLF" / "Mixed".
    pub line_ending: SharedString,
    /// "Spaces: 4" / "Tabs".
    pub indentation: SharedString,
    /// Memory-mapped large file (no syntax).
    pub large: bool,
}

/// A file tab: code view (with breadcrumbs) or image / binary preview.
pub struct FileView {
    path: PathBuf,
    rel_path: SharedString,
    project: Project,
    focus_handle: FocusHandle,
    code_view: Entity<CodeView>,
    is_text: Rc<Cell<bool>>,
    content: Content,
    doc: Option<Document>,
    state: Rc<SourceState>,
    pending_reveal: Option<Reveal>,
    hover: Option<HoverPopover>,
    crumbs: Option<(u32, Vec<OutlineItem>)>,
    show_svg_source: bool,
    highlight_task: Option<Task<()>>,
    load_task: Option<Task<()>>,
    gutter_task: Option<Task<()>>,
    hover_task: Option<Task<()>>,
    reload_pending: bool,
    focus_pending: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<FileViewEvent> for FileView {}

impl Focusable for FileView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.content {
            Content::Image(view) if !self.show_svg_source => view.focus_handle(cx),
            Content::Binary | Content::Error(_) => self.focus_handle.clone(),
            _ => self.code_view.focus_handle(cx),
        }
    }
}

fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .and_then(ImageFormat::from_extension)
        .is_some()
}

impl FileView {
    /// Opens `path` (absolute) from `project`, loading in the background.
    pub fn new(path: PathBuf, project: Project, cx: &mut Context<Self>) -> Self {
        let rel_path: SharedString = project
            .relative_path(&path)
            .unwrap_or_else(|| path.display().to_string())
            .into();
        let code_view = cx.new(|cx| {
            let mut v = CodeView::new(Rc::new(InMemorySource::new("")), cx);
            v.set_path(Some(path.clone()));
            v
        });
        let this = cx.entity().downgrade();
        code_view.update(cx, |v, _| {
            let weak = this.clone();
            v.on_hover(move |target, window, cx| {
                // Called while the code view is being updated: defer.
                let target = target.clone();
                let weak = weak.clone();
                window.defer(cx, move |window, cx| {
                    let _ = weak.update(cx, |this, cx| this.show_hover(target, window, cx));
                });
            });
        });
        let cv = code_view.downgrade();
        code_view.update(cx, |v, _| {
            v.on_cmd_click(move |pos, window, cx| {
                let _ = cv.update(cx, |v, cx| v.set_cursor(pos, cx));
                window.dispatch_action(Box::new(GoToDefinition), cx);
            });
        });
        let subscription = cx.subscribe(
            &code_view,
            |this, _, event: &CodeViewEvent, cx| match event {
                CodeViewEvent::SelectionChanged => {
                    cx.emit(FileViewEvent::CursorMoved);
                    cx.notify();
                }
                CodeViewEvent::HoverMoved(pos) => this.hover_moved(*pos, cx),
                CodeViewEvent::Scrolled => {
                    if this.hover.as_ref().is_some_and(|h| !h.hovered) {
                        this.hover = None;
                        cx.notify();
                    }
                }
            },
        );
        let is_text = Rc::new(Cell::new(!is_image_path(&path)));
        let mut view = FileView {
            path,
            rel_path,
            project,
            focus_handle: cx.focus_handle(),
            code_view,
            is_text,
            content: Content::Loading,
            doc: None,
            state: Rc::new(SourceState::default()),
            pending_reveal: None,
            hover: None,
            crumbs: None,
            show_svg_source: false,
            highlight_task: None,
            load_task: None,
            gutter_task: None,
            hover_task: None,
            reload_pending: false,
            focus_pending: false,
            _subscriptions: vec![subscription],
        };
        view.load(false, cx);
        view
    }

    /// Absolute path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Project-relative path (display).
    pub fn rel_path(&self) -> &SharedString {
        &self.rel_path
    }

    /// The project.
    pub fn project(&self) -> &Project {
        &self.project
    }

    /// The code view (also used for SVG source).
    pub fn code_view(&self) -> &Entity<CodeView> {
        &self.code_view
    }

    /// Shared flag: whether the code view is the visible content.
    pub fn is_text_flag(&self) -> Rc<Cell<bool>> {
        self.is_text.clone()
    }

    /// The loaded document.
    pub fn document(&self) -> Option<&Document> {
        self.doc.as_ref()
    }

    /// Whether the content is still loading.
    pub fn is_loading(&self) -> bool {
        matches!(self.content, Content::Loading)
    }

    /// Status-bar facts.
    pub fn meta(&self) -> Option<DocumentMeta> {
        let doc = self.doc.as_ref()?;
        if !self.is_text.get() {
            return None;
        }
        Some(DocumentMeta {
            encoding: doc.encoding().to_string().into(),
            lossy: doc.lossy(),
            line_ending: match doc.line_ending() {
                LineEnding::Lf => "LF",
                LineEnding::CrLf => "CRLF",
                LineEnding::Mixed => "Mixed",
            }
            .into(),
            indentation: match doc.indentation() {
                Indentation::Tabs => "Tabs".into(),
                Indentation::Spaces(n) => format!("Spaces: {n}").into(),
            },
            large: doc.is_large(),
        })
    }

    /// Byte offset of the cursor in the document text.
    pub fn cursor_offset(&self, cx: &App) -> Option<usize> {
        let text = self.doc.as_ref()?.text()?;
        let pos = self.code_view.read(cx).cursor();
        Some(text.point_to_offset(lector_text::Point::new(pos.row, pos.col as u32)))
    }

    /// Moves the cursor / selection to `reveal` (deferred until loaded).
    pub fn reveal(&mut self, reveal: Reveal, cx: &mut Context<Self>) {
        if self.doc.is_none() {
            self.pending_reveal = Some(reveal);
            return;
        }
        self.apply_reveal(reveal, cx);
    }

    fn apply_reveal(&mut self, reveal: Reveal, cx: &mut Context<Self>) {
        self.code_view.update(cx, |v, cx| {
            v.set_selection(
                Selection {
                    anchor: Pos::new(reveal.row, reveal.cols.start),
                    head: Pos::new(reveal.row, reveal.cols.end),
                    goal_column: None,
                },
                cx,
            );
            v.scroll_to_row(reveal.row, reveal.center, cx);
        });
    }

    /// Reloads from disk (file changed), keeping scroll and selection.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.load_task.is_some() {
            self.reload_pending = true;
            return;
        }
        self.load(true, cx);
    }

    fn load(&mut self, reload: bool, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let path = self.path.clone();
        let target_row = if reload {
            self.code_view.read(cx).top_visible_row()
        } else {
            self.pending_reveal.as_ref().map_or(0, |r| r.row)
        };
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let doc = project.load_document(&path)?;
                    // Pre-compute lazily cached data off the main thread.
                    let _ = doc.outline();
                    let _ = doc.fold_ranges();
                    let mut chunks = Vec::new();
                    if doc.syntax().is_some() {
                        let first = target_row.saturating_sub(40) / HIGHLIGHT_CHUNK;
                        let last = (target_row + 120) / HIGHLIGHT_CHUNK;
                        for chunk in first..=last {
                            let rows = chunk * HIGHLIGHT_CHUNK..(chunk + 1) * HIGHLIGHT_CHUNK;
                            if rows.start >= doc.line_count() {
                                break;
                            }
                            chunks.push((chunk, Arc::new(doc.line_highlights(rows))));
                        }
                    }
                    let image = match doc.kind() {
                        FileKind::Image(format) => {
                            Some(ImageInfo::read(&path, *format, doc.size()))
                        }
                        _ => None,
                    };
                    anyhow::Ok((doc, chunks, image))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.load_task = None;
                match loaded {
                    Ok((doc, chunks, image)) => this.loaded(doc, chunks, image, cx),
                    Err(err) => {
                        if !reload {
                            this.is_text.set(false);
                            this.content = Content::Error(err.to_string().into());
                        }
                        cx.notify();
                    }
                }
                if std::mem::take(&mut this.reload_pending) {
                    this.load(true, cx);
                }
            });
        }));
    }

    fn loaded(
        &mut self,
        doc: Document,
        chunks: Vec<(u32, Arc<Vec<lector_workspace::LineHighlights>>)>,
        image: Option<ImageInfo>,
        cx: &mut Context<Self>,
    ) {
        let state = Rc::new(SourceState::default());
        for (chunk, lines) in chunks {
            state.insert_chunk(chunk, lines);
        }
        let (tx, rx) = mpsc::unbounded();
        let source = Rc::new(DocumentSource::new(doc.clone(), state.clone(), Some(tx)));
        self.state = state;
        self.code_view.update(cx, |v, cx| v.set_source(source, cx));
        self.spawn_highlighter(doc.clone(), rx, cx);
        self.crumbs = None;
        self.hover = None;
        match (doc.kind(), image) {
            (FileKind::Image(_), Some(info)) => {
                let svg = doc.text().is_some();
                match &self.content {
                    Content::Image(view) => view.update(cx, |v, cx| v.set_info(info, cx)),
                    _ => {
                        let path = self.path.clone();
                        let view = cx.new(|cx| ImageView::new(path, info, svg, cx));
                        self._subscriptions.push(cx.subscribe(
                            &view,
                            |this, _, _: &image_view::ImageViewEvent, cx| {
                                this.show_svg_source = true;
                                this.is_text.set(true);
                                this.focus_pending = true;
                                cx.notify();
                            },
                        ));
                        self.content = Content::Image(view);
                    }
                }
                self.is_text.set(self.show_svg_source);
            }
            (FileKind::Text, _) => {
                self.content = Content::Text;
                self.is_text.set(true);
            }
            (FileKind::Directory, _) => {
                self.content = Content::Error("This is a folder".into());
                self.is_text.set(false);
            }
            _ => {
                self.content = Content::Binary;
                self.is_text.set(false);
            }
        }
        self.doc = Some(doc);
        if let Some(reveal) = self.pending_reveal.take() {
            self.apply_reveal(reveal, cx);
        }
        self.refresh_gutter(cx);
        cx.emit(FileViewEvent::Loaded);
        cx.notify();
    }

    fn spawn_highlighter(
        &mut self,
        doc: Document,
        mut rx: mpsc::UnboundedReceiver<u32>,
        cx: &mut Context<Self>,
    ) {
        let state = self.state.clone();
        let code_view = self.code_view.downgrade();
        self.highlight_task = Some(cx.spawn(async move |_, cx| {
            while let Some(first) = rx.next().await {
                let mut batch = vec![first];
                while let Ok(chunk) = rx.try_recv() {
                    batch.push(chunk);
                }
                // Newest requests are what is visible now.
                batch.reverse();
                for chunk in batch.drain(batch.len().min(4)..) {
                    state.cancel_chunk(chunk);
                }
                let doc = doc.clone();
                let work = batch.clone();
                let results = cx
                    .background_executor()
                    .spawn(async move {
                        work.into_iter()
                            .map(|chunk| {
                                let rows = chunk * HIGHLIGHT_CHUNK..(chunk + 1) * HIGHLIGHT_CHUNK;
                                (chunk, Arc::new(doc.line_highlights(rows)))
                            })
                            .collect::<Vec<_>>()
                    })
                    .await;
                for (chunk, lines) in results {
                    state.insert_chunk(chunk, lines);
                }
                if code_view.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        }));
    }

    /// Recomputes the diff gutter vs HEAD in the background.
    pub fn refresh_gutter(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.clone() else { return };
        if !AppSettings::get(cx).git.show_gutter || doc.text().is_none() {
            self.state.set_gutter(Vec::new());
            return;
        }
        let project = self.project.clone();
        let state = self.state.clone();
        let code_view = self.code_view.downgrade();
        self.gutter_task = Some(cx.spawn(async move |_, cx| {
            let marks = cx
                .background_executor()
                .spawn(async move { source::convert_gutter(&doc.diff_gutter(&project)) })
                .await;
            state.set_gutter(marks);
            let _ = code_view.update(cx, |_, cx| cx.notify());
        }));
    }

    // Hover.

    fn show_hover(&mut self, target: HoverTarget, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.clone() else { return };
        let Some(text) = doc.text() else { return };
        let offset = text.point_to_offset(lector_text::Point::new(
            target.pos.row,
            target.pos.col as u32,
        ));
        let Some((word, _)) = doc.word_at(offset) else {
            return;
        };
        if self.hover.as_ref().is_some_and(|h| h.word == word) {
            return;
        }
        let project = self.project.clone();
        let word_start = text.offset_to_point(word.start);
        let position = self
            .code_view
            .read(cx)
            .point_for_pos(Pos::new(word_start.row, word_start.column as usize))
            .unwrap_or(target.position);
        self.hover_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let md = lector_workspace::symbols::hover(&project, &doc, offset)
                        .ok()
                        .flatten()?;
                    let md = md.trim();
                    if md.is_empty() {
                        return None;
                    }
                    Some(Arc::new(markdown::parse(md)))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.hover = result.map(|blocks| HoverPopover {
                    word,
                    blocks,
                    position,
                    hovered: false,
                });
                cx.notify();
            });
        }));
    }

    fn hover_moved(&mut self, pos: Option<Pos>, cx: &mut Context<Self>) {
        let Some(hover) = &self.hover else {
            self.hover_task = None;
            return;
        };
        if hover.hovered {
            return;
        }
        let inside = match (pos, self.doc.as_ref().and_then(|d| d.text())) {
            (Some(pos), Some(text)) => {
                let off = text.point_to_offset(lector_text::Point::new(pos.row, pos.col as u32));
                hover.word.start <= off && off <= hover.word.end
            }
            _ => false,
        };
        if !inside {
            self.hover = None;
            self.hover_task = None;
            cx.notify();
        }
    }

    fn hover_action(&mut self, _: &Hover, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.code_view.read(cx);
        let pos = view.cursor();
        let position = view.point_for_pos(pos).unwrap_or_default();
        self.hover = None;
        self.show_hover(HoverTarget { pos, position }, window, cx);
    }

    fn clear_selection(&mut self, _: &ClearSelection, _: &mut Window, cx: &mut Context<Self>) {
        if self.hover.take().is_some() {
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    /// Enclosing symbols at the cursor (outermost first), for breadcrumbs.
    pub fn symbols_at_cursor(&mut self, cx: &App) -> Vec<OutlineItem> {
        let Some(doc) = &self.doc else {
            return Vec::new();
        };
        let Some(text) = doc.text() else {
            return Vec::new();
        };
        let row = self.code_view.read(cx).cursor().row;
        if let Some((r, items)) = &self.crumbs {
            if *r == row {
                return items.clone();
            }
        }
        let items = symbols_containing_row(doc, text, row);
        self.crumbs = Some((row, items.clone()));
        items
    }

    fn jump_to_symbol(&mut self, item: &OutlineItem, cx: &mut Context<Self>) {
        let Some(text) = self.doc.as_ref().and_then(|d| d.text()) else {
            return;
        };
        let p = text.offset_to_point(item.name_range.start);
        let end = text.offset_to_point(item.name_range.end);
        let cols = if end.row == p.row {
            p.column as usize..end.column as usize
        } else {
            p.column as usize..p.column as usize
        };
        self.apply_reveal(
            Reveal {
                row: p.row,
                cols,
                center: true,
            },
            cx,
        );
    }

    fn toggle_svg_source(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_svg_source = !self.show_svg_source;
        self.is_text.set(self.show_svg_source);
        let focus = self.focus_handle(cx);
        window.focus(&focus);
        cx.notify();
    }

    // Rendering.

    fn render_breadcrumbs(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let symbols = self.symbols_at_cursor(cx);
        let sep = || {
            Icon::new(IconName::ChevronRight)
                .size(IconSize::XSmall)
                .color(ui.text_faint.hsla())
        };
        let segments: Vec<SharedString> = self
            .rel_path
            .split(['/', '\\'])
            .filter(|s| !s.is_empty())
            .map(|s| SharedString::from(s.to_string()))
            .collect();
        let n = segments.len();
        let mut row = h_flex()
            .id("breadcrumbs")
            .h(px(26.))
            .flex_none()
            .px(px(12.))
            .gap(px(4.))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(metrics::TEXT_SM)
            .text_color(ui.text_muted.hsla())
            .bg(ui.background.hsla())
            .border_b_1()
            .border_color(ui.border_variant.hsla());
        for (i, seg) in segments.into_iter().enumerate() {
            if i > 0 {
                row = row.child(sep());
            }
            let last = i + 1 == n;
            let icon = if last {
                FileIcon::file(seg.clone()).size(px(14.)).into_any_element()
            } else {
                FileIcon::folder(seg.clone(), false, false)
                    .size(px(14.))
                    .into_any_element()
            };
            row = row.child(
                h_flex()
                    .id(("crumb-path", i))
                    .flex_none()
                    .gap(px(4.))
                    .px(px(3.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(ui.hover.hsla()).text_color(ui.text.hsla()))
                    .when(last && symbols.is_empty(), |el| {
                        el.text_color(ui.text.hsla())
                    })
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(
                            Box::new(crate::actions::workspace::RevealActiveFile),
                            cx,
                        );
                    })
                    .child(icon)
                    .child(seg),
            );
        }
        let count = symbols.len();
        for (i, item) in symbols.into_iter().enumerate() {
            row = row.child(sep());
            let last = i + 1 == count;
            let weak = cx.entity().downgrade();
            let item_for_click = item.clone();
            row = row.child(
                h_flex()
                    .id(("crumb-sym", i))
                    .flex_none()
                    .gap(px(5.))
                    .px(px(3.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(ui.hover.hsla()).text_color(ui.text.hsla()))
                    .when(last, |el| el.text_color(ui.text.hsla()))
                    .on_click(move |_, window, cx| {
                        let item = item_for_click.clone();
                        let _ = weak.update(cx, |this, cx| {
                            this.jump_to_symbol(&item, cx);
                            let focus = this.code_view.focus_handle(cx);
                            window.focus(&focus);
                        });
                    })
                    .child(symbol_badge(item.kind, cx))
                    .child(SharedString::from(item.name.clone())),
            );
        }
        row
    }

    fn render_banner(&self, cx: &App) -> Option<AnyElement> {
        let doc = self.doc.as_ref()?;
        let ui = &cx.theme().ui;
        let config_dir = lector_config::paths::config_dir();
        let message: SharedString = if self.path.starts_with(&config_dir) {
            "Lector never edits files — change this one in your editor; it reloads live.".into()
        } else if doc.is_large() {
            format!(
                "Large file ({}) — syntax highlighting off",
                format_size(doc.size())
            )
            .into()
        } else if doc.lossy() {
            format!(
                "Opened as {} — some bytes could not be decoded and are shown as \u{FFFD}",
                doc.encoding()
            )
            .into()
        } else if doc.language().is_some()
            && doc.syntax().is_none()
            && doc.size() > lector_syntax::MAX_PARSE_BYTES as u64
        {
            "File too large to parse — syntax highlighting off".into()
        } else {
            return None;
        };
        Some(
            h_flex()
                .h(px(26.))
                .flex_none()
                .px(px(12.))
                .gap(px(8.))
                .bg(ui.surface.hsla())
                .border_b_1()
                .border_color(ui.border_variant.hsla())
                .text_size(metrics::TEXT_SM)
                .text_color(ui.text_muted.hsla())
                .child(
                    Icon::new(IconName::Info)
                        .size(IconSize::XSmall)
                        .color(ui.text_muted.hsla()),
                )
                .child(message)
                .into_any_element(),
        )
    }

    fn render_hover(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let hover = self.hover.as_ref()?;
        let ui = cx.theme().ui.clone();
        let opts = RenderOptions {
            base_dir: self.path.parent().map(Path::to_path_buf),
            on_link: Some(Rc::new(
                |target: &str, _window: &mut Window, cx: &mut App| {
                    if target.contains("://") {
                        cx.open_url(target);
                    }
                },
            )),
            compact: true,
            id: "hover-md".into(),
        };
        let elements = markdown::render_blocks(&hover.blocks, &opts, window, cx);
        let weak = cx.entity().downgrade();
        Some(
            deferred(
                anchored()
                    .position(hover.position)
                    .anchor(Corner::BottomLeft)
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        div()
                            .id("hover-popover")
                            .occlude()
                            .mb(px(4.))
                            .max_w(px(620.))
                            .max_h(px(380.))
                            .overflow_y_scroll()
                            .px(px(12.))
                            .pt(px(10.))
                            .pb(px(4.))
                            .bg(ui.elevated.hsla())
                            .border_1()
                            .border_color(ui.border.hsla())
                            .rounded(metrics::RADIUS_LG)
                            .shadow_lg()
                            .text_size(metrics::TEXT)
                            .text_color(ui.text.hsla())
                            .on_hover(move |hovered, _, cx| {
                                let hovered = *hovered;
                                let _ = weak.update(cx, |this, cx| {
                                    if let Some(h) = &mut this.hover {
                                        h.hovered = hovered;
                                        if !hovered {
                                            this.hover = None;
                                        }
                                        cx.notify();
                                    }
                                });
                            })
                            .children(elements),
                    ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    fn render_binary(&self, cx: &mut Context<Self>) -> AnyElement {
        let ui = cx.theme().ui.clone();
        let size = self.doc.as_ref().map_or(0, |d| d.size());
        let path = self.path.clone();
        centered_panel(
            IconName::Binary,
            "Binary file".into(),
            format!("{} · not shown", format_size(size)).into(),
            cx,
        )
        .child(
            div()
                .id("reveal-binary")
                .mt(px(10.))
                .px(px(10.))
                .py(px(4.))
                .rounded(px(5.))
                .border_1()
                .border_color(ui.border.hsla())
                .text_size(metrics::TEXT_SM)
                .cursor_pointer()
                .hover(|s| s.bg(ui.hover.hsla()))
                .on_click(move |_, _, _| crate::os::reveal_in_file_manager(&path))
                .child(crate::os::REVEAL_LABEL),
        )
        .into_any_element()
    }
}

/// Outline items containing `row`, outermost first.
pub fn symbols_containing_row(
    doc: &Document,
    text: &lector_text::Text,
    row: u32,
) -> Vec<OutlineItem> {
    let outline = doc.outline();
    if outline.is_empty() {
        return Vec::new();
    }
    let range = text.line_range(row);
    let line = text.text_for_range(range.clone());
    let lead = line.len() - line.trim_start().len();
    let offset = if lead >= line.len() {
        range.start
    } else {
        range.start + lead
    };
    let mut out: Vec<OutlineItem> = outline
        .iter()
        .filter(|i| i.range.start <= offset && offset < i.range.end.max(i.range.start + 1))
        .cloned()
        .collect();
    out.sort_by_key(|i| (i.range.start, std::cmp::Reverse(i.range.end)));
    out
}

/// Human-readable byte size.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024. && unit + 1 < UNITS.len() {
        value /= 1024.;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A centered icon + title + detail panel (empty / binary / error states).
pub fn centered_panel(
    icon: IconName,
    title: SharedString,
    detail: SharedString,
    cx: &App,
) -> gpui::Div {
    let ui = &cx.theme().ui;
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .child(
            div()
                .mb(px(6.))
                .p(px(14.))
                .rounded(px(12.))
                .bg(ui.surface.hsla())
                .child(
                    Icon::new(icon)
                        .size(IconSize::Custom(px(30.)))
                        .color(ui.text_faint.hsla()),
                ),
        )
        .child(
            div()
                .text_size(px(15.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(ui.text.hsla())
                .child(title),
        )
        .child(
            div()
                .text_size(metrics::TEXT_SM)
                .text_color(ui.text_muted.hsla())
                .child(detail),
        )
}

/// Capture name used to color a symbol kind.
fn kind_capture(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Module => "namespace",
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Type => "type",
        SymbolKind::Enum => "type",
        SymbolKind::Interface | SymbolKind::Trait => "type",
        SymbolKind::Function | SymbolKind::Method => "function",
        SymbolKind::Field => "property",
        SymbolKind::Variable => "variable",
        SymbolKind::Constant => "constant",
        SymbolKind::Macro => "function.macro",
        SymbolKind::Heading => "markup.heading",
        SymbolKind::Other => "comment",
    }
}

fn kind_letter(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Module => "M",
        SymbolKind::Class => "C",
        SymbolKind::Struct => "S",
        SymbolKind::Enum => "E",
        SymbolKind::Interface => "I",
        SymbolKind::Trait => "T",
        SymbolKind::Function => "ƒ",
        SymbolKind::Method => "m",
        SymbolKind::Field => "F",
        SymbolKind::Variable => "v",
        SymbolKind::Constant => "K",
        SymbolKind::Type => "T",
        SymbolKind::Macro => "!",
        SymbolKind::Heading => "#",
        SymbolKind::Other => "{}",
    }
}

/// A small colored badge for a symbol kind (theme syntax colors).
pub fn symbol_badge(kind: SymbolKind, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let color = theme
        .syntax
        .style_for(kind_capture(kind))
        .map(|s| s.color.hsla())
        .unwrap_or(theme.ui.text_muted.hsla());
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(15.))
        .rounded(px(3.))
        .bg(color.opacity(0.16))
        .text_color(color)
        .text_size(px(10.))
        .font_weight(FontWeight::BOLD)
        .child(kind_letter(kind))
}

impl Render for FileView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let svg_toggle = self.show_svg_source
            && matches!(&self.content, Content::Image(v) if v.read(cx).is_svg());
        if std::mem::take(&mut self.focus_pending) {
            let focus = self.focus_handle(cx);
            window.focus(&focus);
        }
        let content: AnyElement = match &self.content {
            Content::Loading => div()
                .size_full()
                .bg(ui.background.hsla())
                .into_any_element(),
            Content::Text => self.code_view.clone().into_any_element(),
            Content::Image(view) => {
                if self.show_svg_source {
                    self.code_view.clone().into_any_element()
                } else {
                    view.clone().into_any_element()
                }
            }
            Content::Binary => self.render_binary(cx),
            Content::Error(message) => centered_panel(
                IconName::AlertTriangle,
                "Cannot open file".into(),
                message.clone(),
                cx,
            )
            .into_any_element(),
        };
        let show_crumbs = self.is_text.get() && self.doc.is_some();
        let weak = cx.entity().downgrade();
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::hover_action))
            .on_action(cx.listener(Self::clear_selection))
            .when(show_crumbs, |el| el.child(self.render_breadcrumbs(cx)))
            .when(svg_toggle, |el| {
                let showing_source = self.show_svg_source;
                el.child(
                    h_flex()
                        .h(px(30.))
                        .flex_none()
                        .px(px(10.))
                        .gap(px(4.))
                        .border_b_1()
                        .border_color(ui.border_variant.hsla())
                        .child(
                            crate::components::Button::new("svg-preview", "Preview")
                                .compact()
                                .icon(IconName::Image)
                                .selected(!showing_source)
                                .on_click({
                                    let weak = weak.clone();
                                    move |_, window, cx| {
                                        let _ = weak.update(cx, |this, cx| {
                                            if this.show_svg_source {
                                                this.toggle_svg_source(window, cx)
                                            }
                                        });
                                    }
                                }),
                        )
                        .child(
                            crate::components::Button::new("svg-source", "Source")
                                .compact()
                                .icon(IconName::Code)
                                .selected(showing_source)
                                .on_click({
                                    let weak = weak.clone();
                                    move |_, window, cx| {
                                        let _ = weak.update(cx, |this, cx| {
                                            if !this.show_svg_source {
                                                this.toggle_svg_source(window, cx)
                                            }
                                        });
                                    }
                                }),
                        ),
                )
            })
            .when_some(
                self.is_text.get().then(|| self.render_banner(cx)).flatten(),
                |el, banner| el.child(banner),
            )
            .child(div().flex_1().min_h_0().relative().child(content))
            .children(self.render_hover(window, cx))
    }
}

/// The pane item for a [`FileView`].
pub struct FileItem {
    /// The view.
    pub view: Entity<FileView>,
    code_view: Entity<CodeView>,
    is_text: Rc<Cell<bool>>,
    path: PathBuf,
    title: SharedString,
}

impl FileItem {
    /// Item for `view`.
    pub fn new(view: Entity<FileView>, cx: &App) -> Self {
        let v = view.read(cx);
        let path = v.path().to_path_buf();
        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .into();
        FileItem {
            code_view: v.code_view().clone(),
            is_text: v.is_text_flag(),
            view,
            path,
            title,
        }
    }

    /// Weak handle to the view.
    pub fn weak_view(&self) -> WeakEntity<FileView> {
        self.view.downgrade()
    }
}

impl PaneItem for FileItem {
    fn title(&self, _: &App) -> SharedString {
        self.title.clone()
    }

    fn icon(&self, _: &App) -> TabIcon {
        TabIcon::File(self.title.clone())
    }

    fn path(&self, _: &App) -> Option<PathBuf> {
        Some(self.path.clone())
    }

    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }

    fn code_view(&self) -> Option<Entity<CodeView>> {
        self.is_text.get().then(|| self.code_view.clone())
    }

    fn file_view(&self) -> Option<Entity<FileView>> {
        Some(self.view.clone())
    }

    fn clone_item(&self, _window: &mut Window, cx: &mut App) -> Option<Box<dyn PaneItem>> {
        let project = self.view.read(cx).project().clone();
        let path = self.path.clone();
        let selection = self.code_view.read(cx).selection();
        let view = cx.new(|cx| {
            let mut v = FileView::new(path, project, cx);
            v.reveal(
                Reveal {
                    row: selection.head.row,
                    cols: selection.head.col..selection.head.col,
                    center: true,
                },
                cx,
            );
            v
        });
        Some(Box::new(FileItem::new(view, cx)))
    }
}
