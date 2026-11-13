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

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use futures::StreamExt as _;
use gpui::{
    div, prelude::*, AnyWindowHandle, App, Corner, Entity, EventEmitter, MouseButton,
    MouseDownEvent, SharedString, Subscription, Task, WeakEntity, Window,
};
use lector_app::commands::git_sync::{SyncHost, SyncProgress, SyncSubscription};
use lector_app::commands::{CommandContext, CommandError, Source};
use lector_app::journal::Journal;
use lector_app::{Outcome, Priority};
use lector_git::{Progress, Repo, SyncError, SyncEvent};
use lector_ui::actions::{git as git_actions, workspace as ws_actions};
use lector_ui::components::context_menu::ContextMenu;
use lector_ui::components::popover::Popover;
use lector_ui::components::{h_flex, metrics, Button, IconButton, Tooltip};
use lector_ui::icons::{Icon, IconName, IconSize};
use lector_ui::scale::px;
use lector_ui::views::sidebar::SidebarPanel;
use lector_ui::views::workspace::ToastLevel;
use lector_ui::views::Workspace;
use lector_ui::{ActiveTheme, ColorExt};
use serde_json::{json, Value};

use crate::askpass_ui::{self, PromptContext};
use crate::confirm::{confirm, ConfirmSpec};
use crate::staging::{command_error_text, notify_repo_changed};
use crate::store::{GitStore, GitStoreEvent};
use crate::sync_dialogs::{show_input, show_notice, DialogButton, InputSpec, Level, NoticeSpec};
use crate::sync_model::{
    control_view, failure_view, generic_failure, last_fetched_label, plan, success_view,
    validate_branch_name, FailureContext, FailureView, NextStep, OpKind, Plan, PrimaryAction,
    RemoteInfo, SyncAction, SyncStatus,
};

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

fn fetch_head_time(git_dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(git_dir.join("FETCH_HEAD"))
        .and_then(|m| m.modified())
        .ok()
}

