//! File tabs backed by a shared [`DocumentModel`]: the (editable) code view
//! with breadcrumbs, banners, hover popovers, and image / binary previews.
//!
//! Editing seams (the flow is in `docs/ARCHITECTURE.md`, "Opening and editing a document"):
//! * one [`DocumentModel`] per open file, shared by all of its tabs, adapts a
//!   shared `lector_app::documents::DocHandle` (the same document agents,
//!   commands and other windows edit) and owns its
//!   [`crate::code_view::EditBuffer`] mirror; each tab's [`CodeView`] edits it
//!   with its own carets;
//! * [`register_before_save`] adds hooks run before every save (format on
//!   save);
//! * external edits (LSP formatting, rename, code actions) go through
//!   [`DocumentModel::apply_edits`] (one undo step, carets mapped).

pub mod compare;
pub mod completion;
pub mod doc_store;
pub mod image_view;
pub mod inline_diff;
pub mod model;
pub mod signature_help;
pub mod source;

use crate::background::{background, view_key, JobKind, Priority};
use std::cell::Cell;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    prelude::*, App, Entity, EventEmitter, FocusHandle, Focusable, Pixels, Point, PromptLevel,
    SharedString, Subscription, Task, Window,
};
use lector_app::{Document, Indentation, LineEnding, Project};
use lector_fs::{FileKind, ImageFormat};
use lector_syntax::OutlineItem;

pub use doc_store::{document_store, set_document_store};
pub use model::{
    document_for_path, open_documents, register_before_save, BeforeSaveHook, DiskConflict,
    DocumentEvent, DocumentModel, SaveOutcome,
};
pub use source::{DocumentSource, SourceState, HIGHLIGHT_CHUNK};

use crate::actions::editor::{ClearSelection, GoToDefinition, Hover, RevertFile, Save, SaveAs};
use crate::code_view::{CodeView, CodeViewEvent, HoverTarget, InMemorySource, Pos, Selection};
use crate::markdown::{self, Block};
use crate::settings::AppSettings;
use image_view::{ImageInfo, ImageView};

mod item;
mod render;

