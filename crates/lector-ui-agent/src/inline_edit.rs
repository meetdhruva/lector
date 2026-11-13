//! Inline agent edit (v0.7): Ctrl/Cmd+I on a selection (or the current line) asks the agent for
//! a change in place and shows the result as a diff inside the file's own tab.
//!
//! This module is the driver. The state machine (`lector_agent::inline_edit::InlineEdit`) decides
//! what is true; the in-place diff (`lector_ui::editor::inline_diff`) draws it; this module
//! connects them to the Agent panel's session, the file's buffer and the workspace:
//!
//! * [`InlineEdits`] (one per workspace window) holds one [`Active`] edit per file.
//! * Submitting sends a normal agent turn (checkpoint, journal, permission prompts and
//!   confinement all apply) with the focused prompt of `InlineEdit::submit`.
//! * Events of the turn update the status text; when the turn ends the model computes hunks
//!   against the baseline and the tab shows them with Keep / Undo buttons.
//! * Edits the user makes in the buffer are fed to the model (conflict detection); the agent's
//!   own write (an edit without an origin view while the turn runs) is not a user edit.
//! * Undoing a hunk is one buffer edit (one undo step) that leaves the file unsaved; keeping one
//!   writes nothing (the text is already on disk).
//!
//! Why the agent's write lands first and the editor shows a diff against the baseline (instead of
//! holding the proposal back as an overlay) is documented in `docs/ARCHITECTURE.md`, "Inline agent
//! edit and turn summaries (v0.7)".

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use futures::StreamExt;
use gpui::{
    div, prelude::*, App, Context, Corner, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, KeyDownEvent, SharedString, Subscription, Task, WeakEntity, Window,
};
use lector_agent::inline_edit::{
    availability, status_line, AvailabilityInput, EditEffect, EditSpan, FallbackReason, InlineEdit,
    InlineError, InlineOutcome, InlinePhase, InlinePrompt, InlineRequest, Unavailable,
};
use lector_agent::{AgentSession, SessionStatus, TurnId};
use lector_ui::actions::agent as agent_actions;
use lector_ui::code_view::edit::{pos_to_offset, EditBufferEvent};
use lector_ui::code_view::Pos;
use lector_ui::components::popover::surface;
use lector_ui::components::text_area::TextArea;
use lector_ui::components::{h_flex, metrics, v_flex};
use lector_ui::editor::inline_diff::{
    InlineActionHandler, InlineEditAction, InlineEditPhase, InlineEditUi, InlineHunkView,
    InlineNoteUi,
};
use lector_ui::editor::{DocumentModel, FileView};
use lector_ui::scale::px;
use lector_ui::views::workspace::ToastLevel;
use lector_ui::views::Workspace;
use lector_ui::{ActiveTheme, ColorExt};

use crate::AgentPanel;

/// How long to wait for the agent to start when the prompt is sent before a session exists.
const SESSION_START_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------------------------
// The prompt box
// ---------------------------------------------------------------------------------------------

/// Events of [`InlinePromptBox`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptEvent {
    /// Enter: send this instruction.
    Submit(String),
    /// Escape: give up.
    Cancelled,
}

/// The small prompt box anchored at the selection: instruction, which agent, what context.
pub struct InlinePromptBox {
    area: Entity<TextArea>,
    agent: SharedString,
    context: SharedString,
    follow_up: bool,
}

impl EventEmitter<PromptEvent> for InlinePromptBox {}
impl EventEmitter<DismissEvent> for InlinePromptBox {}

impl Focusable for InlinePromptBox {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.area.read(cx).focus_handle(cx)
    }
}

impl InlinePromptBox {
    /// A box for `agent` with a context chip; `text` pre-fills it (after a refused prompt).
    pub fn new(
        agent: impl Into<SharedString>,
        context: impl Into<SharedString>,
        follow_up: bool,
        text: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let area = cx.new(|cx| {
            let mut a = TextArea::new(
                if follow_up {
                    "Refine the pending changes\u{2026}"
                } else {
                    "What should change?"
                },
                cx,
            );
            a.set_line_limits(1, 6, cx);
            if let Some(t) = text {
                a.set_text(t, cx);
            }
            a
        });
        InlinePromptBox {
            area,
            agent: agent.into(),
            context: context.into(),
            follow_up,
        }
    }