/// Reads the full sync status (blocking; background thread).
pub fn load_status(repo: &Repo) -> Result<SyncStatus, String> {
    let st = repo.upstream_status().map_err(|e| e.to_string())?;
    let head_short = repo.head().ok().and_then(|h| h.oid).map(|o| o.short());
    let remotes = repo
        .remotes()
        .unwrap_or_default()
        .into_iter()
        .map(|r| RemoteInfo {
            url: lector_git::sync::redact_credentials(&r.url),
            name: r.name,
        })
        .collect();
    Ok(SyncStatus::from_upstream(
        &st,
        head_short,
        remotes,
        fetch_head_time(repo.git_dir()),
    ))
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

fn credential_helpers(workdir: &Path) -> Vec<String> {
    let cmd = lector_process::ProcessCommand::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["config", "--get-all", "credential.helper"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .kind(lector_process::ProcessKind::Git)
        .label("git config credential.helper");
    let opts = lector_process::RunOptions {
        timeout: Some(Duration::from_secs(5)),
        ..Default::default()
    };
    match lector_process::run(&cmd, &opts) {
        Ok(out) if out.success() => out
            .stdout_text()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
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
        let work = cx.background_spawn(async move { load_status(&repo) });
        self.status_task = Some(cx.spawn(async move |this, cx| {
            let result = work.await;
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
        let scheduler = core.scheduler().clone();
        let job = scheduler
            .spawn(Priority::Interactive, None, move |_| {
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
            })
            .into_future();
        let store = self.store.clone();
        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
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
                    let work = cx.background_spawn(async move { credential_helpers(&workdir) });
                    cx.spawn(async move |this, cx| {
                        ctx.credential_helpers = work.await;
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

    // -----------------------------------------------------------------------------------------
    // Branches
    // -----------------------------------------------------------------------------------------

    fn local_branches(repo: &Repo) -> Vec<String> {
        repo.branches()
            .unwrap_or_default()
            .into_iter()
            .filter(|b| !b.remote)
            .map(|b| b.name)
            .collect()
    }

    /// "New Branch…": name validated as you type, start point (HEAD or a chosen ref), "Switch
    /// to it" on by default.
    pub fn new_branch_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(repo), Some(ws)) = (
            self.store.read(cx).repo().cloned(),
            self.workspace_entity(cx),
        ) else {
            return;
        };
        let work = cx.background_spawn(async move {
            let branches = repo.branches().unwrap_or_default();
            let local: Vec<String> = branches
                .iter()
                .filter(|b| !b.remote)
                .map(|b| b.name.clone())
                .collect();
            let mut choices: Vec<String> = branches
                .iter()
                .filter(|b| !b.name.ends_with("/HEAD"))
                .map(|b| b.name.clone())
                .collect();
            choices.extend(
                repo.tags()
                    .unwrap_or_default()
                    .into_iter()
                    .rev()
                    .map(|t| t.name),
            );
            (local, choices)
        });
        let this = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                let (local, choices) = work.await;
                let _ = cx.update(|window, cx| {
                    let existing = local.clone();
                    let spec = InputSpec {
                        title: "New Branch".into(),
                        message: None,
                        placeholder: "Branch name".into(),
                        initial: String::new(),
                        validate: Some(Rc::new(move |name: &str| {
                            validate_branch_name(name.trim(), &existing)
                        })),
                        start_point: Some(("HEAD".into(), choices)),
                        checks: vec![("Switch to it".into(), true)],
                        confirm_label: "Create Branch".into(),
                    };
                    show_input(&ws, spec, window, cx, move |result, window, cx| {
                        let switch = result.checks.first().copied().unwrap_or(true);
                        let _ = this.update(cx, |this, cx| {
                            this.create_branch(result.text, result.start, switch, window, cx)
                        });
                    });
                });
            })
            .detach();
    }

    /// Creates `name` at `start` (HEAD when `None`), switching to it if asked (unsaved editor
    /// buffers are offered for saving first).
    pub fn create_branch(
        &mut self,
        name: String,
        start: Option<String>,
        switch: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut args = json!({ "name": name, "switch": switch });
        if let Some(s) = &start {
            args["start_point"] = json!(s);
        }
        let steps = vec![("git.create_branch", args)];
        let label = format!("Creating {name}");
        let op = OpKind::CreateBranch(name);
        if switch {
            self.after_saving(window, cx, move |this, cx| {
                this.execute(op, steps, label, false, cx)
            });
        } else {
            self.execute(op, steps, label, false, cx);
        }
    }

    /// Asks to save unsaved editor buffers (the workspace's unsaved-changes flow) before an
    /// operation that changes files on disk, then runs `then` unless the user cancelled.
    fn after_saving(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        let dirty = self
            .workspace_entity(cx)
            .map(|ws| ws.read(cx).dirty_file_views(cx))
            .unwrap_or_default();
        if dirty.is_empty() {
            then(self, cx);
            return;
        }
        let task = lector_ui::editor::confirm_discard_or_save(dirty, window, cx);
        cx.spawn(async move |this, cx| {
            if task.await.unwrap_or(false) {
                let _ = this.update(cx, |this, cx| then(this, cx));
            }
        })
        .detach();
    }

    /// Switches to `name` (a local branch, or a remote-only name that git turns into a
    /// tracking branch). Unsaved buffers are offered for saving first; when local changes
    /// would be overwritten, the failure dialog lists them and offers "Stash Changes and
    /// Switch" — never a silent stash.
    pub fn switch_branch(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.status.branch.as_deref() == Some(name.as_str()) {
            self.toast(format!("Already on {name}"), cx);
            return;
        }
        let steps = vec![("git.switch_branch", json!({ "name": name }))];
        let label = format!("Switching to {name}");
        self.after_saving(window, cx, move |this, cx| {
            this.execute(OpKind::Switch(name), steps, label, false, cx)
        });
    }

    /// Picks a branch to switch to.
    pub fn pick_branch_to_switch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_ref(
            "Switch to branch…",
            RefFilter::Local,
            window,
            cx,
            |this, name, window, cx| this.switch_branch(name, window, cx),
        );
    }

    /// Checks out remote branch `remote_name` (`origin/feature`) as a local tracking branch.
    pub fn checkout_remote(
        &mut self,
        remote_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let local = remote_name
            .split_once('/')
            .map_or(remote_name.as_str(), |(_, b)| b)
            .to_string();
        let existing = self
            .store
            .read(cx)
            .repo()
            .map(Self::local_branches)
            .unwrap_or_default();
        if existing.contains(&local) {
            self.toast(
                format!("A local branch {local} exists already; switching to it"),
                cx,
            );
            self.switch_branch(local, window, cx);
            return;
        }
        let steps = vec![
            (
                "git.create_branch",
                json!({ "name": local, "start_point": remote_name, "switch": true }),
            ),
            (
                "git.set_upstream",
                json!({ "branch": local, "upstream": remote_name }),
            ),
        ];
        let label = format!("Checking out {remote_name}");
        self.after_saving(window, cx, move |this, cx| {
            this.execute(OpKind::Switch(local), steps, label, false, cx)
        });
    }

    /// Picks a remote branch to check out.
    pub fn pick_remote_to_checkout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_ref(
            "Check out remote branch…",
            RefFilter::Remote,
            window,
            cx,
            |this, name, window, cx| this.checkout_remote(name, window, cx),
        );
    }

    /// "Rename…" (the current branch when `name` is `None`).
    pub fn rename_branch_dialog(
        &mut self,
        name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(old) = name.or_else(|| self.status.branch.clone()) else {
            self.toast("HEAD is detached: there is no branch to rename", cx);
            return;
        };
        let (Some(repo), Some(ws)) = (
            self.store.read(cx).repo().cloned(),
            self.workspace_entity(cx),
        ) else {
            return;
        };
        let existing = Self::local_branches(&repo);
        let this = cx.entity().downgrade();
        let old2 = old.clone();
        let spec = InputSpec {
            title: format!("Rename {old}").into(),
            message: None,
            placeholder: "New name".into(),
            initial: old.clone(),
            validate: Some(Rc::new(move |name: &str| {
                let others: Vec<String> =
                    existing.iter().filter(|b| **b != old2).cloned().collect();
                if name.trim() == old2 {
                    return Err("Enter a different name".into());
                }
                validate_branch_name(name.trim(), &others)
            })),
            start_point: None,
            checks: Vec::new(),
            confirm_label: "Rename".into(),
        };
        show_input(&ws, spec, window, cx, move |result, _, cx| {
            let _ = this.update(cx, |this, cx| {
                let steps = vec![(
                    "git.rename_branch",
                    json!({ "name": old, "new_name": result.text }),
                )];
                this.execute(
                    OpKind::RenameBranch(old.clone()),
                    steps,
                    format!("Renaming {old}"),
                    false,
                    cx,
                );
            });
        });
    }

    /// Deletes local branch `name`. Merged: at once with an Undo toast. Not merged: a
    /// confirmation naming the commits that would become unreachable, then a forced delete
    /// (still undoable from the toast and the Activity log).
    pub fn delete_branch(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.status.branch.as_deref() == Some(name.as_str()) {
            self.toast(
                format!("{name} is checked out: switch to another branch first"),
                cx,
            );
            return;
        }
        let Some(repo) = self.store.read(cx).repo().cloned() else {
            return;
        };
        let n = name.clone();
        let work = cx.background_spawn(async move { unreachable_commits(&repo, &n) });
        let this = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                let lost = work.await;
                let _ = cx.update(|window, cx| {
                    let _ = this.update(cx, |this, cx| {
                        if lost.is_empty() {
                            let steps = vec![("git.delete_branch", json!({ "name": name }))];
                            this.execute(
                                OpKind::DeleteBranch(name.clone()),
                                steps,
                                format!("Deleting {name}"),
                                false,
                                cx,
                            );
                            return;
                        }
                        let Some(ws) = this.workspace_entity(cx) else {
                            return;
                        };
                        let count = lost.len();
                        let spec = ConfirmSpec {
                            title: format!("Delete unmerged branch {name}?").into(),
                            message: vec![
                                format!(
                                    "{name} has {count} commit{} that are on no other branch \
                                     you have. Deleting it leaves them unreachable.",
                                    if count == 1 { "" } else { "s" }
                                )
                                .into(),
                                "You can undo this right after (toast, Activity log).".into(),
                            ],
                            items: lost.into_iter().map(SharedString::from).collect(),
                            confirm_label: "Delete Branch".into(),
                        };
                        let weak = cx.entity().downgrade();
                        confirm(&ws, spec, window, cx, move |_, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                let steps = vec![(
                                    "git.delete_branch",
                                    json!({ "name": name, "force": true }),
                                )];
                                this.execute(
                                    OpKind::DeleteBranch(name.clone()),
                                    steps,
                                    format!("Deleting {name}"),
                                    false,
                                    cx,
                                );
                            });
                        });
                    });
                });
            })
            .detach();
    }

    /// Picks a branch to delete.
    pub fn pick_branch_to_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_ref(
            "Delete branch…",
            RefFilter::LocalNotCurrent,
            window,
            cx,
            |this, name, window, cx| this.delete_branch(name, window, cx),
        );
    }

    /// "Set Upstream…": picks a remote branch for the current branch.
    pub fn pick_upstream(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(branch) = self.status.branch.clone() else {
            self.toast(
                "HEAD is detached: there is no branch to set an upstream for",
                cx,
            );
            return;
        };
        self.pick_ref(
            "Set upstream to…",
            RefFilter::Remote,
            window,
            cx,
            move |this, upstream, _, cx| {
                let steps = vec![(
                    "git.set_upstream",
                    json!({ "branch": branch, "upstream": upstream }),
                )];
                this.execute(
                    OpKind::SetUpstream,
                    steps,
                    "Setting the upstream".into(),
                    false,
                    cx,
                );
            },
        );
    }

    fn pick_ref(
        &mut self,
        placeholder: &'static str,
        filter: RefFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_pick: impl Fn(&mut Self, String, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let (Some(repo), Some(ws)) = (
            self.store.read(cx).repo().cloned(),
            self.workspace_entity(cx),
        ) else {
            return;
        };
        let work = cx.background_spawn(async move {
            let mut branches = repo.branches().unwrap_or_default();
            branches.sort_by_key(|b| std::cmp::Reverse(b.time));
            branches
                .into_iter()
                .filter(|b| match filter {
                    RefFilter::Local => !b.remote,
                    RefFilter::LocalNotCurrent => !b.remote && !b.head,
                    RefFilter::Remote => b.remote && !b.name.ends_with("/HEAD"),
                })
                .map(|b| crate::ref_picker::RefChoice {
                    rev: b.name.clone(),
                    label: b.name.clone(),
                    detail: match (b.head, b.remote, &b.upstream) {
                        (true, _, _) => "current branch".into(),
                        (_, true, _) => "remote branch".into(),
                        (_, _, Some(up)) => format!("tracks {up}"),
                        _ => "branch".into(),
                    },
                    icon: if b.remote {
                        IconName::Globe
                    } else {
                        IconName::GitBranch
                    },
                })
                .collect::<Vec<_>>()
        });
        let this = cx.entity().downgrade();
        let on_pick = Rc::new(on_pick);
        window
            .spawn(cx, async move |cx| {
                let choices = work.await;
                let _ = cx.update(|window, cx| {
                    if choices.is_empty() {
                        ws.update(cx, |ws, cx| {
                            ws.show_toast_level("No matching branches", ToastLevel::Info, cx)
                        });
                        return;
                    }
                    crate::ref_picker::pick_ref(
                        &ws,
                        placeholder,
                        choices,
                        move |choice, window, cx| {
                            let on_pick = on_pick.clone();
                            let _ =
                                this.update(cx, |this, cx| on_pick(this, choice.rev, window, cx));
                        },
                        window,
                        cx,
                    );
                });
            })
            .detach();
    }

    // -----------------------------------------------------------------------------------------
    // Stash
    // -----------------------------------------------------------------------------------------

    /// "Stash Changes…": message and "Include untracked files".
    pub fn stash_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ws) = self.workspace_entity(cx) else {
            return;
        };
        let this = cx.entity().downgrade();
        let spec = InputSpec {
            title: "Stash Changes".into(),
            message: Some(
                "Local changes are saved in a stash and removed from the working tree. Apply or \
                 pop the stash later from the Branches panel."
                    .into(),
            ),
            placeholder: "Message (optional)".into(),
            initial: String::new(),
            validate: None,
            start_point: None,
            checks: vec![("Include untracked files".into(), true)],
            confirm_label: "Stash".into(),
        };
        show_input(&ws, spec, window, cx, move |result, _, cx| {
            let _ = this.update(cx, |this, cx| {
                let mut args = json!({
                    "include_untracked": result.checks.first().copied().unwrap_or(true),
                });
                if !result.text.is_empty() {
                    args["message"] = json!(result.text);
                }
                this.execute(
                    OpKind::Stash,
                    vec![("git.stash_push", args)],
                    "Stashing".into(),
                    false,
                    cx,
                );
            });
        });
    }

    /// Apply / Pop / Drop the stash `oid` (`verb` = "Apply" | "Pop" | "Drop", `command` the
    /// registry id). Drop is undoable from the toast instead of asking first.
    pub fn stash_op(
        &mut self,
        verb: &'static str,
        command: &'static str,
        oid: String,
        cx: &mut Context<Self>,
    ) {
        let steps = vec![(command, json!({ "oid": oid }))];
        self.execute(
            OpKind::StashApply(verb),
            steps,
            format!("{verb} stash"),
            false,
            cx,
        );
    }

    /// Picks a stash for `verb`.
    pub fn pick_stash(
        &mut self,
        verb: &'static str,
        command: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(repo), Some(ws)) = (
            self.store.read(cx).repo().cloned(),
            self.workspace_entity(cx),
        ) else {
            return;
        };
        let work = cx.background_spawn(async move { repo.stashes().unwrap_or_default() });
        let this = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                let stashes = work.await;
                let _ = cx.update(|window, cx| {
                    if stashes.is_empty() {
                        ws.update(cx, |ws, cx| {
                            ws.show_toast_level("No stashes", ToastLevel::Info, cx)
                        });
                        return;
                    }
                    let choices = stashes
                        .into_iter()
                        .map(|s| crate::ref_picker::RefChoice {
                            rev: s.oid.to_string(),
                            label: s.message.clone(),
                            detail: format!("stash@{{{}}}", s.index),
                            icon: IconName::Archive,
                        })
                        .collect();
                    let placeholder = match verb {
                        "Apply" => "Apply stash…",
                        "Pop" => "Pop stash…",
                        _ => "Drop stash…",
                    };
                    crate::ref_picker::pick_ref(
                        &ws,
                        placeholder,
                        choices,
                        move |choice, _, cx| {
                            let _ = this.update(cx, |this, cx| {
                                this.stash_op(verb, command, choice.rev, cx)
                            });
                        },
                        window,
                        cx,
                    );
                });
            })
            .detach();
    }
}

