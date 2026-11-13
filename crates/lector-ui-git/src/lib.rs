//! Git review views for Lector, registered into the workspace via [`init`]:
//! the Changes, History and Branches panels, diff tabs (split / unified),
//! changeset review and commit tabs, blame, file/line history, permalinks.
#![forbid(unsafe_code)]

pub mod activity;
pub mod askpass_ui;
pub mod blame;
pub mod branches_panel;
pub mod changes_panel;
pub mod commit_composer;
pub mod confirm;
pub mod conflict_actions;
pub mod diff_source;
pub mod diff_view;
pub mod history_panel;
pub mod inline_conflicts;
pub mod merge_editor;
pub mod operation_banner;
pub mod pull_requests; // Agent FORGE (v0.6)
pub mod ref_picker;
pub mod review_model;
pub mod review_tab;
pub mod staging;
pub mod store;
pub mod sync;
pub mod sync_dialogs;
pub mod sync_model;
pub mod util;

#[cfg(test)]
mod sync_tests;

use std::rc::Rc;

use gpui::{actions, App, AppContext, ClipboardItem, Entity, SharedString, Window};
use lector_git::Oid;
use lector_ui::actions::{editor, git as git_actions, workspace as ws_actions};
use lector_ui::code_view::{CodeSource, CodeView};
use lector_ui::views::pane::CodeItem;
use lector_ui::views::sidebar::SidebarPanel;
use lector_ui::views::{Workspace, WorkspaceEvent};

use crate::branches_panel::BranchesPanel;
use crate::changes_panel::{ChangesPanel, PathOp};
use crate::diff_source::RevisionSource;
use crate::diff_view::{open_diff, DiffTarget};
use crate::history_panel::{HistoryPanel, HistoryScope};
use crate::inline_conflicts::InlineConflicts;
use crate::store::{GitStore, GitStoreEvent};

actions!(
    diff,
    [
        /// Diff the active file against a branch, tag or commit.
        CompareWith,
        /// Review all changes of the working tree in a tab.
        ReviewWorkingTree,
        /// Compare the current branch with its base (pull-request view).
        ReviewBranch,
    ]
);

/// Views of one workspace window, reachable from the store.
#[derive(Clone)]
pub struct GitViews {
    /// Changes panel.
    pub changes: Entity<ChangesPanel>,
    /// History panel.
    pub history: Entity<HistoryPanel>,
    /// Branches panel.
    pub branches: Entity<BranchesPanel>,
    /// Inline conflict actions of the window's file views.
    pub conflicts: Entity<InlineConflicts>,
}

/// Git stores by workspace window, for app-wide hooks (`commands_bridge`).
#[derive(Default)]
struct StoresByWorkspace(std::collections::HashMap<gpui::EntityId, gpui::WeakEntity<GitStore>>);

impl gpui::Global for StoresByWorkspace {}

/// The git store of a workspace window, if it is in a repository.
pub fn store_for(workspace: gpui::EntityId, cx: &App) -> Option<Entity<GitStore>> {
    cx.try_global::<StoresByWorkspace>()?
        .0
        .get(&workspace)?
        .upgrade()
}

/// The changeset shown by the active tab (diff, review or commit tab).
fn active_changeset(ws: &Workspace, cx: &App) -> Option<lector_git::Changeset> {
    let item = ws.active_pane().read(cx).active_item()?;
    match item.tab_kind(cx)? {
        lector_config::TabKind::Diff { changeset_key, .. } => {
            lector_workspace::review::parse_changeset_key(&changeset_key)
        }
        lector_config::TabKind::Commit { oid } => {
            oid.parse().ok().map(lector_git::Changeset::Commit)
        }
        lector_config::TabKind::Other { label } => label
            .strip_prefix("review:")
            .and_then(lector_workspace::review::parse_changeset_key),
        lector_config::TabKind::File { .. } => None,
    }
}

/// Hooks the git views into the command API: the focus carries the active
/// changeset, and `UiRequest::OpenChangeset` opens a review / commit tab.
fn init_command_hooks(cx: &mut App) {
    cx.set_global(StoresByWorkspace::default());
    lector_ui::commands_bridge::register_focus_provider(cx, |ws, focus, cx| {
        if focus.changeset.is_none() {
            focus.changeset = active_changeset(ws, cx);
        }
    });
    lector_ui::commands_bridge::set_undo_offer(cx, |ws, entry, summary, level, _window, cx| {
        match store_for(cx.entity_id(), cx) {
            Some(store) => activity::offer_undo(ws, &store, entry, summary, level, cx),
            None => ws.show_toast_level(summary, level, cx),
        }
    });
    lector_ui::commands_bridge::set_changeset_opener(cx, |_ws, cs, window, cx| {
        let Some(store) = store_for(cx.entity_id(), cx) else {
            return;
        };
        window.defer(cx, move |window, cx| match cs {
            lector_git::Changeset::Commit(oid) => {
                review_tab::open_commit_tab(&store, oid, false, window, cx)
            }
            cs => review_tab::open_review(&store, cs, window, cx),
        });
    });
}