    /// What the Enter key does with `text` (pure, tested): `None` when there is nothing to send.
    pub fn instruction_from(text: &str) -> Option<String> {
        let t = text.trim();
        (!t.is_empty()).then(|| t.to_string())
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.area.read(cx).text().to_string();
        if let Some(instruction) = Self::instruction_from(&text) {
            cx.emit(PromptEvent::Submit(instruction));
            cx.emit(DismissEvent);
        }
    }
}

impl Render for InlinePromptBox {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        surface(cx)
            .id("inline-prompt-box")
            .border_color(ui.border_focused.hsla())
            .w(px(420.))
            .capture_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                let m = &e.keystroke.modifiers;
                match e.keystroke.key.as_str() {
                    // Enter sends; Shift+Enter (a newline for the text area) passes through.
                    "enter" if !m.shift => {
                        this.submit(cx);
                        cx.stop_propagation();
                    }
                    "escape" => {
                        cx.emit(PromptEvent::Cancelled);
                        cx.emit(DismissEvent);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .on_mouse_down_out(cx.listener(|_, _, _, cx| {
                cx.emit(PromptEvent::Cancelled);
                cx.emit(DismissEvent);
            }))
            .child(
                v_flex()
                    .px(px(8.))
                    .py(px(6.))
                    .gap(px(4.))
                    .child(
                        h_flex()
                            .gap(px(6.))
                            .text_size(metrics::text_xs())
                            .text_color(ui.text_muted.hsla())
                            .child(format!(
                                "{} with {}",
                                if self.follow_up { "Refine" } else { "Edit" },
                                self.agent
                            ))
                            .child(
                                div()
                                    .px(px(6.))
                                    .rounded(metrics::radius_lg())
                                    .bg(ui.surface.hsla())
                                    .child(self.context.clone()),
                            ),
                    )
                    .child(div().text_size(metrics::text()).child(self.area.clone()))
                    .child(
                        div()
                            .text_size(metrics::text_xs())
                            .text_color(ui.text_faint.hsla())
                            .child("Enter to send, Shift+Enter for a new line, Escape to cancel"),
                    ),
            )
    }
}

// ---------------------------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------------------------

/// One inline edit of one file.
struct Active {
    edit: InlineEdit,
    view: WeakEntity<FileView>,
    model: WeakEntity<DocumentModel>,
    agent: SharedString,
    status: SharedString,
    /// The instruction was submitted but the turn has not started (the agent is starting).
    submitted: bool,
    finishing: bool,
    session: Option<AgentSession>,
    shown_permission: bool,
    _session_events: Option<lector_agent::Subscription>,
    _event_task: Option<Task<()>>,
    _buffer_sub: Option<Subscription>,
}

/// The inline edits of one workspace window.
pub struct InlineEdits {
    me: WeakEntity<InlineEdits>,
    workspace: WeakEntity<Workspace>,
    panel: WeakEntity<AgentPanel>,
    active: HashMap<PathBuf, Active>,
}

impl InlineEdits {
    /// The driver of a workspace window.
    pub fn new(
        me: WeakEntity<InlineEdits>,
        workspace: WeakEntity<Workspace>,
        panel: WeakEntity<AgentPanel>,
    ) -> Self {
        InlineEdits {
            me,
            workspace,
            panel,
            active: HashMap::new(),
        }
    }

    /// Whether `path` has an inline edit that is not finished (tests).
    pub fn has_edit(&self, path: &Path) -> bool {
        self.active.contains_key(path)
    }

    fn toast(&self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast_level(message, ToastLevel::Success, cx);
    }

    /// A note that nothing happened (no check mark).
    fn note(&self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast_level(message, ToastLevel::Info, cx);
    }

    /// Something did not work or needs a look.
    fn warn(&self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast_level(message, ToastLevel::Warning, cx);
    }

