//! A small fuzzy picker over git refs (branches, tags, recent commits) used
//! by "Compare with…", "Branch vs base" and "Commit range".

use std::rc::Rc;

use gpui::{prelude::*, AnyElement, App, DismissEvent, Entity, SharedString, Window};
use lector_git::Repo;
use lector_ui::components::modal::toggle_modal;
use lector_ui::components::picker::{Picker, PickerDelegate, PickerItem};
use lector_ui::fuzzy::match_and_sort;
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
    choices: Vec<RefChoice>,
    matches: Vec<(usize, Vec<usize>)>,
    selected: usize,
    on_pick: OnPick,
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
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.matches = match_and_sort(query, self.choices.iter().map(|c| c.label.as_str()))
            .into_iter()
            .map(|(ix, m)| (ix, m.positions))
            .collect();
        // A typed revision that matches nothing can still be used verbatim.
        if self.matches.is_empty() && !query.trim().is_empty() {
            self.choices.retain(|c| c.detail != "revision");
            self.choices.push(RefChoice {
                rev: query.trim().to_string(),
                label: query.trim().to_string(),
                detail: "revision".into(),
                icon: IconName::Hash,
            });
            self.matches = vec![(self.choices.len() - 1, Vec::new())];
        }
        self.selected = 0;
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some((ix, _)) = self.matches.get(self.selected) {
            let choice = self.choices[*ix].clone();
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
        let c = &self.choices[*cix];
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
    let delegate = RefPickerDelegate {
        placeholder: placeholder.into(),
        choices,
        matches: Vec::new(),
        selected: 0,
        on_pick: Rc::new(on_pick),
    };
    toggle_modal(&layer, window, cx, move |window, cx| {
        Picker::new(delegate, window, cx)
    });
}
