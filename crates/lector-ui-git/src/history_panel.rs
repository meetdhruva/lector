//! The History sidebar panel: a virtualized commit log with a painted graph
//! column, ref pills, filtering, paging, and file/line history modes.

use std::ops::Range;
use std::rc::Rc;

use gpui::{
    canvas, div, point, prelude::*, px, quad, size, App, BorderStyle, Bounds, ClipboardItem,
    Entity, FocusHandle, Focusable, FontWeight, Hsla, MouseButton, MouseDownEvent, PathBuilder,
    Pixels, Point, SharedString, Subscription, Task, Window,
};
use lector_git::{graph, Changeset, CommitInfo, EdgeKind, GraphRow, LogQuery, Oid, RefKind};
use lector_ui::components::context_menu::{ContextMenu, ContextMenuState};
use lector_ui::components::list::{virtual_list, VirtualListState};
use lector_ui::components::text_input::{TextInput, TextInputEvent};
use lector_ui::components::{h_flex, metrics, v_flex, IconButton, Tooltip};
use lector_ui::icons::{Icon, IconName, IconSize};
use lector_ui::{ActiveTheme, ColorExt};
use lector_workspace::review::WORKDIR_REV;

use crate::review_tab::ref_pill;
use crate::store::{GitStore, GitStoreEvent};
use crate::util::{lane_color, now_secs, short_relative_time};

const PAGE: usize = 300;
const ROW_H: f32 = 40.;
const LANE_W: f32 = 12.;
const GRAPH_PAD: f32 = 8.;
const MAX_LANES: u16 = 8;
const NODE_Y: f32 = 12.;

/// What the history shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryScope {
    /// `HEAD` (or the given revision).
    Log {
        /// Start revision; `None` = `HEAD`.
        rev: Option<String>,
        /// Include all branches and tags.
        all: bool,
    },
    /// Commits touching a file (follows renames).
    File(String),
    /// Commits touching lines of a file (as in `HEAD`).
    Lines(String, Range<u32>),
}

/// The History panel.
pub struct HistoryPanel {
    store: Entity<GitStore>,
    scope: HistoryScope,
    filter: Entity<TextInput>,
    commits: Vec<CommitInfo>,
    graph: Rc<Vec<GraphRow>>,
    max_lanes: u16,
    head: Option<Oid>,
    exhausted: bool,
    loading: Option<Task<()>>,
    generation: u64,
    selected: Option<Oid>,
    list: VirtualListState,
    error: Option<SharedString>,
    menu: ContextMenuState,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for HistoryPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

struct Filter {
    author: Option<String>,
    path: Option<String>,
    message: Option<String>,
}

fn parse_filter(text: &str) -> Filter {
    let mut f = Filter {
        author: None,
        path: None,
        message: None,
    };
    let mut words = Vec::new();
    for tok in text.split_whitespace() {
        if let Some(a) = tok
            .strip_prefix("author:")
            .or_else(|| tok.strip_prefix('@'))
        {
            f.author = Some(a.to_string());
        } else if let Some(p) = tok.strip_prefix("path:") {
            f.path = Some(p.trim_end_matches('/').to_string());
        } else {
            words.push(tok);
        }
    }
    if !words.is_empty() {
        f.message = Some(words.join(" "));
    }
    f
}

impl HistoryPanel {
    /// The panel for `store`'s workspace.
    pub fn new(store: Entity<GitStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| TextInput::new("Filter: text, @author, path:dir/", cx));
        let subs = vec![
            cx.subscribe(&store, |this, store, _: &GitStoreEvent, cx| {
                let head = store.read(cx).head.as_ref().and_then(|h| h.oid);
                if head != this.head || this.commits.is_empty() {
                    this.reload(cx);
                }
            }),
            cx.subscribe_in(&filter, window, |this, _, _: &TextInputEvent, _, cx| {
                this.reload(cx)
            }),
        ];
        let mut panel = HistoryPanel {
            store,
            scope: HistoryScope::Log {
                rev: None,
                all: false,
            },
            filter,
            commits: Vec::new(),
            graph: Rc::new(Vec::new()),
            max_lanes: 1,
            head: None,
            exhausted: false,
            loading: None,
            generation: 0,
            selected: None,
            list: VirtualListState::new(),
            error: None,
            menu: ContextMenuState::default(),
            focus_handle: cx.focus_handle(),
            _subscriptions: subs,
        };
        panel.reload(cx);
        panel
    }