    fn toast_level(
        &self,
        message: impl Into<SharedString>,
        level: ToastLevel,
        cx: &mut Context<Self>,
    ) {
        let message = message.into();
        // Deferred: this runs inside the workspace's own action handlers, which hold its lease.
        let ws = self.workspace.clone();
        cx.defer(move |cx| {
            let _ = ws.update(cx, |ws, cx| ws.show_toast_level(message, level, cx));
        });
    }

    // -- starting ---------------------------------------------------------------------------

    /// `agent::EditWithAgent`.
    pub fn start(&mut self, ws: &Workspace, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = ws.active_file_view(cx) else {
            return self.note("Open a text file to edit it with the agent.", cx);
        };
        let path = view.read(cx).path().to_path_buf();
        // The prompt box of a pending edit just comes back.
        if let Some(a) = self.active.get(&path) {
            if matches!(a.edit.phase(), InlinePhase::Prompting) && !a.submitted {
                return self.open_prompt(&path, false, None, window, cx);
            }
        }
        let model = view.read(cx).model().clone();
        let cv = view.read(cx).code_view().clone();
        let busy = self
            .panel
            .upgrade()
            .and_then(|p| p.read(cx).session().map(|s| s.status()))
            .is_some_and(|s| matches!(s, SessionStatus::Busy(_)));
        let input = AvailabilityInput {
            is_text_file: view.read(cx).is_text_flag().get() && cv.read(cx).buffer().is_some(),
            read_only: cv.read(cx).is_read_only(),
            agent_configured: !lector_agent::commands::configs().is_empty(),
            has_unresolved_edit: self.active.contains_key(&path),
            session_busy: busy,
        };
        if let Err(why) = availability(&input) {
            return self.unavailable(why, window, cx);
        }
        let Some(buffer) = model.read(cx).buffer().cloned() else {
            return;
        };
        let rope = buffer.read(cx).text().clone();
        let (a, b) = cv.read(cx).selection().range();
        let selection = pos_to_offset(&rope, a)..pos_to_offset(&rope, b);
        let rel = ws
            .project()
            .relative_path(&path)
            .unwrap_or_else(|| path.display().to_string());
        let request = InlineRequest::from_selection(&rel, &rope.to_string(), selection);
        let agent = self.agent_name(cx);
        let edit = InlineEdit::new(request);
        let sub = cx.subscribe_in(&buffer, window, {
            let path = path.clone();
            move |this, buffer, event: &EditBufferEvent, window, cx| {
                this.buffer_edited(&path, buffer, event, window, cx)
            }
        });
        self.active.insert(
            path.clone(),
            Active {
                edit,
                view: view.downgrade(),
                model: model.downgrade(),
                agent,
                status: SharedString::default(),
                submitted: false,
                finishing: false,
                session: None,
                shown_permission: false,
                _session_events: None,
                _event_task: None,
                _buffer_sub: Some(sub),
            },
        );
        self.open_prompt(&path, false, None, window, cx);
    }

    fn agent_name(&self, cx: &App) -> SharedString {
        let Some(panel) = self.panel.upgrade() else {
            return "the agent".into();
        };
        let panel = panel.read(cx);
        panel
            .session()
            .map(|s| s.config().name.clone())
            .or_else(|| panel.selected().map(|c| c.name.clone()))
            .or_else(|| {
                lector_agent::commands::configs()
                    .first()
                    .map(|c| c.name.clone())
            })
            .map_or_else(|| "the agent".into(), SharedString::from)
    }

    fn unavailable(&mut self, why: Unavailable, window: &mut Window, cx: &mut Context<Self>) {
        let _ = window;
        if why == Unavailable::NoAgent {
            let ws = self.workspace.clone();
            cx.defer(move |cx| {
                let _ = ws.update(cx, |ws, cx| {
                    ws.show_action_toast_level(
                        why.reason(),
                        ToastLevel::Info,
                        "Set up an agent",
                        Duration::from_secs(8),
                        |_, window, cx| {
                            window.dispatch_action(Box::new(agent_actions::NewSession), cx)
                        },
                        cx,
                    );
                });
            });
        } else {
            self.note(why.reason(), cx);
        }
    }

