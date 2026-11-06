//! The review queue of the [`ReviewView`] (v0.4): files in risk order with a badge and the top
//! reason, generated and lock files in a collapsed "Low-risk & generated" group, reviewed-hunk
//! progress and "next unreviewed".

use std::sync::Arc;

use gpui::{div, prelude::*, AnyElement, Context, Task};
use lector_app::review_queue::{
    next_unreviewed, totals, FileProgress, HunkReviewStore, Position, QueueGroup,
};
use lector_ui::components::{h_flex, metrics};
use lector_ui::icons::{Icon, IconName, IconSize};
use lector_ui::scale::px;
use lector_ui::{ActiveTheme, ColorExt};
use lector_workspace::risk::RiskLevel;

use super::{DiffViewEvent, ReviewView};
use crate::review_model::{analyze_changes, ChangesAnalysis};

/// Height of every row of the file list (the list needs uniform rows: a file name and the reason).
pub(super) const ROW_HEIGHT: f32 = 36.;

/// A row of the file list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ListRow {
    /// A file (index into the changeset's files).
    File(usize),
    /// The header of the collapsed low-risk group.
    LowHeader,
}

/// Queue state of a [`ReviewView`].
pub(super) struct QueueState {
    pub analysis: Option<Arc<ChangesAnalysis>>,
    pub task: Option<Task<()>>,
    /// Reviewed flag per hunk, per file (aligned with the files).
    pub marks: Vec<Vec<bool>>,
    /// File indices in display order.
    pub order: Vec<usize>,
    /// Position in `order` where the low-risk group starts (`order.len()` without one).
    pub low_start: usize,
    pub low_collapsed: bool,
    pub rows: Vec<ListRow>,
    pub user_selected: bool,
    pub risk_order: bool,
}

impl QueueState {
    pub(super) fn new(risk_order: bool) -> Self {
        QueueState {
            analysis: None,
            task: None,
            marks: Vec::new(),
            order: Vec::new(),
            low_start: 0,
            low_collapsed: true,
            rows: Vec::new(),
            user_selected: false,
            risk_order,
        }
    }
}

impl ReviewView {
    /// Called after the file list loaded: plain order right away, risk order once analyzed.
    pub(super) fn queue_after_load(&mut self, cx: &mut Context<Self>) {
        let Some(view) = &self.view else {
            return;
        };
        let n = view.files.len();
        let worktree_like = matches!(
            &self.changeset,
            lector_git::Changeset::WorkingTree
                | lector_git::Changeset::Staged
                | lector_git::Changeset::Unstaged
                | lector_git::Changeset::Untracked
        ) || matches!(&self.changeset, lector_git::Changeset::Range { to, .. }
            if to == lector_workspace::review::WORKDIR_REV);
        if worktree_like && self.pr.is_none() {
            // Looking at the working-tree review counts as having seen the changes.
            let model = self.store.read(cx).review_model.clone();
            model.update(cx, |m, cx| m.mark_seen(cx));
        }
        let changes: Vec<_> = view.files.iter().map(|f| f.change.clone()).collect();
        let key = view.key.clone();
        self.queue.marks.resize(n, Vec::new());
        self.queue.marks.truncate(n);
        self.rebuild_queue();
        let store = self.store.read(cx);
        let pr_repo = self.pr.as_ref().map(|p| p.shadow_repo.clone());
        let (Some(repo), Some(project)) =
            (pr_repo.or_else(|| store.repo().cloned()), store.project())
        else {
            return;
        };
        let languages = project.languages().clone();
        let settings = store.settings(cx);
        let opts = lector_workspace::review::diff_options(&settings.diff);
        let detect = settings.review.detect_moved;
        self.queue.risk_order = settings.review.risk_order;
        let root = repo.workdir().to_path_buf();
        let db = lector_ui::commands_bridge::core(cx).db().clone();
        self.queue.task = Some(cx.spawn(async move |this, cx| {
            let analysis = cx
                .background_executor()
                .spawn(async move {
                    let analysis = analyze_changes(&repo, &languages, &changes, &opts, detect);
                    // Persisted reviewed hunks of every analyzed file.
                    let store = HunkReviewStore::new(&db);
                    let marks: Vec<(String, Vec<bool>)> = analysis
                        .order
                        .iter()
                        .filter_map(|path| {
                            let file = analysis.get(path)?;
                            Some((
                                path.clone(),
                                store.reviewed(&root, &key, path, &file.hunks, None),
                            ))
                        })
                        .collect();
                    (analysis, marks)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let (analysis, marks) = analysis;
                if let Some(view) = &this.view {
                    let mut all = vec![Vec::new(); view.files.len()];
                    for (path, flags) in marks {
                        if let Some(ix) = view.index_of(&path) {
                            all[ix] = flags;
                        }
                    }
                    this.queue.marks = all;
                }
                this.queue.analysis = Some(Arc::new(analysis));
                this.rebuild_queue();
                if !this.queue.user_selected && this.queue.risk_order {
                    if let Some(&first) = this.queue.order.first() {
                        this.selected = first;
                        this.rebuild_queue(); // the low-risk group may have been opened for the old selection
                    }
                }
                this.show_selected(cx);
                cx.notify();
            });
        }));
    }

