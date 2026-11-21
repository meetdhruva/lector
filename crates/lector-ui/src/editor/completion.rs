//! The completion popup of file tabs (v0.7).
//!
//! One [`CompletionController`] per file tab (created for every `FileView` by
//! [`init`], found again with [`controller_for`]) owns a
//! [`lector_lsp::CompletionSession`] and drives it by the rules in that type's module
//! documentation. It is also the popup's view:
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
//!   While an acceptance waits for the server it is a [`PendingAccept`] of its
//!   own (the documentation resolve and every selection change leave it alone):
//!   plain typed text is held back and typed right after the commit, in order,
//!   the commit character first; the list does not move. Any other input (a
//!   command, a click, a composition) first finishes the acceptance through the
//!   view's command hook ([`finish_before_command`]: the item as it stands, then
//!   the held-back text, then the command; the late answer is dropped); Escape
//!   cancels the acceptance (nothing is committed, the held-back text is typed);
//!   a failed, stale or missing answer commits the item as the server listed it.
//! * **Snippets** (`editor.completion.snippets`): there is no tab-stop session
//!   in the editor. The caret is placed at the first stop, the rest are ignored.

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::Either;
use gpui::{
    App, Context, Entity, Pixels, ScrollWheelEvent, SharedString, Size, Subscription, Task,
    WeakEntity,
};
use lector_base::CancelToken;
use lector_lsp::{
    CloseReason, CompletionResponse, CompletionSession, InsertMode, LspError, LspManager,
    OpenReason, RequestOutcome, ResolvedCompletion, ServerState, SessionOptions, Update, WordChars,
};
use lector_text::Rope;

use super::signature_help::SignatureController;
use super::DocumentModel;
use crate::code_view::{
    edit, popup::DismissHook, CodeView, CodeViewEvent, EditorPopup, EditorSettings, Pos, Vertical,
};
use crate::components::metrics;
use crate::markdown::{self, Block};
use crate::scale::px;
use crate::settings::AppSettings;

mod commit;
mod render;
mod rows;
mod wiring;

use commit::{apply_commit, clear_popup_state, commit_at, finish_before_command, EarlyFinish};
pub use rows::{badge_for, label_runs, list_width, row_views, scroll_to_show, Badge, RowView};
pub use wiring::{controller_for, init};

/// Popup key in [`CodeView::set_popup`].
pub const POPUP_KEY: &str = "completion";
/// Key context flag set while the popup shows.
pub const FLAG_OPEN: &str = "completion";
/// Key context flag set while a session waits for the server's first answer
/// (only Escape is bound: it cancels the request).
pub const FLAG_PENDING: &str = "completion_pending";
/// Key context flag set while an acceptance waits for the server: it arms the view's
/// [`CommandHook`](crate::code_view::CommandHook) that finishes the acceptance first.
pub const FLAG_ACCEPTING: &str = "completion_accepting";
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

type ResolveFuture = std::pin::Pin<
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
    /// The documentation resolve of the selected item (replaced on every selection change).
    resolve: Option<Task<()>>,
    resolve_token: CancelToken,
    docs: Docs,
    /// When the server's answer reached the UI thread, until the first render logged it.
    reply_at: Option<Instant>,
    /// An acceptance waiting for the resolve answer (never touched by selection changes).
    accept: Option<PendingAccept>,
}

