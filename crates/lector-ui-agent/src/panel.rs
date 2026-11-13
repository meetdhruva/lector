//! [`AgentPanel`]: state and behaviour of the Agent sidebar panel (rendering is in
//! `render.rs`). Talks to [`lector_agent::AgentSession`] off the main thread; session
//! events arrive through a channel and are coalesced into one refresh per frame-ish.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, ScrollHandle, SharedString,
    Subscription, Task, WeakEntity, Window,
};
use lector_agent::{
    AgentConfig, AgentError, AgentManager, AgentSession, PermissionChoice, PermissionRequestId,
    PromptContext, SessionStatus, Turn, TurnId,
};
use lector_app::commands::Source;
use lector_app::AppCore;
use lector_ui::components::context_menu::{ContextMenu, ContextMenuState};
use lector_ui::components::text_area::{TextArea, TextAreaEvent};
use lector_ui::markdown::Block;
use lector_ui::views::workspace::ToastLevel;
use lector_ui::views::Workspace;
use lector_workspace::Project;
use serde_json::json;

use crate::feedback::Chip;

/// A message's parsed Markdown and the text length it was parsed at.
pub(crate) type ParsedMessage = (usize, std::rc::Rc<Vec<Block>>);

/// Painted bounds of the changed-file rows, keyed by (turn, row).
pub(crate) type RowBounds =
    std::rc::Rc<std::cell::RefCell<HashMap<(TurnId, usize), gpui::Bounds<gpui::Pixels>>>>;

/// What the panel shows for an undo that found files edited since the turn.
#[derive(Clone, Debug)]
pub struct UndoConflict {
    /// The turn.
    pub turn: TurnId,
    /// Files changed since.
    pub paths: Vec<String>,
}

/// The summary card of one turn (see `lector_agent::summary`).
#[derive(Clone)]
pub(crate) enum SummaryState {
    /// Being computed on a background thread.
    Computing,
    /// Ready to show.
    Ready(std::rc::Rc<lector_agent::summary::TurnSummary>),
    /// The changes could not be read (no repository...): the plain footer is all there is.
    Unavailable,
}

