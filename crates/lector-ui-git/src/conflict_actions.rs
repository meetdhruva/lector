//! Workspace-level `git::*` conflict actions (so menus, the palette and key
//! bindings work from a normal editor tab): open the merge editor, next /
//! previous conflict, accept current / incoming / both (at the cursor), accept
//! all (confirmed), mark resolved (save + stage), continue / abort the
//! operation in progress. The merge editor handles the same actions itself
//! when it has focus.

use gpui::{App, Context, Entity, Window};
use lector_ui::actions::git as git_actions;
use lector_ui::editor::FileView;
use lector_ui::views::Workspace;
use lector_workspace::conflicts::Resolution;

use crate::changes_panel::ChangesPanel;
use crate::confirm::{confirm, ConfirmSpec};
use crate::inline_conflicts::{count_label, FileConflicts};
use crate::merge_editor::{open_merge_editor, rel_path};
use crate::staging::{run_op, GitOp};
use crate::store::GitStore;
use crate::util::split_path;

/// The conflict tracker of the active file view.
fn active_tracker(
    ws: &Workspace,
    store: &Entity<GitStore>,
    cx: &App,
) -> Option<(Entity<FileView>, Entity<FileConflicts>)> {
    let fv = ws.active_file_view(cx)?;
    let views = store.read(cx).views.clone()?;
    let tracker = views.conflicts.read(cx).tracker(&fv)?;
    Some((fv, tracker))
}

fn toast(store: &Entity<GitStore>, msg: &str, cx: &mut App) {
    if let Some(ws) = store.read(cx).workspace.upgrade() {
        let msg = msg.to_string();
        ws.update(cx, |ws, cx| ws.show_toast(msg, cx));
    }
}

/// Registers the actions on `ws`.
pub fn register(ws: &mut Workspace, store: &Entity<GitStore>, changes: &Entity<ChangesPanel>) {
    let s = store.clone();
    ws.register_action(move |ws, _: &git_actions::OpenMergeEditor, window, cx| {
        let entries = s.read(cx).conflicts.entries.clone();
        let active = ws
            .active_path(cx)
            .and_then(|p| rel_path(&s, &p, cx))
            .filter(|rel| {
                entries.iter().any(|e| &e.path == rel)
                    || active_tracker(ws, &s, cx).is_some_and(|(_, t)| {
                        t.read(cx)
                            .parsed()
                            .is_some_and(|p| !p.parse.regions.is_empty())
                    })
            });
        let target = active.or_else(|| entries.first().map(|e| e.path.clone()));
        let s = s.clone();
        window.defer(cx, move |window, cx| match target {
            Some(rel) => open_merge_editor(&s, rel, window, cx),
            None => toast(&s, "No merge conflicts", cx),
        });
    });
    for forward in [true, false] {
        let s = store.clone();
        let handler =
            move |ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| {
                let Some((_, tracker)) = active_tracker(ws, &s, cx) else {
                    return;
                };
                window.defer(cx, move |_, cx| {
                    tracker.update(cx, |t, cx| t.go_to_adjacent(forward, cx));
                });
            };
        if forward {
            ws.register_action(move |ws, _: &git_actions::NextConflict, window, cx| {
                handler(ws, window, cx)
            });
        } else {
            ws.register_action(move |ws, _: &git_actions::PrevConflict, window, cx| {
                handler(ws, window, cx)
            });
        }
    }
    fn accept_at_cursor(
        ws: &Workspace,
        store: &Entity<GitStore>,
        resolution: Resolution,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some((fv, tracker)) = active_tracker(ws, store, cx) else {
            return;
        };
        let row = fv.read(cx).code_view().read(cx).selection().head.row;
        window.defer(cx, move |_, cx| {
            tracker.update(cx, |t, cx| {
                if let Some(ix) = t.conflict_for_row(row) {
                    t.resolve(ix, resolution, cx);
                }
            });
        });
    }
    let s = store.clone();
    ws.register_action(move |ws, _: &git_actions::AcceptOurs, window, cx| {
        accept_at_cursor(ws, &s, Resolution::Ours, window, cx)
    });
    let s = store.clone();
    ws.register_action(move |ws, _: &git_actions::AcceptTheirs, window, cx| {
        accept_at_cursor(ws, &s, Resolution::Theirs, window, cx)
    });
    let s = store.clone();
    ws.register_action(move |ws, _: &git_actions::AcceptBoth, window, cx| {
        accept_at_cursor(ws, &s, Resolution::OursThenTheirs, window, cx)
    });
    for ours in [true, false] {
        let s = store.clone();
        let handler =
            move |ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| {
                let Some((fv, tracker)) = active_tracker(ws, &s, cx) else {
                    return;
                };
                let s = s.clone();
                window.defer(cx, move |window, cx| {
                    accept_all_confirmed(&s, &fv, &tracker, ours, window, cx)
                });
            };
        if ours {
            ws.register_action(move |ws, _: &git_actions::AcceptAllOurs, window, cx| {
                handler(ws, window, cx)
            });
        } else {
            ws.register_action(move |ws, _: &git_actions::AcceptAllTheirs, window, cx| {
                handler(ws, window, cx)
            });
        }
    }
    let s = store.clone();
    ws.register_action(move |ws, _: &git_actions::MarkResolved, window, cx| {
        let Some((fv, tracker)) = active_tracker(ws, &s, cx) else {
            return;
        };
        let s = s.clone();
        window.defer(cx, move |window, cx| {
            mark_file_resolved(&s, &fv, &tracker, false, window, cx)
        });
    });
    let c = changes.clone();
    let s = store.clone();
    ws.register_action(move |_, _: &git_actions::ContinueOperation, window, cx| {
        let (c, s) = (c.clone(), s.clone());
        window.defer(cx, move |window, cx| {
            if s.read(cx).conflicts.operation.is_none() {
                toast(
                    &s,
                    "No merge, rebase, cherry-pick or revert in progress",
                    cx,
                );
                return;
            }
            let banner = c.read(cx).banner().clone();
            banner.update(cx, |b, cx| b.continue_op(window, cx));
        });
    });
    let c = changes.clone();
    let s = store.clone();
    ws.register_action(move |_, _: &git_actions::AbortOperation, window, cx| {
        let (c, s) = (c.clone(), s.clone());
        window.defer(cx, move |window, cx| {
            if s.read(cx).conflicts.operation.is_none() {
                toast(
                    &s,
                    "No merge, rebase, cherry-pick or revert in progress",
                    cx,
                );
                return;
            }
            let banner = c.read(cx).banner().clone();
            banner.update(cx, |b, cx| b.abort_op(window, cx));
        });
    });
}