    fn open_prompt(
        &mut self,
        path: &Path,
        follow_up: bool,
        text: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(a) = self.active.get(path) else {
            return;
        };
        let Some(view) = a.view.upgrade() else {
            return;
        };
        let context = format!(
            "{} {}",
            a.edit.request().path,
            a.edit.request().range_label()
        );
        let agent = a.agent.clone();
        let cv = view.read(cx).code_view().clone();
        let row = a.edit.request().lines.start;
        let anchor = cv
            .read(cx)
            .point_for_pos(Pos { row, col: 0 })
            .map(|p| gpui::point(p.x, p.y + cv.read(cx).line_height().unwrap_or(px(20.))))
            .unwrap_or_else(|| window.mouse_position());
        let prompt = cx.new(|cx| InlinePromptBox::new(agent, context, follow_up, text, cx));
        let path = path.to_path_buf();
        cx.subscribe_in(
            &prompt,
            window,
            move |this, _, ev: &PromptEvent, window, cx| match ev {
                PromptEvent::Submit(text) => {
                    this.submit(&path, text.clone(), follow_up, window, cx)
                }
                PromptEvent::Cancelled => this.prompt_cancelled(&path, follow_up, cx),
            },
        )
        .detach();
        cv.update(cx, |v, cx| {
            v.show_overlay(prompt, anchor, Corner::TopLeft, window, cx)
        });
    }

    fn prompt_cancelled(&mut self, path: &Path, follow_up: bool, cx: &mut Context<Self>) {
        if follow_up {
            return;
        }
        if let Some(a) = self.active.get_mut(path) {
            if a.edit.cancel_prompt() {
                self.active.remove(path);
            }
        }
        cx.notify();
    }

    // -- sending ----------------------------------------------------------------------------

