//! A small fuzzy picker over git refs (branches, tags, recent commits) used
//! by "Compare with…", "Branch vs base" and "Commit range".

use crate::store::GitStore;
use lector_ui::background::{background, view_key, JobKind, Priority};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{prelude::*, AnyElement, App, DismissEvent, Entity, SharedString, Task, Window};
use lector_git::Repo;
use lector_ui::components::modal::toggle_modal;
use lector_ui::components::picker::{Picker, PickerDelegate, PickerItem};
use lector_ui::components::ranker::{ListRanker, Ranking};
use lector_ui::icons::{Icon, IconName, IconSize};
use lector_ui::views::Workspace;

/// One choice.
#[derive(Clone, Debug)]
pub struct RefChoice {
    /// The revision to use (`main`, `v1.0`, full sha).
    pub rev: String,
    /// Label shown.
    pub label: String,
    /// Dimmed detail.
    pub detail: String,
    /// Icon.
    pub icon: IconName,
}

type OnPick = Rc<dyn Fn(RefChoice, &mut Window, &mut App)>;

/// Picker delegate over [`RefChoice`]s.
pub struct RefPickerDelegate {
    placeholder: SharedString,
    choices: Arc<Vec<RefChoice>>,
    ranker: ListRanker<RefChoice>,
    rank_task: Option<Task<()>>,
    /// The typed text offered as a revision when nothing matches; match index `choices.len()`.
    typed: Option<RefChoice>,
    matches: Vec<(usize, Vec<usize>)>,
    selected: usize,
    on_pick: OnPick,
}

/// Most matches kept for a query (refs of a big repository can number in the tens of thousands).
const MAX_MATCHES: usize = 1000;

impl RefPickerDelegate {
    /// A delegate over `choices` that calls `on_pick` with the confirmed one.
    fn new(
        placeholder: SharedString,
        choices: Vec<RefChoice>,
        on_pick: impl Fn(RefChoice, &mut Window, &mut App) + 'static,
    ) -> Self {
        let choices = Arc::new(choices);
        RefPickerDelegate {
            placeholder,
            ranker: ListRanker::new(choices.clone(), |c| c.label.as_str(), MAX_MATCHES),
            choices,
            rank_task: None,
            typed: None,
            matches: Vec::new(),
            selected: 0,
            on_pick: Rc::new(on_pick),
        }
    }

    /// Confirms the selected row, which belongs to the latest query.
    fn confirm_selected(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(choice) = self
            .matches
            .get(self.selected)
            .and_then(|(ix, _)| self.choice(*ix))
            .cloned()
        {
            let on_pick = self.on_pick.clone();
            cx.emit(DismissEvent);
            window.defer(cx, move |window, cx| on_pick(choice, window, cx));
        }
    }

    fn choice(&self, ix: usize) -> Option<&RefChoice> {
        self.choices.get(ix).or(self.typed.as_ref())
    }

    /// Installs ranked rows for `query`. A typed revision that matches nothing can still be used
    /// verbatim.
    fn set_matches(&mut self, query: &str, rows: Vec<(usize, Vec<usize>)>) {
        self.typed = None;
        self.matches = rows;
        if self.matches.is_empty() && !query.trim().is_empty() {
            self.typed = Some(RefChoice {
                rev: query.trim().to_string(),
                label: query.trim().to_string(),
                detail: "revision".into(),
                icon: IconName::Hash,
            });
            self.matches = vec![(self.choices.len(), Vec::new())];
        }
    }
}

/// Lists local branches, remote branches, tags and the 30 most recent commits.
/// Blocking; call from a background thread.
pub fn ref_choices(repo: &Repo) -> Vec<RefChoice> {
    let mut out = Vec::new();
    if let Ok(mut branches) = repo.branches() {
        branches.sort_by_key(|b| (b.remote, std::cmp::Reverse(b.time)));
        for b in branches {
            out.push(RefChoice {
                rev: b.name.clone(),
                label: b.name.clone(),
                detail: if b.head {
                    "current branch".into()
                } else if b.remote {
                    "remote branch".into()
                } else {
                    "branch".into()
                },
                icon: IconName::GitBranch,
            });
        }
    }
    if let Ok(tags) = repo.tags() {
        for t in tags.into_iter().rev() {
            out.push(RefChoice {
                rev: t.name.clone(),
                label: t.name,
                detail: "tag".into(),
                icon: IconName::Tag,
            });
        }
    }
    let q = lector_git::LogQuery {
        limit: 30,
        ..Default::default()
    };
    if let Ok(commits) = repo.log(&q) {
        for c in commits {
            out.push(RefChoice {
                rev: c.oid.to_string(),
                label: format!("{} {}", c.oid.short(), c.summary),
                detail: c.author.name.clone(),
                icon: IconName::GitCommit,
            });
        }
    }
    out
}

