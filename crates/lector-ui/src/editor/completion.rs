//! The completion popup of file tabs (v0.7).
//!
//! One [`CompletionController`] per file tab (created for every `FileView` by
//! [`init`], found again with [`controller_for`]) owns a
//! [`lector_lsp::CompletionSession`] and drives it as `docs/ARCHITECTURE.md`
//! ("Completion and signature help") describes. It is also the popup's view:
//! the code view hosts it through `CodeView::set_popup` and places it at the
//! start of the word being completed (below the line, above when there is no
//! room, clamped to the window; `code_view::popup`).
//!
//! * **Opening.** Typing an identifier character or a server trigger
//!   character opens a session (never read-only tabs, never during an IME
//!   composition, never with `editor.completion.enabled = false`);
//!   `editor::TriggerSuggest` opens it by hand and tells the user why when it
//!   cannot (no server, still starting, ...). Nothing is asked on open or
//!   focus; the request is made after the document is synced, on a background
//!   task, and the UI thread never waits for the server. Until the server has
//!   answered, no popup is shown and the keys keep their normal meaning.
//! * **Filtering** is synchronous (the session re-filters in well under a
//!   millisecond for 10,000 items); only an `isIncomplete` list is asked for
//!   again.
//! * **The popup** is a hand-virtualized list: only the rows in view are
//!   built (ten at most, `VISIBLE_ROWS`). A side pane shows the selected item's
//!   detail and documentation; documentation the server only gives on
//!   `completionItem/resolve` is requested `RESOLVE_DELAY_MS` after the
//!   selection settled (a newer selection cancels it).
//! * **Keys** while the popup shows (key context `CodeView && completion`,
//!   `completion::*` actions, all rebindable): down / up / ctrl-n / ctrl-p,
//!   pagedown / pageup, enter and tab accept (`editor.completion.accept_on_enter`
//!   turns Enter's acceptance off; Tab always accepts), escape closes only the
//!   popup. Anything else types into the document and updates the session. A
//!   commit character of the selected item accepts it first, then is typed.
//! * **Accepting** resolves the item first when it can still add edits (at
//!   most `ACCEPT_RESOLVE_WAIT_MS`, then it applies what it has), then applies
//!   `CompletionSession::commit` as one undo step through the buffer
//!   (additional edits such as auto-imports included, every caret placed), and
//!   opens signature help when the item asks for it. If the text moved so that
//!   the commit no longer fits, the popup closes quietly.
//! * **Snippets** (`editor.completion.snippets`): there is no tab-stop session
//!   in the editor. The caret is placed at the first stop, the rest are ignored.

use std::collections::HashMap;
use std::ops::Range;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::Either;
use gpui::{
    div, prelude::*, AnyElement, App, Context, Entity, EntityId, Global, HighlightStyle, Hsla,
    IntoElement, MouseButton, Pixels, Render, ScrollWheelEvent, SharedString, Size, StyledText,
    Subscription, Task, WeakEntity, Window,
};
use lector_lsp::{
    CancelToken, CloseReason, CompletionCommit, CompletionKind, CompletionResponse,
    CompletionSession, InsertMode, LspError, LspManager, OpenReason, RequestOutcome,
    ResolvedCompletion, ServerState, SessionOptions, Update, WordChars,
};
use lector_syntax::SymbolKind;
use lector_text::Rope;

use super::signature_help::SignatureController;
use super::{DocumentModel, FileView};
use crate::actions::completion as act;
use crate::actions::editor::{
    ClearSelection, MoveDown, MoveUp, Newline, PageDown, PageUp, Tab, TriggerParameterHints,
    TriggerSuggest,
};
use crate::actions::signature as sig_act;
use crate::code_view::{
    edit, popup::DismissHook, CodeView, CodeViewEvent, EditorPopup, EditorSettings, Pos, Vertical,
};
use crate::components::popover::surface;
use crate::components::{h_flex, metrics, v_flex};
use crate::markdown::{self, Block, RenderOptions};
use crate::scale::px;
use crate::settings::AppSettings;
use crate::theme::{ActiveTheme, ColorExt};
use crate::views::Workspace;

/// Popup key in [`CodeView::set_popup`].
pub const POPUP_KEY: &str = "completion";
/// Key context flag set while the popup shows.
pub const FLAG_OPEN: &str = "completion";
/// Key context flag set while a session waits for the server's first answer
/// (only Escape is bound: it cancels the request).
pub const FLAG_PENDING: &str = "completion_pending";
/// Rows in view at most (and the page size of PageUp / PageDown).
pub const VISIBLE_ROWS: usize = 10;
/// Wait after the selection settled before the documentation is resolved.
pub const RESOLVE_DELAY_MS: u64 = 120;
/// How long accepting waits for a resolve that may add edits.
pub const ACCEPT_RESOLVE_WAIT_MS: u64 = 300;
/// Re-asks after a `Stale` answer before giving up.
const MAX_STALE_RETRIES: u8 = 3;
/// Rows looked at to size the popup's width.
const WIDTH_SAMPLE_ROWS: usize = 64;
/// Documentation longer than this many chars is cut for the pane.
const MAX_DOC_CHARS: usize = 6000;

type ResolveFuture = Pin<
    Box<
        dyn std::future::Future<Output = Result<RequestOutcome<ResolvedCompletion>, LspError>>
            + Send,
    >,
>;

// ---------------------------------------------------------------------------
// Shared helpers (also used by `signature_help`).

/// Syncs the model's buffer text to its language server unless the server
/// already has this version, and returns the text.
pub(crate) fn ensure_synced(model: &Entity<DocumentModel>, cx: &App) -> Option<Rope> {
    let m = model.read(cx);
    let lsp = m.project().lsp()?;
    let rope = m.buffer()?.read(cx).text().clone();
    if lsp
        .document_state(m.path())
        .is_some_and(|d| d.tag == Some(rope.version()))
    {
        return Some(rope);
    }
    crate::lsp::sync_now(model, cx)
}

/// The text, the primary caret as a byte offset and as a position.
pub(crate) fn caret_state(
    code_view: &WeakEntity<CodeView>,
    cx: &App,
) -> Option<(Rope, usize, Pos)> {
    let v = code_view.upgrade()?;
    let v = v.read(cx);
    let rope = v.buffer()?.read(cx).text().clone();
    let head = v.selection().head;
    Some((rope.clone(), edit::pos_to_offset(&rope, head), head))
}

fn single_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Model-level data for painting (UI-free, tested without a window).

/// What one row of the list shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowView {
    /// Row index in the filtered list.
    pub row: usize,
    /// The label.
    pub label: String,
    /// Byte ranges of the label that match the typed text.
    pub highlights: Vec<Range<usize>>,
    /// Dimmed text right after the label (`labelDetails.detail`).
    pub label_detail: Option<String>,
    /// Dimmed text at the right edge (`labelDetails.description`, else the item's detail).
    pub description: Option<String>,
    /// Item kind (badge).
    pub kind: CompletionKind,
    /// Drawn struck through.
    pub deprecated: bool,
    /// The selected row.
    pub selected: bool,
}

