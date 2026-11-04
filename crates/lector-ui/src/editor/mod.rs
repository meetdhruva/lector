//! File tabs backed by a shared [`DocumentModel`]: the (editable) code view
//! with breadcrumbs, banners, hover popovers, and image / binary previews.
//!
//! Editing seams (see `docs/ARCHITECTURE.md`, "Editing"):
//! * one [`DocumentModel`] per open file, shared by all of its tabs, owns the
//!   [`crate::code_view::EditBuffer`]; each tab's [`CodeView`] edits it with
//!   its own carets;
//! * [`register_before_save`] adds hooks run before every save (format on
//!   save);
//! * external edits (LSP formatting, rename, code actions) go through
//!   [`DocumentModel::apply_edits`] (one undo step, carets mapped).

pub mod compare;
pub mod image_view;
pub mod model;
pub mod source;

use std::cell::Cell;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    anchored, deferred, div, prelude::*, px, AnyElement, AnyView, App, Corner, Entity,
    EventEmitter, FocusHandle, Focusable, FontWeight, Pixels, Point, PromptLevel, SharedString,
    Subscription, Task, WeakEntity, Window,
};
use lector_fs::{FileKind, ImageFormat};
use lector_syntax::{OutlineItem, SymbolKind};
use lector_workspace::{Document, Indentation, LineEnding, Project};

pub use model::{
    document_for_path, open_documents, register_before_save, BeforeSaveHook, DocumentEvent,
    DocumentModel, SaveOutcome,
};
pub use source::{DocumentSource, SourceState, HIGHLIGHT_CHUNK};

use crate::actions::editor::{ClearSelection, GoToDefinition, Hover, RevertFile, Save, SaveAs};
use crate::code_view::{CodeView, CodeViewEvent, HoverTarget, InMemorySource, Pos, Selection};
use crate::components::{h_flex, metrics, v_flex, Button, ButtonStyle};
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
    /// Unsaved-changes or read-only state changed (tab, title, status bar).
    StateChanged,
    /// The settled outline changed (outline panel).
    OutlineChanged,
    /// The file was saved under another path.
    PathChanged,
    /// The "file deleted on disk" banner asked to close this tab.
    CloseRequested,
    /// Save As replaces the file at this path: the tabs of other views
    /// showing it must close first.
    CloseTabsOf(PathBuf),
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
    /// "UTF-8", "UTF-8 with BOM", ...
    pub encoding: SharedString,
    /// Decoding replaced invalid bytes.
    pub lossy: bool,
    /// "LF" / "CRLF" / "Mixed".
    pub line_ending: SharedString,
    /// "Spaces: 4" / "Tabs".
    pub indentation: SharedString,
    /// Memory-mapped large file (no syntax).
    pub large: bool,
    /// Unsaved changes.
    pub dirty: bool,
    /// Editing is blocked in this tab.
    pub read_only: bool,
    /// The file type can be edited at all (read-only can be toggled).
    pub editable: bool,
}