fn accept_all_confirmed(
    store: &Entity<GitStore>,
    fv: &Entity<FileView>,
    tracker: &Entity<FileConflicts>,
    ours: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let n = tracker
        .read(cx)
        .parsed()
        .map_or(0, |p| p.parse.regions.len());
    let Some(ws) = store.read(cx).workspace.upgrade() else {
        return;
    };
    if n == 0 {
        toast(store, "No conflict markers in this file", cx);
        return;
    }
    let side = if ours { "current" } else { "incoming" };
    let name = fv.read(cx).rel_path().to_string();
    let spec = ConfirmSpec {
        title: format!("Accept all {side} changes in {}?", split_path(&name).0).into(),
        message: vec![format!(
            "Resolves {} with the {side} version; the other side's changes in them are dropped. \
                 Undo with Ctrl+Z; nothing is written until you save.",
            count_label(n, "conflict")
        )
        .into()],
        items: vec![name.into()],
        confirm_label: if ours {
            "Accept All Current".into()
        } else {
            "Accept All Incoming".into()
        },
    };
    let tracker = tracker.clone();
    confirm(&ws, spec, window, cx, move |_, cx| {
        let res = if ours {
            Resolution::Ours
        } else {
            Resolution::Theirs
        };
        tracker.update(cx, |t, cx| t.resolve_all(res, cx));
    });
}

/// Saves and stages a file from a normal editor tab (asks first when markers remain).
pub fn mark_file_resolved(
    store: &Entity<GitStore>,
    fv: &Entity<FileView>,
    tracker: &Entity<FileConflicts>,
    force: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(rel) = rel_path(store, fv.read(cx).path(), cx) else {
        return;
    };
    let markers = tracker
        .update(cx, |t, cx| t.fresh(cx))
        .map_or(0, |p| p.parse.regions.len() + p.parse.problems.len());
    if markers > 0 && !force {
        let Some(ws) = store.read(cx).workspace.upgrade() else {
            return;
        };
        let spec = ConfirmSpec {
            title: format!("{} still has conflict markers", split_path(&rel).0).into(),
            message: vec![
                "Marking the file resolved saves and stages it with the markers in it, and they \
                 would be committed as text."
                    .into(),
            ],
            items: vec![rel.clone().into()],
            confirm_label: "Resolve Anyway".into(),
        };
        let (s, f, t) = (store.clone(), fv.clone(), tracker.clone());
        confirm(&ws, spec, window, cx, move |window, cx| {
            mark_file_resolved(&s, &f, &t, true, window, cx)
        });
        return;
    }
    let save = fv.update(cx, |v, cx| v.save(window, cx));
    let store = store.clone();
    window
        .spawn(cx, async move |cx| {
            match save.await {
                Ok(true) => {}
                Ok(false) => return,
                Err(e) => {
                    let _ = cx.update(|_, cx| toast(&store, &format!("Saving failed: {e:#}"), cx));
                    return;
                }
            }
            let Ok(task) = cx.update(|_, cx| run_op(&store, GitOp::Stage(vec![rel.clone()]), cx))
            else {
                return;
            };
            let result = task.await;
            let _ = cx.update(|_, cx| match result {
                Ok(_) => toast(&store, &format!("Resolved {}", split_path(&rel).0), cx),
                Err(e) => toast(&store, &e, cx),
            });
        })
        .detach();
}