/// The rows `top..top + count` of the session's filtered list: all the popup
/// needs to paint its first page. Cost depends on `count`, not on the list size.
pub fn row_views(session: &CompletionSession, top: usize, count: usize) -> Vec<RowView> {
    let selected = session.selected();
    let matches = session.matches();
    (top..matches.len().min(top.saturating_add(count)))
        .map(|row| {
            let item = &session.items()[matches[row].item];
            let label_detail = item
                .label_details
                .as_ref()
                .and_then(|d| d.detail.clone())
                .filter(|d| !d.is_empty());
            let description = item
                .label_details
                .as_ref()
                .and_then(|d| d.description.clone())
                .or_else(|| item.detail.as_ref().map(|d| first_line(d)))
                .filter(|d| !d.is_empty());
            RowView {
                row,
                label: item.label.clone(),
                highlights: session.label_highlights(row),
                label_detail,
                description,
                kind: item.kind,
                deprecated: item.deprecated,
                selected: selected == Some(row),
            }
        })
        .collect()
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    match line.char_indices().nth(120) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}

/// How a kind is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    /// The outline's symbol badge (colored from the syntax theme).
    Symbol(SymbolKind),
    /// A letter badge.
    Letter(&'static str),
}

/// The badge of a completion kind: the outline's symbol badge where there is
/// a matching symbol kind, a letter otherwise.
pub fn badge_for(kind: CompletionKind) -> Badge {
    use CompletionKind as K;
    match kind {
        K::Method => Badge::Symbol(SymbolKind::Method),
        K::Function | K::Constructor => Badge::Symbol(SymbolKind::Function),
        K::Field | K::Property => Badge::Symbol(SymbolKind::Field),
        K::Variable | K::Value => Badge::Symbol(SymbolKind::Variable),
        K::Class => Badge::Symbol(SymbolKind::Class),
        K::Interface => Badge::Symbol(SymbolKind::Interface),
        K::Module => Badge::Symbol(SymbolKind::Module),
        K::Enum | K::EnumMember => Badge::Symbol(SymbolKind::Enum),
        K::Struct => Badge::Symbol(SymbolKind::Struct),
        K::Constant => Badge::Symbol(SymbolKind::Constant),
        K::TypeParameter => Badge::Symbol(SymbolKind::Type),
        K::Keyword => Badge::Letter("k"),
        K::Snippet => Badge::Letter("s"),
        K::Text => Badge::Letter("t"),
        K::Unit => Badge::Letter("u"),
        K::Color => Badge::Letter("c"),
        K::File => Badge::Letter("f"),
        K::Folder => Badge::Letter("d"),
        K::Reference => Badge::Letter("r"),
        K::Event => Badge::Letter("e"),
        K::Operator => Badge::Letter("o"),
        K::Other => Badge::Letter("·"),
    }
}

/// Text styles for a label: the matched ranges accented, the whole label
/// struck through when deprecated. Ranges are sorted, disjoint, on char boundaries.
pub fn label_runs(
    label: &str,
    matches: &[Range<usize>],
    deprecated: bool,
    accent: Hsla,
    strike: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let strikethrough = deprecated.then_some(gpui::StrikethroughStyle {
        thickness: px(1.),
        color: Some(strike),
    });
    let mut runs = Vec::new();
    let mut at = 0usize;
    let valid = |r: &Range<usize>, at: usize| {
        r.start >= at
            && r.start < r.end
            && r.end <= label.len()
            && label.is_char_boundary(r.start)
            && label.is_char_boundary(r.end)
    };
    for r in matches.iter().filter(|r| valid(r, 0)) {
        if !valid(r, at) {
            continue;
        }
        if deprecated && r.start > at {
            runs.push((
                at..r.start,
                HighlightStyle {
                    strikethrough,
                    ..Default::default()
                },
            ));
        }
        runs.push((
            r.clone(),
            HighlightStyle {
                color: Some(accent),
                font_weight: Some(gpui::FontWeight::SEMIBOLD),
                strikethrough,
                ..Default::default()
            },
        ));
        at = r.end;
    }
    if deprecated && at < label.len() {
        runs.push((
            at..label.len(),
            HighlightStyle {
                strikethrough,
                ..Default::default()
            },
        ));
    }
    runs
}

/// Width of the list part: wide enough for the first rows' text, within bounds.
pub fn list_width(rows: &[RowView], char_width: Pixels) -> Pixels {
    let chars = rows
        .iter()
        .take(WIDTH_SAMPLE_ROWS)
        .map(|r| {
            r.label.chars().count()
                + r.label_detail.as_ref().map_or(0, |d| d.chars().count())
                + r.description
                    .as_ref()
                    .map_or(0, |d| d.chars().count().min(28) + 2)
        })
        .max()
        .unwrap_or(0);
    (char_width * chars as f32 + px(64.)).clamp(px(280.), px(560.))
}

/// Offset in the list so that `selected` is in view with `top` as the current
/// first row; the list never scrolls past its last row.
pub fn scroll_to_show(top: usize, selected: usize, total: usize, visible: usize) -> usize {
    let visible = visible.max(1);
    let mut top = top;
    if selected < top {
        top = selected;
    } else if selected >= top + visible {
        top = selected + 1 - visible;
    }
    top.min(total.saturating_sub(visible))
}

// ---------------------------------------------------------------------------
// The controller.

/// Why opening a session did not happen.
#[derive(Debug, PartialEq, Eq)]
enum OpenError {
    Disabled,
    ReadOnly,
    Composing,
    NoServer(String),
}

impl OpenError {
    fn message(&self) -> Option<String> {
        match self {
            OpenError::Disabled => {
                Some("Code completion is turned off (editor.completion.enabled)".into())
            }
            OpenError::ReadOnly => Some("This tab is read-only".into()),
            OpenError::Composing => None,
            OpenError::NoServer(m) => Some(m.clone()),
        }
    }
}

/// What the selected item shows in the side pane.
#[derive(Default)]
struct Docs {
    /// `(item index, resolved)` the content is for.
    key: Option<(usize, bool)>,
    detail: Option<SharedString>,
    blocks: Option<Arc<Vec<Block>>>,
    plain: Option<SharedString>,
}

impl Docs {
    fn is_empty(&self) -> bool {
        self.detail.is_none() && self.blocks.is_none() && self.plain.is_none()
    }
}

struct Open {
    id: u64,
    session: CompletionSession,
    token: CancelToken,
    request: Option<Task<()>>,
    stale_retries: u8,
    resolve: Option<Task<()>>,
    resolve_token: CancelToken,
    docs: Docs,
    /// When the server's answer reached the UI thread, until the first render logged it.
    reply_at: Option<Instant>,
    accepting: bool,
}