impl PickerDelegate for RefPickerDelegate {
    fn placeholder(&self) -> SharedString {
        self.placeholder.clone()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _cx: &mut Context<Picker<Self>>) {
        self.selected = ix;
    }

    fn update_matches(&mut self, query: &str, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selected = 0;
        match self.ranker.set_query(query) {
            Ranking::Ready(rows) => {
                self.rank_task = None;
                self.set_matches(query, rows);
            }
            // A large list is ranked off the UI thread; the previous rows stay until it is done,
            // and an Enter in the meantime waits for the new rows (`confirm`).
            Ranking::Pending(pending) => {
                let query = query.to_string();
                self.rank_task = Some(cx.spawn_in(window, async move |this, cx| {
                    let (generation, rows) = pending.wait().await;
                    let Some(rows) = rows else { return };
                    let _ = this.update_in(cx, |picker, window, cx| {
                        let d = &mut picker.delegate;
                        if d.ranker.land(generation) {
                            d.set_matches(&query, rows);
                            d.selected = 0;
                            cx.notify();
                            if d.ranker.take_deferred_confirm() {
                                d.confirm_selected(window, cx);
                            }
                        }
                    });
                }));
            }
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        // The rows on screen may belong to an older query while a large list is being ranked:
        // confirm only rows of the latest query, otherwise wait for them.
        if self.ranker.confirm_or_defer() {
            self.confirm_selected(window, cx);
        }
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {
        self.ranker.dismiss();
        self.rank_task = None;
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> AnyElement {
        let (cix, positions) = &self.matches[ix];
        let Some(c) = self.choice(*cix) else {
            return gpui::Empty.into_any_element();
        };
        PickerItem::new(c.label.clone(), selected)
            .icon(Icon::new(c.icon).size(IconSize::Small))
            .positions(positions.clone())
            .end_text(Some(c.detail.clone().into()))
            .into_any_element()
    }

    fn empty_message(&self) -> SharedString {
        "No matching refs".into()
    }
}

/// Opens a ref picker over `choices` in `workspace`'s modal layer.
pub fn pick_ref(
    workspace: &Entity<Workspace>,
    placeholder: impl Into<SharedString>,
    choices: Vec<RefChoice>,
    on_pick: impl Fn(RefChoice, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let layer = workspace.read(cx).modal_layer().clone();
    let delegate = RefPickerDelegate::new(placeholder.into(), choices, on_pick);
    toggle_modal(&layer, window, cx, move |window, cx| {
        Picker::new(delegate, window, cx)
    });
}

/// Loads ref choices in the background, then shows a picker.
pub fn pick_from_repo(
    store: &Entity<GitStore>,
    placeholder: &'static str,
    window: &mut Window,
    cx: &mut App,
    on_pick: impl Fn(crate::ref_picker::RefChoice, &mut Window, &mut App) + 'static,
) {
    let Some(repo) = store.read(cx).repo().cloned() else {
        return;
    };
    let Some(ws) = store.read(cx).workspace.upgrade() else {
        return;
    };
    let key = view_key(JobKind::GitRefs, store.entity_id());
    let task = background(cx, key, Priority::Visible, move |_| ref_choices(&repo));
    window
        .spawn(cx, async move |cx| {
            let Ok(choices) = task.await else { return };
            let _ =
                cx.update(|window, cx| pick_ref(&ws, placeholder, choices, on_pick, window, cx));
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use gpui::{Focusable as _, TestAppContext, VisualTestContext};
    use lector_ui::actions::picker as picker_actions;

    use super::*;

    type Picked = Rc<RefCell<Vec<String>>>;

    /// A picker over more refs than fit the inline work budget (62,500 candidates at the very
    /// least), so every query is answered later. Confirming
    /// a ref switches, deletes or compares branches, so a stale row must never be confirmed.
    fn big_picker(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Picker<RefPickerDelegate>>,
        Picked,
        &mut VisualTestContext,
    ) {
        cx.update(|cx| {
            lector_ui::ThemeState::init(
                cx,
                lector_theme::ThemeRegistry::new(),
                lector_ui::ThemeMode::Dark,
                lector_theme::Appearance::Dark,
            );
        });
        cx.executor().allow_parking();
        let choices = (0..70_000)
            .map(|i| RefChoice {
                rev: format!("branch_{i}_x"),
                label: format!("branch_{i}_x"),
                detail: String::new(),
                icon: IconName::GitBranch,
            })
            .collect();
        let picked = Picked::default();
        let sink = picked.clone();
        let (picker, cx) = cx.add_window_view(|window, cx| {
            let delegate = RefPickerDelegate::new("Ref".into(), choices, move |c, _, _| {
                sink.borrow_mut().push(c.rev);
            });
            Picker::new(delegate, window, cx)
        });
        cx.update(|window, cx| window.focus(&picker.focus_handle(cx)));
        (picker, picked, cx)
    }

    fn query(picker: &Entity<Picker<RefPickerDelegate>>, q: &str, cx: &mut VisualTestContext) {
        let q = q.to_string();
        picker.update_in(cx, |p, window, cx| p.set_query(q, window, cx));
    }

    fn settle(cx: &mut VisualTestContext, done: impl Fn(&mut VisualTestContext) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "ranking never landed");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn top(picker: &Entity<Picker<RefPickerDelegate>>, cx: &mut VisualTestContext) -> String {
        picker.read_with(cx, |p, _| {
            let d = &p.delegate;
            d.matches
                .first()
                .and_then(|(ix, _)| d.choice(*ix))
                .map(|c| c.rev.clone())
                .unwrap_or_default()
        })
    }

    #[gpui::test]
    fn enter_never_confirms_a_ref_of_an_older_query(cx: &mut TestAppContext) {
        let (picker, picked, cx) = big_picker(cx);
        query(&picker, "branch_12_", cx);
        settle(cx, |cx| top(&picker, cx) == "branch_12_x");

        // Nothing yields to the executor between the keystrokes, so B's ranking cannot land yet.
        query(&picker, "branch_5999", cx);
        picker.update_in(cx, |p, window, cx| p.delegate.confirm(window, cx));

        settle(cx, |_| !picked.borrow().is_empty());
        assert_eq!(*picked.borrow(), ["branch_5999_x"]);
    }

    #[gpui::test]
    fn dismissing_cancels_a_deferred_enter(cx: &mut TestAppContext) {
        let (picker, picked, cx) = big_picker(cx);
        query(&picker, "branch_12_", cx);
        settle(cx, |cx| top(&picker, cx) == "branch_12_x");
        query(&picker, "branch_5999", cx);
        picker.update_in(cx, |p, window, cx| {
            p.delegate.confirm(window, cx);
            p.delegate.dismissed(window, cx);
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        assert!(
            picked.borrow().is_empty(),
            "confirmed {:?}",
            picked.borrow()
        );
    }

    #[gpui::test]
    fn typing_again_drops_a_deferred_enter(cx: &mut TestAppContext) {
        let (picker, picked, cx) = big_picker(cx);
        query(&picker, "branch_12_", cx);
        settle(cx, |cx| top(&picker, cx) == "branch_12_x");
        query(&picker, "branch_5999", cx);
        picker.update_in(cx, |p, window, cx| p.delegate.confirm(window, cx));
        query(&picker, "branch_4000", cx);
        settle(cx, |cx| top(&picker, cx) == "branch_4000_x");
        cx.run_until_parked();
        assert!(
            picked.borrow().is_empty(),
            "confirmed {:?}",
            picked.borrow()
        );
    }

    #[gpui::test]
    fn a_typed_revision_is_confirmed_only_for_its_own_query(cx: &mut TestAppContext) {
        let (picker, picked, cx) = big_picker(cx);
        query(&picker, "branch_12_", cx);
        settle(cx, |cx| top(&picker, cx) == "branch_12_x");
        // Nothing matches: the typed text itself is offered as a revision.
        query(&picker, "zzzz-no-such-ref", cx);
        picker.update_in(cx, |p, window, cx| p.delegate.confirm(window, cx));
        settle(cx, |_| !picked.borrow().is_empty());
        assert_eq!(*picked.borrow(), ["zzzz-no-such-ref"]);
    }

    #[gpui::test]
    fn enter_on_settled_rows_confirms_at_once(cx: &mut TestAppContext) {
        let (picker, picked, cx) = big_picker(cx);
        query(&picker, "branch_4000", cx);
        settle(cx, |cx| top(&picker, cx) == "branch_4000_x");
        cx.dispatch_action(picker_actions::Confirm);
        cx.run_until_parked();
        assert_eq!(*picked.borrow(), ["branch_4000_x"]);
    }
}