/// A file tab: code view (with breadcrumbs) or image / binary preview.
pub struct FileView {
    path: PathBuf,
    rel_path: SharedString,
    project: Project,
    model: Entity<DocumentModel>,
    focus_handle: FocusHandle,
    code_view: Entity<CodeView>,
    is_text: Rc<Cell<bool>>,
    content: Content,
    pending_reveal: Option<Reveal>,
    hover: Option<HoverPopover>,
    crumbs: Option<(u32, u64, Vec<OutlineItem>)>,
    show_svg_source: bool,
    hover_task: Option<Task<()>>,
    image_task: Option<Task<()>>,
    save_task: Option<Task<()>>,
    focus_pending: bool,
    setup_done: bool,
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
    /// Opens `path` (absolute) from `project`, loading in the background (or
    /// sharing the document of another tab showing the same file).
    pub fn new(path: PathBuf, project: Project, cx: &mut Context<Self>) -> Self {
        let rel_path: SharedString = project
            .relative_path(&path)
            .unwrap_or_else(|| path.display().to_string())
            .into();
        let model = DocumentModel::open(path.clone(), project.clone(), 0, cx);
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
        let code_view_sub =
            cx.subscribe(
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
                    CodeViewEvent::Edited => {
                        if this.hover.take().is_some() {
                            cx.notify();
                        }
                    }
                    CodeViewEvent::ReadOnlyChanged => {
                        cx.emit(FileViewEvent::StateChanged);
                        cx.notify();
                    }
                },
            );
        let model_sub = cx.subscribe(&model, |this, _, event: &DocumentEvent, cx| {
            this.model_event(event, cx)
        });
        let is_text = Rc::new(Cell::new(!is_image_path(&path)));
        let mut view = FileView {
            path,
            rel_path,
            project,
            model,
            focus_handle: cx.focus_handle(),
            code_view,
            is_text,
            content: Content::Loading,
            pending_reveal: None,
            hover: None,
            crumbs: None,
            show_svg_source: false,
            hover_task: None,
            image_task: None,
            save_task: None,
            focus_pending: false,
            setup_done: false,
            _subscriptions: vec![code_view_sub, model_sub],
        };
        if view.model.read(cx).is_loaded() {
            view.setup_from_model(cx);
        }
        view
    }

    fn model_event(&mut self, event: &DocumentEvent, cx: &mut Context<Self>) {
        match event {
            DocumentEvent::Loaded => {
                self.setup_from_model(cx);
            }
            DocumentEvent::SourceChanged => {
                if let Some(source) = self.model.read(cx).source() {
                    self.code_view.update(cx, |v, cx| v.set_source(source, cx));
                }
                cx.notify();
            }
            DocumentEvent::Repaint => {
                self.code_view.update(cx, |_, cx| cx.notify());
            }
            DocumentEvent::Settled => {
                self.crumbs = None;
                cx.emit(FileViewEvent::OutlineChanged);
                cx.notify();
            }
            DocumentEvent::DirtyChanged | DocumentEvent::Saved | DocumentEvent::ConflictChanged => {
                cx.emit(FileViewEvent::StateChanged);
                cx.notify();
            }
            DocumentEvent::PathChanged => {
                let path = self.model.read(cx).path().to_path_buf();
                self.rel_path = self
                    .project
                    .relative_path(&path)
                    .unwrap_or_else(|| path.display().to_string())
                    .into();
                self.code_view
                    .update(cx, |v, _| v.set_path(Some(path.clone())));
                self.path = path;
                cx.emit(FileViewEvent::PathChanged);
                cx.emit(FileViewEvent::StateChanged);
                cx.notify();
            }
        }
    }

    /// Connects the code view to the loaded model and picks the content.
    fn setup_from_model(&mut self, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        if let Some(err) = model.load_error().cloned() {
            if !self.setup_done {
                self.is_text.set(false);
                self.content = Content::Error(err);
            }
            cx.notify();
            return;
        }
        let Some(doc) = model.live_document().cloned() else {
            return;
        };
        let source = model.source();
        let buffer = model.buffer().cloned();
        let editable = model.is_editable();
        let read_only = AppSettings::get(cx).editor.read_only || !editable;
        let first_setup = !self.setup_done;
        self.code_view.update(cx, |v, cx| {
            if let Some(source) = source {
                v.set_source(source, cx);
            }
            v.set_buffer(buffer, false, cx);
            if first_setup || !editable {
                v.set_read_only(read_only, cx);
            }
        });
        self.crumbs = None;
        self.hover = None;
        match doc.kind() {
            FileKind::Image(format) => {
                let svg = doc.text().is_some();
                let path = self.path.clone();
                let format = *format;
                let size = doc.size();
                self.image_task = Some(cx.spawn(async move |this, cx| {
                    let info = cx
                        .background_executor()
                        .spawn({
                            let path = path.clone();
                            async move { ImageInfo::read(&path, format, size) }
                        })
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        match &this.content {
                            Content::Image(view) => view.update(cx, |v, cx| v.set_info(info, cx)),
                            _ => {
                                let view = cx.new(|cx| ImageView::new(path, info, svg, cx));
                                this._subscriptions.push(cx.subscribe(
                                    &view,
                                    |this, _, _: &image_view::ImageViewEvent, cx| {
                                        this.show_svg_source = true;
                                        this.is_text.set(true);
                                        this.focus_pending = true;
                                        cx.notify();
                                    },
                                ));
                                this.content = Content::Image(view);
                            }
                        }
                        this.is_text.set(this.show_svg_source);
                        cx.notify();
                    });
                }));
            }
            FileKind::Text => {
                self.content = Content::Text;
                self.is_text.set(true);
            }
            FileKind::Directory => {
                self.content = Content::Error("This is a folder".into());
                self.is_text.set(false);
            }
            _ => {
                self.content = Content::Binary;
                self.is_text.set(false);
            }
        }
        self.setup_done = true;
        if let Some(reveal) = self.pending_reveal.take() {
            self.apply_reveal(reveal, cx);
        }
        cx.emit(FileViewEvent::Loaded);
        cx.notify();
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

    /// The shared document model.
    pub fn model(&self) -> &Entity<DocumentModel> {
        &self.model
    }

    /// The code view (also used for SVG source).
    pub fn code_view(&self) -> &Entity<CodeView> {
        &self.code_view
    }

    /// Shared flag: whether the code view is the visible content.
    pub fn is_text_flag(&self) -> Rc<Cell<bool>> {
        self.is_text.clone()
    }

    /// The settled document: outline and folds are computed; while typing
    /// it may lag the text by a few hundred milliseconds. Use
    /// [`FileView::live_document`] for anything that needs exact offsets.
    pub fn document<'a>(&self, cx: &'a App) -> Option<&'a Document> {
        self.model.read(cx).settled_document()
    }

    /// The latest snapshot (text always matches the buffer).
    pub fn live_document<'a>(&self, cx: &'a App) -> Option<&'a Document> {
        self.model.read(cx).live_document()
    }

    /// Whether the content is still loading.
    pub fn is_loading(&self) -> bool {
        matches!(self.content, Content::Loading)
    }

    /// Unsaved changes.
    pub fn is_dirty(&self, cx: &App) -> bool {
        self.model.read(cx).is_dirty(cx)
    }

    /// Whether editing is blocked in this tab.
    pub fn is_read_only(&self, cx: &App) -> bool {
        self.code_view.read(cx).is_read_only()
    }

    /// Status-bar facts.
    pub fn meta(&self, cx: &App) -> Option<DocumentMeta> {
        let model = self.model.read(cx);
        let doc = model.live_document()?;
        if !self.is_text.get() {
            return None;
        }
        let buffer_indent = model.buffer().map(|b| b.read(cx).buffer().options().indent);
        let indentation: SharedString = match (buffer_indent, doc.indentation()) {
            (Some(lector_buffer::IndentStyle::Tabs), _) | (None, Indentation::Tabs) => {
                "Tabs".into()
            }
            (Some(lector_buffer::IndentStyle::Spaces(n)), _) | (None, Indentation::Spaces(n)) => {
                format!("Spaces: {n}").into()
            }
        };
        let encoding: SharedString = match model.save_encoding() {
            lector_buffer::save::Encoding::Utf8Bom if model.is_editable() => {
                "UTF-8 with BOM".into()
            }
            _ => doc.encoding().to_string().into(),
        };
        Some(DocumentMeta {
            encoding,
            lossy: doc.lossy(),
            line_ending: match doc.line_ending() {
                LineEnding::Lf => "LF",
                LineEnding::CrLf => "CRLF",
                LineEnding::Mixed => "Mixed",
            }
            .into(),
            indentation,
            large: doc.is_large(),
            dirty: model.is_dirty(cx),
            read_only: self.code_view.read(cx).is_read_only(),
            editable: model.is_editable(),
        })
    }

    /// Byte offset of the cursor in the live document text.
    pub fn cursor_offset(&self, cx: &App) -> Option<usize> {
        let text = self.live_document(cx)?.text()?;
        let pos = self.code_view.read(cx).cursor();
        Some(text.point_to_offset(lector_text::Point::new(pos.row, pos.col as u32)))
    }

    /// Moves the cursor / selection to `reveal` (deferred until loaded).
    pub fn reveal(&mut self, reveal: Reveal, cx: &mut Context<Self>) {
        if !self.setup_done {
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

    /// The file changed on disk: reload a clean buffer in place (keeping
    /// carets), or show the "changed on disk" banner for a dirty one.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| m.file_changed_on_disk(cx));
    }

    /// Recomputes the diff gutter vs HEAD in the background.
    pub fn refresh_gutter(&mut self, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| m.refresh_gutter(cx));
    }

    /// Saves (prompting Overwrite / Reload / Cancel when the file changed on
    /// disk). Resolves to `true` when the buffer is saved (or was clean).
    pub fn save(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<bool>> {
        let model = self.model.clone();
        if !model.read(cx).is_editable() {
            return Task::ready(Ok(true));
        }
        let name = self.file_name();
        let save = model.update(cx, |m, cx| m.save(false, cx));
        cx.spawn_in(window, async move |_, cx| {
            match save.await? {
                SaveOutcome::Saved | SaveOutcome::Unchanged => Ok(true),
                SaveOutcome::Conflict => {
                    let answer = cx.update(|window, cx| {
                        window.prompt(
                            PromptLevel::Warning,
                            &format!("“{name}” changed on disk since it was opened."),
                            Some("Overwrite it with your version, reload it (discarding your changes), or cancel."),
                            &["Overwrite", "Reload", "Cancel"],
                            cx,
                        )
                    })?;
                    match answer.await {
                        Ok(0) => {
                            let force = model.update(cx, |m, cx| m.save(true, cx))?;
                            Ok(force.await? == SaveOutcome::Saved)
                        }
                        Ok(1) => {
                            model.update(cx, |m, cx| m.revert(cx))?;
                            Ok(false)
                        }
                        _ => Ok(false),
                    }
                }
            }
        })
    }

    fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    fn save_action(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        if !self.model.read(cx).is_editable() {
            cx.propagate();
            return;
        }
        let task = self.save(window, cx);
        self.save_task = Some(cx.spawn(async move |_, _| {
            if let Err(err) = task.await {
                tracing::warn!("save failed: {err:#}");
            }
        }));
    }

    /// Prompts for a new path and saves there; the tab follows the new file.
    pub fn save_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.model.read(cx).is_editable() {
            return;
        }
        let dir = self
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.project.root().to_path_buf());
        let name = self.file_name();
        let receiver = cx.prompt_for_new_path(&dir, Some(&name));
        let model = self.model.clone();
        self.save_task = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = receiver.await else {
                return;
            };
            // The target is open in another tab: replace it only after asking,
            // and never while that tab has unsaved changes.
            let other = cx
                .update(|_, cx| document_for_path(&path, cx))
                .ok()
                .flatten()
                .filter(|other| *other != model);
            if let Some(other) = other {
                let Ok(dirty) = cx.update(|_, cx| other.read(cx).is_dirty(cx)) else {
                    return;
                };
                let target = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if dirty {
                    let Ok(answer) = cx.update(|window, cx| {
                        window.prompt(
                            PromptLevel::Warning,
                            &format!("“{target}” has unsaved changes in another tab."),
                            Some("Save or close that tab first, then use Save As again."),
                            &["OK"],
                            cx,
                        )
                    }) else {
                        return;
                    };
                    let _ = answer.await;
                    return;
                }
                let Ok(answer) = cx.update(|window, cx| {
                    window.prompt(
                        PromptLevel::Warning,
                        &format!("“{target}” is already open. Replace it?"),
                        Some("The tab showing it closes and the file is overwritten."),
                        &["Replace", "Cancel"],
                        cx,
                    )
                }) else {
                    return;
                };
                if answer.await != Ok(0) {
                    return;
                }
                let closed = this.update(cx, |_, cx| {
                    cx.emit(FileViewEvent::CloseTabsOf(path.clone()))
                });
                if closed.is_err() {
                    return;
                }
            }
            let Ok(task) = model.update(cx, |m, cx| m.save_as(path, cx)) else {
                return;
            };
            if let Err(err) = task.await {
                tracing::warn!("save as failed: {err:#}");
            }
        }));
    }

    fn save_as_action(&mut self, _: &SaveAs, window: &mut Window, cx: &mut Context<Self>) {
        if !self.model.read(cx).is_editable() {
            cx.propagate();
            return;
        }
        self.save_as(window, cx);
    }

    /// Asks for confirmation (when dirty) and reloads from disk.
    pub fn revert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        if !model.read(cx).is_dirty(cx) {
            model.update(cx, |m, cx| m.revert(cx));
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Revert “{}”?", self.file_name()),
            Some("Your unsaved changes will be lost. The file is reloaded from disk."),
            &["Revert", "Cancel"],
            cx,
        );
        self.save_task = Some(cx.spawn(async move |_, cx| {
            if answer.await == Ok(0) {
                let _ = model.update(cx, |m, cx| m.revert(cx));
            }
        }));
    }

    fn revert_action(&mut self, _: &RevertFile, window: &mut Window, cx: &mut Context<Self>) {
        if !self.model.read(cx).is_editable() {
            cx.propagate();
            return;
        }
        self.revert(window, cx);
    }

    /// Opens a comparison of the file on disk with the buffer.
    fn compare_with_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let Some(conflict) = model.conflict() else {
            return;
        };
        let disk = conflict
            .disk
            .text()
            .map(|t| t.to_string())
            .unwrap_or_default();
        let mine = model
            .buffer()
            .map(|b| b.read(cx).text().to_string())
            .unwrap_or_default();
        let name = self.file_name();
        let item = compare::CompareItem::new(
            format!("{name} (disk ↔ yours)"),
            Some(self.path.clone()),
            &disk,
            &mine,
            cx,
        );
        compare::open_compare_item(item, window, cx);
    }

    // Hover.

    fn show_hover(&mut self, target: HoverTarget, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(doc) = self.live_document(cx).cloned() else {
            return;
        };
        let Some(text) = doc.text() else { return };
        let offset = text.point_to_offset(lector_text::Point::new(
            target.pos.row,
            target.pos.col as u32,
        ));
        // LSP diagnostics at the pointer are shown above the hover docs.
        let diagnostics = crate::lsp::diagnostics::hover_markdown(&self.path, offset, cx);
        let word = match doc.word_at(offset) {
            Some((word, _)) => word,
            None if diagnostics.is_some() => offset..offset + 1,
            None => return,
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
                        .flatten()
                        .map(|md| md.trim().to_string())
                        .filter(|md| !md.is_empty());
                    let md = match (diagnostics, md) {
                        (Some(d), Some(md)) => format!("{d}\n\n---\n\n{md}"),
                        (Some(d), None) => d,
                        (None, Some(md)) => md,
                        (None, None) => return None,
                    };
                    Some(Arc::new(markdown::parse(&md)))
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
        let model = self.model.read(cx);
        let inside = match (pos, model.live_document().and_then(|d| d.text())) {
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
    /// Uses the settled snapshot (debounced while typing).
    pub fn symbols_at_cursor(&mut self, cx: &App) -> Vec<OutlineItem> {
        let Some(doc) = self.model.read(cx).settled_document().cloned() else {
            return Vec::new();
        };
        let Some(text) = doc.text() else {
            return Vec::new();
        };
        let version = text.version();
        let row = self.code_view.read(cx).cursor().row;
        if let Some((r, v, items)) = &self.crumbs {
            if *r == row && *v == version {
                return items.clone();
            }
        }
        let items =
            symbols_containing_row(&doc, text, row.min(text.line_count().saturating_sub(1)));
        self.crumbs = Some((row, version, items.clone()));
        items
    }

    fn jump_to_symbol(&mut self, item: &OutlineItem, cx: &mut Context<Self>) {
        let Some(text) = self
            .model
            .read(cx)
            .settled_document()
            .and_then(|d| d.text())
            .cloned()
        else {
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

    /// Whether the "deleted on disk" banner is up: unsaved changes of a file
    /// that is gone (a clean tab of a deleted file only gets a struck-through
    /// title).
    pub fn shows_deleted_banner(&self, cx: &App) -> bool {
        let model = self.model.read(cx);
        model.is_deleted() && model.is_dirty(cx) && model.conflict().is_none()
    }

    /// The banner of a dirty buffer whose file was deleted on disk: Save
    /// writes it again, Close closes the tab (asking about the changes).
    fn render_deleted_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.shows_deleted_banner(cx) {
            return None;
        }
        let ui = cx.theme().ui.clone();
        let warn = cx.theme().git.modified.hsla();
        let weak = cx.entity().downgrade();
        Some(
            h_flex()
                .id("file-deleted-banner")
                .h(px(32.))
                .flex_none()
                .px(px(12.))
                .gap(px(8.))
                .bg(warn.opacity(0.12))
                .border_b_1()
                .border_color(warn.opacity(0.45))
                .text_size(metrics::TEXT_SM)
                .text_color(ui.text.hsla())
                .child(
                    Icon::new(IconName::AlertTriangle)
                        .size(IconSize::Small)
                        .color(warn),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child("This file was deleted on disk."),
                )
                .child(
                    Button::new("deleted-save", "Save")
                        .compact()
                        .style(ButtonStyle::Accent)
                        .on_click({
                            let weak = weak.clone();
                            move |_, window, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    let task = this.save(window, cx);
                                    this.save_task = Some(cx.spawn(async move |_, _| {
                                        if let Err(err) = task.await {
                                            tracing::warn!("save failed: {err:#}");
                                        }
                                    }));
                                });
                            }
                        }),
                )
                .child(
                    Button::new("deleted-close", "Close")
                        .compact()
                        .style(ButtonStyle::Outlined)
                        .on_click({
                            let weak = weak.clone();
                            move |_, _, cx| {
                                let _ =
                                    weak.update(cx, |_, cx| cx.emit(FileViewEvent::CloseRequested));
                            }
                        }),
                )
                .into_any_element(),
        )
    }

    fn render_conflict_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.model.read(cx).conflict()?;
        let ui = cx.theme().ui.clone();
        let warn = cx.theme().git.modified.hsla();
        let weak = cx.entity().downgrade();
        let button = |id: &'static str, label: &'static str, style: ButtonStyle| {
            Button::new(id, label).compact().style(style)
        };
        Some(
            h_flex()
                .id("disk-conflict-banner")
                .h(px(32.))
                .flex_none()
                .px(px(12.))
                .gap(px(8.))
                .bg(warn.opacity(0.12))
                .border_b_1()
                .border_color(warn.opacity(0.45))
                .text_size(metrics::TEXT_SM)
                .text_color(ui.text.hsla())
                .child(
                    Icon::new(IconName::AlertTriangle)
                        .size(IconSize::Small)
                        .color(warn),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child("This file changed on disk. You have unsaved changes."),
                )
                .child(
                    button(
                        "conflict-reload",
                        "Reload (discard mine)",
                        ButtonStyle::Outlined,
                    )
                    .on_click({
                        let weak = weak.clone();
                        move |_, _, cx| {
                            let _ = weak
                                .update(cx, |this, cx| this.model.update(cx, |m, cx| m.revert(cx)));
                        }
                    }),
                )
                .child(
                    button("conflict-keep", "Keep Mine", ButtonStyle::Outlined).on_click({
                        let weak = weak.clone();
                        move |_, _, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.model.update(cx, |m, cx| m.keep_mine(cx))
                            });
                        }
                    }),
                )
                .child(
                    button("conflict-compare", "Compare", ButtonStyle::Accent).on_click({
                        let weak = weak.clone();
                        move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| this.compare_with_disk(window, cx));
                        }
                    }),
                )
                .into_any_element(),
        )
    }

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
        let model = self.model.read(cx);
        let doc = model.live_document()?;
        let ui = &cx.theme().ui;
        let message: SharedString = if doc.is_large() {
            format!(
                "Large file ({}) — syntax highlighting off, read-only",
                format_size(doc.size())
            )
            .into()
        } else if doc.lossy() || (!model.is_editable() && matches!(doc.kind(), FileKind::Text)) {
            format!(
                "Opened as {} — some bytes could not be decoded and are shown as \u{FFFD}; read-only to keep the file intact",
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
        let size = self.live_document(cx).map_or(0, |d| d.size());
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
        let show_crumbs = self.is_text.get() && self.setup_done;
        let conflict_banner = self
            .render_conflict_banner(cx)
            .or_else(|| self.render_deleted_banner(cx));
        let weak = cx.entity().downgrade();
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::hover_action))
            .on_action(cx.listener(Self::clear_selection))
            .on_action(cx.listener(Self::save_action))
            .on_action(cx.listener(Self::save_as_action))
            .on_action(cx.listener(Self::revert_action))
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
            .children(conflict_banner)
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
}