/// What Enter does with the popup open.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EnterOutcome {
    /// No popup: do what Enter does normally.
    NotOpen,
    /// The popup closed and Enter should insert a line break (`accept_on_enter` off).
    CloseAndNewline,
    /// The selected item is being accepted.
    Accepted,
}

/// The completion popup of one file tab. Also the popup's view.
pub struct CompletionController {
    code_view: WeakEntity<CodeView>,
    model: WeakEntity<DocumentModel>,
    signature: Entity<SignatureController>,
    open: Option<Open>,
    /// No automatic opening until a non-word character is typed or the caret moves
    /// (after Escape, an acceptance, or a list nothing matches).
    suppress: bool,
    last_key: Option<(u64, usize)>,
    next_id: u64,
    list_top: usize,
    scroll_remainder: f32,
    this: WeakEntity<CompletionController>,
    _subscriptions: Vec<Subscription>,
}

impl CompletionController {
    fn new(
        code_view: &Entity<CodeView>,
        model: &Entity<DocumentModel>,
        signature: Entity<SignatureController>,
        cx: &mut Context<Self>,
    ) -> Self {
        let this = cx.weak_entity();
        // Commit characters are decided before the character is inserted.
        let weak = this.clone();
        code_view.update(cx, |v, _| {
            v.set_input_interceptor(Some(Rc::new(move |text, window, cx| {
                let Some(c) = single_char(text) else {
                    return false;
                };
                let Some(controller) = weak.upgrade() else {
                    return false;
                };
                if !controller.read(cx).is_commit_character(c) {
                    return false;
                }
                // The view is being updated right now: accept on the next turn.
                let text = text.to_string();
                window.defer(cx, move |_, cx| {
                    controller.update(cx, |this, cx| this.accept(Some(text), cx));
                });
                true
            })));
        });
        let subscription = cx.subscribe(
            code_view,
            |this, _, event: &CodeViewEvent, cx| match event {
                CodeViewEvent::Edited | CodeViewEvent::SelectionChanged => this.after_change(cx),
                CodeViewEvent::ReadOnlyChanged => {
                    let read_only = this
                        .code_view
                        .upgrade()
                        .is_some_and(|v| !v.read(cx).is_editable());
                    if read_only {
                        this.close(CloseReason::Dismissed, cx);
                        this.signature.update(cx, |s, cx| s.close(cx));
                    }
                }
                _ => {}
            },
        );
        CompletionController {
            code_view: code_view.downgrade(),
            model: model.downgrade(),
            signature,
            open: None,
            suppress: false,
            last_key: None,
            next_id: 0,
            list_top: 0,
            scroll_remainder: 0.,
            this,
            _subscriptions: vec![subscription],
        }
    }

    // ---- state for tests and key handling ------------------------------------------------

    /// A session exists (it may still wait for the server's answer).
    pub fn has_session(&self) -> bool {
        self.open.is_some()
    }

    /// The popup is showing (the server answered and something matches).
    pub fn popup_visible(&self) -> bool {
        self.open
            .as_ref()
            .is_some_and(|o| o.session.has_response() && !o.session.matches().is_empty())
    }

    /// The signature help controller of this tab.
    pub fn signature(&self) -> &Entity<SignatureController> {
        &self.signature
    }

    /// The rows in view (labels, highlights, selection), for tests and accessibility.
    pub fn visible_rows(&self) -> Vec<RowView> {
        self.open
            .as_ref()
            .map(|o| row_views(&o.session, self.list_top, VISIBLE_ROWS))
            .unwrap_or_default()
    }

    /// Number of rows in the filtered list.
    pub fn match_count(&self) -> usize {
        self.open.as_ref().map_or(0, |o| o.session.matches().len())
    }

    /// Row of the first visible entry.
    pub fn list_top(&self) -> usize {
        self.list_top
    }

    /// The selected row.
    pub fn selected_row(&self) -> Option<usize> {
        self.open.as_ref().and_then(|o| o.session.selected())
    }

    /// The text typed since the word start, as the session sees it.
    pub fn typed(&self) -> Option<String> {
        self.open.as_ref().map(|o| o.session.typed().to_string())
    }

    /// The documentation pane has content.
    pub fn has_docs(&self) -> bool {
        self.open.as_ref().is_some_and(|o| !o.docs.is_empty())
    }

    fn is_commit_character(&self, c: char) -> bool {
        self.popup_visible()
            && self
                .open
                .as_ref()
                .is_some_and(|o| !o.accepting && o.session.is_commit_character(c))
    }

    // ---- opening ---------------------------------------------------------------------------

    fn options(&self, language: &str, cx: &App) -> SessionOptions {
        let s = AppSettings::get(cx);
        SessionOptions {
            insert_mode: match s.editor.completion.insert_mode {
                lector_config::CompletionInsertMode::Insert => InsertMode::Insert,
                lector_config::CompletionInsertMode::Replace => InsertMode::Replace,
            },
            snippets: s.editor.completion.snippets,
            word_chars: WordChars::for_language(language),
            commit_characters: true,
        }
    }

    /// The server side of opening: the manager, the file's language and what
    /// the server can do, or the reason there is none.
    fn server(
        &self,
        cx: &App,
    ) -> Result<
        (
            LspManager,
            std::path::PathBuf,
            String,
            lector_lsp::CompletionCapabilities,
        ),
        OpenError,
    > {
        let Some(model) = self.model.upgrade() else {
            return Err(OpenError::NoServer(String::new()));
        };
        let m = model.read(cx);
        let path = m.path().to_path_buf();
        let language = m
            .live_document()
            .and_then(|d| d.language().map(|l| l.name.clone()));
        let Some(lsp) = m.project().lsp().cloned() else {
            return Err(OpenError::NoServer(
                "Language servers are turned off (lsp.enabled)".into(),
            ));
        };
        let Some(language) = language else {
            return Err(OpenError::NoServer(
                "No language server for this kind of file".into(),
            ));
        };
        match lsp.capabilities(&path) {
            Some(caps) => match caps.completion {
                Some(c) => Ok((lsp, path, language, c)),
                None => Err(OpenError::NoServer(
                    "The language server offers no completion".into(),
                )),
            },
            None => Err(OpenError::NoServer(no_server_message(&lsp, &language))),
        }
    }

    fn check_editable(&self, cx: &App) -> Result<(), OpenError> {
        if !AppSettings::get(cx).editor.completion.enabled {
            return Err(OpenError::Disabled);
        }
        let Some(code_view) = self.code_view.upgrade() else {
            return Err(OpenError::ReadOnly);
        };
        let v = code_view.read(cx);
        if !v.is_editable() {
            return Err(OpenError::ReadOnly);
        }
        if v.marked_range().is_some() {
            return Err(OpenError::Composing);
        }
        Ok(())
    }

