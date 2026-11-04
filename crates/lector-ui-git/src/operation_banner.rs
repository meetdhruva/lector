//! The merge / rebase / cherry-pick / revert banner at the top of the Changes
//! panel ("Merging feature → main · 2 conflicts", "Rebasing onto main (3/7)")
//! with **Continue** and **Abort…**.
//!
//! Continue: a merge is concluded by committing from the commit composer with
//! `MERGE_MSG` prefilled; the others run `git <op> --continue` (`GIT_EDITOR=true`,
//! never blocks on an editor). Abort runs `git <op> --abort` after a
//! confirmation that says what is lost. Both are explicit clicks only.

use gpui::{
    div, prelude::*, px, App, Context, Entity, FontWeight, SharedString, Subscription, Task, Window,
};
use lector_git::{Oid, OperationKind, RepoOperation};
use lector_ui::components::{h_flex, metrics, v_flex, Button, ButtonStyle, IconButton, Tooltip};
use lector_ui::icons::{Icon, IconName, IconSize};
use lector_ui::{ActiveTheme, ColorExt};

use crate::commit_composer::CommitComposer;
use crate::confirm::{confirm, ConfirmSpec};
use crate::inline_conflicts::count_label;
use crate::staging::{run_op, GitOp};
use crate::store::{GitStore, GitStoreEvent};

/// The banner (rendered by the Changes panel while an operation is in progress).
pub struct OperationBanner {
    store: Entity<GitStore>,
    composer: Entity<CommitComposer>,
    busy: bool,
    error: Option<SharedString>,
    /// The operation whose message was prefilled (not again after the user clears it).
    prefilled: Option<(OperationKind, Option<Oid>)>,
    task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// Title of the banner: "Merging feature → main", "Rebasing feature onto main (3/7)".
pub fn title(op: &RepoOperation) -> String {
    match op.kind {
        OperationKind::Merge => {
            let incoming = op.incoming.as_deref().unwrap_or("commit");
            match &op.head_name {
                Some(head) => format!("Merging {incoming} → {head}"),
                None => format!("Merging {incoming}"),
            }
        }
        _ => op.describe(),
    }
}

impl OperationBanner {
    /// A banner for `store`, prefilling `composer` for merges.
    pub fn new(
        store: Entity<GitStore>,
        composer: Entity<CommitComposer>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subs = vec![cx.subscribe(&store, |this, _, _: &GitStoreEvent, cx| this.sync(cx))];
        let mut banner = OperationBanner {
            store,
            composer,
            busy: false,
            error: None,
            prefilled: None,
            task: None,
            _subscriptions: subs,
        };
        banner.sync(cx);
        banner
    }

    /// The operation in progress.
    pub fn operation(&self, cx: &App) -> Option<RepoOperation> {
        self.store.read(cx).conflicts.operation.clone()
    }

    fn conflict_count(&self, cx: &App) -> usize {
        self.store.read(cx).conflicts.entries.len()
    }

    /// Keeps the composer in merge mode and prefills `MERGE_MSG` once per merge.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let op = self.operation(cx);
        let merging = op.as_ref().is_some_and(|o| o.kind == OperationKind::Merge);
        self.composer.update(cx, |c, cx| c.set_merging(merging, cx));
        if let Some(op) = &op {
            let key = (op.kind, op.incoming_oid);
            if op.kind == OperationKind::Merge && self.prefilled != Some(key) {
                if let Some(msg) = &op.message {
                    let msg = msg.clone();
                    self.composer.update(cx, |c, cx| c.prefill(&msg, cx));
                    self.prefilled = Some(key);
                }
            }
        } else {
            self.prefilled = None;
            self.error = None;
        }
        cx.notify();
    }

    /// `git::ContinueOperation`.
    pub fn continue_op(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(op) = self.operation(cx) else {
            return;
        };
        if self.busy {
            return;
        }
        let conflicts = self.conflict_count(cx);
        if conflicts > 0 {
            self.error = Some(
                format!(
                    "Resolve the {} first (Mark Resolved stages a file).",
                    count_label(conflicts, "conflicted file")
                )
                .into(),
            );
            cx.notify();
            return;
        }
        self.error = None;
        if op.kind == OperationKind::Merge {
            if let Some(msg) = op.message.clone() {
                self.composer.update(cx, |c, cx| c.prefill(&msg, cx));
            }
            self.composer.update(cx, |c, cx| c.commit(window, cx));
            return;
        }
        self.run(GitOp::Continue(op.kind), cx);
    }

