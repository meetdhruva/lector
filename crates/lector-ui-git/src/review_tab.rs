//! Changeset review tab (file list with "viewed" checkboxes + the current
//! file's diff) and the commit tab (metadata above the same component).

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    div, prelude::*, px, AnyView, App, ClickEvent, ClipboardItem, Entity, FocusHandle, Focusable,
    FontWeight, MouseButton, MouseDownEvent, Pixels, Point, SharedString, Subscription, Task,
    Window,
};
use lector_git::{ChangeKind, Changeset, CommitDetail, Oid, RefKind};
use lector_ui::actions::diff as diff_actions;
use lector_ui::components::context_menu::{ContextMenu, ContextMenuState};
use lector_ui::components::list::{virtual_list, VirtualListState};
use lector_ui::components::{h_flex, metrics, v_flex, IconButton, Tooltip};
use lector_ui::icons::{FileIcon, Icon, IconName, IconSize};
use lector_ui::keymap::contexts;
use lector_ui::views::pane::{PaneItem, TabIcon};
use lector_ui::{ActiveTheme, ColorExt};
use lector_workspace::review::{self, changeset_key, ChangesetView};

use crate::diff_view::{
    settings_diff_options, stats_label, DiffTarget, DiffView, DiffViewEvent, ViewedTarget,
};
use crate::store::{GitStore, GitStoreEvent};
use crate::util::{
    format_date, kind_color, kind_letter, now_secs, ref_colors, relative_time, split_path,
};

/// The review component.
pub struct ReviewView {
    store: Entity<GitStore>,
    changeset: Changeset,
    commit: Option<Arc<CommitDetail>>,
    view: Option<ChangesetView>,
    selected: usize,
    diff: Entity<DiffView>,
    list: VirtualListState,
    load_task: Option<Task<()>>,
    error: Option<SharedString>,
    auto_advance: bool,
    menu: ContextMenuState,
    list_width: Pixels,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for ReviewView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.view.as_ref().is_some_and(|v| !v.files.is_empty()) {
            self.diff.focus_handle(cx)
        } else {
            self.focus_handle.clone()
        }
    }
}

impl ReviewView {
    /// Review of `changeset`; `commit` shows its metadata on top.
    pub fn new(
        store: Entity<GitStore>,
        changeset: Changeset,
        with_commit_header: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let diff = cx.new(|cx| DiffView::new(store.clone(), false, cx));
        let mut subs = vec![cx.subscribe(&diff, |this, _, event: &DiffViewEvent, cx| {
            if let DiffViewEvent::ViewedToggled(path, viewed) = event {
                this.on_viewed_toggled(path, *viewed, cx);
            }
        })];
        let worktree_like = match &changeset {
            Changeset::WorkingTree
            | Changeset::Staged
            | Changeset::Unstaged
            | Changeset::Untracked => true,
            Changeset::Range { to, .. } => to == review::WORKDIR_REV,
            _ => false,
        };
        if worktree_like {
            subs.push(cx.subscribe(&store, |this, _, _: &GitStoreEvent, cx| this.reload(cx)));
        }
        let mut this = ReviewView {
            store,
            changeset,
            commit: None,
            view: None,
            selected: 0,
            diff,
            list: VirtualListState::new(),
            load_task: None,
            error: None,
            auto_advance: true,
            menu: ContextMenuState::default(),
            list_width: px(300.),
            focus_handle: cx.focus_handle(),
            _subscriptions: subs,
        };
        this.load(with_commit_header, cx);
        this
    }