/// An acceptance that waits (at most `ACCEPT_RESOLVE_WAIT_MS`) for `completionItem/resolve`.
struct PendingAccept {
    /// Applies the commit when the answer arrived or the wait ran out. Dropping it cancels.
    _task: Task<()>,
    token: CancelToken,
    /// The row's item index in the session.
    item: usize,
    /// Text typed meanwhile (the commit character first): typed after the commit, in order.
    queued: Vec<String>,
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
                let Some(controller) = weak.upgrade() else {
                    return false;
                };
                // An acceptance is waiting for the server: hold the text back, it is typed
                // right after the commit.
                if controller.update(cx, |this, _| this.hold_back(text)) {
                    return true;
                }
                let Some(c) = single_char(text) else {
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
        // Anything but plain text (a command, a click, a composition) first finishes a
        // pending acceptance, so the user's input applies in the order it was given.
        let weak = this.clone();
        code_view.update(cx, |v, _| {
            v.set_command_hook(Some((
                FLAG_ACCEPTING,
                Rc::new(move |view, cx| finish_before_command(&weak, view, cx)),
            )));
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
                .is_some_and(|o| o.accept.is_none() && o.session.is_commit_character(c))
    }

    /// An acceptance is waiting for the server's answer.
    pub fn is_accepting(&self) -> bool {
        self.open.as_ref().is_some_and(|o| o.accept.is_some())
    }

    /// The session id and item of the pending acceptance.
    fn pending_accept(&self) -> Option<(u64, usize)> {
        let open = self.open.as_ref()?;
        open.accept.as_ref().map(|p| (open.id, p.item))
    }

    /// Finishes a pending acceptance right now (see [`CompletionController::finish_accept`]):
    /// the item as it stands, then the held-back text. `false` when none was pending.
    fn finish_pending(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((id, item)) = self.pending_accept() else {
            return false;
        };
        self.finish_accept(id, item, Vec::new(), cx);
        true
    }

    /// While an acceptance is pending: keeps `text` to type after the commit. `false` when
    /// none is pending (the text is typed as usual).
    fn hold_back(&mut self, text: &str) -> bool {
        match self.open.as_mut().and_then(|o| o.accept.as_mut()) {
            Some(pending) => {
                pending.queued.push(text.to_string());
                true
            }
            None => false,
        }
    }

    /// Types `chunks` at the carets as keystrokes would (held-back text after a commit or a
    /// cancelled acceptance).
    fn type_held_back(&self, chunks: Vec<String>, cx: &mut Context<Self>) {
        if chunks.is_empty() {
            return;
        }
        if let Some(code_view) = self.code_view.upgrade() {
            code_view.update(cx, |v, cx| {
                for chunk in &chunks {
                    v.insert_typed(chunk, cx);
                }
            });
        }
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
        // A commit is on its way: the list it belongs to stays until it landed (or Escape).
        if self.is_accepting() {
            return Ok(());
        }
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
            accept: None,
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
        // The view's commands finish a pending acceptance before they run. A change that
        // got here with one still pending came from outside (another view, a programmatic
        // edit): finish it now; the commit checks that the text still fits.
        if let Some((id, item)) = self.pending_accept() {
            self.finish_accept(id, item, Vec::new(), cx);
        }
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
        // Documentation only: a pending acceptance has its own task (`open.accept`).
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
        let held_back = self.end_session(reason);
        if let Some(code_view) = self.code_view.upgrade() {
            let any = {
                let v = code_view.read(cx);
                v.has_popup(POPUP_KEY)
                    || v.has_context_flag(FLAG_OPEN)
                    || v.has_context_flag(FLAG_PENDING)
                    || v.has_context_flag(FLAG_ACCEPTING)
            };
            if any {
                code_view.update(cx, clear_popup_state);
            }
        }
        self.type_held_back(held_back, cx);
        cx.notify();
    }

    /// The part of [`CompletionController::close`] that does not touch the view: drops the
    /// session, cancels its requests and a pending acceptance (and its resolve answer).
    /// Returns the text held back by that acceptance.
    fn end_session(&mut self, reason: CloseReason) -> Vec<String> {
        let mut held_back = Vec::new();
        if let Some(mut open) = self.open.take() {
            open.token.cancel();
            open.resolve_token.cancel();
            // Closing under a pending acceptance cancels it; what was typed meanwhile is not lost.
            if let Some(pending) = open.accept.take() {
                pending.token.cancel();
                held_back = pending.queued;
            }
            open.session.close(reason);
            self.suppress = matches!(
                reason,
                CloseReason::Dismissed | CloseReason::NoMatches | CloseReason::Committed
            );
        }
        self.list_top = 0;
        self.scroll_remainder = 0.;
        held_back
    }

    /// A view command is about to run with an acceptance pending: takes the acceptance over
    /// (the late resolve answer is dropped with its task) and ends the session. What is left
    /// for the view to apply, in order: the item as it stands, then the held-back text.
    fn take_pending_for_command(&mut self, view: &CodeView, cx: &App) -> Option<EarlyFinish> {
        let open = self.open.as_mut()?;
        let pending = open.accept.take()?;
        pending.token.cancel();
        let commit = commit_at(&open.session, view, pending.item, cx);
        let reason = if commit.is_some() {
            CloseReason::Committed
        } else {
            CloseReason::Dismissed
        };
        self.end_session(reason);
        Some(EarlyFinish {
            commit,
            typed: pending.queued,
        })
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
        // Moving the selection is a command: not handled here, so the key's plain action
        // runs on the view, whose command hook finishes the acceptance first.
        if self.is_accepting() {
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
        // Enter while an acceptance waits is a command like any other: report "not open", so
        // the line break is dispatched to the view, whose command hook finishes the
        // acceptance first.
        if self.is_accepting() {
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
    ///
    /// An item that may still add edits is resolved first, for at most
    /// `ACCEPT_RESOLVE_WAIT_MS`. Meanwhile the acceptance is `Open::accept`: text typed
    /// (and `then_type`) is held back and typed after the commit; Escape cancels it.
    pub fn accept(&mut self, then_type: Option<String>, cx: &mut Context<Self>) -> bool {
        if !self.popup_visible() {
            return false;
        }
        let Some(open) = self.open.as_mut() else {
            return false;
        };
        if let Some(pending) = open.accept.as_mut() {
            // A commit character typed meanwhile is held back like any other text. Enter
            // and Tab are commands: they finish the acceptance and then do their own thing.
            return match then_type {
                Some(text) => {
                    pending.queued.push(text);
                    true
                }
                // Not handled: the key's plain action runs on the view, whose command hook
                // finishes the acceptance first.
                None => false,
            };
        }
        let Some((item_ix, item)) = open.session.selected_item() else {
            return false;
        };
        let id = open.id;
        if !open.session.needs_resolve(item_ix) {
            self.finish_accept(id, item_ix, then_type.into_iter().collect(), cx);
            return true;
        }
        // The item may still add edits (auto-imports): ask, but never wait long.
        let item = item.clone();
        let generation = open.session.generation();
        open.resolve = None;
        open.resolve_token.cancel();
        let Ok((lsp, path, ..)) = self.server(cx) else {
            self.finish_accept(id, item_ix, then_type.into_iter().collect(), cx);
            return true;
        };
        if let Some(model) = self.model.upgrade() {
            let _ = ensure_synced(&model, cx);
        }
        let token = CancelToken::new();
        let future = lsp.resolve_completion(&path, &item, Some(&token));
        let wait_token = token.clone();
        let task = cx.spawn(async move |this, cx| {
            let timeout = cx
                .background_executor()
                .timer(Duration::from_millis(ACCEPT_RESOLVE_WAIT_MS));
            let resolved = match futures::future::select(Box::pin(future), Box::pin(timeout)).await
            {
                Either::Left((Ok(RequestOutcome::Done(r)), _)) => Some(r),
                // A failed, cancelled or missing answer: the item as listed.
                _ => None,
            };
            wait_token.cancel();
            let _ = this.update(cx, |this, cx| {
                // Finished early by a command or a cancel meanwhile: the answer is moot.
                if this.pending_accept() != Some((id, item_ix)) {
                    return;
                }
                if let (Some(r), Some(open)) = (resolved, this.open.as_mut().filter(|o| o.id == id))
                {
                    open.session.apply_resolved(generation, item_ix, r);
                }
                this.finish_accept(id, item_ix, Vec::new(), cx);
            });
        });
        if let Some(open) = self.open.as_mut() {
            open.accept = Some(PendingAccept {
                _task: task,
                token,
                item: item_ix,
                queued: then_type.into_iter().collect(),
            });
        }
        if let Some(code_view) = self.code_view.upgrade() {
            code_view.update(cx, |v, cx| v.set_context_flag(FLAG_ACCEPTING, true, cx));
        }
        true
    }

    /// Applies the commit of `item`, closes the list, and types `then_type` followed by
    /// whatever was held back during a pending acceptance.
    fn finish_accept(
        &mut self,
        id: u64,
        item: usize,
        then_type: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(code_view) = self.code_view.upgrade() else {
            return;
        };
        let mut then_type = then_type;
        if let Some(pending) = self
            .open
            .as_mut()
            .filter(|o| o.id == id)
            .and_then(|o| o.accept.take())
        {
            then_type.extend(pending.queued);
        }
        let commit = {
            let Some(open) = self.open.as_ref().filter(|o| o.id == id) else {
                // Closed meanwhile (`close` typed what was held back).
                return;
            };
            commit_at(&open.session, code_view.read(cx), item, cx)
        };
        let Some(commit) = commit else {
            // The text moved on: nothing sensible to insert any more.
            self.close(CloseReason::Dismissed, cx);
            self.type_held_back(then_type, cx);
            return;
        };
        let applied = apply_commit(&code_view, &commit, cx);
        self.close(CloseReason::Committed, cx);
        if !applied {
            self.type_held_back(then_type, cx);
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
        self.type_held_back(then_type, cx);
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
        if self.finish_pending(cx) {
            return;
        }
        if let Some(open) = self.open.as_mut() {
            open.session.select(row);
        }
        self.accept(None, cx);
    }
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

#[cfg(test)]
mod tests;