    fn submit(
        &mut self,
        path: &Path,
        text: String,
        follow_up: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(a) = self.active.get_mut(path) else {
            return;
        };
        let prompt: Result<InlinePrompt, InlineError> = if follow_up {
            a.edit.prepare_follow_up(&text)
        } else {
            a.edit.submit(&text)
        };
        let prompt = match prompt {
            Ok(p) => p,
            Err(InlineError::EmptyInstruction) => {
                return self.open_prompt(path, follow_up, Some(text), window, cx)
            }
            Err(e) => return self.warn(e.to_string(), cx),
        };
        a.submitted = true;
        a.status = "Starting the agent\u{2026}".into();
        self.push_ui(path, cx);
        // A session may have to start first: wait for it, then send.
        if let Some(panel) = self.panel.upgrade() {
            if panel.read(cx).session().is_none() {
                panel.update(cx, |p, cx| p.start(cx));
            }
        }
        let path = path.to_path_buf();
        cx.spawn_in(window, async move |this, cx| {
            let deadline = std::time::Instant::now() + SESSION_START_TIMEOUT;
            let session = loop {
                let found = this
                    .update(cx, |this, cx| {
                        let panel = this.panel.upgrade()?;
                        let p = panel.read(cx);
                        match p.session().map(|s| (s.clone(), s.status())) {
                            Some((s, SessionStatus::Ready)) => Some(Ok(s)),
                            Some((_, SessionStatus::Busy(_))) => {
                                Some(Err("The agent is busy with another request.".to_string()))
                            }
                            Some((_, SessionStatus::Crashed(why))) => Some(Err(why)),
                            _ => p.error.as_ref().map(|e| Err(e.to_string())),
                        }
                    })
                    .ok()
                    .flatten();
                match found {
                    Some(r) => break r,
                    None if std::time::Instant::now() > deadline => {
                        break Err("The agent did not start in time.".to_string())
                    }
                    None => {
                        cx.background_executor()
                            .timer(Duration::from_millis(100))
                            .await
                    }
                }
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.send(&path, text, follow_up, prompt, session, window, cx)
            });
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn send(
        &mut self,
        path: &Path,
        text: String,
        follow_up: bool,
        prompt: InlinePrompt,
        session: Result<AgentSession, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(a) = self.active.get_mut(path) else {
            return;
        };
        a.submitted = false;
        let sent = session.and_then(|s| {
            s.prompt(prompt.text, prompt.context)
                .map(|t| (s, t))
                .map_err(|e| e.to_string())
        });
        match sent {
            Err(why) => {
                a.status = SharedString::default();
                self.push_ui(path, cx);
                self.warn(format!("Could not send the request: {why}"), cx);
                self.open_prompt(path, follow_up, Some(text), window, cx);
            }
            Ok((session, turn)) => {
                if follow_up {
                    a.edit.follow_up_started(&text, turn);
                } else {
                    a.edit.turn_started(&text, turn);
                }
                a.status = "Working\u{2026}".into();
                a.finishing = false;
                a.shown_permission = false;
                a.session = Some(session.clone());
                if a._session_events.is_none() {
                    let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
                    a._session_events = Some(session.subscribe(move |_| {
                        let _ = tx.unbounded_send(());
                    }));
                    let path = path.to_path_buf();
                    a._event_task = Some(cx.spawn_in(window, async move |this, cx| {
                        while rx.next().await.is_some() {
                            cx.background_executor()
                                .timer(Duration::from_millis(40))
                                .await;
                            while rx.try_recv().is_ok() {}
                            let ok = this
                                .update_in(cx, |this, window, cx| {
                                    this.on_session_event(&path, window, cx)
                                })
                                .is_ok();
                            if !ok {
                                break;
                            }
                        }
                    }));
                }
                self.push_ui(path, cx);
            }
        }
    }

    // -- the turn ---------------------------------------------------------------------------

    fn on_session_event(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some(a) = self.active.get_mut(path) else {
            return;
        };
        let (Some(turn_id), Some(session)) = (a.edit.turn(), a.session.clone()) else {
            return;
        };
        if !matches!(a.edit.phase(), InlinePhase::Running { .. }) {
            return;
        }
        let Some(turn) = session.turn(turn_id) else {
            return;
        };
        a.status = status_line(&turn).into();
        let waiting = turn.permissions.iter().any(|p| p.pending);
        if waiting && !a.shown_permission {
            a.shown_permission = true;
            if let Some(ws) = self.workspace.upgrade() {
                window.defer(cx, move |window, cx| {
                    ws.update(cx, |ws, cx| crate::reveal(ws, window, cx));
                });
            }
        }
        if turn.state.is_finished() && !a.finishing {
            a.finishing = true;
            self.finish_when_synced(path, turn_id, window, cx);
        }
        self.push_ui(path, cx);
    }

    /// The turn is over: wait (briefly) until the buffer shows what is on disk, then compute the
    /// hunks. The agent's write goes through the shared document, so this normally needs no wait;
    /// a file written around the document is picked up by a reload when the buffer is clean.
    fn finish_when_synced(
        &mut self,
        path: &Path,
        _turn: TurnId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = path.to_path_buf();
        cx.spawn_in(window, async move |this, cx| {
            let mut nudged = false;
            let mut buffer_text = String::new();
            for _ in 0..10 {
                let snapshot = this.update(cx, |this, cx| {
                    let a = this.active.get(&path)?;
                    let model = a.model.upgrade()?;
                    let m = model.read(cx);
                    let text = m.buffer()?.read(cx).text().to_string();
                    Some((text, m.is_dirty(cx), a.edit.request().path.clone(), model))
                });
                let Ok(Some((text, dirty, rel, model))) = snapshot else {
                    return;
                };
                buffer_text = text;
                let abs = path.clone();
                let _ = rel;
                let disk = cx
                    .background_executor()
                    .spawn(async move { std::fs::read_to_string(abs).ok() })
                    .await;
                if dirty || disk.as_deref().is_none_or(|d| d == buffer_text) {
                    break;
                }
                if !nudged {
                    nudged = true;
                    let _ = model.update(cx, |m, cx| m.file_changed_on_disk(cx));
                }
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                this.finish(&path, buffer_text, window, cx)
            });
        })
        .detach();
    }

    fn finish(
        &mut self,
        path: &Path,
        buffer_text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(a) = self.active.get_mut(path) else {
            return;
        };
        let (Some(turn), Some(session)) = (a.edit.turn(), a.session.clone()) else {
            return;
        };
        a.edit.finish_from_session(&session, turn, buffer_text);
        let phase = a.edit.phase().clone();
        if let InlinePhase::Done(InlineOutcome::FellBack(reason)) = phase {
            return self.fall_back(path, reason, window, cx);
        }
        self.push_ui(path, cx);
    }

    fn fall_back(
        &mut self,
        path: &Path,
        reason: FallbackReason,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let turn = self.active.get(path).and_then(|a| a.edit.turn());
        self.clear(path, cx);
        self.warn(reason.message(), cx);
        if let (Some(turn), Some(panel)) = (turn, self.panel.upgrade()) {
            panel.update(cx, |p, cx| p.review_turn(turn, window, cx));
        }
    }

    // -- buffer edits -----------------------------------------------------------------------

    fn buffer_edited(
        &mut self,
        path: &Path,
        buffer: &Entity<lector_ui::code_view::EditBuffer>,
        event: &EditBufferEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let EditBufferEvent::Edited {
            origin,
            transaction,
        } = event;
        let Some(a) = self.active.get_mut(path) else {
            return;
        };
        if transaction.is_empty() {
            return;
        }
        let running = matches!(a.edit.phase(), InlinePhase::Running { .. });
        // While the turn runs, an edit without a view origin is the agent's own write.
        if running && origin.is_none() {
            return;
        }
        let new_text = buffer.read(cx).text().to_string();
        // Our own undo of a hunk: the model already moved on.
        if matches!(a.edit.phase(), InlinePhase::Reviewing) && new_text == a.edit.current_text() {
            return;
        }
        let spans: Vec<EditSpan> = transaction
            .deltas
            .iter()
            .rev()
            .map(|d| EditSpan {
                old: d.start_byte..d.old_end_byte,
                new_len: d.new_end_byte.saturating_sub(d.start_byte),
            })
            .collect();
        match a.edit.user_edited(&spans, &new_text) {
            EditEffect::Conflict(reason) => self.fall_back(path, reason, window, cx),
            EditEffect::SelectionEdited | EditEffect::None => self.push_ui(path, cx),
        }
    }

    // -- showing ----------------------------------------------------------------------------

    fn clear(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Some(a) = self.active.remove(path) {
            if let Some(view) = a.view.upgrade() {
                view.update(cx, |v, cx| v.set_inline_edit(None, cx));
            }
        }
    }

    /// Pushes what the tab should show.
    fn push_ui(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(a) = self.active.get(path) else {
            return;
        };
        let Some(view) = a.view.upgrade() else {
            return;
        };
        let ui = self.build_ui(path, a, cx);
        view.update(cx, |v, cx| v.set_inline_edit(Some(ui), cx));
    }

    fn build_ui(&self, path: &Path, a: &Active, _cx: &App) -> InlineEditUi {
        let edit = &a.edit;
        let mut notes: Vec<InlineNoteUi> = edit
            .notes()
            .iter()
            .map(|n| InlineNoteUi {
                text: n.message().into(),
                action: None,
            })
            .collect();
        let has_turn = edit.turn().is_some();
        if let Some(text) = edit.other_files_notice() {
            notes.push(InlineNoteUi {
                text: text.into(),
                action: has_turn.then(|| ("Review the turn".into(), InlineEditAction::OpenReview)),
            });
        }
        let phase = match edit.phase() {
            InlinePhase::Prompting if a.submitted => InlineEditPhase::Working {
                rows: edit.request().lines.clone(),
                status: a.status.clone(),
            },
            InlinePhase::Prompting => InlineEditPhase::Message {
                text: "Waiting for your instruction\u{2026}".into(),
            },
            InlinePhase::Running { .. } => InlineEditPhase::Working {
                rows: edit.working_rows(),
                status: a.status.clone(),
            },
            InlinePhase::Reviewing => InlineEditPhase::Review {
                hunks: edit
                    .pending()
                    .map(|h| InlineHunkView {
                        id: u64::from(h.id),
                        new_rows: edit.hunk_rows(h),
                        old_lines: split_old_lines(&h.old_text),
                    })
                    .collect(),
            },
            InlinePhase::Done(InlineOutcome::NoChanges) => InlineEditPhase::Message {
                text: "The agent made no changes to this file.".into(),
            },
            InlinePhase::Done(_) => InlineEditPhase::Message {
                text: "Done.".into(),
            },
            InlinePhase::Cancelled => InlineEditPhase::Message {
                text: "Cancelled. Nothing in this file changed.".into(),
            },
            InlinePhase::Failed(why) => InlineEditPhase::Message {
                text: format!("The agent failed: {why}").into(),
            },
        };
        let me = cx_weak(self);
        let path = path.to_path_buf();
        let on_action: InlineActionHandler = Rc::new(move |action, window, cx| {
            let (me, path) = (me.clone(), path.clone());
            window.defer(cx, move |window, cx| {
                let _ = me.update(cx, |this, cx| this.handle_action(&path, action, window, cx));
            });
        });
        InlineEditUi {
            agent: a.agent.clone(),
            phase,
            notes,
            on_action,
        }
    }

    // -- user actions -----------------------------------------------------------------------

    /// A click or key in the in-place diff.
    pub fn handle_action(
        &mut self,
        path: &Path,
        action: InlineEditAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            InlineEditAction::AcceptHunk(id) => {
                if let Some(a) = self.active.get_mut(path) {
                    if let Err(e) = a.edit.accept(id as u32) {
                        return self.warn(e.to_string(), cx);
                    }
                }
                self.after_decision(path, cx);
            }
            InlineEditAction::RejectHunk(id) => {
                let edit = match self.active.get_mut(path) {
                    Some(a) => a.edit.reject(id as u32),
                    None => return,
                };
                match edit {
                    Ok(edit) => self.apply(path, vec![edit], cx),
                    Err(e) => return self.warn(e.to_string(), cx),
                }
                self.after_decision(path, cx);
            }
            InlineEditAction::AcceptAll => {
                if let Some(a) = self.active.get_mut(path) {
                    a.edit.accept_all();
                }
                self.after_decision(path, cx);
            }
            InlineEditAction::RejectAll => {
                let edits = match self.active.get_mut(path) {
                    Some(a) => a.edit.reject_all(),
                    None => return,
                };
                self.apply(path, edits, cx);
                self.after_decision(path, cx);
            }
            InlineEditAction::Cancel => self.cancel(path, cx),
            InlineEditAction::Dismiss => self.clear(path, cx),
            InlineEditAction::FollowUp => {
                if self
                    .active
                    .get(path)
                    .is_some_and(|a| matches!(a.edit.phase(), InlinePhase::Reviewing))
                {
                    self.open_prompt(path, true, None, window, cx);
                }
            }
            InlineEditAction::OpenReview => {
                let turn = self.active.get(path).and_then(|a| a.edit.turn());
                if let (Some(turn), Some(panel)) = (turn, self.panel.upgrade()) {
                    panel.update(cx, |p, cx| p.review_turn(turn, window, cx));
                }
            }
        }
    }

