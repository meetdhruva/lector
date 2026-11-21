//! "Sync with the remote" (v0.7): the per-window [`SyncController`] that runs fetch / pull /
//! push, branch and stash commands, and the views over it — the status bar control
//! ([`SyncControl`]: branch, `↑2 ↓1`, state word, click = the obvious next step, dropdown) and
//! the Changes panel bar ([`SyncPanelBar`]: Fetch / Pull / Push buttons, progress, Cancel).
//!
//! Every write is the direct result of a user action and runs off the main thread through
//! the command registry on the app scheduler (journaled; Undo where the command provides
//! one). One operation at a time per repository: while one runs, the others are disabled
//! with a tooltip saying why, and actions asking for one get a toast. Network operations
//! show git's phase and percent from [`SyncHost::subscribe`] and can be cancelled (the whole
//! process tree is killed). Failures open a dialog with the next step as a button
//! ([`crate::sync_model::failure_view`]). Nothing runs on a timer unless `git.auto_fetch` is
//! set (user-level, default off).

use lector_ui::background::{background, view_key, JobKind, Priority};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use futures::StreamExt as _;
use gpui::{
    prelude::*, AnyWindowHandle, App, Entity, EventEmitter, SharedString, Subscription, Task,
    WeakEntity, Window,
};
use lector_app::commands::git_sync::{
    credential_helpers, SyncHost, SyncProgress, SyncSubscription,
};
use lector_app::commands::{CommandContext, CommandError, Source};
use lector_app::journal::Journal;
use lector_app::Outcome;
use lector_git::{Progress, SyncError, SyncEvent};
use lector_ui::actions::git as git_actions;
use lector_ui::components::metrics;
use lector_ui::scale::px;
use lector_ui::views::sidebar::SidebarPanel;
use lector_ui::views::workspace::ToastLevel;
use lector_ui::views::Workspace;
use serde_json::{json, Value};

use crate::askpass_ui::{self, PromptContext};
use crate::confirm::{confirm, ConfirmSpec};
use crate::staging::{command_error_text, notify_repo_changed};
use crate::store::{GitStore, GitStoreEvent};
use crate::sync_dialogs::{show_notice, DialogButton, Level, NoticeSpec};
use crate::sync_model::{
    control_view, failure_view, generic_failure, plan, success_view, FailureContext, FailureView,
    NextStep, OpKind, Plan, PrimaryAction, SyncAction, SyncStatus,
};
use crate::sync_support::{load_status, toast_level};

mod branches;
mod control;

pub use control::{sync_menu, SyncControl, SyncPanelBar};

/// Commands of one operation, run in order.
type Steps = Vec<(&'static str, Value)>;

/// A running operation.
#[derive(Clone, Debug)]
pub struct RunningOp {
    /// What runs.
    pub op: OpKind,
    /// "Pushing main → origin/main".
    pub label: String,
    /// The latest progress event.
    pub progress: Option<Progress>,
    /// Started at.
    pub started: Instant,
    /// A background (auto-fetch) operation: failures do not open a dialog.
    pub quiet: bool,
}

impl RunningOp {
    /// "Pushing 45% · Writing objects".
    pub fn progress_label(&self) -> String {
        match &self.progress {
            Some(p) => {
                let mut s = self.label.clone();
                if let Some(pc) = p.percent {
                    s.push_str(&format!(" {pc}%"));
                }
                if !p.phase.is_empty() {
                    s.push_str(&format!(" · {}", p.phase));
                }
                s
            }
            None => format!("{}…", self.label),
        }
    }
}

/// Emitted when the status, the running operation or the last result changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncEvent2 {
    /// Re-render.
    Changed,
}

