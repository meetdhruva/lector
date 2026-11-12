//! The Branches sidebar panel: local branches (current marked, upstream
//! ahead/behind), remote branches, tags and stashes.

use std::collections::HashSet;
use std::rc::Rc;

use gpui::{
    div, prelude::*, App, ClipboardItem, Entity, FocusHandle, Focusable, FontWeight, MouseButton,
    MouseDownEvent, Pixels, Point, SharedString, Subscription, Task, Window,
};
use lector_git::{Branch, Changeset, LogQuery, Oid, Repo, Stash, Tag};
use lector_ui::actions::{git as git_actions, picker as picker_actions};
use lector_ui::components::context_menu::{ContextMenu, ContextMenuState};
use lector_ui::components::list::{virtual_list, VirtualListState};
use lector_ui::components::{h_flex, metrics, IconButton, Tooltip};
use lector_ui::icons::{Icon, IconName, IconSize};
use lector_ui::keymap::contexts;
use lector_ui::scale::px;
use lector_ui::views::sidebar::SidebarPanel;
use lector_ui::{ActiveTheme, ColorExt};

use crate::history_panel::HistoryScope;
use crate::store::{GitStore, GitStoreEvent};
use crate::util::{now_secs, short_relative_time};

/// Which kind of ref a context menu is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RefRow {
    Local,
    Remote,
    Tag,
}

/// A local branch with its upstream divergence.
#[derive(Clone, Debug)]
pub struct BranchInfo {
    /// The branch.
    pub branch: Branch,
    /// (ahead, behind) of the upstream.
    pub divergence: Option<(u32, u32)>,
}

#[derive(Clone, Default)]
struct Data {
    local: Vec<BranchInfo>,
    remote: Vec<Branch>,
    tags: Vec<Tag>,
    stashes: Vec<Stash>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Group {
    Local,
    Remote,
    Tags,
    Stashes,
}

impl Group {
    fn title(self) -> &'static str {
        match self {
            Group::Local => "Branches",
            Group::Remote => "Remotes",
            Group::Tags => "Tags",
            Group::Stashes => "Stashes",
        }
    }
}

#[derive(Clone)]
enum Row {
    Header(Group, usize),
    Local(usize),
    Remote(usize),
    Tag(usize),
    Stash(usize),
}