/// Which branches a picker lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RefFilter {
    Local,
    LocalNotCurrent,
    Remote,
}

/// Commits of `branch` that no other ref has (shown before deleting an unmerged branch):
/// those not on `HEAD` and not on the branch's upstream. At most 50, "abc1234 summary".
pub fn unreachable_commits(repo: &Repo, branch: &str) -> Vec<String> {
    let log = |rev: String| {
        repo.log(&lector_git::LogQuery {
            rev: Some(rev),
            limit: 200,
            ..Default::default()
        })
        .unwrap_or_default()
    };
    let mut lost = log(format!("HEAD..{branch}"));
    let others: Vec<lector_git::Branch> = repo
        .branches()
        .unwrap_or_default()
        .into_iter()
        .filter(|b| b.name != branch && !b.head)
        .collect();
    for other in others {
        if lost.is_empty() {
            break;
        }
        let keep: std::collections::HashSet<lector_git::Oid> =
            log(format!("{}..{branch}", other.name))
                .into_iter()
                .map(|c| c.oid)
                .collect();
        lost.retain(|c| keep.contains(&c.oid));
    }
    lost.into_iter()
        .take(50)
        .map(|c| format!("{} {}", c.oid.short(), c.summary))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Status bar control
// ---------------------------------------------------------------------------------------------

/// The status bar sync control: branch, ahead/behind, state word; click = the obvious next
/// step; the chevron opens the sync menu; while an operation runs: phase, percent, Cancel.
pub struct SyncControl {
    controller: Entity<SyncController>,
    menu: Popover<ContextMenu>,
    _subscriptions: Vec<Subscription>,
}

impl SyncControl {
    /// The control over `controller`.
    pub fn new(controller: Entity<SyncController>, cx: &mut Context<Self>) -> Self {
        let subs = vec![cx.observe(&controller, |_, _, cx| cx.notify())];
        SyncControl {
            controller,
            menu: Popover::new(),
            _subscriptions: subs,
        }
    }

    /// Opens the sync menu above `position`.
    pub fn deploy_menu(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let busy = self.controller.read(cx).busy_reason(cx);
        let menu = sync_menu(busy.is_some(), window, cx);
        self.menu.show(
            menu,
            position,
            Corner::BottomLeft,
            window,
            cx,
            |this: &mut Self, _, cx| {
                this.menu.close();
                cx.notify();
            },
        );
        cx.notify();
    }
}

/// The sync menu (status bar dropdown); every item dispatches a `git::*` action.
pub fn sync_menu(busy: bool, window: &mut Window, cx: &mut App) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, move |menu, _, _| {
        let item =
            |menu: ContextMenu, label: &str, icon: IconName, action: Box<dyn gpui::Action>| {
                if busy {
                    menu.disabled_action(label.to_string(), action)
                } else {
                    menu.icon_action(label.to_string(), icon, action)
                }
            };
        let mut menu = menu.header("Sync");
        menu = item(
            menu,
            "Fetch",
            IconName::Refresh,
            Box::new(git_actions::Fetch),
        );
        menu = item(
            menu,
            "Pull (Fast-Forward)",
            IconName::ArrowDown,
            Box::new(git_actions::Pull),
        );
        menu = item(
            menu,
            "Pull (Rebase)",
            IconName::ArrowDown,
            Box::new(git_actions::PullRebase),
        );
        menu = item(menu, "Push", IconName::ArrowUp, Box::new(git_actions::Push));
        menu = item(
            menu,
            "Push (Force with Lease)…",
            IconName::AlertTriangle,
            Box::new(git_actions::ForcePushWithLease),
        );
        menu = item(
            menu,
            "Publish Branch",
            IconName::Globe,
            Box::new(git_actions::PublishBranch),
        );
        menu = item(
            menu,
            "Set Upstream…",
            IconName::Link,
            Box::new(git_actions::SetUpstream),
        );
        if busy {
            menu = menu.icon_action("Cancel", IconName::Close, Box::new(git_actions::CancelSync));
        }
        menu.separator()
            .icon_action(
                "New Branch…",
                IconName::Plus,
                Box::new(git_actions::NewBranch),
            )
            .icon_action(
                "Switch Branch…",
                IconName::GitBranch,
                Box::new(git_actions::SwitchBranch),
            )
            .icon_action(
                "Branches",
                IconName::ListTree,
                Box::new(ws_actions::ShowBranches),
            )
    })
}