    fn cancel(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(a) = self.active.get_mut(path) else {
            return;
        };
        if a.edit.needs_turn_cancel() {
            if let Some(s) = &a.session {
                s.cancel();
            }
            a.status = "Cancelling\u{2026}".into();
            self.push_ui(path, cx);
        } else if a.edit.cancel_prompt() {
            self.clear(path, cx);
        }
    }

    /// Applies the undo edits to the buffer as one undo step (the file is then unsaved).
    fn apply(&mut self, path: &Path, edits: Vec<lector_buffer::TextEdit>, cx: &mut Context<Self>) {
        if edits.is_empty() {
            return;
        }
        if let Some(model) = self.active.get(path).and_then(|a| a.model.upgrade()) {
            model.update(cx, |m, cx| {
                m.apply_edits(edits, cx);
            });
        }
    }

    fn after_decision(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(a) = self.active.get(path) else {
            return;
        };
        if let InlinePhase::Done(InlineOutcome::Resolved { kept, undone }) = a.edit.phase().clone()
        {
            self.clear(path, cx);
            let mut msg = format!("Kept {kept}, undid {undone}.");
            if undone > 0 {
                msg.push_str(" Save the file to keep the undone text.");
            }
            self.toast(msg, cx);
        } else {
            self.push_ui(path, cx);
        }
    }