    /// Recomputes the display order and the list rows.
    pub(super) fn rebuild_queue(&mut self) {
        let Some(view) = &self.view else {
            self.queue.order.clear();
            self.queue.rows.clear();
            return;
        };
        let n = view.files.len();
        let analysis = self
            .queue
            .analysis
            .clone()
            .filter(|_| self.queue.risk_order);
        let mut order: Vec<usize> = Vec::with_capacity(n);
        let mut low_start = n;
        if let Some(analysis) = &analysis {
            let mut seen = vec![false; n];
            let (mut attention, mut low) = (Vec::new(), Vec::new());
            for path in &analysis.order {
                let Some(ix) = view.index_of(path) else {
                    continue;
                };
                seen[ix] = true;
                let is_low = analysis
                    .get(path)
                    .is_some_and(|f| f.risk.group == QueueGroup::LowRisk);
                if is_low {
                    low.push(ix);
                } else {
                    attention.push(ix);
                }
            }
            // Files the analysis did not cover (too many files, unreadable) keep path order.
            attention.extend((0..n).filter(|&i| !seen[i]));
            low_start = attention.len();
            order = attention;
            order.extend(low);
        } else {
            order.extend(0..n);
        }
        let mut rows: Vec<ListRow> = order[..low_start.min(order.len())]
            .iter()
            .map(|&i| ListRow::File(i))
            .collect();
        if low_start < order.len() {
            rows.push(ListRow::LowHeader);
            if !self.queue.low_collapsed || self.selected_in_low(&order, low_start) {
                rows.extend(order[low_start..].iter().map(|&i| ListRow::File(i)));
            }
        }
        self.queue.order = order;
        self.queue.low_start = low_start;
        self.queue.rows = rows;
    }

    fn selected_in_low(&self, order: &[usize], low_start: usize) -> bool {
        order.iter().skip(low_start).any(|&i| i == self.selected)
    }

    /// Number of list rows (files plus the low-risk header).
    pub(super) fn queue_row_count(&self, files: usize) -> usize {
        if self.queue.rows.is_empty() {
            files
        } else {
            self.queue.rows.len()
        }
    }

    /// The list row showing file `ix`.
    pub(super) fn queue_row_of_file(&self, ix: usize) -> usize {
        self.queue
            .rows
            .iter()
            .position(|r| *r == ListRow::File(ix))
            .unwrap_or(ix)
    }

    /// Renders list row `row`.
    pub(super) fn render_queue_row(&self, row: usize, cx: &mut Context<Self>) -> AnyElement {
        match self.queue.rows.get(row).copied() {
            Some(ListRow::File(ix)) => self.render_file_row(ix, cx),
            Some(ListRow::LowHeader) => self.render_low_header(row, cx),
            None => self.render_file_row(row, cx),
        }
    }

