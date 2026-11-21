//! A small fuzzy picker over git refs (branches, tags, recent commits) used
//! by "Compare with…", "Branch vs base" and "Commit range".

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

    fn update_matches(
        &mut self,
        query: &str,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected = 0;
        match self.ranker.set_query(query) {
            Ranking::Ready(rows) => {
                self.rank_task = None;
                self.set_matches(query, rows);
            }
            // A large list is ranked off the UI thread; the previous rows stay until it is done.
            Ranking::Pending(pending) => {
                let query = query.to_string();
                self.rank_task = Some(cx.spawn(async move |this, cx| {
                    let (generation, rows) = pending.wait().await;
                    let Some(rows) = rows else { return };
                    let _ = this.update(cx, |picker, cx| {
                        let d = &mut picker.delegate;
                        if d.ranker.is_current(generation) {
                            d.set_matches(&query, rows);
                            d.selected = 0;
                            cx.notify();
                        }
                    });
                }));
            }
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
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
    let choices = Arc::new(choices);
    let delegate = RefPickerDelegate {
        placeholder: placeholder.into(),
        ranker: ListRanker::new(choices.clone(), |c| c.label.as_str(), MAX_MATCHES),
        choices,
        rank_task: None,
        typed: None,
        matches: Vec::new(),
        selected: 0,
        on_pick: Rc::new(on_pick),
    };
    toggle_modal(&layer, window, cx, move |window, cx| {
        Picker::new(delegate, window, cx)
    });
}
