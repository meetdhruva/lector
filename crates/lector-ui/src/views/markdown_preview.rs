//! Rendered Markdown preview (pane item), shown side by side with the
//! source; follows the source's scroll position and reloads on change.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use std::time::Duration;

use crate::scale::px;
use gpui::{
    div, prelude::*, AnyView, App, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle,
    SharedString, Subscription, Task, Window,
};
use lector_workspace::Project;

use crate::code_view::{CodeView, CodeViewEvent};
use crate::components::v_flex;
use crate::editor::{DocumentEvent, DocumentModel};
use crate::icons::IconName;
use crate::markdown::{self, Block, RenderOptions};
use crate::theme::{ActiveTheme, ColorExt};
use crate::views::pane::{PaneItem, TabIcon};

/// Events emitted by [`MarkdownPreview`].
#[derive(Clone, Debug, PartialEq)]
pub enum MarkdownPreviewEvent {
    /// A relative link to a file was clicked.
    OpenFile(PathBuf),
}

/// How long typing must pause before the preview re-renders.
const LIVE_DEBOUNCE: Duration = Duration::from_millis(250);

/// The preview view.
pub struct MarkdownPreview {
    path: PathBuf,
    project: Project,
    blocks: Arc<Vec<Block>>,
    loaded: bool,
    scroll: ScrollHandle,
    focus_handle: FocusHandle,
    task: Option<Task<()>>,
    follow: Option<Subscription>,
    last_synced_block: Option<usize>,
    /// The open document whose live (possibly unsaved) text is rendered.
    model: Option<Entity<DocumentModel>>,
    model_subscription: Option<Subscription>,
    debounce: Option<Task<()>>,
}

impl EventEmitter<MarkdownPreviewEvent> for MarkdownPreview {}

impl Focusable for MarkdownPreview {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn slug(text: &str) -> String {
    text.trim()
        .to_lowercase()
        .chars()
        .filter_map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                Some(c)
            } else if c.is_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

impl MarkdownPreview {
    /// Preview of `path`, loading in the background.
    pub fn new(path: PathBuf, project: Project, cx: &mut Context<Self>) -> Self {
        let mut this = MarkdownPreview {
            path,
            project,
            blocks: Arc::new(Vec::new()),
            loaded: false,
            scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            task: None,
            follow: None,
            last_synced_block: None,
            model: None,
            model_subscription: None,
            debounce: None,
        };
        this.reload(cx);
        this
    }

    /// Plain text of the headings and paragraphs currently rendered.
    pub fn rendered_text(&self) -> Vec<String> {
        self.blocks
            .iter()
            .filter_map(|b| match b {
                Block::Heading { text, .. } | Block::Paragraph { text, .. } => {
                    Some(text.text.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// The Markdown file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-renders: the live buffer text when a document is attached (see
    /// [`MarkdownPreview::follow_document`]), else the file on disk.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.live_text(cx) {
            self.render_text(text, cx);
            return;
        }
        let project = self.project.clone();
        let path = self.path.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let blocks = cx
                .background_executor()
                .spawn(async move {
                    let doc = project.load_document(&path).ok()?;
                    let text = doc.text()?.to_string();
                    Some(markdown::parse(&text))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.blocks = Arc::new(blocks.unwrap_or_default());
                this.loaded = true;
                cx.notify();
            });
        }));
    }

    /// The text of the attached document (its unsaved edits included).
    fn live_text(&self, cx: &App) -> Option<lector_text::Text> {
        let model = self.model.as_ref()?;
        let model = model.read(cx);
        model.live_document()?.text().cloned()
    }