/// The action behind "Review your N uncommitted changes".
fn review_working_tree_action() -> Box<dyn gpui::Action> {
    Box::new(ReviewWorkingTree)
}

/// Registers the git views with every new workspace window.
pub fn init(cx: &mut App) {
    init_command_hooks(cx);
    // The askpass server starts with the first network operation; its socket directory goes
    // when the app quits.
    cx.on_app_quit(|cx| {
        askpass_ui::shutdown(cx);
        async {}
    })
    .detach();
    cx.observe_new(|ws: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let workspace = cx.entity();
        let project = ws.project().clone();
        let store = cx.new(|cx| GitStore::new(&workspace, Some(project), window, cx));
        if store.read(cx).repo().is_none() {
            return;
        }
        let ws_id = workspace.entity_id();
        cx.default_global::<StoresByWorkspace>()
            .0
            .insert(ws_id, store.downgrade());
        // v0.7: sync with the remote (status bar control, Changes panel bar, actions).
        let window_handle = window.window_handle();
        let controller = cx.new(|cx| sync::SyncController::new(store.clone(), window_handle, cx));
        store.update(cx, |s, _| s.sync = Some(controller.clone()));
        let control = cx.new(|cx| sync::SyncControl::new(controller.clone(), cx));
        ws.set_branch_status_view(Some(control.clone().into()), cx);
        sync::register(ws, &controller, &control);
        let changes = cx.new(|cx| ChangesPanel::new(store.clone(), cx));
        let history = cx.new(|cx| HistoryPanel::new(store.clone(), window, cx));
        let branches = cx.new(|cx| BranchesPanel::new(store.clone(), cx));
        let prs = cx.new(|cx| pull_requests::PullRequestsPanel::new(store.clone(), cx));
        let inline = cx.new(|_| InlineConflicts::new(store.clone()));
        store.update(cx, |s, _| {
            s.views = Some(GitViews {
                changes: changes.clone(),
                history: history.clone(),
                branches: branches.clone(),
                conflicts: inline.clone(),
            })
        });
        // Inline conflict actions on every file tab (attached as tabs appear).
        let i = inline.clone();
        cx.subscribe(&workspace, move |ws, _, event: &WorkspaceEvent, cx| {
            if matches!(
                event,
                WorkspaceEvent::ActiveItemChanged | WorkspaceEvent::FilesChanged(_)
            ) {
                i.update(cx, |i, cx| i.scan(ws, cx));
            }
        })
        .detach();
        inline.update(cx, |i, cx| i.scan(ws, cx));
        ws.sidebar().update(cx, |sb, cx| {
            sb.set_panel_view(SidebarPanel::Changes, changes.clone().into(), cx);
            sb.set_panel_view(SidebarPanel::History, history.into(), cx);
            sb.set_panel_view(SidebarPanel::Branches, branches.into(), cx);
            sb.set_panel_view(SidebarPanel::PullRequests, prs.into(), cx);
        });
        // Branch + ahead/behind in the status bar.
        cx.subscribe(&store, |ws, store, _: &GitStoreEvent, cx| {
            let s = store.read(cx);
            let branch = s.branch_label().map(SharedString::from);
            let status = s.head.as_ref().and_then(|h| {
                let b = s.branch_label()?;
                let mut label = b;
                if h.ahead > 0 {
                    label.push_str(&format!(" ↑{}", h.ahead));
                }
                if h.behind > 0 {
                    label.push_str(&format!(" ↓{}", h.behind));
                }
                Some(SharedString::from(label))
            });
            ws.set_branch(branch, status, cx);
        })
        .detach();

        // "Review your N uncommitted changes" (welcome view) and the "new changes" chip.
        let model = store.read(cx).review_model.clone();
        cx.subscribe(
            &model,
            |ws, model, event: &review_model::ReviewModelEvent, cx| {
                if *event != review_model::ReviewModelEvent::Hint {
                    return;
                }
                let (uncommitted, new_since_last) = model.read(cx).hint();
                ws.set_review_hint(
                    (uncommitted > 0).then_some(lector_ui::views::ReviewHint {
                        uncommitted,
                        new_since_last,
                        open_review: review_working_tree_action,
                    }),
                    cx,
                );
            },
        )
        .detach();

        activity::register(ws, &store);
        let s = store.clone();
        ws.register_tab_restorer(move |kind, _window, cx| restore_tab(&s, kind, cx));
        // The workspace restores its session before extensions are
        // registered, so git tabs from the session are reopened here.
        restore_git_tabs(ws, &store, window, cx);
        // v0.6: pull requests and review comments.
        ws.register_action(
            |ws, _: &lector_ui::actions::review::ShowPullRequests, window, cx| {
                ws.show_panel(SidebarPanel::PullRequests, window, cx)
            },
        );
        let s = store.clone();
        ws.register_action(
            move |_, _: &lector_ui::actions::review::SignIn, window, cx| {
                let s = s.clone();
                window.defer(cx, move |window, cx| {
                    pull_requests::show_sign_in_help(&s, window, cx)
                });
            },
        );
        ws.register_action(|ws, _: &lector_ui::actions::review::SubmitReview, _, cx| {
            ws.show_toast(
                "Open a pull request review (Pull Requests panel) to submit a review",
                cx,
            )
        });
        let s = store.clone();
        ws.register_action(move |ws, _: &editor::ToggleBlame, window, cx| {
            blame::toggle_blame(ws, &s, window, cx);
        });
        // Handlers below call into helpers that read the workspace, so they
        // run deferred (the workspace is leased while its actions dispatch).
        let s = store.clone();
        ws.register_action(move |ws, _: &editor::ShowFileHistory, window, cx| {
            if let Some(rel) = active_rel_path(ws, &s, cx) {
                let s = s.clone();
                window.defer(cx, move |window, cx| show_file_history(&s, rel, window, cx));
            }
        });
        let s = store.clone();
        ws.register_action(move |ws, _: &editor::ShowLineHistory, window, cx| {
            let (Some(rel), Some(view)) = (active_rel_path(ws, &s, cx), ws.active_code_view(cx))
            else {
                return;
            };
            let (a, b) = view.read(cx).selection().range();
            let end = if b.col == 0 && b.row > a.row {
                b.row
            } else {
                b.row + 1
            };
            let s = s.clone();
            window.defer(cx, move |window, cx| {
                show_history_scope(&s, HistoryScope::Lines(rel, a.row..end), window, cx)
            });
        });
        register_git_actions(ws, &store, &changes);
        conflict_actions::register(ws, &store, &changes);
        let s = store.clone();
        ws.register_action(move |ws, _: &editor::CopyPermalink, window, cx| {
            copy_permalink(ws, &s, window, cx);
        });
        let s = store.clone();
        ws.register_action(move |ws, _: &CompareWith, window, cx| {
            let Some(rel) = active_rel_path(ws, &s, cx) else {
                return;
            };
            let s = s.clone();
            window.defer(cx, move |window, cx| {
                let s2 = s.clone();
                changes_panel::pick_from_repo(
                    &s,
                    "Compare file with…",
                    window,
                    cx,
                    move |c, window, cx| open_file_vs_rev(&s2, rel.clone(), c.rev, window, cx),
                );
            });
        });
        let s = store.clone();
        ws.register_action(move |_, _: &ReviewWorkingTree, window, cx| {
            let s = s.clone();
            window.defer(cx, move |window, cx| {
                review_tab::open_review(&s, lector_git::Changeset::WorkingTree, window, cx)
            });
        });
        let s = store.clone();
        ws.register_action(move |_, _: &ReviewBranch, window, cx| {
            let st = s.read(cx);
            let (Some(repo), Some(branch)) =
                (st.repo(), st.head.as_ref().and_then(|h| h.branch.clone()))
            else {
                return;
            };
            let Some(base) = lector_workspace::review::default_base(repo, &branch) else {
                return;
            };
            let s = s.clone();
            window.defer(cx, move |window, cx| {
                review_tab::open_review(
                    &s,
                    lector_git::Changeset::BranchVsBase { branch, base },
                    window,
                    cx,
                )
            });
        });
    })
    .detach();
}