    /// `editor::TriggerSuggest`. The error is a message for the user.
    pub fn trigger_manual(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        match self.begin(OpenReason::Manual, cx) {
            Ok(()) => Ok(()),
            Err(e) => Err(e.message().unwrap_or_default()),
        }
    }

    fn begin(&mut self, reason: OpenReason, cx: &mut Context<Self>) -> Result<(), OpenError> {
        self.check_editable(cx)?;
        let (lsp, path, language, _caps) = self.server(cx)?;
        let Some(model) = self.model.upgrade() else {
            return Ok(());
        };
        let Some(rope) = ensure_synced(&model, cx) else {
            return Ok(());
        };
        let Some((_, caret, _)) = caret_state(&self.code_view, cx) else {
            return Ok(());
        };
        self.close(CloseReason::Dismissed, cx);
        self.suppress = false;
        let session = CompletionSession::open(&rope, caret, reason, self.options(&language, cx));
        self.next_id += 1;
        let open = Open {
            id: self.next_id,
            session,
            token: CancelToken::new(),
            request: None,
            stale_retries: 0,
            resolve: None,
            resolve_token: CancelToken::new(),
            docs: Docs::default(),
            reply_at: None,
            accepting: false,
        };
        self.open = Some(open);
        self.list_top = 0;
        self.refresh(cx);
        self.send_request(&lsp, &path, cx);
        Ok(())
    }

    /// Sends the session's own request (the opening one, or a retry after `Stale`).
    fn send_request(&mut self, lsp: &LspManager, path: &std::path::Path, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        let request = open.session.request();
        self.send(lsp, path, request, cx);
    }

    fn send(
        &mut self,
        lsp: &LspManager,
        path: &std::path::Path,
        request: lector_lsp::CompletionRequest,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        open.token.cancel();
        open.token = CancelToken::new();
        let future = lsp.completion(path, request, Some(&open.token));
        let id = open.id;
        open.request = Some(cx.spawn(async move |this, cx| {
            let outcome = future.await;
            let _ = this.update(cx, |this, cx| this.on_response(id, outcome, cx));
        }));
    }

    fn on_response(
        &mut self,
        id: u64,
        outcome: Result<RequestOutcome<CompletionResponse>, LspError>,
        cx: &mut Context<Self>,
    ) {
        let arrived = Instant::now();
        if self.open.as_ref().map(|o| o.id) != Some(id) {
            return;
        }
        match outcome {
            Ok(RequestOutcome::Done(response)) => {
                let Some((rope, caret, _)) = caret_state(&self.code_view, cx) else {
                    return;
                };
                let Some(open) = self.open.as_mut() else {
                    return;
                };
                let items = response.items.len();
                if !open.session.set_response(&rope, caret, response) {
                    self.close(CloseReason::CaretMoved, cx);
                    return;
                }
                open.reply_at = Some(arrived);
                tracing::debug!(
                    target: "lector_ui::perf",
                    items,
                    filter_us = arrived.elapsed().as_micros() as u64,
                    "completion list filtered"
                );
                // The user may have typed since the request: filter for what is there now.
                self.apply_update(&rope, caret, cx);
            }
            Ok(RequestOutcome::Stale) => {
                let retry = self.open.as_mut().is_some_and(|o| {
                    !o.session.has_response() && o.stale_retries < MAX_STALE_RETRIES
                });
                if retry {
                    if let Some(open) = self.open.as_mut() {
                        open.stale_retries += 1;
                    }
                    if let (Ok((lsp, path, ..)), Some(model)) =
                        (self.server(cx), self.model.upgrade())
                    {
                        let _ = ensure_synced(&model, cx);
                        self.send_request(&lsp, &path, cx);
                    }
                }
            }
            Ok(RequestOutcome::Cancelled) => {}
            Ok(RequestOutcome::Unavailable) => self.close(CloseReason::Dismissed, cx),
            Err(err) => {
                tracing::debug!(target: "lector_ui::lsp", "completion request failed: {err}");
                self.close(CloseReason::Dismissed, cx);
            }
        }
    }

    // ---- reacting to the editor ----------------------------------------------------------

    /// The code view edited the text or moved the caret.
    fn after_change(&mut self, cx: &mut Context<Self>) {
        let typed = self
            .code_view
            .upgrade()
            .and_then(|v| v.update(cx, |v, _| v.take_typed()));
        let Some((rope, caret, _)) = caret_state(&self.code_view, cx) else {
            return;
        };
        let composing = self
            .code_view
            .upgrade()
            .is_some_and(|v| v.read(cx).marked_range().is_some());
        if composing {
            self.close(CloseReason::Dismissed, cx);
            self.signature.update(cx, |s, cx| s.close(cx));
            return;
        }
        let key = (rope.version(), caret);
        if typed.is_none() && self.last_key == Some(key) {
            return;
        }
        self.last_key = Some(key);
        if typed.is_none() {
            // A caret move or an edit that is not typing: a new word may open again.
            let moved = self.open.is_none();
            if moved {
                self.suppress = false;
            }
        }
        let mut closed_by = None;
        if self.open.is_some() {
            closed_by = self.apply_update(&rope, caret, cx);
        }
        if let Some(text) = typed.as_deref() {
            if self.open.is_none() {
                self.typed_char(text, closed_by, cx);
            }
        }
        // The signature help follows the same changes.
        let signature = self.signature.clone();
        signature.update(cx, |s, cx| s.after_change(typed.as_deref(), cx));
    }

    /// Feeds the session the current text and caret. Returns why it closed, if it did.
    fn apply_update(
        &mut self,
        rope: &Rope,
        caret: usize,
        cx: &mut Context<Self>,
    ) -> Option<CloseReason> {
        let open = self.open.as_mut()?;
        match open.session.update(rope, caret) {
            Update::Continue => {
                self.list_changed(cx);
                None
            }
            Update::Requery(request) => {
                self.list_changed(cx);
                if let (Ok((lsp, path, ..)), Some(model)) = (self.server(cx), self.model.upgrade())
                {
                    let _ = ensure_synced(&model, cx);
                    self.send(&lsp, &path, request, cx);
                }
                None
            }
            Update::Close(reason) => {
                self.close(reason, cx);
                Some(reason)
            }
        }
    }