/// Sync state and operations of one workspace window.
pub struct SyncController {
    store: Entity<GitStore>,
    window: AnyWindowHandle,
    status: SyncStatus,
    head_key: Option<(Option<String>, Option<String>, Option<String>)>,
    status_task: Option<Task<()>>,
    running: Option<RunningOp>,
    last_request: Option<(OpKind, Steps)>,
    last_failure: Option<FailureView>,
    last_message: Option<SharedString>,
    auto_fetch_error: Option<String>,
    push_after_pull: bool,
    prompt_answered: Option<bool>,
    op_task: Option<Task<()>>,
    _progress: Option<SyncSubscription>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SyncEvent2> for SyncController {}

/// Controllers of every window (askpass falls back to one of them).
#[derive(Default)]
struct Controllers(Vec<WeakEntity<SyncController>>);

impl gpui::Global for Controllers {}

/// Any live controller (askpass fallback window).
pub fn any_controller(cx: &App) -> Option<Entity<SyncController>> {
    cx.try_global::<Controllers>()?
        .0
        .iter()
        .find_map(WeakEntity::upgrade)
}

/// How a failed command is reported.
enum Failure {
    Sync(SyncError),
    Cancelled,
    Text(String),
}

fn classify(err: &CommandError) -> Failure {
    if let CommandError::Failed(e) = err {
        if let Some(g) = e.downcast_ref::<lector_git::Error>() {
            return match g {
                lector_git::Error::Sync(s) => Failure::Sync((**s).clone()),
                lector_git::Error::Cancelled => Failure::Cancelled,
                other => Failure::Text(crate::staging::git_error_text(other)),
            };
        }
    }
    Failure::Text(command_error_text(err))
}

impl SyncController {
    /// A controller for `store`'s window.
    pub fn new(store: Entity<GitStore>, window: AnyWindowHandle, cx: &mut Context<Self>) -> Self {
        let mut subs = vec![cx.subscribe(&store, |this, _, _: &GitStoreEvent, cx| {
            this.on_store_changed(cx)
        })];
        subs.push(cx.observe_global::<lector_ui::settings::AppSettings>(|_, cx| cx.notify()));
        let mut tasks = Vec::new();
        let progress = store.read(cx).repo().map(|repo| {
            let core = lector_ui::commands_bridge::core(cx);
            let host = SyncHost::of(&core);
            let workdir = repo.workdir().to_path_buf();
            let (tx, mut rx) = futures::channel::mpsc::unbounded::<SyncProgress>();
            let sub = host.subscribe(move |p| {
                if p.repo == workdir {
                    let _ = tx.unbounded_send(p.clone());
                }
            });
            tasks.push(cx.spawn(async move |this, cx| {
                while let Some(mut p) = rx.next().await {
                    // Only the newest update matters.
                    while let Ok(newer) = rx.try_recv() {
                        if matches!(newer.event, SyncEvent::Progress(_)) {
                            p = newer;
                        }
                    }
                    if this.update(cx, |this, cx| this.on_progress(p, cx)).is_err() {
                        break;
                    }
                }
            }));
            sub
        });
        tasks.push(cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_secs(30))
                .await;
            if this
                .update(cx, |this, cx| this.auto_fetch_tick(cx))
                .is_err()
            {
                break;
            }
        }));
        let weak = cx.entity().downgrade();
        cx.default_global::<Controllers>().0.push(weak);
        let mut this = SyncController {
            store,
            window,
            status: SyncStatus::default(),
            head_key: None,
            status_task: None,
            running: None,
            last_request: None,
            last_failure: None,
            last_message: None,
            auto_fetch_error: None,
            push_after_pull: false,
            prompt_answered: None,
            op_task: None,
            _progress: progress,
            _tasks: tasks,
            _subscriptions: subs,
        };
        this.refresh_status(cx);
        this
    }

    /// The window this controller belongs to.
    pub fn window_handle(&self) -> AnyWindowHandle {
        self.window
    }

    /// The workspace.
    pub fn workspace(&self, cx: &App) -> Option<WeakEntity<Workspace>> {
        Some(self.store.read(cx).workspace.clone())
    }

    /// The git store.
    pub fn store(&self) -> &Entity<GitStore> {
        &self.store
    }

    /// The current status.
    pub fn status(&self) -> &SyncStatus {
        &self.status
    }

    /// The running operation.
    pub fn running(&self) -> Option<&RunningOp> {
        self.running.as_ref()
    }

    /// The last failure (shown in a dialog unless it was a background fetch).
    pub fn last_failure(&self) -> Option<&FailureView> {
        self.last_failure.as_ref()
    }

    /// The last success / info message (toast text).
    pub fn last_message(&self) -> Option<&SharedString> {
        self.last_message.as_ref()
    }

    /// Error of the last automatic fetch (tooltip).
    pub fn auto_fetch_error(&self) -> Option<&str> {
        self.auto_fetch_error.as_deref()
    }

    fn workdir(&self, cx: &App) -> Option<PathBuf> {
        self.store
            .read(cx)
            .repo()
            .map(|r| r.workdir().to_path_buf())
    }

    /// Why a new operation cannot start now: one runs in this window, or a sync command runs
    /// in this repository from another window.
    pub fn busy_reason(&self, cx: &App) -> Option<String> {
        if let Some(r) = &self.running {
            return Some(r.label.clone());
        }
        let workdir = self.workdir(cx)?;
        let core = lector_ui::commands_bridge::core(cx);
        SyncHost::of(&core)
            .running()
            .into_iter()
            .find(|r| r.repo == workdir)
            .map(|r| match r.command.as_str() {
                "git.fetch" => "A fetch".to_string(),
                "git.pull" => "A pull".to_string(),
                "git.push" => "A push".to_string(),
                other => format!("`{other}`"),
            })
    }

    fn on_store_changed(&mut self, cx: &mut Context<Self>) {
        let head = self.store.read(cx).head.clone();
        let Some(head) = head else {
            return;
        };
        // Cheap: ahead/behind come with every HEAD snapshot.
        let key = (
            head.branch.clone(),
            head.upstream.clone(),
            head.oid.map(|o| o.to_string()),
        );
        if self.status.loaded {
            self.status.ahead = head.ahead;
            self.status.behind = head.behind;
            cx.emit(SyncEvent2::Changed);
            cx.notify();
        }
        if self.head_key.as_ref() != Some(&key) {
            self.head_key = Some(key);
            self.refresh_status(cx);
        }
    }

    /// Re-reads upstream, remotes and the last fetch time in the background.
    pub fn refresh_status(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.store.read(cx).repo().cloned() else {
            return;
        };
        let key = view_key(JobKind::GitSyncStatus, cx.entity_id());
        let work = background(cx, key, Priority::Background, move |_| load_status(&repo));
        self.status_task = Some(cx.spawn(async move |this, cx| {
            let Ok(result) = work.await else { return };
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(status) => this.status = status,
                    Err(e) => tracing::debug!("sync status: {e}"),
                }
                cx.emit(SyncEvent2::Changed);
                cx.notify();
            });
        }));
    }

    fn on_progress(&mut self, p: SyncProgress, cx: &mut Context<Self>) {
        let Some(running) = self.running.as_mut() else {
            return;
        };
        if let SyncEvent::Progress(progress) = p.event {
            running.progress = Some(progress);
            cx.emit(SyncEvent2::Changed);
            cx.notify();
        }
    }

    fn auto_fetch_tick(&mut self, cx: &mut Context<Self>) {
        let Some(interval) = lector_ui::settings::AppSettings::get(cx)
            .git
            .auto_fetch
            .interval()
        else {
            return;
        };
        if self.busy_reason(cx).is_some() || self.status.remotes.is_empty() {
            return;
        }
        let due = match self.status.last_fetch {
            Some(t) => SystemTime::now()
                .duration_since(t)
                .is_ok_and(|age| age >= interval),
            None => true,
        };
        if due {
            let (id, args) = SyncAction::Fetch.command();
            self.execute(
                OpKind::Sync(SyncAction::Fetch),
                vec![(id, args)],
                "Automatic fetch".into(),
                true,
                cx,
            );
        }
    }

    fn workspace_entity(&self, cx: &App) -> Option<Entity<Workspace>> {
        self.store.read(cx).workspace.upgrade()
    }

    /// A note (blocked, nothing to do, already there): the info icon. Results that worked go
    /// through [`Self::toast_level`] with their outcome's level.
    fn toast(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast_level(message, ToastLevel::Info, cx);
    }

    fn toast_level(
        &mut self,
        message: impl Into<SharedString>,
        level: ToastLevel,
        cx: &mut Context<Self>,
    ) {
        let message = message.into();
        self.last_message = Some(message.clone());
        if let Some(ws) = self.workspace_entity(cx) {
            ws.update(cx, |ws, cx| ws.show_toast_level(message, level, cx));
        }
    }

    fn action_toast(
        &mut self,
        message: String,
        label: &'static str,
        action: impl Fn(&mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) {
        self.last_message = Some(message.clone().into());
        if let Some(ws) = self.workspace_entity(cx) {
            ws.update(cx, |ws, cx| {
                ws.show_action_toast(
                    message,
                    label,
                    lector_ui::views::workspace::UNDO_TOAST_DURATION,
                    move |_, window, cx| action(window, cx),
                    cx,
                )
            });
        }
    }

    // -----------------------------------------------------------------------------------------
    // Running
    // -----------------------------------------------------------------------------------------

    /// [`run_action`](Self::run_action) started fresh by the user (menu, palette, key,
    /// button): forgets a pending "push again after pulling".
    pub fn user_action(&mut self, action: SyncAction, window: &mut Window, cx: &mut Context<Self>) {
        self.push_after_pull = false;
        self.run_action(action, window, cx);
    }

    /// Runs `action` as the user asked (menu, palette, key, button): planned, confirmed when
    /// dangerous, blocked with a toast while another operation runs.
    pub fn run_action(&mut self, action: SyncAction, window: &mut Window, cx: &mut Context<Self>) {
        let busy = self.busy_reason(cx);
        match plan(action, &self.status, busy.as_deref()) {
            Plan::Blocked(why) => self.toast(why, cx),
            Plan::Run(steps) => {
                let label = self.op_label(action);
                self.execute(OpKind::Sync(action), steps, label, false, cx);
            }
            Plan::Confirm(text, steps) => {
                let Some(ws) = self.workspace_entity(cx) else {
                    return;
                };
                let this = cx.entity().downgrade();
                let label = self.op_label(action);
                let spec = ConfirmSpec {
                    title: text.title.into(),
                    message: text.message.into_iter().map(SharedString::from).collect(),
                    items: Vec::new(),
                    confirm_label: text.confirm_label.into(),
                };
                confirm(&ws, spec, window, cx, move |_, cx| {
                    let _ = this.update(cx, |this, cx| {
                        this.execute(OpKind::Sync(action), steps, label, false, cx)
                    });
                });
            }
        }
    }

    fn op_label(&self, action: SyncAction) -> String {
        let verb = action.running_verb();
        match action {
            SyncAction::Fetch => verb.to_string(),
            SyncAction::Pull | SyncAction::PullRebase => match &self.status.upstream {
                Some(up) => format!("{verb} from {up}"),
                None => verb.to_string(),
            },
            _ => match (self.status.branch.as_deref(), self.status.push_target()) {
                (Some(b), Some(t)) => format!("{verb} {b} → {t}"),
                _ => verb.to_string(),
            },
        }
    }

    /// Runs the commands of one operation in the background (scheduler, journaled), then
    /// refreshes every git view and reports. `quiet` = no failure dialog (auto-fetch).
    pub fn execute(
        &mut self,
        op: OpKind,
        steps: Steps,
        label: String,
        quiet: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(why) = self.busy_reason(cx) {
            if !quiet {
                self.toast(
                    format!("{why} is still running in this repository. Wait for it or cancel it."),
                    cx,
                );
            }
            return;
        }
        let Some(project) = self
            .store
            .read(cx)
            .project()
            .filter(|p| p.repo().is_some())
            .cloned()
        else {
            return;
        };
        let network = op.is_network();
        let mut prompt_op = None;
        if network {
            askpass_ui::ensure_started(cx);
            if let Some(ws) = self.workspace(cx) {
                let this = cx.entity().downgrade();
                prompt_op = Some(askpass_ui::begin_operation(
                    PromptContext {
                        id: 0,
                        window: self.window,
                        workspace: ws,
                        label: label.clone(),
                        remote: self.status.remote_url().map(str::to_string),
                        on_prompt: Rc::new(move |answered, cx| {
                            let _ =
                                this.update(cx, |this, _| this.prompt_answered = Some(answered));
                        }),
                    },
                    cx,
                ));
            }
        }
        self.prompt_answered = None;
        self.last_failure = None;
        self.last_request = Some((op.clone(), steps.clone()));
        self.running = Some(RunningOp {
            op: op.clone(),
            label,
            progress: None,
            started: Instant::now(),
            quiet,
        });
        cx.emit(SyncEvent2::Changed);
        cx.notify();
        let core = lector_ui::commands_bridge::core(cx);
        // Network and writes: the blocking lane. Cancelled by `SyncHost::cancel_repo` (kills git).
        let key = view_key(JobKind::NetworkCommand, cx.entity_id());
        let job = background(cx, key, Priority::Visible, move |_| {
            Journal::install(&core);
            let ctx = CommandContext::new(core.clone(), Some(project), Source::Ui);
            let mut outcomes: Vec<(&'static str, Outcome)> = Vec::new();
            for (id, args) in steps {
                match core.registry().invoke(id, args, &ctx) {
                    Ok(o) => outcomes.push((id, o)),
                    Err(e) => return Err((outcomes, e)),
                }
            }
            Ok(outcomes)
        });
        let store = self.store.clone();
        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await.ok();
            let _ = cx.update(|cx| {
                if let Some(id) = prompt_op {
                    askpass_ui::end_operation(id, cx);
                }
                notify_repo_changed(&store, cx);
            });
            let _ = this.update(cx, |this, cx| {
                let running = this.running.take();
                let quiet = running.as_ref().is_some_and(|r| r.quiet);
                this.refresh_status(cx);
                match result {
                    Some(Ok(outcomes)) => this.on_success(op, outcomes, quiet, cx),
                    Some(Err((_, err))) => this.on_failure(op, &err, quiet, cx),
                    None => {
                        let view = generic_failure(&op, "the operation stopped unexpectedly");
                        this.show_failure(view, quiet, cx);
                    }
                }
                cx.emit(SyncEvent2::Changed);
                cx.notify();
            });
        }));
    }

    /// Cancels the running network operation (git, ssh and helpers are killed) and any
    /// credential prompt it is waiting for.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        let Some(workdir) = self.workdir(cx) else {
            return;
        };
        let core = lector_ui::commands_bridge::core(cx);
        let n = SyncHost::of(&core).cancel_repo(&workdir);
        askpass_ui::cancel_pending(cx);
        if n == 0 && self.running.is_some() {
            self.toast(
                "This operation cannot be cancelled; it finishes shortly",
                cx,
            );
        }
    }

    fn on_success(
        &mut self,
        op: OpKind,
        outcomes: Vec<(&'static str, Outcome)>,
        quiet: bool,
        cx: &mut Context<Self>,
    ) {
        if quiet {
            self.auto_fetch_error = None;
            return;
        }
        let Some((_, last)) = outcomes.last() else {
            return;
        };
        let undo = last.undo.is_some().then_some(last.journal_id).flatten();
        match op {
            OpKind::Sync(action) | OpKind::StashAndPull(action) => {
                let view = success_view(action, &last.data, &last.summary);
                let stashed = match &op {
                    OpKind::StashAndPull(_) => outcomes
                        .first()
                        .and_then(|(_, o)| o.data.get("stash"))
                        .and_then(|s| s.get("oid"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    _ => None,
                };
                let push_after = std::mem::take(&mut self.push_after_pull);
                if view.conflicts {
                    self.toast_level(view.message, ToastLevel::Warning, cx);
                    self.show_changes(cx);
                } else if let Some(oid) = stashed {
                    let this = cx.entity().downgrade();
                    self.action_toast(
                        format!("{} · your changes are stashed", view.message),
                        "Pop Stash",
                        move |_, cx| {
                            let oid = oid.clone();
                            let _ = this.update(cx, |this, cx| {
                                this.stash_op("Pop", "git.stash_pop", oid, cx)
                            });
                        },
                        cx,
                    );
                } else if push_after && action.pull_mode().is_some() {
                    let this = cx.entity().downgrade();
                    self.action_toast(
                        format!("{} · now push again", view.message),
                        "Push",
                        move |window, cx| {
                            let _ = this.update(cx, |this, cx| {
                                this.run_action(SyncAction::Push, window, cx)
                            });
                        },
                        cx,
                    );
                } else if let (Some(id), Some(_)) = (undo, action.pull_mode()) {
                    self.last_message = Some(view.message.clone().into());
                    crate::activity::show_undo_toast(&self.store, id, view.message, cx);
                } else {
                    self.toast_level(view.message, toast_level(last), cx);
                }
            }
            OpKind::StashApply(_) => {
                let conflicts = last
                    .data
                    .get("conflicts")
                    .and_then(Value::as_array)
                    .is_some_and(|c| !c.is_empty());
                self.report_with_undo(last.summary.clone(), undo, toast_level(last), cx);
                if conflicts {
                    self.show_changes(cx);
                }
            }
            _ => self.report_with_undo(last.summary.clone(), undo, toast_level(last), cx),
        }
    }

    fn report_with_undo(
        &mut self,
        summary: String,
        undo: Option<u64>,
        level: ToastLevel,
        cx: &mut Context<Self>,
    ) {
        match undo {
            Some(id) => {
                self.last_message = Some(summary.clone().into());
                crate::activity::show_undo_toast_level(&self.store, id, summary, level, cx);
            }
            None => self.toast_level(summary, level, cx),
        }
    }

    /// Reveals the Changes panel (conflicts to resolve). `ShowChanges` toggles, so this goes
    /// through [`Workspace::reveal_panel`]: an open panel stays open.
    fn show_changes(&mut self, cx: &mut Context<Self>) {
        let Some(ws) = self.store.read(cx).workspace.upgrade() else {
            return;
        };
        let _ = self.window.update(cx, |_, window, cx| {
            window.defer(cx, move |window, cx| {
                ws.update(cx, |ws, cx| {
                    ws.reveal_panel(SidebarPanel::Changes, window, cx)
                })
            });
        });
    }

    fn on_failure(&mut self, op: OpKind, err: &CommandError, quiet: bool, cx: &mut Context<Self>) {
        // "Pull first" that failed (diverged) keeps the intent: the next pull offers Push.
        let pull = matches!(&op, OpKind::Sync(a) if a.pull_mode().is_some());
        if !pull {
            self.push_after_pull = false;
        }
        let failure = classify(err);
        if quiet {
            self.auto_fetch_error = Some(match &failure {
                Failure::Sync(e) => e.to_string(),
                Failure::Cancelled => "cancelled".into(),
                Failure::Text(t) => t.clone(),
            });
            return;
        }
        match failure {
            Failure::Cancelled => self.toast(format!("{} cancelled", op.title()), cx),
            Failure::Text(text) => self.show_failure(generic_failure(&op, &text), false, cx),
            Failure::Sync(e) => {
                let mut ctx = FailureContext {
                    remote_url: self.status.remote_url().map(str::to_string),
                    prompt_answered: self.prompt_answered,
                    ssh_agent: std::env::var_os("SSH_AUTH_SOCK").is_some(),
                    credential_helpers: Vec::new(),
                };
                if e.kind == lector_git::SyncErrorKind::AuthenticationFailed {
                    // Which helpers git asked: a quick read in the background.
                    let Some(workdir) = self.workdir(cx) else {
                        return;
                    };
                    let key = view_key(JobKind::GitConfigRead, cx.entity_id());
                    let work = background(cx, key, Priority::Visible, move |_| {
                        credential_helpers(&workdir)
                    });
                    cx.spawn(async move |this, cx| {
                        ctx.credential_helpers = work.await.unwrap_or_default();
                        let _ = this.update(cx, |this, cx| {
                            let mut view = failure_view(&op, &e, &ctx);
                            if let Some(why) = askpass_ui::start_error(cx) {
                                view.message.push(format!(
                                    "Lector could not offer a credential prompt in this session: {why}"
                                ));
                            }
                            this.show_failure(view, false, cx);
                        });
                    })
                    .detach();
                    return;
                }
                let view = failure_view(&op, &e, &ctx);
                self.show_failure(view, false, cx);
            }
        }
    }

    fn show_failure(&mut self, view: FailureView, quiet: bool, cx: &mut Context<Self>) {
        self.last_failure = Some(view.clone());
        cx.emit(SyncEvent2::Changed);
        if quiet {
            return;
        }
        let Some(ws) = self.workspace_entity(cx) else {
            return;
        };
        let this = cx.entity().downgrade();
        let buttons: Vec<DialogButton> = view
            .steps
            .iter()
            .enumerate()
            .map(|(ix, step)| {
                let this = this.clone();
                let step = step.clone();
                DialogButton::new(step.label(), ix == 0, move |window, cx| {
                    let _ = this.update(cx, |this, cx| this.run_step(step, window, cx));
                })
            })
            .collect();
        let spec = NoticeSpec {
            title: view.title.clone().into(),
            level: if view.paths.is_empty() {
                Level::Error
            } else {
                Level::Warning
            },
            message: view
                .message
                .iter()
                .cloned()
                .map(SharedString::from)
                .collect(),
            items: view.paths.iter().cloned().map(SharedString::from).collect(),
            details: view.details.clone().map(SharedString::from),
            close_label: if view.steps.is_empty() {
                "Close".into()
            } else {
                "Cancel".into()
            },
        };
        let _ = self.window.update(cx, move |_, window, cx| {
            show_notice(&ws, spec, buttons, window, cx);
        });
    }

    /// Runs a next step offered after a failure.
    pub fn run_step(&mut self, step: NextStep, window: &mut Window, cx: &mut Context<Self>) {
        match step {
            NextStep::PullFirst => {
                self.push_after_pull = true;
                self.run_action(SyncAction::Pull, window, cx);
            }
            NextStep::PullRebase => {
                let after_push = matches!(
                    self.last_request,
                    Some((OpKind::Sync(SyncAction::Push | SyncAction::Publish), _))
                );
                self.push_after_pull |= after_push;
                self.run_action(SyncAction::PullRebase, window, cx);
            }
            NextStep::Publish => self.run_action(SyncAction::Publish, window, cx),
            NextStep::Fetch => self.run_action(SyncAction::Fetch, window, cx),
            NextStep::SetUpstream => self.pick_upstream(window, cx),
            NextStep::NewBranch => self.new_branch_dialog(window, cx),
            NextStep::ShowConflicts => self.show_changes(cx),
            NextStep::StashAndPull(action) => {
                let steps = vec![
                    (
                        "git.stash_push",
                        json!({ "message": "lector: before pull", "include_untracked": true }),
                    ),
                    action.command(),
                ];
                let label = format!("Stashing and {}", self.op_label(action).to_lowercase());
                self.execute(OpKind::StashAndPull(action), steps, label, false, cx);
            }
            NextStep::StashAndSwitch(name) => {
                let steps = vec![(
                    "git.switch_branch",
                    json!({ "name": name, "stash_changes": true }),
                )];
                self.execute(
                    OpKind::Switch(name.clone()),
                    steps,
                    format!("Switching to {name}"),
                    false,
                    cx,
                );
            }
            NextStep::Retry => {
                if let Some((op, steps)) = self.last_request.clone() {
                    let label = match &op {
                        OpKind::Sync(a) => self.op_label(*a),
                        other => other.title(),
                    };
                    self.execute(op, steps, label, false, cx);
                }
            }
        }
    }

    /// The obvious next step (status bar click / `git::Sync`).
    pub fn run_primary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match control_view(&self.status).primary {
            PrimaryAction::Sync(action) => self.run_action(action, window, cx),
            PrimaryAction::NewBranch => self.new_branch_dialog(window, cx),
            PrimaryAction::None => {
                let tip = control_view(&self.status).tooltip;
                self.toast(tip, cx);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------------------------

/// Registers the `git::*` sync, branch and stash actions on `ws` (menus, palette, keys and
/// the views above all dispatch these).
pub fn register(
    ws: &mut Workspace,
    controller: &Entity<SyncController>,
    control: &Entity<SyncControl>,
) {
    fn on<A: gpui::Action>(
        ws: &mut Workspace,
        controller: &Entity<SyncController>,
        f: impl Fn(&mut SyncController, &mut Window, &mut Context<SyncController>) + 'static,
    ) {
        let c = controller.clone();
        let f = Rc::new(f);
        ws.register_action(move |_, _: &A, window, cx| {
            let (c, f) = (c.clone(), f.clone());
            window.defer(cx, move |window, cx| c.update(cx, |c, cx| f(c, window, cx)));
        });
    }
    on::<git_actions::Sync>(ws, controller, |c, w, cx| c.run_primary(w, cx));
    on::<git_actions::Fetch>(ws, controller, |c, w, cx| {
        c.user_action(SyncAction::Fetch, w, cx)
    });
    on::<git_actions::Pull>(ws, controller, |c, w, cx| {
        c.user_action(SyncAction::Pull, w, cx)
    });
    on::<git_actions::PullRebase>(ws, controller, |c, w, cx| {
        c.user_action(SyncAction::PullRebase, w, cx)
    });
    on::<git_actions::Push>(ws, controller, |c, w, cx| {
        c.user_action(SyncAction::Push, w, cx)
    });
    on::<git_actions::ForcePushWithLease>(ws, controller, |c, w, cx| {
        c.user_action(SyncAction::ForcePushWithLease, w, cx)
    });
    on::<git_actions::PublishBranch>(ws, controller, |c, w, cx| {
        c.user_action(SyncAction::Publish, w, cx)
    });
    on::<git_actions::SetUpstream>(ws, controller, |c, w, cx| c.pick_upstream(w, cx));
    on::<git_actions::CancelSync>(ws, controller, |c, _, cx| c.cancel(cx));
    on::<git_actions::NewBranch>(ws, controller, |c, w, cx| c.new_branch_dialog(w, cx));
    on::<git_actions::SwitchBranch>(ws, controller, |c, w, cx| c.pick_branch_to_switch(w, cx));
    on::<git_actions::RenameBranch>(ws, controller, |c, w, cx| {
        c.rename_branch_dialog(None, w, cx)
    });
    on::<git_actions::DeleteBranch>(ws, controller, |c, w, cx| c.pick_branch_to_delete(w, cx));
    on::<git_actions::CheckoutRemoteBranch>(ws, controller, |c, w, cx| {
        c.pick_remote_to_checkout(w, cx)
    });
    on::<git_actions::StashChanges>(ws, controller, |c, w, cx| c.stash_dialog(w, cx));
    on::<git_actions::ApplyStash>(ws, controller, |c, w, cx| {
        c.pick_stash("Apply", "git.stash_apply", w, cx)
    });
    on::<git_actions::PopStash>(ws, controller, |c, w, cx| {
        c.pick_stash("Pop", "git.stash_pop", w, cx)
    });
    on::<git_actions::DropStash>(ws, controller, |c, w, cx| {
        c.pick_stash("Drop", "git.stash_drop", w, cx)
    });
    let control = control.clone();
    ws.register_action(move |_, _: &git_actions::ShowSyncMenu, window, cx| {
        let control = control.clone();
        window.defer(cx, move |window, cx| {
            // Above the left end of the status bar, where the control is drawn.
            let size = window.viewport_size();
            let position = gpui::point(px(8.), size.height - metrics::status_bar_height());
            control.update(cx, |ctl, cx| ctl.deploy_menu(position, window, cx));
        });
    });
}