    fn render_low_header(&self, row: usize, cx: &mut Context<Self>) -> AnyElement {
        let ui = cx.theme().ui.clone();
        let count = self.queue.order.len().saturating_sub(self.queue.low_start);
        let done = self
            .queue
            .order
            .iter()
            .skip(self.queue.low_start)
            .filter(|&&i| self.file_progress(i).is_done())
            .count();
        let collapsed = self.queue.low_collapsed
            && !self.selected_in_low(&self.queue.order, self.queue.low_start);
        h_flex()
            .id(("review-low-header", row))
            .w_full()
            .h(px(ROW_HEIGHT))
            .pl(px(6.))
            .pr(px(10.))
            .gap(px(4.))
            .cursor_pointer()
            .hover(|s| s.bg(ui.hover.hsla()))
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall)
                .color(ui.text_faint.hsla()),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(metrics::text_xs())
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(ui.text_muted.hsla())
                    .child("LOW-RISK & GENERATED"),
            )
            .child(
                div()
                    .text_size(metrics::text_xs())
                    .text_color(ui.text_faint.hsla())
                    .child(format!("{done}/{count}")),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.queue.low_collapsed = !this.queue.low_collapsed
                    || this.selected_in_low(&this.queue.order, this.queue.low_start);
                this.rebuild_queue();
                cx.notify();
            }))
            .into_any_element()
    }

    /// Review state of file `ix`.
    pub(super) fn file_progress(&self, ix: usize) -> FileProgress {
        FileProgress {
            hunks: self.queue.marks.get(ix).cloned().unwrap_or_default(),
            viewed: self
                .view
                .as_ref()
                .and_then(|v| v.files.get(ix))
                .is_some_and(|f| f.viewed),
        }
    }

    fn all_progress(&self) -> Vec<FileProgress> {
        let n = self.view.as_ref().map_or(0, |v| v.files.len());
        (0..n).map(|i| self.file_progress(i)).collect()
    }

    /// "7 of 12 files · 18 of 40 hunks reviewed".
    pub(super) fn queue_progress(&self) -> (String, f32, bool) {
        let t = totals(&self.all_progress());
        let frac = if t.hunks > 0 {
            t.hunks_done as f32 / t.hunks as f32
        } else if t.files > 0 {
            t.files_done as f32 / t.files as f32
        } else {
            0.
        };
        (t.label(), frac, t.complete())
    }

    /// The file that follows (`delta = 1`) or precedes `selected` in the display order.
    pub(super) fn queue_neighbor(&self, selected: usize, delta: isize, n: usize) -> usize {
        let order = &self.queue.order;
        match order.iter().position(|&i| i == selected) {
            Some(pos) if order.len() == n => {
                let next = (pos as isize + delta).clamp(0, n as isize - 1) as usize;
                order[next]
            }
            _ => (selected as isize + delta).clamp(0, n as isize - 1) as usize,
        }
    }

    /// The risk badge of file `ix` (nothing for low-risk files).
    pub(super) fn risk_badge(&self, ix: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let risk = self.risk_of(ix)?;
        let theme = cx.theme().clone();
        let (label, color) = match risk.level {
            RiskLevel::High => ("High", theme.ui.error.hsla()),
            RiskLevel::Medium => ("Med", theme.git.modified.hsla()),
            RiskLevel::Low => return None,
        };
        Some(
            div()
                .flex_none()
                .px(px(5.))
                .rounded(px(3.))
                .text_size(metrics::text_xs())
                .font_weight(gpui::FontWeight::MEDIUM)
                .bg(color.opacity(0.14))
                .text_color(color)
                .child(label)
                .into_any_element(),
        )
    }

    /// The sentence under the file name for risky files.
    pub(super) fn risk_reason(&self, ix: usize) -> Option<String> {
        let risk = self.risk_of(ix)?;
        (risk.level != RiskLevel::Low)
            .then(|| risk.reason.clone())
            .flatten()
    }

    fn risk_of(&self, ix: usize) -> Option<&crate::review_model::FileRisk> {
        let analysis = self
            .queue
            .analysis
            .as_ref()
            .filter(|_| self.queue.risk_order)?;
        let path = self.view.as_ref()?.files.get(ix)?.path();
        analysis.get(path).map(|f| &f.risk)
    }

    /// "2/5" for a partly reviewed file.
    pub(super) fn hunk_counter(&self, ix: usize) -> Option<String> {
        let p = self.file_progress(ix);
        let total = p.hunks.len();
        let done = p.reviewed_hunks();
        (total > 1 && done > 0 && !p.viewed && done < total).then(|| format!("{done}/{total}"))
    }

    /// Gives the diff the moved code of the selected file.
    pub(super) fn queue_on_selected(&mut self, cx: &mut Context<Self>) {
        let moves = self.view.as_ref().and_then(|v| {
            let path = v.files.get(self.selected)?.path();
            self.queue
                .analysis
                .as_ref()?
                .get(path)
                .map(|f| f.moves.clone())
        });
        self.diff
            .update(cx, |d, cx| d.set_changeset_moves(moves, cx));
        if !self.queue.rows.contains(&ListRow::File(self.selected)) {
            self.rebuild_queue();
        }
        let row = self.queue_row_of_file(self.selected);
        self.list.reveal(row);
    }

    /// Records that the user chose a file (the analysis must not move the selection afterwards).
    pub(super) fn queue_user_selected(&mut self) {
        self.queue.user_selected = true;
        if self.selected_in_low(&self.queue.order.clone(), self.queue.low_start) {
            self.rebuild_queue();
        }
    }

    /// Events of the diff view that concern the queue.
    pub(super) fn queue_diff_event(&mut self, event: &DiffViewEvent, cx: &mut Context<Self>) {
        match event {
            DiffViewEvent::HunksReviewed(path, flags) => {
                if let Some(ix) = self.view.as_ref().and_then(|v| v.index_of(path)) {
                    if let Some(slot) = self.queue.marks.get_mut(ix) {
                        *slot = flags.clone();
                    }
                    cx.notify();
                }
            }
            DiffViewEvent::NextUnreviewed => self.queue_next_unreviewed(cx),
            DiffViewEvent::GoToMove {
                path,
                line,
                old_side,
            } => {
                let Some(ix) = self.view.as_ref().and_then(|v| v.index_of(path)) else {
                    return;
                };
                self.selected = ix;
                self.queue_user_selected();
                self.show_selected(cx);
                let (line, old_side) = (*line, *old_side);
                self.diff
                    .update(cx, |d, cx| d.show_line_when_loaded(old_side, line, cx));
            }
            _ => {}
        }
    }

    /// `review::NextUnreviewed`: the next hunk (across files, in queue order) that is not reviewed.
    pub(super) fn queue_next_unreviewed(&mut self, cx: &mut Context<Self>) {
        let progress = self.all_progress();
        let order = self.queue.order.clone();
        let file = order.iter().position(|&i| i == self.selected);
        let from = file.map(|file| Position {
            file,
            hunk: Some(usize::MAX / 2),
        });
        let Some(next) = next_unreviewed(&progress, &order, from) else {
            cx.notify();
            return;
        };
        let Some(&ix) = order.get(next.file) else {
            return;
        };
        self.selected = ix;
        self.queue_user_selected();
        self.show_selected(cx);
        if let Some(k) = next.hunk {
            self.diff.update(cx, |d, cx| d.show_hunk_when_loaded(k, cx));
        }
    }

    /// A file was marked (not) viewed in the list: its hunks follow.
    pub(super) fn queue_file_viewed(&mut self, ix: usize, state: bool, cx: &mut Context<Self>) {
        let Some(flags) = self.queue.marks.get_mut(ix) else {
            return;
        };
        if flags.is_empty() {
            return;
        }
        *flags = vec![state; flags.len()];
        let (Some(view), Some(analysis)) = (&self.view, &self.queue.analysis) else {
            return;
        };
        let Some(file) = view.files.get(ix) else {
            return;
        };
        let (Some(repo), Some(fa)) = (self.store.read(cx).repo(), analysis.get(file.path())) else {
            return;
        };
        let db = lector_ui::commands_bridge::core(cx).db().clone();
        HunkReviewStore::new(&db).store_reviewed(
            repo.workdir(),
            &view.key,
            file.path(),
            &fa.hunks,
            &self.queue.marks[ix],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use lector_app::{AppCore, CoreOptions};
    use lector_ui::views::Workspace;
    use lector_workspace::Project;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Ada Lovelace")
            .env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada Lovelace")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .status();
        assert!(status.is_ok_and(|s| s.success()), "git {args:?}");
    }

    fn wait_until(
        cx: &mut VisualTestContext,
        what: &str,
        mut done: impl FnMut(&mut VisualTestContext) -> bool,
    ) {
        for _ in 0..100 {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("timed out waiting for {what}");
    }

    const HELPER: &str = "fn normalize(input: &str) -> String {\n    let trimmed = input.trim();\n    let lower = trimmed.to_lowercase();\n    lower.replace(' ', \"_\")\n}\n";

    #[gpui::test]
    fn review_queue_orders_by_risk_tracks_progress_and_follows_moves(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let data = tempfile::tempdir().unwrap();
        git(&root, &["init", "-q"]);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let spacer: String = (0..30).map(|i| format!("// line {i}\n")).collect();
        std::fs::write(
            root.join("src/auth.rs"),
            format!("pub fn check(token: &str) -> bool {{\n    token == \"x\"\n}}\n{spacer}pub fn other() {{}}\n"),
        )
        .unwrap();
        std::fs::write(
            root.join("src/parser.rs"),
            format!("{HELPER}\npub fn parse() {{}}\n"),
        )
        .unwrap();
        std::fs::write(root.join("Cargo.lock"), "# lock\nname = \"a\"\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "initial"]);
        // Risky change in two separate hunks, a block moved to a new file, a lock file bump.
        std::fs::write(
            root.join("src/auth.rs"),
            format!(
                "pub fn check(token: &str, admin: bool) -> bool {{\n    unsafe {{ std::mem::transmute::<u8, bool>(1) }};\n    token == \"x\" || admin\n}}\n{spacer}pub fn other() {{ println!(\"x\"); }}\n"
            ),
        )
        .unwrap();
        std::fs::write(root.join("src/parser.rs"), "pub fn parse() {}\n").unwrap();
        std::fs::write(
            root.join("src/util.rs"),
            format!("{HELPER}\npub fn util() {{}}\n"),
        )
        .unwrap();
        std::fs::write(
            root.join("Cargo.lock"),
            "# lock\nname = \"b\"\nversion = \"2\"\n",
        )
        .unwrap();

        let mut settings = lector_config::Settings::default();
        settings.lsp.enabled = false;
        settings.session.restore = false;
        let core = AppCore::open(CoreOptions {
            data_dir: Some(data.path().to_path_buf()),
            ..CoreOptions::for_tests()
        });
        cx.update(|cx| {
            lector_ui::ThemeState::init(
                cx,
                lector_theme::ThemeRegistry::new(),
                lector_ui::ThemeMode::Dark,
                lector_theme::Appearance::Dark,
            );
            cx.set_global(lector_ui::code_view::EditorSettings::default());
            cx.set_global(lector_ui::settings::AppSettings {
                settings: std::sync::Arc::new(settings.clone()),
                warnings: Default::default(),
            });
            cx.set_global(lector_ui::commands_bridge::CoreGlobal(core));
            crate::init(cx);
        });
        let project = Project::open(&root, settings).unwrap();
        let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(project, window, cx));
        cx.run_until_parked();
        let store = cx
            .update(|_, cx| crate::store_for(ws.entity_id(), cx))
            .expect("git store for the workspace");

        // The window knows about the changes before anything is opened: chip and welcome entry.
        wait_until(cx, "the review hint", |cx| {
            ws.read_with(cx, |ws, _| {
                ws.review_hint().is_some_and(|h| h.new_since_last)
            })
        });
        let hint = ws
            .read_with(cx, |ws, _| ws.review_hint().cloned())
            .expect("hint");
        assert_eq!(hint.uncommitted, 4);

        cx.update(|window, cx| {
            crate::review_tab::open_review(&store, lector_git::Changeset::WorkingTree, window, cx)
        });
        let item_view = ws
            .read_with(cx, |ws, cx| {
                ws.active_pane().read(cx).active_item().map(|i| i.view())
            })
            .expect("review tab");
        let review: Entity<ReviewView> = item_view.downcast().expect("a ReviewView");
        wait_until(cx, "the analysis", |cx| {
            review.read_with(cx, |r, _| r.queue.analysis.is_some())
        });

        // Opening the review counts as having looked at the changes.
        wait_until(cx, "the chip to go away", |cx| {
            ws.read_with(cx, |ws, _| {
                ws.review_hint().is_some_and(|h| !h.new_since_last)
            })
        });

        review.read_with(cx, |r, _| {
            let view = r.view.as_ref().expect("loaded");
            let paths: Vec<&str> = r
                .queue
                .order
                .iter()
                .map(|&i| view.files[i].path())
                .collect();
            assert_eq!(
                paths.first(),
                Some(&"src/auth.rs"),
                "risky file first: {paths:?}"
            );
            assert!(r.queue.low_start < paths.len(), "a low-risk group exists");
            assert!(
                paths[r.queue.low_start..].contains(&"Cargo.lock"),
                "lock file in the low-risk group: {paths:?}"
            );
            assert!(
                !paths[..r.queue.low_start].contains(&"Cargo.lock"),
                "{paths:?}"
            );
            assert!(r.queue.rows.contains(&ListRow::LowHeader));
            assert!(
                !r.queue.rows.iter().any(
                    |row| matches!(row, ListRow::File(i) if view.files[*i].path() == "Cargo.lock")
                ),
                "the low-risk group starts collapsed"
            );
            assert_eq!(
                view.files[r.selected].path(),
                "src/auth.rs",
                "starts at the top of the queue"
            );
            // The moved block is known on both ends, across files.
            let analysis = r.queue.analysis.as_ref().expect("analysis");
            let util = analysis.get("src/util.rs").expect("util analysis");
            assert_eq!(
                util.moves.added.first().map(|m| m.other_path.as_str()),
                Some("src/parser.rs")
            );
            let parser = analysis.get("src/parser.rs").expect("parser analysis");
            assert_eq!(
                parser.moves.removed.first().map(|m| m.other_path.as_str()),
                Some("src/util.rs")
            );
            assert!(r.queue_progress().0.starts_with("0 of 4 files"));
        });

        // Review the first hunk of auth.rs: progress counts it, the file is not done yet.
        let diff = review.read_with(cx, |r, _| r.diff.clone());
        wait_until(cx, "the diff to load", |cx| {
            diff.read_with(cx, |d, _| d.reviewed_hunks().len() == 2)
        });
        cx.update(|_, cx| diff.update(cx, |d, cx| d.set_hunk_reviewed(0, true, cx)));
        cx.run_until_parked();
        review.read_with(cx, |r, _| {
            let label = r.queue_progress().0;
            assert!(
                label.contains("1 of 7 hunks reviewed") || label.contains(" 1 of "),
                "{label}"
            );
            assert!(label.starts_with("0 of 4 files"), "{label}");
        });

        // "Next unreviewed" (the diff found nothing left after the cursor) moves on to the next
        // file of the queue.
        review.update(cx, |r, cx| r.queue_next_unreviewed(cx));
        cx.run_until_parked();
        review.read_with(cx, |r, _| {
            let view = r.view.as_ref().expect("loaded");
            assert_eq!(view.files[r.selected].path(), "src/util.rs");
        });
        // Back in auth.rs the first hunk is still marked (persisted), the second is open.
        review.update(cx, |r, cx| {
            r.selected = r
                .view
                .as_ref()
                .and_then(|v| v.index_of("src/auth.rs"))
                .unwrap_or(0);
            r.show_selected(cx);
        });
        wait_until(cx, "auth.rs to reload", |cx| {
            diff.read_with(cx, |d, _| d.reviewed_hunks() == [true, false])
        });
        // Reviewing the last hunk completes the file (it counts as viewed).
        cx.update(|_, cx| diff.update(cx, |d, cx| d.set_hunk_reviewed(1, true, cx)));
        cx.run_until_parked();
        review.read_with(cx, |r, _| {
            let view = r.view.as_ref().expect("loaded");
            let auth = view.index_of("src/auth.rs").expect("auth");
            assert!(
                view.files[auth].viewed,
                "all hunks reviewed marks the file viewed"
            );
            assert!(r.queue_progress().0.starts_with("1 of 4 files"));
        });
    }
}