    /// A character was typed and no session is open: maybe open one.
    fn typed_char(&mut self, text: &str, closed_by: Option<CloseReason>, cx: &mut Context<Self>) {
        let Some(c) = single_char(text) else {
            self.suppress = false;
            return;
        };
        let (trigger, word) = {
            let Some(model) = self.model.upgrade() else {
                return;
            };
            let m = model.read(cx);
            let trigger = m
                .project()
                .lsp()
                .and_then(|l| l.capabilities(m.path()))
                .and_then(|c| c.completion)
                .is_some_and(|caps| caps.is_trigger(c));
            let language = m
                .live_document()
                .and_then(|d| d.language().map(|l| l.name.clone()))
                .unwrap_or_default();
            (trigger, WordChars::for_language(&language).is_word(c))
        };
        if !word {
            self.suppress = false;
        }
        let reason = if trigger {
            Some(OpenReason::TriggerCharacter(c))
        } else if word && !self.suppress && closed_by != Some(CloseReason::NoMatches) {
            Some(OpenReason::Typing)
        } else {
            None
        };
        if let Some(reason) = reason {
            if let Err(err) = self.begin(reason, cx) {
                tracing::trace!(target: "lector_ui::lsp", "completion not opened: {err:?}");
            }
        }
    }

    // ---- list state ----------------------------------------------------------------------

    /// The filtered list changed (typing, a new answer): keep the view and the pane consistent.
    fn list_changed(&mut self, cx: &mut Context<Self>) {
        self.after_selection(cx);
    }