/// `git::*` workspace actions, so menus and the palette can stage, discard and
/// commit from anywhere. When the Changes list, the commit editor or a diff has
/// focus, their own handlers run first; these act on the active file.
fn register_git_actions(
    ws: &mut Workspace,
    store: &Entity<GitStore>,
    changes: &Entity<ChangesPanel>,
) {
    fn path_action<A: gpui::Action>(
        ws: &mut Workspace,
        store: &Entity<GitStore>,
        changes: &Entity<ChangesPanel>,
        op: PathOp,
    ) {
        let (s, c) = (store.clone(), changes.clone());
        ws.register_action(move |ws, _: &A, window, cx| {
            let Some(rel) = active_rel_path(ws, &s, cx) else {
                return;
            };
            let c = c.clone();
            window.defer(cx, move |window, cx| {
                c.update(cx, |p, cx| p.path_op(&rel, op, window, cx));
            });
        });
    }
    path_action::<git_actions::Stage>(ws, store, changes, PathOp::Stage);
    path_action::<git_actions::Unstage>(ws, store, changes, PathOp::Unstage);
    path_action::<git_actions::DiscardChanges>(ws, store, changes, PathOp::Discard);

    let c = changes.clone();
    ws.register_action(move |_, _: &git_actions::StageAll, window, cx| {
        let c = c.clone();
        window.defer(cx, move |_, cx| c.update(cx, |p, cx| p.stage_all(None, cx)));
    });
    let c = changes.clone();
    ws.register_action(move |_, _: &git_actions::UnstageAll, window, cx| {
        let c = c.clone();
        window.defer(cx, move |_, cx| c.update(cx, |p, cx| p.unstage_all(cx)));
    });
    let c = changes.clone();
    ws.register_action(move |_, _: &git_actions::DiscardAll, window, cx| {
        let c = c.clone();
        window.defer(cx, move |window, cx| {
            c.update(cx, |p, cx| p.discard_all(None, window, cx))
        });
    });
    let c = changes.clone();
    ws.register_action(move |_, _: &git_actions::Refresh, window, cx| {
        let c = c.clone();
        window.defer(cx, move |_, cx| c.update(cx, |p, cx| p.refresh(cx)));
    });
    let c = changes.clone();
    ws.register_action(move |ws, _: &git_actions::FocusCommitMessage, window, cx| {
        let showing = ws.is_panel_shown(SidebarPanel::Changes, cx);
        let c = c.clone();
        window.defer(cx, move |window, cx| {
            if !showing {
                window.dispatch_action(Box::new(ws_actions::ShowChanges), cx);
            }
            let composer = c.read(cx).composer().clone();
            window.defer(cx, move |window, cx| {
                composer.read(cx).focus_message(window, cx);
            });
        });
    });
    let c = changes.clone();
    ws.register_action(move |_, _: &git_actions::Commit, window, cx| {
        let c = c.clone();
        window.defer(cx, move |window, cx| {
            let composer = c.read(cx).composer().clone();
            composer.update(cx, |p, cx| p.commit(window, cx));
        });
    });
    let c = changes.clone();
    ws.register_action(move |_, _: &git_actions::ToggleAmend, window, cx| {
        let c = c.clone();
        window.defer(cx, move |_, cx| {
            let composer = c.read(cx).composer().clone();
            composer.update(cx, |p, cx| p.toggle_amend(cx));
        });
    });
}

