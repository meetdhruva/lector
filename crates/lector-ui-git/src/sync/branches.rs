//! Branch and stash commands of the [`SyncController`]: new / switch /
//! check out / rename / delete branch, set upstream, the ref pickers, and
//! stash push / pop / apply / drop.

use std::rc::Rc;

use gpui::{prelude::*, SharedString, Window};
use lector_git::Repo;
use lector_ui::background::{background, view_key, JobKind, Priority};
use lector_ui::icons::IconName;
use lector_ui::views::workspace::ToastLevel;
use serde_json::json;

use super::SyncController;
use crate::confirm::{confirm, ConfirmSpec};
use crate::sync_dialogs::{show_input, InputSpec};
use crate::sync_model::{validate_branch_name, OpKind};
use crate::sync_support::unreachable_commits;

impl SyncController {
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
        let key = view_key(JobKind::GitRefs, cx.entity_id());
        let work = background(cx, key, Priority::Visible, move |_| {
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
                let Ok(found) = work.await else { return };
                let (local, choices) = found;
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
        let key = view_key(JobKind::GitRefs, cx.entity_id());
        let work = background(cx, key, Priority::Visible, move |_| {
            unreachable_commits(&repo, &n)
        });
        let this = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                let Ok(lost) = work.await else { return };
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
        let key = view_key(JobKind::GitRefs, cx.entity_id());
        let work = background(cx, key, Priority::Visible, move |_| {
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
                let Ok(choices) = work.await else { return };
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
        let key = view_key(JobKind::GitRefs, cx.entity_id());
        let work = background(cx, key, Priority::Visible, move |_| {
            repo.stashes().unwrap_or_default()
        });
        let this = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                let Ok(stashes) = work.await else { return };
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