impl Render for SyncControl {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let fg = ui.status_bar_fg.hsla();
        let c = self.controller.read(cx);
        let view = control_view(c.status());
        let fetched = last_fetched_label(c.status().last_fetch, SystemTime::now());
        let auto = lector_ui::settings::AppSettings::get(cx).git.auto_fetch;
        let meta: SharedString = {
            let mut m = fetched;
            if let lector_config::AutoFetch::Minutes(n) = auto {
                m.push_str(&format!(" · auto-fetch every {n} min"));
            }
            if let Some(e) = c.auto_fetch_error() {
                m.push_str(&format!(" · last automatic fetch failed: {e}"));
            }
            m.into()
        };
        let running = c.running().cloned();
        let entity = cx.entity().downgrade();
        let mut row = h_flex().id("status-sync").gap(px(1.)).flex_none();
        if let Some(r) = running {
            row = row
                .child(
                    h_flex()
                        .id("status-sync-progress")
                        .px(px(4.))
                        .gap(px(4.))
                        .text_color(fg)
                        .child(Icon::new(IconName::Loader).size(IconSize::XSmall).color(fg))
                        .child(SharedString::from(r.progress_label()))
                        .tooltip(Tooltip::text("A git network operation is running")),
                )
                .child(
                    IconButton::new("status-sync-cancel", IconName::Close)
                        .icon_size(IconSize::XSmall)
                        .color(fg)
                        .tooltip(Tooltip::for_action(
                            "Cancel (stops git and its helpers)",
                            &git_actions::CancelSync,
                        ))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(git_actions::CancelSync), cx)
                        }),
                );
        } else {
            let mut label = view.branch.clone();
            if !view.counts.is_empty() {
                label.push_str(&format!(" {}", view.counts));
            }
            if !view.state_word.is_empty() {
                label.push_str(&format!(" · {}", view.state_word));
            }
            let primary = view.primary;
            row = row.child(
                Button::new("status-branch", label)
                    .compact()
                    .color(fg)
                    .icon(IconName::GitBranch)
                    .tooltip(Tooltip::with_meta(view.tooltip.clone(), meta))
                    .on_click(move |_, window, cx| match primary.action_name() {
                        Some(_) => window.dispatch_action(Box::new(git_actions::Sync), cx),
                        None => window.dispatch_action(Box::new(ws_actions::ShowBranches), cx),
                    }),
            );
        }
        row.child(
            div()
                .id("status-sync-menu")
                .on_mouse_down(MouseButton::Left, move |e: &MouseDownEvent, window, cx| {
                    let _ = entity.update(cx, |this, cx| this.deploy_menu(e.position, window, cx));
                    cx.stop_propagation();
                })
                .child(
                    IconButton::new("status-sync-chevron", IconName::ChevronUp)
                        .icon_size(IconSize::XSmall)
                        .color(fg)
                        .tooltip(Tooltip::for_action(
                            "Sync menu: fetch, pull, push, branches",
                            &git_actions::ShowSyncMenu,
                        )),
                ),
        )
        .children(self.menu.render())
    }
}