    /// `git::AbortOperation` (asks first).
    pub fn abort_op(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(op) = self.operation(cx) else {
            return;
        };
        let Some(ws) = self.store.read(cx).workspace.upgrade() else {
            return;
        };
        let branch = op
            .head_name
            .clone()
            .unwrap_or_else(|| "the detached HEAD".into());
        let (title, message) = match op.kind {
            OperationKind::Merge => (
                "Abort the merge?".to_string(),
                format!(
                    "Puts {branch} back to where it was before the merge. Your conflict \
                     resolutions and every other uncommitted change made since the merge \
                     started are lost (git merge --abort)."
                ),
            ),
            OperationKind::Rebase => (
                "Abort the rebase?".to_string(),
                format!(
                    "Stops rebasing and restores {branch} to where it was before the rebase. \
                     Commits already replayed and your conflict resolutions are discarded \
                     (git rebase --abort)."
                ),
            ),
            OperationKind::CherryPick | OperationKind::Revert | OperationKind::Am => (
                format!("Abort the {}?", op.kind.command()),
                format!(
                    "Restores {branch} to where it was before the {} started; your conflict \
                     resolutions are lost (git {} --abort).",
                    op.kind.command(),
                    op.kind.command()
                ),
            ),
        };
        let spec = ConfirmSpec {
            title: title.into(),
            message: vec![message.into(), "This cannot be undone.".into()],
            items: self
                .store
                .read(cx)
                .conflicts
                .entries
                .iter()
                .map(|e| SharedString::from(format!("{}  ({})", e.path, e.kind.label())))
                .collect(),
            confirm_label: "Abort".into(),
        };
        let this = cx.entity().downgrade();
        let kind = op.kind;
        confirm(&ws, spec, window, cx, move |_, cx| {
            let _ = this.update(cx, |this, cx| this.run(GitOp::Abort(kind), cx));
        });
    }

    fn run(&mut self, op: GitOp, cx: &mut Context<Self>) {
        self.busy = true;
        self.error = None;
        let task = run_op(&self.store, op, cx);
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.task = None;
                if let Err(e) = result {
                    this.error = Some(e.into());
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}

impl Render for OperationBanner {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(op) = self.operation(cx) else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let conflicts = self.conflict_count(cx);
        let accent = if conflicts > 0 {
            theme.git.conflicted.hsla()
        } else {
            ui.accent.hsla()
        };
        let subtitle: SharedString = if conflicts > 0 {
            format!(
                "{} · resolve, then Continue",
                count_label(conflicts, "conflict")
            )
            .into()
        } else if op.kind == OperationKind::Merge {
            "No conflicts left · ready to commit the merge".into()
        } else {
            "No conflicts left · ready to continue".into()
        };
        let icon = match op.kind {
            OperationKind::Merge => IconName::GitMerge,
            OperationKind::Rebase => IconName::GitBranch,
            OperationKind::CherryPick | OperationKind::Am => IconName::GitCommit,
            OperationKind::Revert => IconName::Undo,
        };
        let continue_tip: SharedString = match (conflicts > 0, op.kind) {
            (true, _) => "Resolve every conflict first".into(),
            (false, OperationKind::Merge) => "Commit the merge with the message below".into(),
            (false, k) => format!("git {} --continue", k.command()).into(),
        };
        v_flex()
            .id("operation-banner")
            .mx(px(8.))
            .mb(px(6.))
            .p(px(8.))
            .gap(px(6.))
            .rounded(metrics::RADIUS)
            .border_1()
            .border_color(accent.opacity(0.55))
            .bg(accent.opacity(0.08))
            .child(
                h_flex()
                    .gap(px(7.))
                    .items_start()
                    .child(
                        div()
                            .pt(px(1.))
                            .child(Icon::new(icon).size(IconSize::Small).color(accent)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(1.))
                            .child(
                                div()
                                    .text_size(metrics::TEXT_SM)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(ui.text.hsla())
                                    .child(title(&op)),
                            )
                            .child(
                                div()
                                    .text_size(metrics::TEXT_XS)
                                    .text_color(ui.text_muted.hsla())
                                    .child(subtitle),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(
                        Button::new(
                            "operation-continue",
                            if self.busy { "Working…" } else { "Continue" },
                        )
                        .icon(IconName::Check)
                        .style(ButtonStyle::Accent)
                        .compact()
                        .disabled(self.busy || conflicts > 0)
                        .tooltip(Tooltip::text(continue_tip))
                        .on_click(cx.listener(|this, _, window, cx| this.continue_op(window, cx))),
                    )
                    .child(
                        Button::new("operation-abort", "Abort…")
                            .icon(IconName::Close)
                            .style(ButtonStyle::Outlined)
                            .compact()
                            .disabled(self.busy)
                            .tooltip(Tooltip::text(format!("git {} --abort", op.kind.command())))
                            .on_click(cx.listener(|this, _, window, cx| this.abort_op(window, cx))),
                    ),
            )
            .when_some(self.error.clone(), |el, e| {
                el.child(
                    h_flex()
                        .items_start()
                        .gap(px(6.))
                        .text_size(metrics::TEXT_XS)
                        .text_color(ui.text.hsla())
                        .child(
                            Icon::new(IconName::CircleX)
                                .size(IconSize::XSmall)
                                .color(ui.error.hsla()),
                        )
                        .child(
                            div()
                                .id("operation-error")
                                .flex_1()
                                .min_w_0()
                                .max_h(px(100.))
                                .overflow_y_scroll()
                                .child(e),
                        )
                        .child(
                            IconButton::new("operation-error-close", IconName::Close)
                                .icon_size(IconSize::XSmall)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles() {
        let op = RepoOperation {
            kind: OperationKind::Merge,
            interactive: false,
            head_name: Some("main".into()),
            incoming: Some("feature".into()),
            incoming_oid: None,
            step: None,
            message: None,
        };
        assert_eq!(title(&op), "Merging feature → main");
        let rebase = RepoOperation {
            kind: OperationKind::Rebase,
            head_name: None,
            incoming: Some("main".into()),
            step: Some((3, 7)),
            ..op
        };
        assert_eq!(title(&rebase), "Rebasing onto main (3/7)");
    }
}