    fn after_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(open) = self.open.as_ref() {
            let total = open.session.matches().len();
            self.list_top = match open.session.selected() {
                Some(sel) => scroll_to_show(self.list_top, sel, total, VISIBLE_ROWS),
                None => 0,
            };
        }
        self.refresh_docs();
        self.schedule_resolve(cx);
        self.refresh(cx);
    }

    fn refresh_docs(&mut self) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        let Some((ix, item)) = open.session.selected_item() else {
            open.docs = Docs::default();
            return;
        };
        let key = Some((ix, item.resolved));
        if open.docs.key == key {
            return;
        }
        let detail = item
            .detail
            .as_ref()
            .map(|d| d.trim())
            .filter(|d| !d.is_empty())
            .map(|d| SharedString::from(d.to_string()));
        let mut docs = Docs {
            key,
            detail,
            blocks: None,
            plain: None,
        };
        if let Some(d) = item
            .documentation
            .as_ref()
            .filter(|d| !d.value.trim().is_empty())
        {
            let text: String = d.value.chars().take(MAX_DOC_CHARS).collect();
            if d.markdown {
                docs.blocks = Some(Arc::new(markdown::parse(&text)));
            } else {
                docs.plain = Some(text.into());
            }
        }
        open.docs = docs;
    }

    fn schedule_resolve(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        open.resolve = None;
        open.resolve_token.cancel();
        let Some((ix, _)) = open.session.selected_item() else {
            return;
        };
        if !open.session.needs_resolve(ix) {
            return;
        }
        let token = CancelToken::new();
        open.resolve_token = token.clone();
        let (id, generation) = (open.id, open.session.generation());
        open.resolve = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(RESOLVE_DELAY_MS))
                .await;
            let Ok(Some(future)) =
                this.update(cx, |this, cx| this.resolve_future(id, ix, &token, cx))
            else {
                return;
            };
            let outcome = future.await;
            let _ = this.update(cx, |this, cx| {
                this.on_resolved(id, generation, ix, outcome, cx)
            });
        }));
    }

    fn resolve_future(
        &mut self,
        id: u64,
        item: usize,
        token: &CancelToken,
        cx: &mut Context<Self>,
    ) -> Option<ResolveFuture> {
        let open = self.open.as_ref().filter(|o| o.id == id)?;
        if !open.session.needs_resolve(item) {
            return None;
        }
        let item = open.session.item(item)?.clone();
        let (lsp, path, ..) = self.server(cx).ok()?;
        // The additional edits are computed for the synced text: keep it current.
        let model = self.model.upgrade()?;
        let _ = ensure_synced(&model, cx);
        Some(Box::pin(lsp.resolve_completion(&path, &item, Some(token))))
    }

    fn on_resolved(
        &mut self,
        id: u64,
        generation: u64,
        item: usize,
        outcome: Result<RequestOutcome<ResolvedCompletion>, LspError>,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.open.as_mut().filter(|o| o.id == id) else {
            return;
        };
        if let Ok(RequestOutcome::Done(resolved)) = outcome {
            if open.session.apply_resolved(generation, item, resolved) {
                self.refresh_docs();
                self.refresh(cx);
            }
        }
    }

    /// Puts the popup, its size, anchor and key context flag in line with the session.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(code_view) = self.code_view.upgrade() else {
            return;
        };
        if !self.popup_visible() {
            let pending = self.open.is_some();
            code_view.update(cx, |v, cx| {
                v.set_popup(POPUP_KEY, None, cx);
                v.set_context_flag(FLAG_OPEN, false, cx);
                v.set_context_flag(FLAG_PENDING, pending, cx);
            });
            cx.notify();
            return;
        }
        let Some((rope, ..)) = caret_state(&self.code_view, cx) else {
            return;
        };
        let (anchor, size) = {
            let Some(open) = self.open.as_ref() else {
                return;
            };
            let anchor = edit::offset_to_pos(&rope, open.session.anchor().min(rope.len()));
            let rows = row_views(&open.session, 0, WIDTH_SAMPLE_ROWS);
            let editor = EditorSettings::get(cx);
            let list_w = list_width(&rows, editor.font_size * 0.6);
            let visible = open.session.matches().len().min(VISIBLE_ROWS);
            let height = metrics::row_height() * visible as f32 + px(2.);
            let width = list_w
                + if open.docs.is_empty() {
                    px(0.)
                } else {
                    docs_width()
                };
            (anchor, Size { width, height })
        };
        let weak = self.this.clone();
        let on_dismiss: DismissHook = Rc::new(move |_, cx: &mut App| {
            if let Some(this) = weak.upgrade() {
                this.update(cx, |this, cx| this.close(CloseReason::Dismissed, cx));
            }
        });
        let view = cx.entity();
        code_view.update(cx, |v, cx| {
            v.set_context_flag(FLAG_OPEN, true, cx);
            v.set_context_flag(FLAG_PENDING, false, cx);
            v.set_popup(
                POPUP_KEY,
                Some(EditorPopup {
                    view: view.into(),
                    anchor,
                    size,
                    prefer: Vertical::Below,
                    on_dismiss: Some(on_dismiss),
                }),
                cx,
            );
        });
        cx.notify();
    }

    /// Ends the session and hides the popup.
    pub fn close(&mut self, reason: CloseReason, cx: &mut Context<Self>) {
        if let Some(mut open) = self.open.take() {
            open.token.cancel();
            open.resolve_token.cancel();
            open.session.close(reason);
            self.suppress = matches!(
                reason,
                CloseReason::Dismissed | CloseReason::NoMatches | CloseReason::Committed
            );
        }
        self.list_top = 0;
        self.scroll_remainder = 0.;
        if let Some(code_view) = self.code_view.upgrade() {
            let any = {
                let v = code_view.read(cx);
                v.has_popup(POPUP_KEY)
                    || v.has_context_flag(FLAG_OPEN)
                    || v.has_context_flag(FLAG_PENDING)
            };
            if any {
                code_view.update(cx, |v, cx| {
                    v.set_popup(POPUP_KEY, None, cx);
                    v.set_context_flag(FLAG_OPEN, false, cx);
                    v.set_context_flag(FLAG_PENDING, false, cx);
                });
            }
        }
        cx.notify();
    }

    // ---- key actions ---------------------------------------------------------------------

    /// Moves the selection by one row (wraps). `false` when no popup shows.
    pub fn select_next(&mut self, cx: &mut Context<Self>) -> bool {
        self.select(|s| s.select_next(), cx)
    }

    /// Moves the selection up by one row (wraps).
    pub fn select_previous(&mut self, cx: &mut Context<Self>) -> bool {
        self.select(|s| s.select_previous(), cx)
    }

    /// Moves the selection down by a page (no wrap).
    pub fn page_down(&mut self, cx: &mut Context<Self>) -> bool {
        self.select(|s| s.select_page(VISIBLE_ROWS as isize), cx)
    }

    /// Moves the selection up by a page (no wrap).
    pub fn page_up(&mut self, cx: &mut Context<Self>) -> bool {
        self.select(|s| s.select_page(-(VISIBLE_ROWS as isize)), cx)
    }

    fn select(&mut self, f: impl FnOnce(&mut CompletionSession), cx: &mut Context<Self>) -> bool {
        if !self.popup_visible() {
            return false;
        }
        if let Some(open) = self.open.as_mut() {
            f(&mut open.session);
        }
        self.after_selection(cx);
        true
    }

    /// Escape: closes the popup, or cancels a request that has no popup yet.
    /// `false` when there was neither (Escape is then the editor's).
    pub fn dismiss(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.has_session() {
            return false;
        }
        self.close(CloseReason::Dismissed, cx);
        true
    }

    /// Enter: accepts, or (with `accept_on_enter` off) closes the list.
    pub(crate) fn accept_on_enter(&mut self, cx: &mut Context<Self>) -> EnterOutcome {
        if !self.popup_visible() {
            return EnterOutcome::NotOpen;
        }
        if AppSettings::get(cx).editor.completion.accept_on_enter {
            self.accept(None, cx);
            EnterOutcome::Accepted
        } else {
            self.close(CloseReason::Dismissed, cx);
            EnterOutcome::CloseAndNewline
        }
    }

    /// Accepts the selected item; `then_type` is inserted afterwards (a commit
    /// character). `false` when no popup shows.
    pub fn accept(&mut self, then_type: Option<String>, cx: &mut Context<Self>) -> bool {
        if !self.popup_visible() {
            return false;
        }
        let Some(open) = self.open.as_mut() else {
            return false;
        };
        if open.accepting {
            return true;
        }
        let Some((item_ix, item)) = open.session.selected_item() else {
            return false;
        };
        let id = open.id;
        if !open.session.needs_resolve(item_ix) {
            self.finish_accept(id, item_ix, then_type, cx);
            return true;
        }
        // The item may still add edits (auto-imports): ask, but never wait long.
        let item = item.clone();
        let generation = open.session.generation();
        open.accepting = true;
        open.resolve = None;
        open.resolve_token.cancel();
        let Ok((lsp, path, ..)) = self.server(cx) else {
            self.finish_accept(id, item_ix, then_type, cx);
            return true;
        };
        if let Some(model) = self.model.upgrade() {
            let _ = ensure_synced(&model, cx);
        }
        let token = CancelToken::new();
        let future = lsp.resolve_completion(&path, &item, Some(&token));
        if let Some(open) = self.open.as_mut() {
            open.resolve_token = token.clone();
            open.resolve = Some(cx.spawn(async move |this, cx| {
                let timeout = cx
                    .background_executor()
                    .timer(Duration::from_millis(ACCEPT_RESOLVE_WAIT_MS));
                let resolved =
                    match futures::future::select(Box::pin(future), Box::pin(timeout)).await {
                        Either::Left((Ok(RequestOutcome::Done(r)), _)) => Some(r),
                        _ => None,
                    };
                token.cancel();
                let _ = this.update(cx, |this, cx| {
                    if let (Some(r), Some(open)) =
                        (resolved, this.open.as_mut().filter(|o| o.id == id))
                    {
                        open.session.apply_resolved(generation, item_ix, r);
                    }
                    this.finish_accept(id, item_ix, then_type, cx);
                });
            }));
        }
        true
    }

    fn finish_accept(
        &mut self,
        id: u64,
        item: usize,
        then_type: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(code_view) = self.code_view.upgrade() else {
            return;
        };
        let commit = {
            let Some(open) = self.open.as_ref().filter(|o| o.id == id) else {
                return;
            };
            let Some((rope, ..)) = caret_state(&self.code_view, cx) else {
                return;
            };
            let v = code_view.read(cx);
            let carets: Vec<usize> = v
                .selections()
                .iter()
                .map(|s| edit::pos_to_offset(&rope, s.head))
                .collect();
            open.session.commit(&rope, &carets, v.primary_index(), item)
        };
        let commit = match commit {
            Ok(c) => c,
            Err(err) => {
                // The text moved on: nothing sensible to insert any more.
                tracing::debug!(target: "lector_ui::lsp", "completion not applied: {err}");
                self.close(CloseReason::Dismissed, cx);
                return;
            }
        };
        let applied = apply_commit(&code_view, &commit, cx);
        self.close(CloseReason::Committed, cx);
        if !applied {
            return;
        }
        // The edit's own change event must not read as a caret move.
        if let Some((rope, caret, _)) = caret_state(&self.code_view, cx) {
            self.last_key = Some((rope.version(), caret));
        }
        if commit.triggers_signature_help() {
            self.signature
                .update(cx, |s, cx| s.trigger_after_accept(cx));
        }
        if let Some(text) = then_type {
            code_view.update(cx, |v, cx| v.insert_typed(&text, cx));
        }
    }

    fn scroll_by(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let row_h = metrics::row_height();
        let dy = event.delta.pixel_delta(row_h).y;
        self.scroll_remainder += -(dy / row_h);
        let whole = self.scroll_remainder.trunc();
        if whole == 0. {
            return;
        }
        self.scroll_remainder -= whole;
        let total = self.match_count();
        let max_top = total.saturating_sub(VISIBLE_ROWS);
        let top = (self.list_top as f32 + whole).clamp(0., max_top as f32) as usize;
        if top != self.list_top {
            self.list_top = top;
            cx.notify();
        }
    }

    fn click_row(&mut self, row: usize, cx: &mut Context<Self>) {
        if let Some(open) = self.open.as_mut() {
            open.session.select(row);
        }
        self.accept(None, cx);
    }
}