    /// Shows `scope` (clears the filter).
    pub fn set_scope(&mut self, scope: HistoryScope, cx: &mut Context<Self>) {
        self.scope = scope;
        self.filter.update(cx, |f, cx| f.set_text("", cx));
        self.reload(cx);
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.commits.clear();
        self.graph = Rc::new(Vec::new());
        self.exhausted = false;
        self.loading = None;
        self.head = self.store.read(cx).head.as_ref().and_then(|h| h.oid);
        self.load_more(cx);
        cx.notify();
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        if self.loading.is_some() || self.exhausted {
            return;
        }
        let Some(repo) = self.store.read(cx).repo().cloned() else {
            self.error = Some("Not a git repository".into());
            return;
        };
        let generation = self.generation;
        let skip = self.commits.len();
        let scope = self.scope.clone();
        let filter = parse_filter(self.filter.read(cx).text());
        let work = cx.background_spawn(async move {
            match scope {
                HistoryScope::Log { rev, all } => repo.log(&LogQuery {
                    rev,
                    all_refs: all,
                    path: filter.path,
                    author: filter.author,
                    message: filter.message,
                    skip,
                    limit: PAGE,
                }),
                HistoryScope::File(path) => {
                    if skip > 0 {
                        return Ok(Vec::new());
                    }
                    repo.file_history(&path, 2000).map(|mut v| {
                        retain_filtered(&mut v, &filter);
                        v
                    })
                }
                HistoryScope::Lines(path, lines) => {
                    if skip > 0 {
                        return Ok(Vec::new());
                    }
                    repo.line_history(&path, lines, 500).map(|mut v| {
                        retain_filtered(&mut v, &filter);
                        v
                    })
                }
            }
        });
        self.loading = Some(cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = None;
                match result {
                    Ok(page) => {
                        this.error = None;
                        let paged = matches!(this.scope, HistoryScope::Log { .. });
                        if page.len() < PAGE || !paged {
                            this.exhausted = true;
                        }
                        this.commits.extend(page);
                        this.graph = Rc::new(if paged {
                            graph(&this.commits)
                        } else {
                            Vec::new()
                        });
                        this.max_lanes = this.graph.iter().map(|r| r.lanes).max().unwrap_or(1);
                    }
                    Err(e) => {
                        this.error = Some(e.to_string().into());
                        this.exhausted = true;
                    }
                }
                cx.notify();
            });
        }));
    }

    fn scope_label(&self, cx: &App) -> (IconName, String) {
        match &self.scope {
            HistoryScope::Log {
                rev: None,
                all: false,
            } => (
                IconName::GitBranch,
                self.store
                    .read(cx)
                    .branch_label()
                    .unwrap_or_else(|| "HEAD".into()),
            ),
            HistoryScope::Log { all: true, .. } => (IconName::GitFork, "All branches".into()),
            HistoryScope::Log { rev: Some(r), .. } => (IconName::GitBranch, r.clone()),
            HistoryScope::File(p) => (IconName::File, p.clone()),
            HistoryScope::Lines(p, l) => (
                IconName::FileText,
                format!("{}:{}–{}", p, l.start + 1, l.end),
            ),
        }
    }

    fn open_commit(
        &mut self,
        ix: usize,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(c) = self.commits.get(ix) else {
            return;
        };
        self.selected = Some(c.oid);
        crate::review_tab::open_commit_tab(&self.store, c.oid, preview, window, cx);
        cx.notify();
    }

    fn deploy_menu(
        &mut self,
        ix: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(c) = self.commits.get(ix) else {
            return;
        };
        let oid = c.oid;
        let store = self.store.clone();
        let link = self.store.read(cx).repo().and_then(|r| commit_url(r, oid));
        let file_scope = match &self.scope {
            HistoryScope::File(p) | HistoryScope::Lines(p, _) => Some(p.clone()),
            _ => None,
        };
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let t = this.clone();
            let mut menu = menu
                .icon_entry("Open Commit", IconName::GitCommit, move |window, cx| {
                    let _ = t.update(cx, |p, cx| p.open_commit(ix, false, window, cx));
                })
                .separator()
                .icon_entry("Copy Commit Hash", IconName::Copy, move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(oid.to_string()))
                })
                .entry("Copy Short Hash", move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(oid.short()))
                });
            if let Some(link) = link.clone() {
                menu = menu.icon_entry("Copy Commit Link", IconName::Link, move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(link.clone()))
                });
            }
            let (s1, s2) = (store.clone(), store.clone());
            menu = menu
                .separator()
                .icon_entry(
                    "Compare with Working Tree",
                    IconName::GitCompare,
                    move |window, cx| {
                        crate::review_tab::open_review(
                            &s1,
                            Changeset::Range {
                                from: oid.to_string(),
                                to: WORKDIR_REV.into(),
                            },
                            window,
                            cx,
                        );
                    },
                )
                .entry("Compare with HEAD", move |window, cx| {
                    crate::review_tab::open_review(
                        &s2,
                        Changeset::Range {
                            from: oid.to_string(),
                            to: "HEAD".into(),
                        },
                        window,
                        cx,
                    );
                });
            if let Some(path) = file_scope.clone() {
                let (s3, s4, p3, p4) = (store.clone(), store.clone(), path.clone(), path);
                menu = menu
                    .separator()
                    .icon_entry(
                        "View File at This Revision",
                        IconName::FileText,
                        move |window, cx| {
                            crate::open_file_at_revision(
                                &s3,
                                oid.to_string(),
                                p3.clone(),
                                window,
                                cx,
                            )
                        },
                    )
                    .entry("Compare File with Working Copy", move |window, cx| {
                        crate::open_file_vs_rev(&s4, p4.clone(), oid.to_string(), window, cx)
                    });
            }
            menu
        });
        self.menu
            .deploy(menu, position, window, cx, |this: &mut Self| &mut this.menu);
        cx.notify();
    }

    fn render_row(&mut self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        if !self.exhausted && ix + 60 >= self.commits.len() {
            self.load_more(cx);
        }
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let Some(c) = self.commits.get(ix) else {
            return div().h(px(ROW_H)).into_any_element();
        };
        let selected = self.selected == Some(c.oid);
        let graph_row = self.graph.get(ix).cloned();
        let prev_row = ix.checked_sub(1).and_then(|p| self.graph.get(p).cloned());
        let lanes = self.max_lanes;
        let graph_w = if graph_row.is_some() {
            px(GRAPH_PAD * 2. + LANE_W * lanes.clamp(1, MAX_LANES) as f32)
        } else {
            px(10.)
        };
        let is_head = self.head == Some(c.oid);
        let is_merge = c.parents.len() > 1;
        let lane_colors: Vec<Hsla> = (0..(lanes as usize + 2))
            .map(|l| lane_color(l, cx))
            .collect();
        let bg = if selected {
            ui.active.hsla()
        } else {
            ui.surface.hsla()
        };
        let now = now_secs();
        let refs = c.refs.clone();
        let refs: Vec<_> = refs
            .into_iter()
            .filter(|r| r.kind != RefKind::Head || is_head)
            .collect();
        let summary = c.summary.clone();
        let author = c.author.name.clone();
        let date = short_relative_time(c.author.time, now);
        let short = c.oid.short();
        h_flex()
            .id(("history-row", ix))
            .h(px(ROW_H))
            .w_full()
            .pr(px(10.))
            .cursor_pointer()
            .when(selected, |el| el.bg(ui.active.hsla()))
            .when(!selected, |el| el.hover(|s| s.bg(ui.hover.hsla())))
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                            paint_graph(
                                bounds,
                                prev_row.as_ref(),
                                graph_row.as_ref(),
                                &lane_colors,
                                is_head,
                                is_merge,
                                bg,
                                window,
                            )
                        })
                    },
                )
                .flex_none()
                .w(graph_w)
                .h_full(),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .gap(px(1.))
                    .child(
                        h_flex().w_full().min_w_0().child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(metrics::TEXT)
                                .text_color(ui.text.hsla())
                                .when(is_head, |el| el.font_weight(FontWeight::MEDIUM))
                                .child(summary),
                        ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap(px(5.))
                            .text_size(metrics::TEXT_XS)
                            .text_color(ui.text_faint.hsla())
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .children(refs.iter().take(2).map(|r| ref_pill(&r.name, r.kind, cx)))
                            .when(refs.len() > 2, |el| {
                                el.child(format!("+{}", refs.len() - 2))
                            })
                            .when(refs.is_empty(), |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .font_family(lector_ui::assets::MONO_FONT)
                                        .child(short),
                                )
                            })
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_color(ui.text_muted.hsla())
                                    .child(author),
                            )
                            .child(div().flex_1())
                            .child(div().flex_none().child(date)),
                    ),
            )
            .on_click(cx.listener(move |this, e: &gpui::ClickEvent, window, cx| {
                this.open_commit(ix, e.click_count() < 2, window, cx)
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                    this.deploy_menu(ix, e.position, window, cx);
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }

    fn deploy_scope_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        let set = move |scope: HistoryScope| {
            let t = this.clone();
            move |_: &mut Window, cx: &mut App| {
                let _ = t.update(cx, |p, cx| p.set_scope(scope.clone(), cx));
            }
        };
        let store = self.store.clone();
        let set2 = set.clone();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            menu.header("Show history of")
                .icon_entry(
                    "Current Branch",
                    IconName::GitBranch,
                    set(HistoryScope::Log {
                        rev: None,
                        all: false,
                    }),
                )
                .icon_entry(
                    "All Branches",
                    IconName::GitFork,
                    set(HistoryScope::Log {
                        rev: None,
                        all: true,
                    }),
                )
                .entry("Branch or Tag…", move |window, cx| {
                    let set2 = set2.clone();
                    crate::changes_panel::pick_from_repo(
                        &store,
                        "Show history of…",
                        window,
                        cx,
                        move |c, window, cx| {
                            set2(HistoryScope::Log {
                                rev: Some(c.rev),
                                all: false,
                            })(window, cx)
                        },
                    );
                })
        });
        self.menu
            .deploy(menu, position, window, cx, |this: &mut Self| &mut this.menu);
        cx.notify();
    }
}

