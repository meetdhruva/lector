//! Per-workspace git state shared by all git views: the project, `HEAD`,
//! status, the review store, and background refresh.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, Subscription, Task, WeakEntity, Window,
};
use lector_config::{ReviewStore, Settings};
use lector_git::{HeadInfo, Repo, StatusEntry};
use lector_ui::views::{Workspace, WorkspaceEvent};
use lector_workspace::Project;

/// Emitted when `HEAD`, refs or status changed.
#[derive(Clone, Debug, PartialEq)]
pub enum GitStoreEvent {
    /// Status / HEAD changed (views should reload).
    Changed,
}

/// Shared git state of one workspace window.
pub struct GitStore {
    /// The workspace.
    pub workspace: WeakEntity<Workspace>,
    project: Option<Project>,
    /// `HEAD` information.
    pub head: Option<HeadInfo>,
    /// `git status` entries.
    pub status: Arc<Vec<StatusEntry>>,
    /// Viewed marks.
    pub review: ReviewStore,
    /// Bumped on each observed change.
    pub generation: u64,
    /// The git views of this window (set by `init`).
    pub views: Option<crate::GitViews>,
    refresh_task: Option<Task<()>>,
    refresh_again: bool,
    last_refresh: Option<Instant>,
    last_cost: Duration,
    _subscriptions: Vec<Subscription>,
    _poll: Option<Task<()>>,
}

impl EventEmitter<GitStoreEvent> for GitStore {}

fn app_settings(cx: &App) -> Settings {
    lector_ui::settings::AppSettings::get(cx).as_ref().clone()
}

impl GitStore {
    /// Creates the store for `workspace` and starts loading. `project` must
    /// be the workspace's project (the workspace may be mid-update here).
    pub fn new(
        workspace: &Entity<Workspace>,
        project: Option<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(
            workspace,
            |this, _, event: &WorkspaceEvent, cx| match event {
                WorkspaceEvent::GitStateChanged | WorkspaceEvent::FilesChanged(_) => {
                    this.refresh(cx)
                }
                _ => {}
            },
        ));
        // Poll cheaply while the window is active (catches changes the
        // watcher missed, e.g. on network file systems).
        let poll = cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            let ok = this.update_in(cx, |this, window, cx| {
                if window.is_window_active() {
                    let stale = this.last_refresh.is_none_or(|t| {
                        t.elapsed() > (this.last_cost * 20).max(Duration::from_secs(3))
                    });
                    if stale {
                        this.refresh(cx);
                    }
                }
            });
            if ok.is_err() {
                break;
            }
        });
        let mut store = GitStore {
            workspace: workspace.downgrade(),
            project,
            head: None,
            status: Arc::new(Vec::new()),
            review: ReviewStore::default(),
            generation: 0,
            views: None,
            refresh_task: None,
            refresh_again: false,
            last_refresh: None,
            last_cost: Duration::from_millis(50),
            _subscriptions: subscriptions,
            _poll: Some(poll),
        };
        store.refresh(cx);
        store
    }

    /// The project.
    pub fn project(&self) -> Option<&Project> {
        self.project.as_ref()
    }

    /// The repository, if the project is in one.
    pub fn repo(&self) -> Option<&Repo> {
        self.project.as_ref().and_then(|p| p.repo())
    }

    /// Current settings (from the app).
    pub fn settings(&self, cx: &App) -> Settings {
        app_settings(cx)
    }

    /// Short branch name or detached short sha.
    pub fn branch_label(&self) -> Option<String> {
        let head = self.head.as_ref()?;
        match (&head.branch, head.oid) {
            (Some(b), _) => Some(b.clone()),
            (None, Some(oid)) => Some(oid.short()),
            _ => None,
        }
    }

    /// Re-reads HEAD and status in the background (coalesced).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo().cloned() else {
            return;
        };
        if self.refresh_task.is_some() {
            self.refresh_again = true;
            return;
        }
        let started = Instant::now();
        let work = cx.background_spawn(async move {
            let head = repo.head().ok();
            let status = repo.status().ok();
            (head, status)
        });
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let (head, status) = work.await;
            let _ = this.update(cx, |this, cx| {
                this.refresh_task = None;
                this.last_refresh = Some(Instant::now());
                this.last_cost = started.elapsed();
                let mut changed = false;
                if head != this.head {
                    this.head = head;
                    changed = true;
                }
                if let Some(status) = status {
                    if *this.status != status {
                        this.status = Arc::new(status);
                        changed = true;
                    }
                }
                if changed || this.generation == 0 {
                    this.generation += 1;
                    cx.emit(GitStoreEvent::Changed);
                    cx.notify();
                }
                if std::mem::take(&mut this.refresh_again) {
                    this.refresh(cx);
                }
            });
        }));
    }
}
