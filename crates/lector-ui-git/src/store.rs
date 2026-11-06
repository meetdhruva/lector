//! Per-workspace view of the shared git state: the project, `HEAD`, status,
//! conflicts and the review store.
//!
//! Since v0.4 the state itself lives in `lector_app::git_service::RepoState`
//! (one per repository, shared by every window, refreshed from the file
//! watcher). A `GitStore` subscribes to it, mirrors the latest snapshot into its
//! public fields and emits [`GitStoreEvent::Changed`]; it never polls.

use std::sync::Arc;

use futures::StreamExt as _;
use gpui::{App, Context, Entity, EventEmitter, Task, WeakEntity, Window};
use lector_app::git_service::{GitSnapshot, RepoState, Subscription};
use lector_config::{ReviewStore, Settings};
use lector_git::{HeadInfo, Repo, StatusEntry};
use lector_ui::views::{Workspace, WorkspaceEvent};
use lector_workspace::conflicts::ConflictState;
use lector_workspace::Project;

/// Emitted when `HEAD`, refs or status changed.
#[derive(Clone, Debug, PartialEq)]
pub enum GitStoreEvent {
    /// Status / HEAD changed (views should reload).
    Changed,
}

/// Git state of one workspace window (a view over the shared `RepoState`).
pub struct GitStore {
    /// The workspace.
    pub workspace: WeakEntity<Workspace>,
    project: Option<Project>,
    /// `HEAD` information.
    pub head: Option<HeadInfo>,
    /// `git status` entries.
    pub status: Arc<Vec<StatusEntry>>,
    /// Merge / rebase / cherry-pick / revert in progress and conflicted paths.
    pub conflicts: Arc<ConflictState>,
    /// Viewed marks (database-backed, shared by every window).
    pub review: ReviewStore,
    /// Bumped on each observed change.
    pub generation: u64,
    /// The git views of this window (set by `init`).
    pub views: Option<crate::GitViews>,
    state: Option<Arc<RepoState>>,
    _subscriptions: Vec<gpui::Subscription>,
    _repo_subscription: Option<Subscription>,
    _updates: Option<Task<()>>,
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
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let core = lector_ui::commands_bridge::core(cx);
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
        let state = project
            .as_ref()
            .and_then(|p| p.repo())
            .map(|repo| core.git().repo_for(repo));
        let (repo_subscription, updates) = match &state {
            Some(state) => {
                let (tx, mut rx) = futures::channel::mpsc::unbounded::<Arc<GitSnapshot>>();
                let sub = state.subscribe(move |snap| {
                    let _ = tx.unbounded_send(snap.clone());
                });
                let initial = state.snapshot();
                let task = cx.spawn(async move |this, cx| {
                    if initial.generation > 0
                        && this
                            .update(cx, |this, cx| this.apply(&initial, cx))
                            .is_err()
                    {
                        return;
                    }
                    while let Some(mut snap) = rx.next().await {
                        // Only the newest pending snapshot matters.
                        while let Ok(newer) = rx.try_recv() {
                            snap = newer;
                        }
                        if this.update(cx, |this, cx| this.apply(&snap, cx)).is_err() {
                            break;
                        }
                    }
                });
                (Some(sub), Some(task))
            }
            None => (None, None),
        };
        GitStore {
            workspace: workspace.downgrade(),
            project,
            head: None,
            status: Arc::new(Vec::new()),
            conflicts: Arc::new(ConflictState::default()),
            review: core.review_store(),
            generation: 0,
            views: None,
            state,
            _subscriptions: subscriptions,
            _repo_subscription: repo_subscription,
            _updates: updates,
        }
    }

    fn apply(&mut self, snap: &GitSnapshot, cx: &mut Context<Self>) {
        // `RepoState` publishes only changed snapshots, or forced ones after a
        // write (identical contents, views still reload), so every one counts.
        self.head = snap.head.clone();
        self.status = snap.status.clone();
        self.conflicts = snap.conflicts.clone();
        self.generation += 1;
        cx.emit(GitStoreEvent::Changed);
        cx.notify();
    }

    /// The project.
    pub fn project(&self) -> Option<&Project> {
        self.project.as_ref()
    }

    /// The repository, if the project is in one.
    pub fn repo(&self) -> Option<&Repo> {
        self.project.as_ref().and_then(|p| p.repo())
    }

    /// The shared repository state (one per repository, all windows).
    pub fn repo_state(&self) -> Option<&Arc<RepoState>> {
        self.state.as_ref()
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

    /// Asks the shared state to re-read HEAD and status (coalesced across
    /// windows); the store updates when the new snapshot arrives.
    pub fn refresh(&mut self, _cx: &mut Context<Self>) {
        if let Some(state) = &self.state {
            state.refresh();
        }
    }

    /// Like [`GitStore::refresh`], but views reload even if nothing seems to
    /// have changed (after a write).
    pub fn refresh_and_notify(&mut self, _cx: &mut Context<Self>) {
        if let Some(state) = &self.state {
            state.refresh_and_notify();
        }
    }
}