// ---------------------------------------------------------------------------------------------
// Changes panel bar
// ---------------------------------------------------------------------------------------------

/// The Fetch / Pull / Push row at the top of the Changes panel, with progress and Cancel
/// while an operation runs.
pub struct SyncPanelBar {
    controller: Entity<SyncController>,
    _subscriptions: Vec<Subscription>,
}

impl SyncPanelBar {
    /// The bar over `controller`.
    pub fn new(controller: Entity<SyncController>, cx: &mut Context<Self>) -> Self {
        let subs = vec![cx.observe(&controller, |_, _, cx| cx.notify())];
        SyncPanelBar {
            controller,
            _subscriptions: subs,
        }
    }
}

impl Render for SyncPanelBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let c = self.controller.read(cx);
        let view = control_view(c.status());
        let busy = c.busy_reason(cx);
        let running = c.running().cloned();
        let mut text = view.branch.clone();
        if !view.counts.is_empty() {
            text.push_str(&format!(" {}", view.counts));
        }
        if !view.state_word.is_empty() {
            text.push_str(&format!(" · {}", view.state_word));
        }
        let button =
            |id: &'static str, icon: IconName, tip: &'static str, action: Box<dyn gpui::Action>| {
                let busy = busy.clone();
                IconButton::new(id, icon)
                    .icon_size(IconSize::Small)
                    .disabled(busy.is_some())
                    .tooltip(move |window, cx| match &busy {
                        Some(b) => Tooltip::text(format!("{b} is running"))(window, cx),
                        None => Tooltip::for_action(tip, action.as_ref())(window, cx),
                    })
                    .on_click({
                        let name = id;
                        move |_, window, cx| {
                            let action: Box<dyn gpui::Action> = match name {
                                "sync-bar-fetch" => Box::new(git_actions::Fetch),
                                "sync-bar-pull" => Box::new(git_actions::Pull),
                                _ => Box::new(git_actions::Push),
                            };
                            window.dispatch_action(action, cx)
                        }
                    })
            };
        h_flex()
            .id("sync-panel-bar")
            .flex_none()
            .h(px(26.))
            .gap(px(2.))
            .text_size(metrics::text_sm())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(ui.text_muted.hsla())
                    .child(SharedString::from(match &running {
                        Some(r) => r.progress_label(),
                        None => text,
                    })),
            )
            .when_some(
                running.as_ref().and_then(|r| r.progress.as_ref()?.percent),
                |el, pc| {
                    el.child(
                        div()
                            .w(px(48.))
                            .h(px(4.))
                            .rounded(px(2.))
                            .bg(ui.border.hsla())
                            .child(
                                div()
                                    .h_full()
                                    .rounded(px(2.))
                                    .bg(ui.accent.hsla())
                                    .w(px(48. * f32::from(pc.min(100)) / 100.)),
                            ),
                    )
                },
            )
            .when(running.is_some(), |el| {
                el.child(
                    Button::new("sync-bar-cancel", "Cancel")
                        .compact()
                        .tooltip(Tooltip::for_action(
                            "Stop git and everything it started",
                            &git_actions::CancelSync,
                        ))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(git_actions::CancelSync), cx)
                        }),
                )
            })
            .when(running.is_none(), |el| {
                el.child(button(
                    "sync-bar-fetch",
                    IconName::Refresh,
                    "Fetch from all remotes",
                    Box::new(git_actions::Fetch),
                ))
                .child(button(
                    "sync-bar-pull",
                    IconName::ArrowDown,
                    "Pull (fast-forward only)",
                    Box::new(git_actions::Pull),
                ))
                .child(button(
                    "sync-bar-push",
                    IconName::ArrowUp,
                    "Push",
                    Box::new(git_actions::Push),
                ))
            })
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

/// The toast icon of a command outcome.
fn toast_level(outcome: &Outcome) -> ToastLevel {
    use lector_app::commands::NoticeLevel;
    match outcome.level {
        NoticeLevel::Success => ToastLevel::Success,
        NoticeLevel::Info => ToastLevel::Info,
        NoticeLevel::Warning => ToastLevel::Warning,
    }
}