impl FileItem {
    /// Item for `view`.
    pub fn new(view: Entity<FileView>, cx: &App) -> Self {
        let v = view.read(cx);
        FileItem {
            code_view: v.code_view().clone(),
            is_text: v.is_text_flag(),
            view,
        }
    }

    /// Weak handle to the view.
    pub fn weak_view(&self) -> WeakEntity<FileView> {
        self.view.downgrade()
    }
}

impl PaneItem for FileItem {
    fn title(&self, cx: &App) -> SharedString {
        self.view
            .read(cx)
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .into()
    }

    fn icon(&self, cx: &App) -> TabIcon {
        TabIcon::File(self.title(cx))
    }

    fn path(&self, cx: &App) -> Option<PathBuf> {
        Some(self.view.read(cx).path().to_path_buf())
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

    fn is_dirty(&self, cx: &App) -> bool {
        self.view.read(cx).is_dirty(cx)
    }

    fn is_deleted(&self, cx: &App) -> bool {
        self.view.read(cx).model().read(cx).is_deleted()
    }

    fn is_read_only(&self, cx: &App) -> bool {
        let v = self.view.read(cx);
        self.is_text.get() && v.model().read(cx).is_editable() && v.is_read_only(cx)
    }

    fn clone_item(&self, _window: &mut Window, cx: &mut App) -> Option<Box<dyn PaneItem>> {
        let project = self.view.read(cx).project().clone();
        let path = self.view.read(cx).path().to_path_buf();
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

/// Closes the window after asking about unsaved changes in `workspace`
/// (Save All / Don't Save / Cancel). Resolves to `true` when it may close.
pub fn confirm_discard_or_save(
    dirty: Vec<Entity<FileView>>,
    window: &mut Window,
    cx: &mut App,
) -> Task<anyhow::Result<bool>> {
    if dirty.is_empty() {
        return Task::ready(Ok(true));
    }
    let names: Vec<String> = dirty.iter().map(|v| v.read(cx).file_name()).collect();
    let (message, detail) = if names.len() == 1 {
        (
            format!(
                "Do you want to save the changes you made to “{}”?",
                names[0]
            ),
            "Your changes will be lost if you don't save them.".to_string(),
        )
    } else {
        (
            format!("Do you want to save the changes to {} files?", names.len()),
            format!(
                "{}\n\nYour changes will be lost if you don't save them.",
                names.join("\n")
            ),
        )
    };
    let save_label = if names.len() == 1 { "Save" } else { "Save All" };
    let answer = window.prompt(
        PromptLevel::Warning,
        &message,
        Some(&detail),
        &[save_label, "Don't Save", "Cancel"],
        cx,
    );
    window.spawn(cx, async move |cx| match answer.await {
        Ok(0) => {
            for view in dirty {
                let task = view.update_in(cx, |v, window, cx| v.save(window, cx))?;
                if !task.await? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Ok(1) => Ok(true),
        _ => Ok(false),
    })
}