    /// Keep / Undo of the hunk under the cursor of the active tab, or all of them.
    pub fn decide_in_active(
        &mut self,
        ws: &Workspace,
        which: Decision,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = ws.active_file_view(cx) else {
            return;
        };
        let path = view.read(cx).path().to_path_buf();
        if !self
            .active
            .get(&path)
            .is_some_and(|a| a.edit.pending_count() > 0)
        {
            return self.note("No pending agent changes in this file.", cx);
        }
        let action = match which {
            Decision::AcceptHunk | Decision::RejectHunk => {
                let Some(id) = view.read(cx).inline_hunk_at_cursor(cx) else {
                    return;
                };
                if which == Decision::AcceptHunk {
                    InlineEditAction::AcceptHunk(id)
                } else {
                    InlineEditAction::RejectHunk(id)
                }
            }
            Decision::AcceptAll => InlineEditAction::AcceptAll,
            Decision::RejectAll => InlineEditAction::RejectAll,
        };
        self.handle_action(&path, action, window, cx);
    }
}

/// Which decision a key or menu command asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Keep the hunk under the cursor.
    AcceptHunk,
    /// Undo the hunk under the cursor.
    RejectHunk,
    /// Keep every hunk.
    AcceptAll,
    /// Undo every hunk.
    RejectAll,
}