fn restore_tab(
    store: &Entity<GitStore>,
    kind: &lector_config::TabKind,
    cx: &mut App,
) -> Option<Box<dyn lector_ui::views::pane::PaneItem>> {
    review_tab::restore_item(store, kind, cx)
        .or_else(|| diff_view::restore_item(store, kind, cx))
        .or_else(|| merge_editor::restore_item(store, kind, cx))
        .or_else(|| activity::restore_item(store, kind, cx))
}

fn restore_git_tabs(
    ws: &mut Workspace,
    store: &Entity<GitStore>,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    if !lector_ui::settings::AppSettings::get(cx).session.restore {
        return;
    }
    let Some(session) = lector_ui::commands_bridge::session_store().load(ws.project().root())
    else {
        return;
    };
    let pane = ws.active_pane().clone();
    let mut activate = None;
    for tab in session.panes.iter().flat_map(|p| p.tabs.iter()) {
        if matches!(tab.kind, lector_config::TabKind::File { .. }) {
            continue;
        }
        let Some(item) = restore_tab(store, &tab.kind, cx) else {
            continue;
        };
        let key = item.item_key(cx);
        if key
            .as_ref()
            .is_some_and(|k| pane.read(cx).index_for_key(k, cx).is_some())
        {
            continue;
        }
        pane.update(cx, |p, cx| p.add_item(item, false, window, cx));
        if tab.active {
            activate = key;
        }
    }
    if let Some(key) = activate {
        if let Some(ix) = pane.read(cx).index_for_key(&key, cx) {
            pane.update(cx, |p, cx| p.activate(ix, false, window, cx));
        }
    }
}