    /// Parses `text` in the background and swaps the rendered blocks in.
    fn render_text(&mut self, text: lector_text::Text, cx: &mut Context<Self>) {
        self.task = Some(cx.spawn(async move |this, cx| {
            let blocks = cx
                .background_executor()
                .spawn(async move { markdown::parse(&text.to_string()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.blocks = Arc::new(blocks);
                this.loaded = true;
                cx.notify();
            });
        }));
    }

    /// Renders the live text of `model` and keeps up with every edit
    /// (debounced), so the preview shows what is in the editor, not the
    /// version on disk.
    pub fn follow_document(&mut self, model: Entity<DocumentModel>, cx: &mut Context<Self>) {
        self.model_subscription = Some(cx.subscribe(
            &model,
            |this, _, event: &DocumentEvent, cx| match event {
                DocumentEvent::SourceChanged | DocumentEvent::Loaded => this.refresh_soon(cx),
                _ => {}
            },
        ));
        self.model = Some(model);
        self.reload(cx);
    }

    /// Re-renders once typing has paused for [`LIVE_DEBOUNCE`].
    fn refresh_soon(&mut self, cx: &mut Context<Self>) {
        self.debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LIVE_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.reload(cx));
        }));
    }

    /// Keeps the preview scrolled to the block at the top of `source`.
    pub fn follow(&mut self, source: Entity<CodeView>, cx: &mut Context<Self>) {
        self.follow = Some(
            cx.subscribe(&source, |this, source, event: &CodeViewEvent, cx| {
                if *event == CodeViewEvent::Scrolled {
                    let top = source.read(cx).top_visible_row();
                    this.sync_to_line(top);
                    cx.notify();
                }
            }),
        );
    }

    fn sync_to_line(&mut self, line: u32) {
        let ix = self
            .blocks
            .iter()
            .rposition(|b| b.line() <= line)
            .unwrap_or(0);
        if self.last_synced_block != Some(ix) {
            self.last_synced_block = Some(ix);
            self.scroll.scroll_to_item(ix);
        }
    }

    fn open_link(&mut self, target: &str, cx: &mut Context<Self>) {
        if let Some(anchor) = target.strip_prefix('#') {
            let wanted = slug(anchor);
            if let Some(ix) = self.blocks.iter().position(
                |b| matches!(b, Block::Heading { text, .. } if slug(&text.text) == wanted),
            ) {
                self.scroll.scroll_to_item(ix);
                cx.notify();
            }
            return;
        }
        let base = self.path.parent().map(Path::to_path_buf);
        let (resolved, is_url) = markdown::resolve_link(target, base.as_deref());
        if is_url {
            cx.open_url(&resolved);
        } else {
            let path = PathBuf::from(resolved);
            if path.exists() {
                cx.emit(MarkdownPreviewEvent::OpenFile(path));
            }
        }
    }
}

impl Render for MarkdownPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let weak = cx.entity().downgrade();
        let opts = RenderOptions {
            base_dir: self.path.parent().map(Path::to_path_buf),
            on_link: Some(Rc::new(
                move |target: &str, _window: &mut Window, cx: &mut App| {
                    let target = target.to_string();
                    let _ = weak.update(cx, |this, cx| this.open_link(&target, cx));
                },
            )),
            compact: false,
            id: "md-preview".into(),
        };
        let blocks = self.blocks.clone();
        let elements = markdown::render_blocks(&blocks, &opts, window, cx);
        let empty = self.loaded && elements.is_empty();
        v_flex()
            .id("markdown-preview")
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(ui.background.hsla())
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .items_center()
            .pt(px(28.))
            .pb(px(120.))
            .text_size(px(14.))
            .text_color(ui.text.hsla())
            .children(
                elements
                    .into_iter()
                    .map(|el| div().w_full().max_w(px(860.)).px(px(36.)).child(el)),
            )
            .when(empty, |el| {
                el.child(
                    div()
                        .text_color(ui.text_faint.hsla())
                        .child("This document is empty."),
                )
            })
    }
}

/// Pane item wrapper for [`MarkdownPreview`].
pub struct PreviewItem {
    view: Entity<MarkdownPreview>,
    key: SharedString,
    title: SharedString,
}

impl PreviewItem {
    /// Item for `view` with identity `key`.
    pub fn new(view: Entity<MarkdownPreview>, key: SharedString) -> Self {
        let title = String::new();
        PreviewItem {
            view,
            key,
            title: title.into(),
        }
    }
}

impl PaneItem for PreviewItem {
    fn title(&self, cx: &App) -> SharedString {
        if !self.title.is_empty() {
            return self.title.clone();
        }
        let name = self
            .view
            .read(cx)
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        format!("Preview {name}").into()
    }

    fn icon(&self, _: &App) -> TabIcon {
        TabIcon::Ui(IconName::BookOpen)
    }

    fn path(&self, _: &App) -> Option<PathBuf> {
        None
    }

    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }

    fn tab_kind(&self, _: &App) -> Option<lector_config::TabKind> {
        None
    }

    fn item_key(&self, _: &App) -> Option<SharedString> {
        Some(self.key.clone())
    }

    fn clone_item(&self, _window: &mut Window, _cx: &mut App) -> Option<Box<dyn PaneItem>> {
        None
    }
}