fn retain_filtered(v: &mut Vec<CommitInfo>, f: &Filter) {
    let lower = |s: &str| s.to_lowercase();
    v.retain(|c| {
        f.author.as_ref().is_none_or(|a| {
            lower(&format!("{} <{}>", c.author.name, c.author.email)).contains(&lower(a))
        }) && f
            .message
            .as_ref()
            .is_none_or(|m| lower(&c.summary).contains(&lower(m)))
    });
}

/// Web URL of a commit on the `origin` (or first) remote's forge.
pub fn commit_url(repo: &lector_git::Repo, oid: Oid) -> Option<String> {
    let remotes = repo.remotes().ok()?;
    let remote = remotes
        .iter()
        .find(|r| r.name == "origin")
        .or_else(|| remotes.first())?;
    let web = lector_git::parse_remote_url(&remote.url)?;
    let sep = if web.host.contains("gitlab") {
        "/-/commit/"
    } else {
        "/commit/"
    };
    let sep = if web.host.contains("bitbucket") {
        "/commits/"
    } else {
        sep
    };
    Some(format!(
        "{}://{}/{}{}{}",
        web.scheme, web.host, web.path, sep, oid
    ))
}

#[allow(clippy::too_many_arguments)]
fn paint_graph(
    bounds: Bounds<Pixels>,
    prev: Option<&GraphRow>,
    row: Option<&GraphRow>,
    colors: &[Hsla],
    is_head: bool,
    is_merge: bool,
    bg: Hsla,
    window: &mut Window,
) {
    let Some(row) = row else {
        return;
    };
    let x = |lane: u16| bounds.left() + px(GRAPH_PAD + LANE_W * lane as f32 + LANE_W / 2.);
    let top = bounds.top();
    let cy = bounds.top() + px(NODE_Y);
    let bottom = bounds.bottom();
    let color = |lane: u16| colors.get(lane as usize).copied().unwrap_or(colors[0]);
    let stroke = |from: Point<Pixels>, to: Point<Pixels>, c: Hsla, window: &mut Window| {
        let mut b = PathBuilder::stroke(px(1.6));
        b.move_to(from);
        if from.x == to.x {
            b.line_to(to);
        } else {
            let mid_y = (from.y + to.y) / 2.;
            b.cubic_bezier_to(to, point(from.x, mid_y), point(to.x, mid_y));
        }
        if let Ok(path) = b.build() {
            window.paint_path(path, c);
        }
    };
    let edge_color = |e: &lector_git::GraphEdge| match e.kind {
        EdgeKind::MergeIn => color(e.to_lane),
        EdgeKind::BranchOut => color(e.from_lane),
        EdgeKind::Straight => color(e.from_lane),
    };
    // Incoming halves: previous row's edges, from the boundary to this center.
    if let Some(prev) = prev {
        for e in &prev.edges {
            let mid_x = (x(e.from_lane) + x(e.to_lane)) / 2.;
            if e.from_lane == e.to_lane {
                stroke(
                    point(x(e.to_lane), top),
                    point(x(e.to_lane), cy),
                    edge_color(e),
                    window,
                );
            } else {
                stroke(
                    point(mid_x, top),
                    point(x(e.to_lane), cy),
                    edge_color(e),
                    window,
                );
            }
        }
    }
    // Outgoing halves.
    for e in &row.edges {
        let mid_x = (x(e.from_lane) + x(e.to_lane)) / 2.;
        if e.from_lane == e.to_lane {
            stroke(
                point(x(e.from_lane), cy),
                point(x(e.from_lane), bottom),
                edge_color(e),
                window,
            );
        } else {
            // Bend within the lower part of the row, then continue straight.
            let bend = cy + (bottom - cy) * 0.75;
            stroke(
                point(x(e.from_lane), cy),
                point(mid_x, bend),
                edge_color(e),
                window,
            );
            stroke(
                point(mid_x, bend),
                point(mid_x, bottom),
                edge_color(e),
                window,
            );
        }
    }
    // Node.
    let c = color(row.lane);
    let r = if is_head { px(5.) } else { px(4.) };
    let node = Bounds::new(point(x(row.lane) - r, cy - r), size(r * 2., r * 2.));
    if is_merge || is_head {
        window.paint_quad(quad(node, r, bg, px(2.), c, BorderStyle::Solid));
        if is_head {
            let inner = px(2.);
            window.paint_quad(quad(
                Bounds::new(
                    point(x(row.lane) - inner, cy - inner),
                    size(inner * 2., inner * 2.),
                ),
                inner,
                c,
                px(0.),
                c,
                BorderStyle::Solid,
            ));
        }
    } else {
        window.paint_quad(quad(node, r, c, px(1.5), bg, BorderStyle::Solid));
    }
}