    fn title(&self, cx: &App) -> SharedString {
        if let Some(c) = &self.commit {
            return format!("{} {}", c.info.oid.short(), c.info.summary).into();
        }
        match &self.view {
            Some(v) => v.title.clone().into(),
            None => review::changeset_title(self.store.read(cx).repo(), &self.changeset).into(),
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let with_commit = self.commit.is_some();
        self.load(with_commit, cx);
    }

    fn load(&mut self, with_commit: bool, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let Some(repo) = store.repo().cloned() else {
            self.error = Some("Not a git repository".into());
            return;
        };
        let review_store = store.review.clone();
        let opts = settings_diff_options(store, cx);
        let cs = self.changeset.clone();
        let work = cx.background_spawn(async move {
            let commit = match (&cs, with_commit) {
                (Changeset::Commit(oid), true) => {
                    Some(Arc::new(repo.commit(*oid).map_err(|e| e.to_string())?))
                }
                _ => None,
            };
            let view = match &cs {
                Changeset::Range { from, to } if to == review::WORKDIR_REV => {
                    review::load_vs_workdir_with(&repo, from, &review_store, &opts)
                }
                _ => review::load_changeset_with(&repo, &cs, &review_store, &opts),
            }
            .map_err(|e| e.to_string())?;
            Ok::<_, String>((commit, view))
        });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                this.load_task = None;
                match result {
                    Ok((commit, view)) => {
                        if commit.is_some() {
                            this.commit = commit;
                        }
                        let current = this
                            .view
                            .as_ref()
                            .and_then(|v| v.files.get(this.selected))
                            .map(|f| f.path().to_string());
                        this.selected = current
                            .and_then(|p| view.index_of(&p))
                            .unwrap_or(0)
                            .min(view.files.len().saturating_sub(1));
                        this.view = Some(view);
                        this.error = None;
                        this.show_selected(cx);
                    }
                    Err(e) => this.error = Some(e.into()),
                }
                cx.notify();
            });
        }));
    }

    fn show_selected(&mut self, cx: &mut Context<Self>) {
        let Some(view) = &self.view else {
            return;
        };
        let Some(file) = view.files.get(self.selected) else {
            return;
        };
        let target = DiffTarget::Change(file.change.clone());
        let viewed = (!file.content_hash.is_empty()).then(|| ViewedTarget {
            key: view.key.clone(),
            file: file.clone(),
        });
        let same = self.diff.read(cx).target() == Some(&target);
        self.diff.update(cx, |d, cx| {
            if same {
                d.set_viewed(viewed, cx);
            } else {
                d.set_target(target, viewed, cx);
            }
        });
        self.list.reveal(self.selected);
        cx.notify();
    }

    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = ix;
        self.show_selected(cx);
        let f = self.diff.focus_handle(cx);
        window.focus(&f);
    }

    fn step_file(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &self.view else {
            return;
        };
        let n = view.files.len() as isize;
        if n == 0 {
            return;
        }
        let ix = (self.selected as isize + delta).clamp(0, n - 1) as usize;
        if ix != self.selected {
            self.select(ix, window, cx);
        }
    }

    fn on_viewed_toggled(&mut self, path: &str, viewed: bool, cx: &mut Context<Self>) {
        let Some(view) = &mut self.view else {
            return;
        };
        let Some(ix) = view.index_of(path) else {
            return;
        };
        view.files[ix].viewed = viewed;
        if viewed && self.auto_advance && ix == self.selected {
            let n = view.files.len();
            let next = (1..n)
                .map(|d| (ix + d) % n)
                .find(|&i| !view.files[i].viewed);
            if let Some(next) = next {
                self.selected = next;
                self.show_selected(cx);
            }
        }
        cx.notify();
    }

    fn toggle_viewed_at(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(view) = &mut self.view else {
            return;
        };
        let key = view.key.clone();
        let Some(file) = view.files.get_mut(ix) else {
            return;
        };
        if file.content_hash.is_empty() {
            return;
        }
        let new_state = !file.viewed;
        let store = self.store.read(cx);
        if let Some(repo) = store.repo() {
            let root = repo.workdir().to_path_buf();
            if let Err(e) = review::set_viewed_with(&store.review, &root, &key, file, new_state) {
                tracing::warn!("could not save viewed state: {e}");
            }
        }
        if ix == self.selected {
            let viewed = ViewedTarget {
                key,
                file: file.clone(),
            };
            self.diff.update(cx, |d, cx| d.set_viewed(Some(viewed), cx));
        }
        cx.notify();
    }

    fn deploy_file_menu(
        &mut self,
        ix: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self.view.as_ref().and_then(|v| v.files.get(ix)) else {
            return;
        };
        let path = file.path().to_string();
        let old_path = file.change.old_path.clone();
        let exists_now = self
            .store
            .read(cx)
            .repo()
            .is_some_and(|r| r.workdir().join(&path).exists());
        let commit = match &self.changeset {
            Changeset::Commit(oid) => Some(*oid),
            _ => None,
        };
        let deleted = file.change.kind == ChangeKind::Deleted;
        let store = self.store.clone();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            if let (Some(oid), false) = (commit, deleted) {
                let (s, p) = (store.clone(), path.clone());
                menu = menu.icon_entry(
                    "View File at This Revision",
                    IconName::FileText,
                    move |window, cx| {
                        crate::open_file_at_revision(&s, oid.to_string(), p.clone(), window, cx);
                    },
                );
            }
            if let (Some(oid), Some(old)) = (commit, old_path.clone()) {
                let s = store.clone();
                menu = menu.entry("View File Before This Revision", move |window, cx| {
                    crate::open_file_at_revision(&s, format!("{oid}^"), old.clone(), window, cx);
                });
            }
            if exists_now {
                let (s, p) = (store.clone(), path.clone());
                menu = menu.icon_entry(
                    "Open Working File",
                    IconName::FileCode,
                    move |window, cx| {
                        let Some(abs) = s.read(cx).repo().map(|r| r.workdir().join(&p)) else {
                            return;
                        };
                        let ws = s.read(cx).workspace.clone();
                        let _ = ws.update(cx, |ws, cx| ws.open_path(abs, false, window, cx));
                    },
                );
                if let Some(oid) = commit {
                    let (s, p) = (store.clone(), path.clone());
                    menu = menu.icon_entry(
                        "Compare Working File with This Revision",
                        IconName::GitCompare,
                        move |window, cx| {
                            crate::open_file_vs_rev(&s, p.clone(), oid.to_string(), window, cx);
                        },
                    );
                }
            }
            let (s, p) = (store.clone(), path.clone());
            let p2 = path.clone();
            menu.separator()
                .icon_entry("Copy Path", IconName::Copy, move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(p2.clone()));
                })
                .icon_entry("File History", IconName::History, move |window, cx| {
                    crate::show_file_history(&s, p.clone(), window, cx);
                })
        });
        self.menu
            .deploy(menu, position, window, cx, |this: &mut Self| &mut this.menu);
        cx.notify();
    }

    fn render_commit_header(
        &self,
        commit: &CommitDetail,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let info = &commit.info;
        let now = now_secs();
        let body = commit
            .message
            .trim()
            .strip_prefix(info.summary.trim())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| commit.message.trim().to_string());
        let initial = info
            .author
            .name
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_default();
        let hue = (info.author.email.bytes().map(|b| b as u32).sum::<u32>() % 360) as f32 / 360.;
        let same_committer = info.committer.name == info.author.name;
        let oid = info.oid;
        let label = |t: &'static str| {
            div()
                .w(px(78.))
                .flex_none()
                .text_size(metrics::TEXT_SM)
                .text_color(ui.text_faint.hsla())
                .child(t)
        };
        let value = || {
            div()
                .text_size(metrics::TEXT_SM)
                .text_color(ui.text_muted.hsla())
        };
        let parents = info.parents.clone();
        v_flex()
            .flex_none()
            .max_h(px(320.))
            .px(px(20.))
            .py(px(14.))
            .gap(px(10.))
            .border_b_1()
            .border_color(ui.border.hsla())
            .bg(ui.surface.hsla())
            .child(
                h_flex()
                    .gap(px(12.))
                    .items_start()
                    .child(
                        div()
                            .flex_none()
                            .size(px(36.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(gpui::hsla(hue, 0.45, 0.5, 1.))
                            .text_color(gpui::white())
                            .font_weight(FontWeight::BOLD)
                            .child(initial),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(4.))
                            .child(
                                h_flex()
                                    .gap(px(6.))
                                    .flex_wrap()
                                    .child(
                                        div()
                                            .text_size(px(15.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(ui.text.hsla())
                                            .child(info.summary.clone()),
                                    )
                                    .children(
                                        info.refs.iter().map(|r| ref_pill(&r.name, r.kind, cx)),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap(px(6.))
                                    .text_size(metrics::TEXT_SM)
                                    .text_color(ui.text_muted.hsla())
                                    .child(
                                        div()
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(ui.text.hsla())
                                            .child(info.author.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_color(ui.text_faint.hsla())
                                            .child(format!("<{}>", info.author.email)),
                                    )
                                    .child("·")
                                    .child(relative_time(info.author.time, now)),
                            ),
                    )
                    .child(
                        h_flex()
                            .id("commit-sha")
                            .flex_none()
                            .gap(px(6.))
                            .px(px(8.))
                            .h(px(24.))
                            .rounded(px(5.))
                            .border_1()
                            .border_color(ui.border.hsla())
                            .font_family(lector_ui::assets::MONO_FONT)
                            .text_size(metrics::TEXT_SM)
                            .text_color(ui.text_muted.hsla())
                            .cursor_pointer()
                            .hover(|s| s.bg(ui.hover.hsla()))
                            .tooltip(Tooltip::text("Copy commit hash"))
                            .child(
                                Icon::new(IconName::GitCommit)
                                    .size(IconSize::XSmall)
                                    .color(ui.text_faint.hsla()),
                            )
                            .child(oid.short())
                            .child(
                                Icon::new(IconName::Copy)
                                    .size(IconSize::XSmall)
                                    .color(ui.text_faint.hsla()),
                            )
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(oid.to_string()))
                            }),
                    ),
            )
            .when(!body.is_empty(), |el| {
                el.child(
                    div()
                        .id("commit-body")
                        .pl(px(48.))
                        .max_h(px(140.))
                        .overflow_y_scroll()
                        .text_size(metrics::TEXT)
                        .text_color(ui.text.hsla())
                        .whitespace_normal()
                        .children(
                            body.lines()
                                .map(|l| div().min_h(px(16.)).child(l.to_string())),
                        ),
                )
            })
            .child(
                v_flex()
                    .pl(px(48.))
                    .gap(px(3.))
                    .child(h_flex().child(label("Authored")).child(
                        value().child(format_date(info.author.time, info.author.offset_minutes)),
                    ))
                    .when(
                        !same_committer || info.committer.time != info.author.time,
                        |el| {
                            el.child(h_flex().child(label("Committed")).child(value().child(
                                format!(
                                    "{} by {}",
                                    format_date(info.committer.time, info.committer.offset_minutes),
                                    info.committer.name
                                ),
                            )))
                        },
                    )
                    .child(
                        h_flex()
                            .child(label(if parents.len() > 1 {
                                "Parents"
                            } else {
                                "Parent"
                            }))
                            .child(
                                h_flex()
                                    .gap(px(6.))
                                    .children(parents.iter().enumerate().map(|(i, p)| {
                                        let p = *p;
                                        div()
                                            .id(("parent", i))
                                            .font_family(lector_ui::assets::MONO_FONT)
                                            .text_size(metrics::TEXT_SM)
                                            .text_color(ui.text_accent.hsla())
                                            .cursor_pointer()
                                            .hover(|s| s.underline())
                                            .child(p.short())
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                crate::open_commit(&this.store, p, window, cx);
                                            }))
                                    }))
                                    .when(parents.is_empty(), |el| {
                                        el.child(value().child("none (root commit)"))
                                    }),
                            ),
                    ),
            )
    }

    fn render_list_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let (viewed, total, stats) = match &self.view {
            Some(v) => (v.viewed_count(), v.files.len(), v.total_stats()),
            None => (0, 0, Default::default()),
        };
        let frac = if total == 0 {
            0.
        } else {
            viewed as f32 / total as f32
        };
        v_flex()
            .flex_none()
            .px(px(10.))
            .pt(px(8.))
            .pb(px(8.))
            .gap(px(6.))
            .border_b_1()
            .border_color(ui.border_variant.hsla())
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(metrics::TEXT_SM)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui.text.hsla())
                            .child(format!("{total} file{}", if total == 1 { "" } else { "s" })),
                    )
                    .child(stats_label(stats, &theme))
                    .child(div().flex_1())
                    .child(
                        IconButton::new("review-prev", IconName::ChevronUp)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::for_action(
                                "Previous File",
                                &diff_actions::PrevFile,
                            ))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.step_file(-1, window, cx)),
                            ),
                    )
                    .child(
                        IconButton::new("review-next", IconName::ChevronDown)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::for_action("Next File", &diff_actions::NextFile))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.step_file(1, window, cx)),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .h(px(4.))
                            .rounded(px(2.))
                            .bg(ui.hover.hsla())
                            .child(div().h_full().rounded(px(2.)).w(gpui::relative(frac)).bg(
                                if viewed == total && total > 0 {
                                    theme.git.added.hsla()
                                } else {
                                    ui.accent.hsla()
                                },
                            )),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(metrics::TEXT_XS)
                            .text_color(ui.text_muted.hsla())
                            .child(format!("{viewed} / {total} viewed")),
                    ),
            )
    }

    fn render_file_row(&self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let Some(file) = self.view.as_ref().and_then(|v| v.files.get(ix)) else {
            return div().into_any_element();
        };
        let selected = ix == self.selected;
        let path = file.path().to_string();
        let (name, dir) = split_path(&path);
        let viewed = file.viewed;
        let can_view = !file.content_hash.is_empty();
        let accent = ui.accent.hsla();
        h_flex()
            .id(("review-file", ix))
            .w_full()
            .h(px(28.))
            .pl(px(6.))
            .pr(px(10.))
            .gap(px(6.))
            .cursor_pointer()
            .when(selected, |el| el.bg(ui.active.hsla()))
            .when(!selected, |el| el.hover(|s| s.bg(ui.hover.hsla())))
            .child(
                div()
                    .id(("review-check", ix))
                    .flex_none()
                    .p(px(2.))
                    .rounded(px(3.))
                    .hover(|s| s.bg(ui.hover.hsla()))
                    .when(can_view, |el| {
                        el.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.toggle_viewed_at(ix, cx);
                            cx.stop_propagation();
                        }))
                    })
                    .child(
                        Icon::new(if viewed {
                            IconName::CheckSquare
                        } else {
                            IconName::Square
                        })
                        .size(IconSize::Small)
                        .color(if viewed {
                            accent
                        } else {
                            ui.text_faint.hsla()
                        }),
                    ),
            )
            .child(FileIcon::file(name.to_string()).size(px(16.)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(
                        div()
                            .text_size(metrics::TEXT)
                            .text_color(if viewed {
                                ui.text_muted.hsla()
                            } else {
                                ui.text.hsla()
                            })
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(name.to_string()),
                    )
                    .when(!dir.is_empty(), |el| {
                        el.child(
                            div()
                                .text_size(metrics::TEXT_XS)
                                .line_height(px(12.))
                                .text_color(ui.text_faint.hsla())
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(dir.to_string()),
                        )
                    }),
            )
            .when_some(file.stats.filter(|s| s.added + s.removed > 0), |el, s| {
                el.child(
                    h_flex()
                        .flex_none()
                        .gap(px(4.))
                        .text_size(metrics::TEXT_XS)
                        .when(s.added > 0, |el| {
                            el.child(
                                div()
                                    .text_color(theme.diff.added_text.hsla())
                                    .child(format!("+{}", s.added)),
                            )
                        })
                        .when(s.removed > 0, |el| {
                            el.child(
                                div()
                                    .text_color(theme.diff.removed_text.hsla())
                                    .child(format!("−{}", s.removed)),
                            )
                        }),
                )
            })
            .child(
                div()
                    .flex_none()
                    .w(px(12.))
                    .text_size(metrics::TEXT_SM)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(kind_color(file.change.kind, cx))
                    .child(kind_letter(file.change.kind)),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.select(ix, window, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                    this.deploy_file_menu(ix, e.position, window, cx);
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }
}

/// A colored pill for a ref label.
pub fn ref_pill(name: &str, kind: RefKind, cx: &App) -> impl IntoElement {
    let (bg, fg) = ref_colors(kind, cx);
    let icon = match kind {
        RefKind::Head => None,
        RefKind::LocalBranch => Some(IconName::GitBranch),
        RefKind::RemoteBranch => Some(IconName::Globe),
        RefKind::Tag => Some(IconName::Tag),
    };
    h_flex()
        .flex_none()
        .h(px(18.))
        .px(px(6.))
        .gap(px(3.))
        .rounded(px(9.))
        .bg(bg)
        .border_1()
        .border_color(fg.opacity(0.35))
        .text_size(metrics::TEXT_XS)
        .font_weight(FontWeight::MEDIUM)
        .text_color(fg)
        .when_some(icon, |el, i| {
            el.child(Icon::new(i).size(IconSize::Custom(px(10.))).color(fg))
        })
        .child(name.to_string())
}

impl Render for ReviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let count = self.view.as_ref().map(|v| v.files.len()).unwrap_or(0);
        let entity = cx.entity();
        let commit = self.commit.clone();
        let body = if let Some(err) = &self.error {
            div()
                .p(px(20.))
                .text_color(ui.text_muted.hsla())
                .child(err.clone())
                .into_any_element()
        } else if self.view.is_some() && count == 0 {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .child(
                    Icon::new(IconName::CircleCheck)
                        .size(IconSize::Custom(px(32.)))
                        .color(ui.text_faint.hsla()),
                )
                .child(div().text_color(ui.text_muted.hsla()).child("No changes"))
                .into_any_element()
        } else {
            h_flex()
                .flex_1()
                .min_h_0()
                .items_start()
                .child(
                    v_flex()
                        .flex_none()
                        .w(self.list_width)
                        .h_full()
                        .border_r_1()
                        .border_color(ui.border.hsla())
                        .bg(ui.surface.hsla())
                        .child(self.render_list_header(cx))
                        .child(div().flex_1().min_h_0().pt(px(2.)).child(virtual_list(
                            "review-files",
                            count,
                            &self.list,
                            cx,
                            move |range, _, cx| {
                                entity.update(cx, |this, cx| {
                                    range.map(|ix| this.render_file_row(ix, cx)).collect()
                                })
                            },
                        ))),
                )
                .child(div().flex_1().min_w_0().h_full().child(self.diff.clone()))
                .into_any_element()
        };
        v_flex()
            .id("review-view")
            .key_context(contexts::DIFF_VIEW)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(ui.background.hsla())
            .on_action(cx.listener(|this, _: &diff_actions::NextFile, window, cx| {
                this.step_file(1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &diff_actions::PrevFile, window, cx| {
                this.step_file(-1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &diff_actions::ToggleViewed, _, cx| {
                let ix = this.selected;
                this.toggle_viewed_at(ix, cx);
                if let Some(path) = this
                    .view
                    .as_ref()
                    .and_then(|v| v.files.get(ix))
                    .map(|f| (f.path().to_string(), f.viewed))
                {
                    this.on_viewed_toggled(&path.0, path.1, cx);
                }
            }))
            .when_some(commit, |el, c| el.child(self.render_commit_header(&c, cx)))
            .child(body)
            .children(self.menu.render())
    }
}

/// A review or commit in a pane tab.
pub struct ReviewItem {
    view: Entity<ReviewView>,
    key: SharedString,
    is_commit: bool,
}

impl PaneItem for ReviewItem {
    fn title(&self, cx: &App) -> SharedString {
        let t = self.view.read(cx).title(cx);
        if self.is_commit {
            t
        } else {
            format!("Review: {t}").into()
        }
    }

    fn icon(&self, _: &App) -> TabIcon {
        TabIcon::Ui(if self.is_commit {
            IconName::GitCommit
        } else {
            IconName::GitPullRequest
        })
    }

    fn path(&self, _: &App) -> Option<PathBuf> {
        None
    }

    fn item_key(&self, _: &App) -> Option<SharedString> {
        Some(self.key.clone())
    }

    fn tab_kind(&self, cx: &App) -> Option<lector_config::TabKind> {
        let v = self.view.read(cx);
        Some(match (&v.changeset, self.is_commit) {
            (Changeset::Commit(oid), true) => lector_config::TabKind::Commit {
                oid: oid.to_string(),
            },
            (cs, _) => lector_config::TabKind::Other {
                label: format!("review:{}", changeset_key(cs)),
            },
        })
    }

    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.read(cx).focus_handle(cx)
    }

    fn clone_item(&self, _window: &mut Window, cx: &mut App) -> Option<Box<dyn PaneItem>> {
        let (store, cs, commit) = {
            let v = self.view.read(cx);
            (v.store.clone(), v.changeset.clone(), v.commit.is_some())
        };
        let view = cx.new(|cx| ReviewView::new(store, cs, commit, cx));
        Some(Box::new(ReviewItem {
            view,
            key: self.key.clone(),
            is_commit: self.is_commit,
        }))
    }
}

fn open_item(
    store: &Entity<GitStore>,
    cs: Changeset,
    commit: bool,
    preview: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(ws) = store.read(cx).workspace.upgrade() else {
        return;
    };
    let key: SharedString = if commit {
        format!("commit:{}", changeset_key(&cs)).into()
    } else {
        format!("review:{}", changeset_key(&cs)).into()
    };
    let pane = ws.read(cx).active_pane().clone();
    if let Some(ix) = pane.read(cx).index_for_key(&key, cx) {
        pane.update(cx, |p, cx| {
            if !preview {
                p.pin(ix, cx);
            }
            p.activate(ix, true, window, cx)
        });
        return;
    }
    let store = store.clone();
    let view = cx.new(|cx| ReviewView::new(store, cs, commit, cx));
    let item = Box::new(ReviewItem {
        view,
        key,
        is_commit: commit,
    });
    ws.update(cx, |ws, cx| ws.add_item(item, preview, window, cx));
}

/// Recreates a review/commit tab from the session.
pub fn restore_item(
    store: &Entity<GitStore>,
    kind: &lector_config::TabKind,
    cx: &mut App,
) -> Option<Box<dyn PaneItem>> {
    let (cs, commit) = match kind {
        lector_config::TabKind::Commit { oid } => (Changeset::Commit(oid.parse().ok()?), true),
        lector_config::TabKind::Other { label } => (
            review::parse_changeset_key(label.strip_prefix("review:")?)?,
            false,
        ),
        _ => return None,
    };
    let key: SharedString = if commit {
        format!("commit:{}", changeset_key(&cs)).into()
    } else {
        format!("review:{}", changeset_key(&cs)).into()
    };
    let store = store.clone();
    let view = cx.new(|cx| ReviewView::new(store, cs, commit, cx));
    Some(Box::new(ReviewItem {
        view,
        key,
        is_commit: commit,
    }))
}

/// Opens (or activates) a review tab for `cs`.
pub fn open_review(store: &Entity<GitStore>, cs: Changeset, window: &mut Window, cx: &mut App) {
    open_item(store, cs, false, false, window, cx);
}

/// Opens (or activates) a commit tab.
pub fn open_commit_tab(
    store: &Entity<GitStore>,
    oid: Oid,
    preview: bool,
    window: &mut Window,
    cx: &mut App,
) {
    open_item(store, Changeset::Commit(oid), true, preview, window, cx);
}