/// Applies a commit to the view as one undo step: the edits (main and additional) in
/// one `apply_edits`, then every caret. Returns whether the buffer took it.
fn apply_commit(code_view: &Entity<CodeView>, commit: &CompletionCommit, cx: &mut App) -> bool {
    let edits: Vec<lector_buffer::TextEdit> = commit
        .edits
        .iter()
        .map(|e| lector_buffer::TextEdit {
            range: e.range.clone(),
            new_text: e.new_text.clone(),
        })
        .collect();
    let carets = commit.carets.clone();
    code_view.update(cx, |v, cx| {
        let primary = v.primary_index();
        v.edit(cx, move |b| {
            let tx = b.apply_edits(edits);
            let n = carets.len() as u64;
            // Same id scheme as the view's own conversion: the largest id is the primary caret.
            let sels = carets
                .iter()
                .enumerate()
                .map(|(ix, c)| lector_buffer::Selection {
                    id: if ix == primary { n + 1 } else { ix as u64 + 1 },
                    anchor: *c,
                    head: *c,
                    goal: None,
                })
                .collect();
            b.set_selections(sels);
            ((), tx)
        })
        .is_some()
    })
}

fn docs_width() -> Pixels {
    px(320.)
}

/// What to tell the user when completion was asked for but the file has no usable server.
fn no_server_message(lsp: &LspManager, language: &str) -> String {
    let servers: Vec<String> = lector_lsp::builtin_servers()
        .into_iter()
        .filter(|c| c.languages.iter().any(|l| l == language))
        .map(|c| c.name)
        .collect();
    if servers.is_empty() {
        return format!("No language server for {language}");
    }
    let statuses = lsp.status();
    let state_of = |name: &str| {
        statuses
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.state.clone())
    };
    if let Some(name) = servers
        .iter()
        .find(|n| matches!(state_of(n), Some(ServerState::Starting)))
    {
        return format!("{name} is still starting…");
    }
    if let Some((name, ServerState::Failed(err))) = servers.iter().find_map(|n| match state_of(n) {
        Some(s @ ServerState::Failed(_)) => Some((n, s)),
        _ => None,
    }) {
        return format!("{name} failed: {err}");
    }
    for name in &servers {
        if let Some(hint) = lector_lsp::install::install_hint(name) {
            if let Some(option) = hint.primary(lector_lsp::install::Os::current()) {
                return format!(
                    "No {} language server found. Install {name}: {}",
                    hint.language, option.command
                );
            }
        }
    }
    format!(
        "No {language} language server is installed (tried {})",
        servers.join(", ")
    )
}

// ---------------------------------------------------------------------------
// Rendering.

fn letter_badge(letter: &'static str, color: Hsla) -> AnyElement {
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
        .font_weight(gpui::FontWeight::BOLD)
        .child(letter)
        .into_any_element()
}

fn kind_badge(kind: CompletionKind, cx: &App) -> AnyElement {
    match badge_for(kind) {
        Badge::Symbol(symbol) => super::symbol_badge(symbol, cx).into_any_element(),
        Badge::Letter(letter) => {
            let color = match kind {
                CompletionKind::Keyword => cx
                    .theme()
                    .syntax
                    .style_for("keyword")
                    .map(|s| s.color.hsla())
                    .unwrap_or(cx.theme().ui.text_muted.hsla()),
                _ => cx.theme().ui.text_muted.hsla(),
            };
            letter_badge(letter, color)
        }
    }
}

impl CompletionController {
    fn render_row(&self, row: &RowView, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let editor = EditorSettings::get(cx);
        let label: SharedString = row.label.clone().into();
        let runs = label_runs(
            &row.label,
            &row.highlights,
            row.deprecated,
            ui.text_accent.hsla(),
            ui.text_muted.hsla(),
        );
        let styled = StyledText::new(label).with_highlights(runs);
        let index = row.row;
        h_flex()
            .id(("completion-row", index))
            .h(metrics::row_height())
            .px(px(6.))
            .gap(px(6.))
            .flex_none()
            .when(row.selected, |el| el.bg(ui.active.hsla()))
            .when(!row.selected, |el| el.hover(|s| s.bg(ui.hover.hsla())))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.click_row(index, cx)
                }),
            )
            .child(kind_badge(row.kind, cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .gap(px(4.))
                    .font_family(editor.font_family.clone())
                    .text_size(metrics::text())
                    .text_color(if row.deprecated {
                        ui.text_muted.hsla()
                    } else {
                        ui.text.hsla()
                    })
                    .child(styled)
                    .children(row.label_detail.clone().map(|d| {
                        div()
                            .text_color(ui.text_faint.hsla())
                            .text_size(metrics::text_sm())
                            .child(d)
                    })),
            )
            .children(row.description.clone().map(|d| {
                div()
                    .flex_none()
                    .max_w(px(180.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(metrics::text_sm())
                    .text_color(ui.text_faint.hsla())
                    .child(d)
            }))
            .into_any_element()
    }

    fn render_docs(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.open.as_ref()?;
        if open.docs.is_empty() {
            return None;
        }
        let ui = cx.theme().ui.clone();
        let editor = EditorSettings::get(cx);
        let item = open.docs.key.map(|(i, _)| i).unwrap_or(0);
        let detail = open.docs.detail.clone();
        let plain = open.docs.plain.clone();
        let blocks = open.docs.blocks.clone();
        let rendered = blocks.map(|blocks| {
            let opts = RenderOptions {
                base_dir: None,
                on_link: None,
                compact: true,
                id: "completion-docs".into(),
            };
            markdown::render_blocks(&blocks, &opts, window, cx)
        });
        Some(
            v_flex()
                .id(("completion-docs", item))
                .w(docs_width())
                .h_full()
                .flex_none()
                .overflow_y_scroll()
                .border_l_1()
                .border_color(ui.border_variant.hsla())
                .px(px(10.))
                .py(px(8.))
                .gap(px(6.))
                .text_size(metrics::text_sm())
                .text_color(ui.text_muted.hsla())
                .children(detail.map(|d| {
                    div()
                        .font_family(editor.font_family.clone())
                        .text_color(ui.text.hsla())
                        .child(d)
                }))
                .children(plain)
                .children(rendered.into_iter().flatten())
                .into_any_element(),
        )
    }
}

impl Render for CompletionController {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.popup_visible() {
            return div().into_any_element();
        }
        if let Some(t) = self.open.as_mut().and_then(|o| o.reply_at.take()) {
            tracing::debug!(
                target: "lector_ui::perf",
                elapsed_us = t.elapsed().as_micros() as u64,
                "completion reply -> first render"
            );
        }
        let ui = cx.theme().ui.clone();
        let total = self.match_count();
        let rows = self.visible_rows();
        let list: Vec<AnyElement> = rows.iter().map(|r| self.render_row(r, cx)).collect();
        // A thin scroll thumb when the list is longer than the window.
        let thumb = (total > VISIBLE_ROWS).then(|| {
            let height = metrics::row_height() * VISIBLE_ROWS as f32;
            let thumb_h = (height * (VISIBLE_ROWS as f32 / total as f32)).max(px(14.));
            let travel = height - thumb_h;
            let max_top = (total - VISIBLE_ROWS) as f32;
            let y = travel * (self.list_top as f32 / max_top);
            div()
                .absolute()
                .right(px(1.))
                .top(y)
                .w(px(3.))
                .h(thumb_h)
                .rounded(px(2.))
                .bg(ui.scrollbar_thumb.hsla())
        });
        let docs = self.render_docs(window, cx);
        surface(cx)
            .size_full()
            .flex()
            .flex_row()
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                this.scroll_by(event, cx);
                cx.stop_propagation();
            }))
            .child(
                v_flex()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_hidden()
                    .children(list)
                    .children(thumb),
            )
            .children(docs)
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// Wiring: one controller per file tab, the workspace actions.