/// The Agent sidebar panel of one workspace window.
pub struct AgentPanel {
    pub(crate) workspace: WeakEntity<Workspace>,
    pub(crate) project: Project,
    pub(crate) core: AppCore,
    pub(crate) session: Option<AgentSession>,
    pub(crate) configs: Vec<AgentConfig>,
    pub(crate) selected_config: usize,
    pub(crate) starting: bool,
    pub(crate) error: Option<SharedString>,
    pub(crate) composer: Entity<TextArea>,
    pub(crate) chips: Vec<Chip>,
    pub(crate) scroll: ScrollHandle,
    pub(crate) turns: Vec<Turn>,
    pub(crate) status: Option<SessionStatus>,
    pub(crate) undo_conflict: Option<UndoConflict>,
    pub(crate) busy_turn_action: Option<TurnId>,
    pub(crate) expanded_thoughts: HashSet<(TurnId, usize)>,
    pub(crate) collapsed_tools: HashSet<(TurnId, usize)>,
    pub(crate) picker_menu: ContextMenuState,
    /// Context menu of a changed-file row (right click, Shift+F10 / Menu key).
    pub(crate) file_menu: ContextMenuState,
    /// Last painted bounds of the changed-file rows (anchor of the keyboard menu).
    pub(crate) row_bounds: RowBounds,
    pub(crate) markdown: HashMap<(TurnId, usize), ParsedMessage>,
    /// Summary cards of finished turns that changed files (computed in the background, once).
    pub(crate) summaries: HashMap<TurnId, SummaryState>,
    pub(crate) focus_handle: FocusHandle,
    /// Focus the prompt box at the next render (after a session started).
    pub(crate) focus_composer: bool,
    follow_bottom: bool,
    session_events: Option<lector_agent::Subscription>,
    _manager_sub: Option<lector_agent::manager::ManagerSubscription>,
    _event_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for AgentPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl AgentPanel {
    /// A panel for `project`; adopts the project's existing session if there is one.
    pub fn new(
        workspace: &Entity<Workspace>,
        project: Project,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let core = lector_ui::commands_bridge::core(cx);
        let composer = cx.new(|cx| {
            let placeholder = if cfg!(target_os = "macos") {
                "Ask the agent… (⌘↩ to send)"
            } else {
                "Ask the agent… (Ctrl+Enter to send)"
            };
            let mut area = TextArea::new(placeholder, cx);
            area.set_line_limits(3, 12, cx);
            area
        });
        let subs = vec![cx.subscribe(&composer, |_, _, _: &TextAreaEvent, cx| cx.notify())];
        let configs = lector_agent::commands::configs();
        let mut this = AgentPanel {
            workspace: workspace.downgrade(),
            project,
            core: core.clone(),
            session: None,
            configs,
            selected_config: 0,
            starting: false,
            error: None,
            composer,
            chips: Vec::new(),
            scroll: ScrollHandle::new(),
            turns: Vec::new(),
            status: None,
            undo_conflict: None,
            busy_turn_action: None,
            expanded_thoughts: HashSet::new(),
            collapsed_tools: HashSet::new(),
            picker_menu: ContextMenuState::default(),
            file_menu: ContextMenuState::default(),
            row_bounds: RowBounds::default(),
            markdown: HashMap::new(),
            summaries: HashMap::new(),
            focus_handle: cx.focus_handle(),
            focus_composer: false,
            follow_bottom: true,
            session_events: None,
            _manager_sub: None,
            _event_task: None,
            _subscriptions: subs,
        };
        // Sessions started elsewhere (palette command, CLI in-process, another window).
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
        let manager = AgentManager::of(&core);
        this._manager_sub = Some(manager.subscribe(move || {
            let _ = tx.unbounded_send(());
        }));
        cx.spawn(async move |this, cx| {
            while rx.next().await.is_some() {
                let ok = this.update(cx, |this, cx| this.adopt_session(cx)).is_ok();
                if !ok {
                    break;
                }
            }
        })
        .detach();
        this.adopt_session(cx);
        this
    }

    fn adopt_session(&mut self, cx: &mut Context<Self>) {
        let latest = AgentManager::of(&self.core).session_for(self.project.root());
        if latest.as_ref() != self.session.as_ref() {
            if let Some(s) = latest {
                self.attach(s, cx);
            }
        }
    }

    /// Follows `session`'s events.
    fn attach(&mut self, session: AgentSession, cx: &mut Context<Self>) {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
        self.session_events = Some(session.subscribe(move |_| {
            let _ = tx.unbounded_send(());
        }));
        if let Some(ix) = self
            .configs
            .iter()
            .position(|c| c.name == session.config().name)
        {
            self.selected_config = ix;
        }
        self.session = Some(session);
        self.markdown.clear();
        self.follow_bottom = true;
        self._event_task = Some(cx.spawn(async move |this, cx| {
            while rx.next().await.is_some() {
                // Coalesce bursts of streaming updates.
                cx.background_executor()
                    .timer(Duration::from_millis(30))
                    .await;
                while rx.try_recv().is_ok() {}
                if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                    break;
                }
            }
        }));
        self.refresh(cx);
    }