pub use item::{confirm_discard_or_save, FileItem};
pub use render::{centered_panel, symbol_badge};

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
    /// Someone asked to allow editing in a tab that is locked read-only: the message says why
    /// it stays read-only (the workspace shows it).
    ReadOnlyLocked(SharedString),
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
    /// Why editing cannot be allowed in this tab (a locked tab, see
    /// [`FileView::set_read_only_lock`]).
    pub read_only_reason: Option<SharedString>,
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
    /// The shared document is locked (`document.set_read_only`).
    doc_locked: bool,
    /// Why this tab can never be made editable (a file opened from a link that points
    /// outside the project, see [`FileView::set_read_only_lock`]). Survives reloads and
    /// changes on disk; the tab's code view carries the same lock.
    read_only_lock: Option<SharedString>,
    /// The in-place diff of an inline agent edit, when one is showing (`inline_diff`).
    inline_edit: Option<inline_diff::InlineEditState>,
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
                    CodeViewEvent::ReadOnlyLocked => {
                        if let Some(reason) = this.read_only_lock.clone() {
                            cx.emit(FileViewEvent::ReadOnlyLocked(reason));
                        }
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
            doc_locked: false,
            read_only_lock: None,
            inline_edit: None,
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
                    let source = self.wrap_inline_source(source, cx);
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
                // A document locked (or unlocked) from outside, e.g. by an agent.
                let locked = self.model.read(cx).is_locked();
                if locked != self.doc_locked {
                    self.doc_locked = locked;
                    let read_only = locked || AppSettings::get(cx).editor.read_only;
                    self.code_view
                        .update(cx, |v, cx| v.set_read_only(read_only, cx));
                }
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
        let locked = model.is_locked();
        let lock = self.read_only_lock.is_some();
        let read_only = AppSettings::get(cx).editor.read_only || !editable || locked || lock;
        let first_setup = !self.setup_done || locked != self.doc_locked;
        self.doc_locked = locked;
        self.code_view.update(cx, |v, cx| {
            if let Some(source) = source {
                v.set_source(source, cx);
            }
            v.set_buffer(buffer, false, cx);
            if first_setup || !editable {
                v.set_read_only(read_only, cx);
            }
            // The restriction of this tab is never decided by the document: loading,
            // reloading and external changes do not lift it.
            v.set_read_only_locked(lock, cx);
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
                    let read = path.clone();
                    let key = view_key(JobKind::ImageDecode, this.entity_id());
                    let info = background(cx, key, Priority::Visible, move |_| {
                        ImageInfo::read(&read, format, size)
                    });
                    let Ok(info) = info.await else { return };
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

    /// Locks this tab read-only for good (`Some(reason)`), or lifts the lock. A locked tab
    /// stays read-only through the load that may still be running, reloads, external
    /// changes, settings changes and session restore; "toggle read-only" only explains
    /// `reason`. Used for files opened from a link that points outside the project.
    pub fn set_read_only_lock(&mut self, reason: Option<SharedString>, cx: &mut Context<Self>) {
        let locked = reason.is_some();
        self.read_only_lock = reason;
        self.code_view
            .update(cx, |v, cx| v.set_read_only_locked(locked, cx));
        cx.notify();
    }

    /// Why this tab is locked read-only, if it is.
    pub fn read_only_lock(&self) -> Option<&SharedString> {
        self.read_only_lock.as_ref()
    }

    /// Flips read-only; a locked tab stays read-only and tells why (`ReadOnlyLocked`).
    pub fn toggle_read_only(&mut self, cx: &mut Context<Self>) {
        self.code_view.update(cx, |v, cx| v.toggle_read_only(cx));
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
            read_only_reason: self.read_only_lock.clone(),
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

    /// The provenance query for `row` when the line is a change against `HEAD` in a saved buffer.
    pub(crate) fn provenance_query(
        &self,
        row: u32,
        cx: &App,
    ) -> Option<lector_app::provenance::LineQuery> {
        if self.is_dirty(cx) {
            return None; // line numbers no longer match the file on disk
        }
        let changed = self
            .code_view
            .read(cx)
            .source()
            .gutter_marks(row..row + 1)
            .iter()
            .any(|m| m.kind != crate::code_view::GutterMarkKind::Deleted);
        if !changed {
            return None;
        }
        let repo = self.project.repo()?;
        let rel = self.project.relative_path(&self.path)?;
        Some(lector_app::provenance::LineQuery {
            repo: repo.workdir().to_path_buf(),
            path: self.project.repo_path(&rel),
            line: row,
            rev: None,
        })
    }

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
        // "Why did this line change?" (commit message, agent prompt) below them, for changed lines.
        let why = self.provenance_query(target.pos.row, cx);
        let word = match doc.word_at(offset) {
            Some((word, _)) => word,
            None if diagnostics.is_some() => offset..offset + 1,
            None if why.is_some() => text.line_range(target.pos.row),
            None => return,
        };
        if self.hover.as_ref().is_some_and(|h| h.word == word) {
            return;
        }
        let project = self.project.clone();
        let core = crate::commands_bridge::core(cx);
        let word_start = text.offset_to_point(word.start);
        let position = self
            .code_view
            .read(cx)
            .point_for_pos(Pos::new(word_start.row, word_start.column as usize))
            .unwrap_or(target.position);
        self.hover_task = Some(cx.spawn(async move |this, cx| {
            // Waits on the language server: a blocking-lane kind.
            let key = view_key(JobKind::LspHover, this.entity_id());
            let result = background(cx, key, Priority::Visible, move |_| {
                let md = lector_app::project::symbols::hover(&project, &doc, offset)
                    .ok()
                    .flatten()
                    .map(|md| md.trim().to_string())
                    .filter(|md| !md.is_empty());
                let why = why
                    .map(|q| core.provenance().explain_all(&q))
                    .map(|items| lector_app::provenance::explanation_markdown(&items))
                    .filter(|md| !md.is_empty());
                let md = [diagnostics, md, why]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join("\n\n---\n\n");
                if md.is_empty() {
                    return None;
                }
                Some(Arc::new(markdown::parse(&md)))
            });
            let Ok(result) = result.await else { return };
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