#[derive(Default)]
struct Registry {
    by_view: HashMap<EntityId, Entity<CompletionController>>,
}

impl Global for Registry {}

/// The completion controller of a file tab.
pub fn controller_for(view: &Entity<FileView>, cx: &App) -> Option<Entity<CompletionController>> {
    cx.try_global::<Registry>()?
        .by_view
        .get(&view.entity_id())
        .cloned()
}

/// Installs completion and signature help for every file tab and the
/// workspace-level action handlers (called by `lsp::init`).
pub fn init(cx: &mut App) {
    if !cx.has_global::<Registry>() {
        cx.set_global(Registry::default());
    }
    cx.observe_new(|_: &mut FileView, _, cx: &mut Context<FileView>| {
        let view = cx.entity();
        // Once the tab is fully built.
        cx.defer(move |cx| attach(&view, cx));
    })
    .detach();
    cx.observe_new(|ws: &mut Workspace, window, _| {
        if window.is_some() {
            register_actions(ws);
        }
    })
    .detach();
}

fn attach(view: &Entity<FileView>, cx: &mut App) {
    let (code_view, model) = {
        let v = view.read(cx);
        (v.code_view().clone(), v.model().clone())
    };
    let signature = {
        let (cv, m) = (code_view.downgrade(), model.downgrade());
        cx.new(|cx| SignatureController::new(cv, m, cx))
    };
    let controller = cx.new(|cx| CompletionController::new(&code_view, &model, signature, cx));
    let id = view.entity_id();
    cx.update_global::<Registry, _>(|r, _| {
        r.by_view.insert(id, controller);
    });
    cx.observe_release(view, move |_, cx| {
        cx.update_global::<Registry, _>(|r, _| {
            r.by_view.remove(&id);
        });
    })
    .detach();
}

/// Runs `f` on the completion controller of the active file tab. `f` returns
/// whether it handled the key; otherwise `fallback` (what the key does without
/// a popup) is dispatched.
fn with_popup(
    ws: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    f: impl FnOnce(&mut CompletionController, &mut Context<CompletionController>) -> bool,
    fallback: Box<dyn gpui::Action>,
) {
    let handled = ws
        .active_file_view(cx)
        .and_then(|view| controller_for(&view, cx))
        .is_some_and(|c| c.update(cx, f));
    if !handled {
        window.dispatch_action(fallback, cx);
    }
}

fn register_actions(ws: &mut Workspace) {
    ws.register_action(|ws, _: &act::SelectNext, window, cx| {
        with_popup(
            ws,
            window,
            cx,
            |c, cx| c.select_next(cx),
            Box::new(MoveDown),
        );
    })
    .register_action(|ws, _: &act::SelectPrev, window, cx| {
        with_popup(
            ws,
            window,
            cx,
            |c, cx| c.select_previous(cx),
            Box::new(MoveUp),
        );
    })
    .register_action(|ws, _: &act::PageDown, window, cx| {
        with_popup(ws, window, cx, |c, cx| c.page_down(cx), Box::new(PageDown));
    })
    .register_action(|ws, _: &act::PageUp, window, cx| {
        with_popup(ws, window, cx, |c, cx| c.page_up(cx), Box::new(PageUp));
    })
    .register_action(|ws, _: &act::Accept, window, cx| {
        with_popup(ws, window, cx, |c, cx| c.accept(None, cx), Box::new(Tab));
    })
    .register_action(|ws, _: &act::AcceptOnEnter, window, cx| {
        let outcome = ws
            .active_file_view(cx)
            .and_then(|view| controller_for(&view, cx))
            .map(|c| c.update(cx, |c, cx| c.accept_on_enter(cx)))
            .unwrap_or(EnterOutcome::NotOpen);
        if outcome != EnterOutcome::Accepted {
            window.dispatch_action(Box::new(Newline), cx);
        }
    })
    .register_action(|ws, _: &act::Dismiss, window, cx| {
        with_popup(
            ws,
            window,
            cx,
            |c, cx| c.dismiss(cx),
            Box::new(ClearSelection),
        );
    })
    .register_action(|ws, _: &TriggerSuggest, _window, cx| {
        let Some(controller) = ws
            .active_file_view(cx)
            .and_then(|view| controller_for(&view, cx))
        else {
            cx.propagate();
            return;
        };
        if let Err(message) = controller.update(cx, |c, cx| c.trigger_manual(cx)) {
            if !message.is_empty() {
                ws.show_toast(message, cx);
            }
        }
    })
    .register_action(|ws, _: &TriggerParameterHints, _window, cx| {
        let Some(controller) = ws
            .active_file_view(cx)
            .and_then(|view| controller_for(&view, cx))
        else {
            cx.propagate();
            return;
        };
        let signature = controller.read(cx).signature().clone();
        if let Err(message) = signature.update(cx, |s, cx| s.trigger_manual(cx)) {
            ws.show_toast(message, cx);
        }
    })
    .register_action(|ws, _: &sig_act::NextSignature, window, cx| {
        with_signature(
            ws,
            window,
            cx,
            |s, cx| s.next_signature(cx),
            Box::new(MoveDown),
        );
    })
    .register_action(|ws, _: &sig_act::PrevSignature, window, cx| {
        with_signature(
            ws,
            window,
            cx,
            |s, cx| s.previous_signature(cx),
            Box::new(MoveUp),
        );
    })
    .register_action(|ws, _: &sig_act::Dismiss, window, cx| {
        with_signature(
            ws,
            window,
            cx,
            |s, cx| s.dismiss(cx),
            Box::new(ClearSelection),
        );
    });
}

fn with_signature(
    ws: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    f: impl FnOnce(&mut SignatureController, &mut Context<SignatureController>) -> bool,
    fallback: Box<dyn gpui::Action>,
) {
    let handled = ws
        .active_file_view(cx)
        .and_then(|view| controller_for(&view, cx))
        .map(|c| c.read(cx).signature().clone())
        .is_some_and(|s| s.update(cx, f));
    if !handled {
        window.dispatch_action(fallback, cx);
    }
}

#[cfg(test)]
mod tests;
