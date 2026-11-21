//! Context menus of the [`ChangesPanel`]: the per-file menu (stage, discard,
//! open, copy path, ...) and the mode menu (working tree or another changeset).

use gpui::{prelude::*, App, ClipboardItem, Pixels, Point, Window};
use lector_git::{ChangeKind, Changeset};
use lector_ui::actions::workspace as ws_actions;
use lector_ui::components::context_menu::ContextMenu;
use lector_ui::icons::IconName;

use super::{ChangesMode, ChangesPanel, Section};
use crate::ref_picker::pick_from_repo;

impl ChangesPanel {
    pub(super) fn deploy_menu(
        &mut self,
        si: usize,
        fi: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((section, _, file)) = self.file(si, fi) else {
            return;
        };
        let rel = file.path().to_string();
        let exists = !matches!(file.change.kind, ChangeKind::Deleted);
        self.selected = Some((section, rel.clone()));
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let t = this.clone();
            let r = rel.clone();
            let toggle = {
                let t = this.clone();
                move |_: &mut Window, cx: &mut App| {
                    let _ = t.update(cx, |p, cx| p.toggle_stage(si, fi, cx));
                }
            };
            let discard = {
                let t = this.clone();
                move |window: &mut Window, cx: &mut App| {
                    let _ = t.update(cx, |p, cx| p.discard_file(si, fi, window, cx));
                }
            };
            let mut menu = match section {
                Section::Staged => menu
                    .icon_entry("Unstage", IconName::Minus, toggle)
                    .separator(),
                Section::Unstaged => menu
                    .icon_entry("Stage", IconName::Plus, toggle)
                    .icon_entry("Discard Changes", IconName::Undo, discard)
                    .separator(),
                Section::Untracked => menu
                    .icon_entry("Stage", IconName::Plus, toggle)
                    .icon_entry("Delete File", IconName::Trash, discard)
                    .separator(),
                Section::Conflicts => {
                    let (t, r) = (this.clone(), rel.clone());
                    menu.icon_entry(
                        "Open Merge Editor",
                        IconName::GitMerge,
                        move |window, cx| {
                            let _ = t.update(cx, |p, cx| {
                                let store = p.store.clone();
                                let rel = r.clone();
                                window.defer(cx, move |window, cx| {
                                    crate::merge_editor::open_merge_editor(&store, rel, window, cx)
                                });
                            });
                        },
                    )
                    .icon_entry("Mark Resolved (Stage)", IconName::Check, toggle)
                    .separator()
                }
                Section::Files => menu,
            };
            menu = menu.icon_entry("Open Diff", IconName::Diff, move |window, cx| {
                let _ = t.update(cx, |p, cx| p.open_file_diff(si, fi, false, window, cx));
            });
            if exists {
                let (t1, r1) = (this.clone(), r.clone());
                let (t2, r2) = (this.clone(), r.clone());
                menu = menu
                    .icon_entry("Open File", IconName::FileCode, move |window, cx| {
                        let _ = t1.update(cx, |p, cx| p.open_file(&r1, false, window, cx));
                    })
                    .icon_entry("Open in Split", IconName::Columns, move |window, cx| {
                        let _ = t2.update(cx, |p, cx| p.open_file(&r2, true, window, cx));
                    });
            }
            let (t3, r3) = (this.clone(), r.clone());
            let (t4, r4) = (this.clone(), r.clone());
            let (t5, r5) = (this.clone(), r.clone());
            let r6 = r.clone();
            menu = menu
                .separator()
                .icon_entry("Copy Path", IconName::Copy, move |_, cx| {
                    let abs = t3
                        .upgrade()
                        .and_then(|p| p.read(cx).absolute(&r3, cx))
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| r3.clone());
                    cx.write_to_clipboard(ClipboardItem::new_string(abs));
                })
                .entry("Copy Relative Path", move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(r6.clone()));
                })
                .separator()
                .icon_entry("File History", IconName::History, move |window, cx| {
                    let Some(p) = t4.upgrade() else { return };
                    let store = p.read(cx).store.clone();
                    crate::show_file_history(&store, r4.clone(), window, cx);
                });
            if exists {
                menu = menu.icon_entry(
                    "Reveal in File Tree",
                    IconName::ListTree,
                    move |window, cx| {
                        let Some(p) = t5.upgrade() else { return };
                        let Some(path) = p.read(cx).absolute(&r5, cx) else {
                            return;
                        };
                        let ws = p.read(cx).store.read(cx).workspace.clone();
                        let _ = ws.update(cx, |ws, cx| ws.open_path(path, true, window, cx));
                        window.dispatch_action(Box::new(ws_actions::RevealActiveFile), cx);
                    },
                );
            }
            menu
        });
        self.menu
            .deploy(menu, position, window, cx, |this: &mut Self| &mut this.menu);
        cx.notify();
    }

    pub(super) fn deploy_mode_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        let store = self.store.clone();
        let branch = self
            .store
            .read(cx)
            .head
            .as_ref()
            .and_then(|h| h.branch.clone());
        let default_base = self.base.base_for(branch.as_deref());
        let set = move |mode: ChangesMode| {
            let t = this.clone();
            move |_: &mut Window, cx: &mut App| {
                let _ = t.update(cx, |p, cx| p.set_mode(mode.clone(), cx));
            }
        };
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let mut menu = menu
                .header("Show changes")
                .icon_entry(
                    "Working Tree",
                    IconName::Files,
                    set(ChangesMode::WorkingTree),
                )
                .entry("Staged", set(ChangesMode::Changeset(Changeset::Staged)))
                .entry("Unstaged", set(ChangesMode::Changeset(Changeset::Unstaged)))
                .separator();
            if let (Some(b), Some(base)) = (branch.clone(), default_base.clone()) {
                menu = menu.icon_entry(
                    format!("Branch vs {base}"),
                    IconName::GitPullRequest,
                    set(ChangesMode::Changeset(Changeset::BranchVsBase {
                        branch: b,
                        base,
                    })),
                );
            }
            let s1 = store.clone();
            let b1 = branch.clone();
            let set1 = set.clone();
            let s2 = store.clone();
            let set2 = set.clone();
            menu.entry("Branch vs Base…", move |window, cx| {
                let Some(branch) = b1.clone() else { return };
                let set1 = set1.clone();
                pick_from_repo(
                    &s1,
                    "Compare current branch with base…",
                    window,
                    cx,
                    move |c, window, cx| {
                        set1(ChangesMode::Changeset(Changeset::BranchVsBase {
                            branch: branch.clone(),
                            base: c.rev,
                        }))(window, cx)
                    },
                );
            })
            .icon_entry(
                "Commit Range…",
                IconName::GitCompare,
                move |window, cx| {
                    let s3 = s2.clone();
                    let set2 = set2.clone();
                    pick_from_repo(
                        &s2,
                        "From revision (older)…",
                        window,
                        cx,
                        move |from, window, cx| {
                            let set2 = set2.clone();
                            pick_from_repo(
                                &s3,
                                "To revision (newer)…",
                                window,
                                cx,
                                move |to, window, cx| {
                                    set2(ChangesMode::Changeset(Changeset::Range {
                                        from: from.rev.clone(),
                                        to: to.rev,
                                    }))(window, cx)
                                },
                            );
                        },
                    );
                },
            )
        });
        self.menu
            .deploy(menu, position, window, cx, |this: &mut Self| &mut this.menu);
        cx.notify();
    }
}