/// Repo-relative path of the workspace's active file.
fn active_rel_path(ws: &Workspace, store: &Entity<GitStore>, cx: &App) -> Option<String> {
    let path = ws.active_path(cx)?;
    let repo = store.read(cx).repo()?;
    let wd = dunce::canonicalize(repo.workdir()).ok()?;
    let abs = dunce::canonicalize(path).ok()?;
    let rel = abs.strip_prefix(&wd).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

fn copy_permalink(
    ws: &mut Workspace,
    store: &Entity<GitStore>,
    _window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let Some(rel) = active_rel_path(ws, store, cx) else {
        return;
    };
    let Some(repo) = store.read(cx).repo().cloned() else {
        return;
    };
    let lines = ws.active_code_view(cx).map(|v| {
        let (a, b) = v.read(cx).selection().range();
        let end = if b.col == 0 && b.row > a.row {
            b.row
        } else {
            b.row + 1
        };
        a.row..end
    });
    let task = cx.background_spawn(async move { repo.permalink(&rel, lines, None) });
    cx.spawn(async move |ws, cx| {
        let result = task.await;
        let _ = ws.update(cx, |ws, cx| match result {
            Ok(Some(link)) => {
                cx.write_to_clipboard(ClipboardItem::new_string(link));
                ws.show_toast("Permalink copied", cx);
            }
            Ok(None) => ws.show_toast("No web remote to build a permalink", cx),
            Err(e) => {
                tracing::warn!("permalink: {e}");
                ws.show_toast("Permalink unavailable", cx);
            }
        });
    })
    .detach();
}

/// Shows the History panel with `scope`.
pub fn show_history_scope(
    store: &Entity<GitStore>,
    scope: HistoryScope,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(views) = store.read(cx).views.clone() else {
        return;
    };
    views.history.update(cx, |h, cx| h.set_scope(scope, cx));
    let Some(ws) = store.read(cx).workspace.upgrade() else {
        return;
    };
    let active = ws.read(cx).sidebar().read(cx).active_panel();
    if active != SidebarPanel::History {
        window.dispatch_action(Box::new(ws_actions::ShowHistory), cx);
    }
}

/// Shows the history of a repo-relative file (follows renames).
pub fn show_file_history(store: &Entity<GitStore>, rel: String, window: &mut Window, cx: &mut App) {
    show_history_scope(store, HistoryScope::File(rel), window, cx);
}

/// Opens a commit tab.
pub fn open_commit(store: &Entity<GitStore>, oid: Oid, window: &mut Window, cx: &mut App) {
    review_tab::open_commit_tab(store, oid, false, window, cx);
}

/// Opens a diff of the working copy of `rel` against `rev`.
pub fn open_file_vs_rev(
    store: &Entity<GitStore>,
    rel: String,
    rev: String,
    window: &mut Window,
    cx: &mut App,
) {
    let ws = store.read(cx).workspace.clone();
    let name = util::split_path(&rel).0.to_string();
    let short = if rev.len() == 40 {
        rev[..7].to_string()
    } else {
        rev.clone()
    };
    let title: SharedString = format!("{name} vs {short}").into();
    let key = format!("vs:{rev}:{rel}");
    open_diff(
        &ws,
        store,
        DiffTarget::FileVsRev { path: rel, rev },
        None,
        Some(title),
        key,
        false,
        window,
        cx,
    );
}

/// Opens a read-only tab with `rel` as of `rev` ("file.rs @ abc1234").
pub fn open_file_at_revision(
    store: &Entity<GitStore>,
    rev: String,
    rel: String,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(project) = store.read(cx).project().cloned() else {
        return;
    };
    let Some(ws) = store.read(cx).workspace.upgrade() else {
        return;
    };
    let task = cx.background_spawn(async move {
        let repo = project
            .repo()
            .ok_or_else(|| "not a repository".to_string())?;
        let short = repo
            .resolve(&rev)
            .map(|o| o.short())
            .unwrap_or_else(|_| rev.clone());
        let bytes = repo
            .file_at(&rev, &rel)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("{rel} does not exist at {short}"))?;
        let side = lector_workspace::review::side_from_bytes(&rel, bytes, project.languages());
        Ok::<_, String>((side, short, rel))
    });
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            let _ = cx.update(|window, cx| {
                let (side, short, rel) = match result {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("view at revision: {e}");
                        return;
                    }
                };
                let title = format!("{} @ {short}", util::split_path(&rel).0);
                let source: Rc<dyn CodeSource> = Rc::new(RevisionSource::new(side));
                let view = cx.new(|cx| CodeView::new(source, cx));
                let item = Box::new(CodeItem::new(view, None, title));
                ws.update(cx, |ws, cx| ws.add_item(item, false, window, cx));
            });
        })
        .detach();
}
