//! Git review views for Lector, registered into the workspace via [`init`]:
//! the Changes, History and Branches panels, diff tabs (split / unified),
//! changeset review and commit tabs, blame, file/line history, permalinks.
#![forbid(unsafe_code)]

pub mod blame;
pub mod branches_panel;
pub mod changes_panel;
pub mod diff_source;
pub mod diff_view;
pub mod history_panel;
pub mod ref_picker;
pub mod review_tab;
pub mod store;
pub mod util;

use std::rc::Rc;

use gpui::{actions, App, AppContext, ClipboardItem, Entity, SharedString, Window};
use lector_git::Oid;
use lector_ui::actions::{editor, workspace as ws_actions};
use lector_ui::code_view::{CodeSource, CodeView};
use lector_ui::views::pane::CodeItem;
use lector_ui::views::sidebar::SidebarPanel;
use lector_ui::views::Workspace;

use crate::branches_panel::BranchesPanel;
use crate::changes_panel::ChangesPanel;
use crate::diff_source::RevisionSource;
use crate::diff_view::{open_diff, DiffTarget};
use crate::history_panel::{HistoryPanel, HistoryScope};
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
}

/// Registers the git views with every new workspace window.
pub fn init(cx: &mut App) {
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
        let changes = cx.new(|cx| ChangesPanel::new(store.clone(), cx));
        let history = cx.new(|cx| HistoryPanel::new(store.clone(), window, cx));
        let branches = cx.new(|cx| BranchesPanel::new(store.clone(), cx));
        store.update(cx, |s, _| {
            s.views = Some(GitViews {
                changes: changes.clone(),
                history: history.clone(),
                branches: branches.clone(),
            })
        });
        ws.sidebar().update(cx, |sb, cx| {
            sb.set_panel_view(SidebarPanel::Changes, changes.into(), cx);
            sb.set_panel_view(SidebarPanel::History, history.into(), cx);
            sb.set_panel_view(SidebarPanel::Branches, branches.into(), cx);
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

        let s = store.clone();
        ws.register_tab_restorer(move |kind, _window, cx| restore_tab(&s, kind, cx));
        // The workspace restores its session before extensions are
        // registered, so git tabs from the session are reopened here.
        restore_git_tabs(ws, &store, window, cx);
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

fn restore_tab(
    store: &Entity<GitStore>,
    kind: &lector_config::TabKind,
    cx: &mut App,
) -> Option<Box<dyn lector_ui::views::pane::PaneItem>> {
    review_tab::restore_item(store, kind, cx).or_else(|| diff_view::restore_item(store, kind, cx))
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
    let Some(session) = lector_config::SessionStore::new().load(ws.project().root()) else {
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
    let wd = repo.workdir().canonicalize().ok()?;
    let abs = path.canonicalize().ok()?;
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