/// The Branches panel.
pub struct BranchesPanel {
    store: Entity<GitStore>,
    data: Rc<Data>,
    rows: Rc<Vec<Row>>,
    collapsed: HashSet<Group>,
    list: VirtualListState,
    load_task: Option<Task<()>>,
    error: Option<SharedString>,
    menu: ContextMenuState,
    selected: Option<usize>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for BranchesPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn count_range(repo: &Repo, range: String) -> u32 {
    repo.log(&LogQuery {
        rev: Some(range),
        limit: 10_000,
        ..Default::default()
    })
    .map(|v| v.len() as u32)
    .unwrap_or(0)
}

fn load(repo: &Repo) -> Result<Data, String> {
    let branches = repo.branches().map_err(|e| e.to_string())?;
    let (mut local, mut remote): (Vec<_>, Vec<_>) = branches.into_iter().partition(|b| !b.remote);
    local.sort_by(|a, b| b.head.cmp(&a.head).then(b.time.cmp(&a.time)));
    remote.sort_by(|a, b| a.name.cmp(&b.name));
    remote.retain(|b| !b.name.ends_with("/HEAD"));
    let local = local
        .into_iter()
        .map(|b| {
            let divergence = b.upstream.as_ref().map(|up| {
                (
                    count_range(repo, format!("{up}..{}", b.name)),
                    count_range(repo, format!("{}..{up}", b.name)),
                )
            });
            BranchInfo {
                branch: b,
                divergence,
            }
        })
        .collect();
    let mut tags = repo.tags().map_err(|e| e.to_string())?;
    // Newest first, by the tagged commit's date.
    let oids: Vec<Oid> = tags.iter().map(|t| t.oid).collect();
    let times: std::collections::HashMap<Oid, i64> = repo
        .commit_infos(&oids)
        .unwrap_or_default()
        .into_iter()
        .map(|c| (c.oid, c.committer.time))
        .collect();
    tags.sort_by_key(|t| std::cmp::Reverse(times.get(&t.oid).copied().unwrap_or(0)));
    let stashes = repo.stashes().unwrap_or_default();
    Ok(Data {
        local,
        remote,
        tags,
        stashes,
    })
}

impl BranchesPanel {
    /// The panel for `store`'s workspace.
    pub fn new(store: Entity<GitStore>, cx: &mut Context<Self>) -> Self {
        let subs = vec![cx.subscribe(&store, |this, _, _: &GitStoreEvent, cx| this.reload(cx))];
        let mut collapsed = HashSet::new();
        collapsed.insert(Group::Remote);
        let mut panel = BranchesPanel {
            store,
            data: Rc::new(Data::default()),
            rows: Rc::new(Vec::new()),
            collapsed,
            list: VirtualListState::new(),
            load_task: None,
            error: None,
            menu: ContextMenuState::default(),
            selected: None,
            focus_handle: cx.focus_handle().tab_stop(true),
            _subscriptions: subs,
        };
        panel.reload(cx);
        panel
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.store.read(cx).repo().cloned() else {
            self.error = Some("Not a git repository".into());
            return;
        };
        let work = cx.background_spawn(async move { load(&repo) });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                this.load_task = None;
                match result {
                    Ok(data) => {
                        this.data = Rc::new(data);
                        this.error = None;
                        this.rebuild();
                    }
                    Err(e) => this.error = Some(e.into()),
                }
                cx.notify();
            });
        }));
    }

    fn rebuild(&mut self) {
        let d = &self.data;
        let mut rows = Vec::new();
        let group = |g: Group, n: usize, make: &dyn Fn(usize) -> Row, rows: &mut Vec<Row>| {
            if n == 0 {
                return;
            }
            rows.push(Row::Header(g, n));
            if !self.collapsed.contains(&g) {
                rows.extend((0..n).map(make));
            }
        };
        group(Group::Local, d.local.len(), &Row::Local, &mut rows);
        group(Group::Remote, d.remote.len(), &Row::Remote, &mut rows);
        group(Group::Tags, d.tags.len(), &Row::Tag, &mut rows);
        group(Group::Stashes, d.stashes.len(), &Row::Stash, &mut rows);
        self.rows = Rc::new(rows);
    }

    fn current_branch(&self, cx: &App) -> Option<String> {
        self.store
            .read(cx)
            .head
            .as_ref()
            .and_then(|h| h.branch.clone())
    }

    fn show_log(&self, rev: String, window: &mut Window, cx: &mut Context<Self>) {
        crate::show_history_scope(
            &self.store,
            HistoryScope::Log {
                rev: Some(rev),
                all: false,
            },
            window,
            cx,
        );
    }

    fn compare_with_current(&self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(current) = self.current_branch(cx) else {
            return;
        };
        crate::review_tab::open_review(
            &self.store,
            Changeset::BranchVsBase {
                branch: name,
                base: current,
            },
            window,
            cx,
        );
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.rows.len();
        if count == 0 {
            return;
        }
        let next = match self.selected {
            Some(ix) => (ix as isize + delta).clamp(0, count as isize - 1) as usize,
            None if delta > 0 => 0,
            None => count - 1,
        };
        self.selected = Some(next);
        self.list.reveal(next);
        cx.notify();
    }

    /// Enter on the selected row: toggle a group, show a ref's log, open a stash.
    fn activate_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.selected else {
            return;
        };
        match self.rows.get(ix).cloned() {
            Some(Row::Header(group, _)) => {
                if !self.collapsed.remove(&group) {
                    self.collapsed.insert(group);
                }
                self.rebuild();
                cx.notify();
            }
            Some(Row::Local(i)) => {
                self.show_log(self.data.local[i].branch.name.clone(), window, cx)
            }
            Some(Row::Remote(i)) => self.show_log(self.data.remote[i].name.clone(), window, cx),
            Some(Row::Tag(i)) => self.show_log(self.data.tags[i].name.clone(), window, cx),
            Some(Row::Stash(i)) => {
                let oid = self.data.stashes[i].oid;
                crate::review_tab::open_commit_tab(&self.store, oid, false, window, cx);
            }
            None => {}
        }
    }

    /// Shift-F10 on the selected row: its context menu.
    fn menu_for_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.selected else {
            cx.propagate();
            return;
        };
        let position = self.list.row_anchor(ix, metrics::row_height());
        let (name, oid, kind) = match self.rows.get(ix).cloned() {
            Some(Row::Local(i)) => {
                let b = &self.data.local[i].branch;
                (b.name.clone(), b.oid, RefRow::Local)
            }
            Some(Row::Remote(i)) => {
                let b = &self.data.remote[i];
                (b.name.clone(), b.oid, RefRow::Remote)
            }
            Some(Row::Tag(i)) => {
                let t = &self.data.tags[i];
                (t.name.clone(), t.oid, RefRow::Tag)
            }
            Some(Row::Stash(i)) => {
                let oid = self.data.stashes[i].oid;
                self.deploy_stash_menu(oid, position, window, cx);
                return;
            }
            _ => return,
        };
        self.deploy_ref_menu(name, oid, kind, position, window, cx);
    }

    /// Apply / Pop / Drop (undoable) / Open for a stash row.
    fn deploy_stash_menu(
        &mut self,
        oid: Oid,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let sync = self.store.read(cx).sync.clone();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            let s = store.clone();
            menu = menu.icon_entry("Open Stash", IconName::Archive, move |window, cx| {
                crate::review_tab::open_commit_tab(&s, oid, false, window, cx);
            });
            if let Some(sync) = sync {
                menu = menu.separator();
                for (label, verb, command) in [
                    ("Apply Stash", "Apply", "git.stash_apply"),
                    ("Pop Stash", "Pop", "git.stash_pop"),
                    ("Drop Stash", "Drop", "git.stash_drop"),
                ] {
                    let sync = sync.clone();
                    menu = menu.entry(label, move |_, cx| {
                        sync.update(cx, |c, cx| c.stash_op(verb, command, oid.to_string(), cx))
                    });
                }
            }
            menu
        });
        self.menu
            .deploy(menu, position, window, cx, |this: &mut Self| &mut this.menu);
        cx.notify();
    }

    fn deploy_ref_menu(
        &mut self,
        name: String,
        oid: Oid,
        kind: RefRow,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_branch = kind != RefRow::Tag;
        let this = cx.entity().downgrade();
        let current = self.current_branch(cx);
        let store = self.store.clone();
        let sync = self.store.read(cx).sync.clone();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let (t1, t2, n1, n2) = (this.clone(), this.clone(), name.clone(), name.clone());
            let n3 = name.clone();
            let s = store.clone();
            let mut menu = menu;
            let is_current = current.as_deref() == Some(name.as_str());
            if let Some(sync) = sync.clone() {
                match kind {
                    RefRow::Local if !is_current => {
                        let (c1, b1) = (sync.clone(), name.clone());
                        menu = menu.icon_entry(
                            format!("Switch to {name}"),
                            IconName::GitBranch,
                            move |window, cx| {
                                c1.update(cx, |c, cx| c.switch_branch(b1.clone(), window, cx))
                            },
                        );
                    }
                    RefRow::Local => {
                        menu = menu
                            .icon_action("Push", IconName::ArrowUp, Box::new(git_actions::Push))
                            .action("Publish Branch", Box::new(git_actions::PublishBranch))
                            .action("Set Upstream…", Box::new(git_actions::SetUpstream));
                    }
                    RefRow::Remote => {
                        let (c1, b1) = (sync.clone(), name.clone());
                        menu = menu.icon_entry(
                            "Check Out as Local Branch",
                            IconName::GitBranch,
                            move |window, cx| {
                                c1.update(cx, |c, cx| c.checkout_remote(b1.clone(), window, cx))
                            },
                        );
                    }
                    RefRow::Tag => {}
                }
                if kind == RefRow::Local {
                    let (c2, b2) = (sync.clone(), name.clone());
                    menu = menu.entry("Rename…", move |window, cx| {
                        c2.update(cx, |c, cx| {
                            c.rename_branch_dialog(Some(b2.clone()), window, cx)
                        })
                    });
                    if !is_current {
                        let (c3, b3) = (sync.clone(), name.clone());
                        menu = menu.entry("Delete…", move |window, cx| {
                            c3.update(cx, |c, cx| c.delete_branch(b3.clone(), window, cx))
                        });
                    }
                }
                if kind != RefRow::Tag {
                    menu = menu.separator();
                }
            }
            menu = menu.icon_entry("Show Log", IconName::History, move |window, cx| {
                let _ = t1.update(cx, |p, cx| p.show_log(n1.clone(), window, cx));
            });
            if current.as_deref() != Some(name.as_str()) && current.is_some() {
                let cur = current.clone().unwrap_or_default();
                menu = menu.icon_entry(
                    format!("Compare with {cur}"),
                    IconName::GitCompare,
                    move |window, cx| {
                        let _ =
                            t2.update(cx, |p, cx| p.compare_with_current(n2.clone(), window, cx));
                    },
                );
                let (s2, n4, cur2) = (store.clone(), name.clone(), cur.clone());
                menu = menu.entry(
                    format!("Changes of {cur} since {}", name),
                    move |window, cx| {
                        crate::review_tab::open_review(
                            &s2,
                            Changeset::BranchVsBase {
                                branch: cur2.clone(),
                                base: n4.clone(),
                            },
                            window,
                            cx,
                        );
                    },
                );
            }
            menu = menu.icon_entry("Open Tip Commit", IconName::GitCommit, move |window, cx| {
                crate::review_tab::open_commit_tab(&s, oid, false, window, cx);
            });
            menu.separator()
                .icon_entry(
                    if is_branch {
                        "Copy Branch Name"
                    } else {
                        "Copy Tag Name"
                    },
                    IconName::Copy,
                    move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(n3.clone())),
                )
                .entry("Copy Commit Hash", move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(oid.to_string()))
                })
        });
        self.menu
            .deploy(menu, position, window, cx, |this: &mut Self| &mut this.menu);
        cx.notify();
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let now = now_secs();
        let row_base = |id: (&'static str, usize)| {
            h_flex()
                .id(id)
                .w_full()
                .h(metrics::row_height())
                .pl(px(22.))
                .pr(px(10.))
                .gap(px(6.))
                .cursor_pointer()
                .when(self.selected == Some(id.1), |el| el.bg(ui.active.hsla()))
                .hover(|s| s.bg(ui.hover.hsla()))
        };
        let name_el = |name: String, strong: bool| {
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(metrics::text())
                .text_color(ui.text.hsla())
                .when(strong, |el| el.font_weight(FontWeight::SEMIBOLD))
                .child(name)
        };
        let faint = |s: String| {
            div()
                .flex_none()
                .text_size(metrics::text_xs())
                .text_color(ui.text_faint.hsla())
                .child(s)
        };
        let d = self.data.clone();
        match self.rows.get(ix).cloned() {
            Some(Row::Header(group, n)) => {
                let collapsed = self.collapsed.contains(&group);
                h_flex()
                    .id(("branches-header", ix))
                    .w_full()
                    .h(metrics::row_height())
                    .pl(px(6.))
                    .pr(px(10.))
                    .gap(px(4.))
                    .cursor_pointer()
                    .when(self.selected == Some(ix), |el| el.bg(ui.active.hsla()))
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
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui.text_muted.hsla())
                            .child(group.title().to_uppercase()),
                    )
                    .child(
                        div()
                            .px(px(6.))
                            .rounded(px(8.))
                            .bg(ui.hover.hsla())
                            .text_size(metrics::text_xs())
                            .text_color(ui.text_muted.hsla())
                            .child(n.to_string()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.collapsed.remove(&group) {
                            this.collapsed.insert(group);
                        }
                        this.rebuild();
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Some(Row::Local(i)) => {
                let info = &d.local[i];
                let b = &info.branch;
                let name = b.name.clone();
                let (n1, n2) = (name.clone(), name.clone());
                let oid = b.oid;
                row_base(("branch-local", ix))
                    .when(b.head, |el| el.bg(ui.accent.hsla().opacity(0.10)))
                    .child(
                        Icon::new(if b.head {
                            IconName::CircleCheck
                        } else {
                            IconName::GitBranch
                        })
                        .size(IconSize::Small)
                        .color(if b.head {
                            ui.accent.hsla()
                        } else {
                            ui.text_muted.hsla()
                        }),
                    )
                    .child(name_el(name, b.head))
                    .when_some(info.divergence, |el, (ahead, behind)| {
                        el.when(ahead > 0, |el| {
                            el.child(
                                div()
                                    .text_size(metrics::text_xs())
                                    .text_color(theme.git.added.hsla())
                                    .child(format!("↑{ahead}")),
                            )
                        })
                        .when(behind > 0, |el| {
                            el.child(
                                div()
                                    .text_size(metrics::text_xs())
                                    .text_color(theme.git.modified.hsla())
                                    .child(format!("↓{behind}")),
                            )
                        })
                    })
                    .child(faint(short_relative_time(b.time, now)))
                    .when_some(b.upstream.clone(), |el, up| {
                        el.tooltip(Tooltip::text(format!("Tracks {up}")))
                    })
                    .on_click(cx.listener(move |this, e: &gpui::ClickEvent, window, cx| {
                        if e.click_count() >= 2 {
                            this.compare_with_current(n1.clone(), window, cx);
                        } else {
                            this.show_log(n1.clone(), window, cx);
                        }
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                            this.deploy_ref_menu(
                                n2.clone(),
                                oid,
                                RefRow::Local,
                                e.position,
                                window,
                                cx,
                            );
                            cx.stop_propagation();
                        }),
                    )
                    .into_any_element()
            }
            Some(Row::Remote(i)) => {
                let b = &d.remote[i];
                let (n1, n2) = (b.name.clone(), b.name.clone());
                let oid = b.oid;
                row_base(("branch-remote", ix))
                    .child(
                        Icon::new(IconName::Globe)
                            .size(IconSize::Small)
                            .color(ui.text_muted.hsla()),
                    )
                    .child(name_el(b.name.clone(), false))
                    .child(faint(short_relative_time(b.time, now)))
                    .on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.show_log(n1.clone(), window, cx)
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                            this.deploy_ref_menu(
                                n2.clone(),
                                oid,
                                RefRow::Remote,
                                e.position,
                                window,
                                cx,
                            );
                            cx.stop_propagation();
                        }),
                    )
                    .into_any_element()
            }
            Some(Row::Tag(i)) => {
                let t = &d.tags[i];
                let (n1, n2) = (t.name.clone(), t.name.clone());
                let oid = t.oid;
                row_base(("branch-tag", ix))
                    .child(
                        Icon::new(IconName::Tag)
                            .size(IconSize::Small)
                            .color(theme.git.modified.hsla()),
                    )
                    .child(name_el(t.name.clone(), false))
                    .child(faint(oid.short()))
                    .on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.show_log(n1.clone(), window, cx)
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                            this.deploy_ref_menu(
                                n2.clone(),
                                oid,
                                RefRow::Tag,
                                e.position,
                                window,
                                cx,
                            );
                            cx.stop_propagation();
                        }),
                    )
                    .into_any_element()
            }
            Some(Row::Stash(i)) => {
                let s = &d.stashes[i];
                let oid = s.oid;
                let msg = s.message.clone();
                row_base(("branch-stash", ix))
                    .child(
                        Icon::new(IconName::Archive)
                            .size(IconSize::Small)
                            .color(ui.text_muted.hsla()),
                    )
                    .child(name_el(msg, false))
                    .child(faint(format!("stash@{{{}}}", s.index)))
                    .on_click(cx.listener(move |this, e: &gpui::ClickEvent, window, cx| {
                        crate::review_tab::open_commit_tab(
                            &this.store,
                            oid,
                            e.click_count() < 2,
                            window,
                            cx,
                        );
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                            this.deploy_stash_menu(oid, e.position, window, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .into_any_element()
            }
            None => div().into_any_element(),
        }
    }
}

impl Render for BranchesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let entity = cx.entity();
        let count = self.rows.len();
        let head = self.store.read(cx).head.clone();
        gpui::div()
            .id("branches-panel")
            .key_context(contexts::LIST_PANEL)
            .track_focus(&self.focus_handle)
            .on_action(
                cx.listener(|this, _: &picker_actions::SelectNext, _, cx| {
                    this.move_selection(1, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &picker_actions::SelectPrev, _, cx| {
                this.move_selection(-1, cx)
            }))
            .on_action(
                cx.listener(|this, _: &picker_actions::Confirm, window, cx| {
                    this.activate_selected(window, cx)
                }),
            )
            .on_action(cx.listener(
                |this, _: &lector_ui::actions::workspace::ShowContextMenu, window, cx| {
                    this.menu_for_selected(window, cx)
                },
            ))
            .size_full()
            .flex()
            .flex_col()
            .when_some(head, |el, h| {
                let label = h.branch.clone().unwrap_or_else(|| {
                    format!(
                        "detached at {}",
                        h.oid.map(|o| o.short()).unwrap_or_default()
                    )
                });
                el.child(
                    h_flex()
                        .flex_none()
                        .mx(px(8.))
                        .mb(px(6.))
                        .px(px(10.))
                        .py(px(8.))
                        .gap(px(8.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(ui.border.hsla())
                        .bg(ui.input_bg.hsla())
                        .child(
                            Icon::new(IconName::GitBranch)
                                .size(IconSize::Medium)
                                .color(ui.accent.hsla()),
                        )
                        .child(
                            gpui::div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .child(label),
                                )
                                .child(
                                    div()
                                        .text_size(metrics::text_xs())
                                        .text_color(ui.text_faint.hsla())
                                        .child(match &h.upstream {
                                            Some(up) => {
                                                format!("{up} · ↑{} ↓{}", h.ahead, h.behind)
                                            }
                                            None => "no upstream".into(),
                                        }),
                                ),
                        )
                        .child(
                            IconButton::new("branches-new", IconName::Plus)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::for_action(
                                    "New Branch…",
                                    &git_actions::NewBranch,
                                ))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(git_actions::NewBranch), cx)
                                }),
                        )
                        .child(
                            IconButton::new("branches-stash", IconName::Archive)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::for_action(
                                    "Stash Changes…",
                                    &git_actions::StashChanges,
                                ))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(git_actions::StashChanges), cx)
                                }),
                        )
                        .child(
                            IconButton::new("branches-refresh", IconName::Refresh)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Refresh"))
                                .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                        ),
                )
            })
            .when_some(self.error.clone(), |el, e| {
                el.child(
                    div()
                        .p(px(12.))
                        .text_size(metrics::text_sm())
                        .text_color(ui.text_faint.hsla())
                        .child(e),
                )
            })
            .child(div().flex_1().min_h_0().child(virtual_list(
                "branches-list",
                count,
                &self.list,
                cx,
                move |range, _, cx| {
                    entity.update(cx, |this, cx| {
                        range.map(|ix| this.render_row(ix, cx)).collect()
                    })
                },
            )))
            .children(self.menu.render())
    }
}

/// Activates the Branches panel (status bar click).
pub fn show_branches_panel(window: &mut Window, cx: &mut App) {
    window.dispatch_action(SidebarPanel::Branches.action(), cx);
}