    /// Re-reads the session.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(s) = &self.session else {
            self.turns.clear();
            self.status = None;
            cx.notify();
            return;
        };
        let previous_len = self.turns.len();
        self.turns = s.turns();
        self.status = Some(s.status());
        self.ensure_summaries(cx);
        if self.follow_bottom || self.turns.len() != previous_len {
            self.scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    /// Whether new content keeps the view scrolled to the bottom.
    pub(crate) fn update_follow(&mut self) {
        let max = self.scroll.max_offset().height;
        let offset = -self.scroll.offset().y;
        self.follow_bottom = max - offset < gpui::px(48.);
    }

    /// The newest turn's id.
    pub fn last_turn_id(&self) -> Option<TurnId> {
        self.turns.last().map(|t| t.id)
    }

    /// The current session.
    pub fn session(&self) -> Option<&AgentSession> {
        self.session.as_ref()
    }

    /// A note (nothing to do, nothing happened): the info icon.
    pub(crate) fn toast(&self, msg: impl Into<SharedString>, cx: &mut App) {
        self.toast_level(msg, ToastLevel::Info, cx);
    }

    fn toast_level(&self, msg: impl Into<SharedString>, level: ToastLevel, cx: &mut App) {
        let msg = msg.into();
        if let Some(ws) = self.workspace.upgrade() {
            ws.update(cx, |ws, cx| ws.show_toast_level(msg, level, cx));
        }
    }

    pub(crate) fn selected(&self) -> Option<&AgentConfig> {
        self.configs.get(self.selected_config)
    }

    /// Starts the selected agent (a new session; the old one stops).
    pub fn start(&mut self, cx: &mut Context<Self>) {
        let Some(cfg) = self.selected().cloned() else {
            self.error = Some(
                "No agent found. Install an ACP agent (gemini, claude-code-acp, codex-acp) or add [[agent.servers]] to settings.toml."
                    .into(),
            );
            cx.notify();
            return;
        };
        if self.starting {
            return;
        }
        self.starting = true;
        self.error = None;
        cx.notify();
        let (core, project) = (self.core.clone(), self.project.clone());
        let work = cx
            .background_spawn(async move { AgentManager::of(&core).start(&core, &project, &cfg) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                this.starting = false;
                match result {
                    Ok(s) => {
                        this.focus_composer = true;
                        this.attach(s, cx)
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// `agent::NewSession`: start the selected agent, or pick one when several exist.
    pub fn new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.configs = lector_agent::commands::configs();
        if self.configs.len() > 1 {
            let pos = window.mouse_position();
            self.deploy_picker(pos, true, window, cx);
        } else {
            self.start(cx);
        }
        self.composer.read(cx).focus(window);
    }

    /// The agent picker menu (`start`: start the picked agent at once).
    pub(crate) fn deploy_picker(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        start: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        let configs = self.configs.clone();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            menu = menu.header("Agents");
            for (ix, cfg) in configs.iter().enumerate() {
                let this = this.clone();
                menu = menu.entry(cfg.name.clone(), move |_, cx| {
                    let _ = this.update(cx, |this, cx| {
                        this.selected_config = ix;
                        if start {
                            this.start(cx);
                        }
                        cx.notify();
                    });
                });
            }
            menu
        });
        self.picker_menu
            .deploy(menu, position, window, cx, |this: &mut Self| {
                &mut this.picker_menu
            });
        cx.notify();
    }

    /// The menu of a changed-file row: open, review the turn, copy the path.
    pub(crate) fn deploy_file_menu(
        &mut self,
        turn: TurnId,
        path: String,
        abs: Option<PathBuf>,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let mut menu = menu;
            if let Some(abs) = abs.clone() {
                let this = this.clone();
                menu = menu.entry("Open File", move |window, cx| {
                    let abs = abs.to_string_lossy().into_owned();
                    let _ = this.update(cx, |this, cx| this.open_location(abs, None, window, cx));
                });
            }
            let review = this.clone();
            menu = menu.entry("Review Turn", move |window, cx| {
                let _ = review.update(cx, |this, cx| this.review_turn(turn, window, cx));
            });
            let path = path.clone();
            menu.entry("Copy Path", move |_, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(path.clone()));
            })
        });
        self.file_menu
            .deploy(menu, position, window, cx, |this: &mut Self| {
                &mut this.file_menu
            });
        cx.notify();
    }

    /// Restarts a crashed or stopped agent.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        let Some(s) = self.session.clone() else {
            self.start(cx);
            return;
        };
        self.starting = true;
        self.error = None;
        cx.notify();
        let work = cx.background_spawn(async move { s.restart() });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                this.starting = false;
                if let Err(e) = result {
                    this.error = Some(e.to_string().into());
                }
                this.refresh(cx);
            });
        })
        .detach();
    }

    /// Sends the prompt box text with the context chips.
    pub fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).text().trim().to_string();
        if text.is_empty() && self.chips.is_empty() {
            return;
        }
        let Some(session) = self.session.clone() else {
            self.start(cx);
            return;
        };
        let text = if text.is_empty() {
            "Please address this feedback.".to_string()
        } else {
            text
        };
        let context: Vec<PromptContext> = self.chips.iter().map(|c| c.context.clone()).collect();
        match session.prompt(text, context) {
            Ok(_) => {
                self.chips.clear();
                self.composer.update(cx, |c, cx| c.set_text("", cx));
                self.error = None;
                self.follow_bottom = true;
                self.refresh(cx);
            }
            Err(e) => {
                self.error = Some(e.to_string().into());
                cx.notify();
            }
        }
        self.composer.read(cx).focus(window);
    }

    /// Stops the current turn.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.session.clone() {
            cx.background_spawn(async move { s.cancel() }).detach();
        }
    }

    /// Answers a permission request.
    pub fn answer(
        &mut self,
        id: PermissionRequestId,
        choice: PermissionChoice,
        cx: &mut Context<Self>,
    ) {
        if let Some(s) = &self.session {
            s.answer_permission(id, choice);
        }
        cx.notify();
    }

    /// Opens the turn's changes in a review tab (in risk order, under the turn's summary card).
    pub fn review_turn(&mut self, id: TurnId, window: &mut Window, cx: &mut Context<Self>) {
        self.review_turn_at(id, None, window, cx);
    }

    /// [`AgentPanel::review_turn`], then the diff of `target` at its evidence line.
    pub fn review_turn_at(
        &mut self,
        id: TurnId,
        target: Option<lector_agent::summary::NavTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(turn) = self.turns.iter().find(|t| t.id == id) else {
            return;
        };
        let Some(cs) = turn.changeset() else {
            self.toast("This turn has no checkpoints to review.", cx);
            return;
        };
        if turn.changed_files.is_empty() {
            self.toast(format!("Turn {id} changed no files."), cx);
            return;
        }
        let Some(ws) = self.workspace.upgrade() else {
            return;
        };
        let Some(store) = lector_ui_git::store_for(ws.entity_id(), cx) else {
            self.toast("Reviewing turns needs a git repository.", cx);
            return;
        };
        let Some(review) = lector_ui_git::review_tab::open_review_view(&store, cs, window, cx)
        else {
            return;
        };
        if let Some(SummaryState::Ready(summary)) = self.summaries.get(&id).cloned() {
            let panel = cx.entity().downgrade();
            let header =
                cx.new(|_| crate::summary_ui::SummaryHeader::new(summary, id, panel, &review));
            review.update(cx, |r, cx| r.set_header_view(Some(header.into()), cx));
        }
        if let Some(target) = target {
            // The review loads its files in the background: retry for a moment.
            cx.spawn(async move |_, cx| {
                for _ in 0..40 {
                    let shown = review
                        .update(cx, |r, cx| {
                            r.show_file_at(&target.path, target.old_side, target.line, cx)
                        })
                        .unwrap_or(true);
                    if shown {
                        return;
                    }
                    cx.background_executor()
                        .timer(Duration::from_millis(100))
                        .await;
                }
            })
            .detach();
        }
    }

    /// Starts the summary of every finished turn that changed files and has none yet.
    fn ensure_summaries(&mut self, cx: &mut Context<Self>) {
        let wanted: Vec<(TurnId, lector_git::Changeset)> = self
            .turns
            .iter()
            .filter(|t| t.state.is_finished() && !t.changed_files.is_empty())
            .filter(|t| !self.summaries.contains_key(&t.id))
            .filter_map(|t| t.changeset().map(|cs| (t.id, cs)))
            .collect();
        for (id, cs) in wanted {
            self.summaries.insert(id, SummaryState::Computing);
            let project = self.project.clone();
            let work = cx.background_spawn(async move {
                let repo = project.repo()?;
                lector_agent::summary::TurnSummary::compute(
                    repo,
                    project.languages(),
                    &cs,
                    &lector_diff::DiffOptions::default(),
                )
                .ok()
            });
            cx.spawn(async move |this, cx| {
                let result = work.await;
                let _ = this.update(cx, |this, cx| {
                    this.summaries.insert(
                        id,
                        match result {
                            Some(s) => SummaryState::Ready(std::rc::Rc::new(s)),
                            None => SummaryState::Unavailable,
                        },
                    );
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// Undoes a turn (with `force` after the user confirmed overwriting newer edits).
    pub fn undo_turn(&mut self, id: TurnId, force: bool, cx: &mut Context<Self>) {
        let Some(s) = self.session.clone() else {
            return;
        };
        self.busy_turn_action = Some(id);
        self.undo_conflict = None;
        cx.notify();
        let work = cx.background_spawn(async move { s.undo_turn(id, force, Source::Ui) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                this.busy_turn_action = None;
                match result {
                    Ok(_) => this.toast_level(
                        format!("Undid turn {id}. Undo it again from the Activity Log."),
                        ToastLevel::Success,
                        cx,
                    ),
                    Err(AgentError::UndoConflict { paths }) => {
                        this.undo_conflict = Some(UndoConflict { turn: id, paths });
                    }
                    Err(e) => this.toast_level(e.to_string(), ToastLevel::Warning, cx),
                }
                this.refresh(cx);
            });
        })
        .detach();
    }

    /// Adds a feedback chip (from `agent::SendFeedback`) and focuses the prompt box.
    pub fn add_feedback(
        &mut self,
        chip: Option<Chip>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match chip {
            Some(chip) => {
                if !self.chips.contains(&chip) {
                    self.chips.push(chip);
                }
            }
            None => self.toast(
                "Select lines in a file or diff (or add review comments) to send feedback.",
                cx,
            ),
        }
        self.composer.read(cx).focus(window);
        cx.notify();
    }

    /// "@file": attaches the active file.
    pub fn attach_active_file(&mut self, cx: &mut Context<Self>) {
        let Some(ws) = self.workspace.upgrade() else {
            return;
        };
        let Some(path) = ws.read(cx).active_path(cx) else {
            self.toast("Open a file to attach it.", cx);
            return;
        };
        let rel = self.rel(&path);
        self.push_chip(Chip {
            label: format!("@{rel}").into(),
            context: PromptContext::File { path: rel },
        });
        cx.notify();
    }

    /// Attaches the selected lines of the active file.
    pub fn attach_selection(&mut self, cx: &mut Context<Self>) {
        let Some(ws) = self.workspace.upgrade() else {
            return;
        };
        let ws = ws.read(cx);
        let (Some(path), Some(view)) = (ws.active_path(cx), ws.active_code_view(cx)) else {
            self.toast("Select lines in a file to attach them.", cx);
            return;
        };
        let (a, b) = view.read(cx).selection().range();
        let end = if b.col == 0 && b.row > a.row {
            b.row
        } else {
            b.row + 1
        };
        let rel = self.rel(&path);
        let context = PromptContext::Range {
            path: rel,
            start: a.row,
            end,
        };
        self.push_chip(Chip {
            label: context.label().into(),
            context,
        });
        cx.notify();
    }

    /// Attaches the working tree diff (the active file's, else the whole tree's).
    pub fn attach_diff(&mut self, cx: &mut Context<Self>) {
        let Some(ws) = self.workspace.upgrade() else {
            return;
        };
        let ctx = lector_ui::commands_bridge::command_context(ws.read(cx), cx);
        let only = ws.read(cx).active_path(cx).map(|p| self.rel(&p));
        let core = self.core.clone();
        let work = cx.background_spawn(async move {
            let mut args = json!({ "changeset": "worktree", "max_files": 30 });
            if let Some(p) = &only {
                args["path"] = json!(p);
            }
            let out = core.registry().invoke("git.show_diff", args.clone(), &ctx);
            let out = match out {
                Ok(o) if o.data["files"].as_array().is_some_and(|f| !f.is_empty()) => Ok(o),
                _ => {
                    args.as_object_mut().map(|a| a.remove("path"));
                    core.registry().invoke("git.show_diff", args, &ctx)
                }
            };
            out.map(|o| unified_text(&o.data))
                .map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((label, text)) if !text.is_empty() => this.push_chip(Chip {
                        label: label.clone().into(),
                        context: PromptContext::Diff { label, diff: text },
                    }),
                    Ok(_) => this.toast("No uncommitted changes to attach.", cx),
                    Err(e) => this.toast_level(e, ToastLevel::Warning, cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn push_chip(&mut self, chip: Chip) {
        if !self.chips.contains(&chip) {
            self.chips.push(chip);
        }
    }

    pub(crate) fn remove_chip(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.chips.len() {
            self.chips.remove(ix);
        }
        cx.notify();
    }

    pub(crate) fn rel(&self, path: &std::path::Path) -> String {
        path.strip_prefix(self.project.root())
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| path.to_string_lossy().into_owned())
    }

    /// Opens a file of a tool call location.
    pub(crate) fn open_location(
        &mut self,
        path: String,
        line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ws) = self.workspace.upgrade() else {
            return;
        };
        let abs = PathBuf::from(&path);
        let abs = if abs.is_absolute() {
            abs
        } else {
            self.project.root().join(abs)
        };
        window.defer(cx, move |window, cx| {
            ws.update(cx, |ws, cx| {
                ws.open_path_at(abs, line.map(|l| l + 1), None, window, cx);
            });
        });
    }
}

/// Unified diff text from `git.show_diff` JSON; returns (label, text).
fn unified_text(data: &serde_json::Value) -> (String, String) {
    let files = data["files"].as_array().cloned().unwrap_or_default();
    let mut out = String::new();
    for f in &files {
        let path = f["path"].as_str().unwrap_or("?");
        out.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
        for h in f["hunks"].as_array().into_iter().flatten() {
            if let Some(header) = h["header"].as_str() {
                out.push_str(header);
                out.push('\n');
            }
            for l in h["lines"].as_array().into_iter().flatten() {
                out.push_str(l.as_str().unwrap_or(""));
                out.push('\n');
            }
        }
    }
    let label = match files.len() {
        1 => format!("Diff: {}", files[0]["path"].as_str().unwrap_or("?")),
        n => format!("Diff: {n} files"),
    };
    (label, out)
}