fn cx_weak(this: &InlineEdits) -> WeakEntity<InlineEdits> {
    this.me.clone()
}

/// The old lines of a hunk: the old text split at line ends (no phantom last line).
pub fn split_old_lines(old_text: &str) -> Vec<String> {
    if old_text.is_empty() {
        return Vec::new();
    }
    let body = old_text.strip_suffix('\n').unwrap_or(old_text);
    body.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect()
}

/// Registers the inline edit actions of a workspace window.
pub fn register_actions(ws: &mut Workspace, inline: &Entity<InlineEdits>) {
    let i = inline.clone();
    ws.register_action(move |ws, _: &agent_actions::EditWithAgent, window, cx| {
        i.update(cx, |i, cx| i.start(ws, window, cx));
    });
    let i = inline.clone();
    ws.register_action(move |ws, _: &agent_actions::AcceptInlineHunk, window, cx| {
        i.update(cx, |i, cx| {
            i.decide_in_active(ws, Decision::AcceptHunk, window, cx)
        })
    });
    let i = inline.clone();
    ws.register_action(move |ws, _: &agent_actions::RejectInlineHunk, window, cx| {
        i.update(cx, |i, cx| {
            i.decide_in_active(ws, Decision::RejectHunk, window, cx)
        })
    });
    let i = inline.clone();
    ws.register_action(
        move |ws, _: &agent_actions::AcceptAllInlineEdits, window, cx| {
            i.update(cx, |i, cx| {
                i.decide_in_active(ws, Decision::AcceptAll, window, cx)
            })
        },
    );
    let i = inline.clone();
    ws.register_action(
        move |ws, _: &agent_actions::RejectAllInlineEdits, window, cx| {
            i.update(cx, |i, cx| {
                i.decide_in_active(ws, Decision::RejectAll, window, cx)
            })
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_lines_drop_the_phantom_last_line() {
        assert_eq!(split_old_lines(""), Vec::<String>::new());
        assert_eq!(split_old_lines("a\n"), vec!["a"]);
        assert_eq!(split_old_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_old_lines("a\nb"), vec!["a", "b"]);
        assert_eq!(split_old_lines("\n"), vec![""]);
        assert_eq!(split_old_lines("a\r\nb\r\n"), vec!["a", "b"]);
    }

    #[test]
    fn the_prompt_box_sends_trimmed_instructions_only() {
        assert_eq!(
            InlinePromptBox::instruction_from("  fix it \n"),
            Some("fix it".into())
        );
        assert_eq!(InlinePromptBox::instruction_from("   \n"), None);
        assert_eq!(InlinePromptBox::instruction_from(""), None);
    }
}