impl Render for HistoryPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let (icon, label) = self.scope_label(cx);
        let count = self.commits.len();
        let entity = cx.entity();
        let file_scope = matches!(self.scope, HistoryScope::File(_) | HistoryScope::Lines(..));
        v_flex()
            .id("history-panel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(
                v_flex()
                    .flex_none()
                    .px(px(8.))
                    .pb(px(6.))
                    .gap(px(6.))
                    .border_b_1()
                    .border_color(ui.border_variant.hsla())
                    .child(
                        h_flex()
                            .gap(px(4.))
                            .child(
                                h_flex()
                                    .id("history-scope")
                                    .flex_1()
                                    .min_w_0()
                                    .h(px(26.))
                                    .px(px(8.))
                                    .gap(px(6.))
                                    .rounded(px(5.))
                                    .border_1()
                                    .border_color(ui.border.hsla())
                                    .bg(ui.input_bg.hsla())
                                    .cursor_pointer()
                                    .hover(|s| s.border_color(ui.text_faint.hsla()))
                                    .child(
                                        Icon::new(icon)
                                            .size(IconSize::XSmall)
                                            .color(ui.text_muted.hsla()),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .text_size(metrics::TEXT_SM)
                                            .child(label),
                                    )
                                    .child(
                                        Icon::new(IconName::ChevronDown)
                                            .size(IconSize::XSmall)
                                            .color(ui.text_faint.hsla()),
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, e: &MouseDownEvent, window, cx| {
                                            this.deploy_scope_menu(e.position, window, cx);
                                            cx.stop_propagation();
                                        }),
                                    ),
                            )
                            .when(file_scope, |el| {
                                el.child(
                                    IconButton::new("history-clear-scope", IconName::Close)
                                        .icon_size(IconSize::Small)
                                        .tooltip(Tooltip::text("Show branch history"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.set_scope(
                                                HistoryScope::Log {
                                                    rev: None,
                                                    all: false,
                                                },
                                                cx,
                                            )
                                        })),
                                )
                            })
                            .child(
                                IconButton::new("history-refresh", IconName::Refresh)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Refresh"))
                                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                            ),
                    )
                    .child(
                        h_flex()
                            .h(px(26.))
                            .px(px(8.))
                            .gap(px(6.))
                            .rounded(px(5.))
                            .border_1()
                            .border_color(ui.border.hsla())
                            .bg(ui.input_bg.hsla())
                            .child(
                                Icon::new(IconName::Filter)
                                    .size(IconSize::XSmall)
                                    .color(ui.text_faint.hsla()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(metrics::TEXT_SM)
                                    .child(self.filter.clone()),
                            ),
                    ),
            )
            .when_some(self.error.clone(), |el, e| {
                el.child(
                    div()
                        .p(px(12.))
                        .text_size(metrics::TEXT_SM)
                        .text_color(ui.text_faint.hsla())
                        .child(e),
                )
            })
            .when(count == 0 && self.error.is_none(), |el| {
                el.child(
                    div()
                        .p(px(12.))
                        .text_size(metrics::TEXT_SM)
                        .text_color(ui.text_faint.hsla())
                        .child(if self.loading.is_some() {
                            "Loading…"
                        } else {
                            "No commits"
                        }),
                )
            })
            .child(div().flex_1().min_h_0().child(virtual_list(
                "history-list",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_parsing() {
        let f = parse_filter("fix parser @alice path:src/");
        assert_eq!(f.author.as_deref(), Some("alice"));
        assert_eq!(f.path.as_deref(), Some("src"));
        assert_eq!(f.message.as_deref(), Some("fix parser"));
        let f = parse_filter("");
        assert!(f.author.is_none() && f.path.is_none() && f.message.is_none());
    }
}
