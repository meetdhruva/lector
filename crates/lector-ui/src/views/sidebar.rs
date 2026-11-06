//! The sidebar: an activity bar (Files, Changes, History, Branches, Search,
//! Outline) and the active panel.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::scale::px;
use gpui::{
    div, prelude::*, Action, AnyView, App, Entity, FocusHandle, Focusable, FontWeight,
    SharedString, Window,
};

use super::file_tree::FsTreeDelegate;
use crate::actions::workspace::*;
use crate::components::tree_view::TreeView;
use crate::components::{
    h_flex, metrics, v_flex, Button, ButtonStyle, ContextMenuState, IconButton, Tooltip,
};
use crate::icons::{Icon, IconName, IconSize};
use crate::theme::{ActiveTheme, ColorExt};

/// Sidebar panels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SidebarPanel {
    /// File tree.
    Files,
    /// Working tree / staged changes.
    Changes,
    /// Commit history.
    History,
    /// Branches, tags, stashes.
    Branches,
    /// Project search.
    Search,
    /// Symbol outline of the active file.
    Outline,
    /// GitHub pull requests / GitLab merge requests (v0.6, lector-ui-git).
    PullRequests,
    /// Coding agent sessions (v0.5, lector-ui-agent).
    Agent,
}

impl SidebarPanel {
    /// All panels in activity-bar order.
    pub const ALL: [SidebarPanel; 8] = [
        SidebarPanel::Files,
        SidebarPanel::Changes,
        SidebarPanel::History,
        SidebarPanel::Branches,
        SidebarPanel::Search,
        SidebarPanel::Outline,
        SidebarPanel::PullRequests,
        SidebarPanel::Agent,
    ];

    /// Activity bar icon.
    pub fn icon(self) -> IconName {
        match self {
            SidebarPanel::Files => IconName::Files,
            SidebarPanel::Changes => IconName::GitCompare,
            SidebarPanel::History => IconName::History,
            SidebarPanel::Branches => IconName::GitBranch,
            SidebarPanel::Search => IconName::Search,
            SidebarPanel::Outline => IconName::ListTree,
            SidebarPanel::PullRequests => IconName::GitPullRequest,
            SidebarPanel::Agent => IconName::Sparkles,
        }
    }

    /// Panel title.
    pub fn title(self) -> &'static str {
        match self {
            SidebarPanel::Files => "Files",
            SidebarPanel::Changes => "Changes",
            SidebarPanel::History => "History",
            SidebarPanel::Branches => "Branches",
            SidebarPanel::Search => "Search",
            SidebarPanel::Outline => "Outline",
            SidebarPanel::PullRequests => "Pull Requests",
            SidebarPanel::Agent => "Agent",
        }
    }

    /// Action that shows this panel.
    pub fn action(self) -> Box<dyn Action> {
        match self {
            SidebarPanel::Files => Box::new(ShowFiles),
            SidebarPanel::Changes => Box::new(ShowChanges),
            SidebarPanel::History => Box::new(ShowHistory),
            SidebarPanel::Branches => Box::new(ShowBranches),
            SidebarPanel::Search => Box::new(ToggleProjectSearch),
            SidebarPanel::Outline => Box::new(ShowOutline),
            SidebarPanel::PullRequests => Box::new(crate::actions::review::ShowPullRequests),
            SidebarPanel::Agent => Box::new(crate::actions::agent::TogglePanel),
        }
    }

    fn empty_state(self, has_folder: bool) -> (&'static str, &'static str) {
        if !has_folder {
            return match self {
                SidebarPanel::Search => ("No folder open", "Open a folder to search its files."),
                SidebarPanel::Outline => {
                    ("No symbols", "Symbols of the active file will appear here.")
                }
                _ => (
                    "No folder open",
                    "Open a folder to browse its files and review its changes.",
                ),
            };
        }
        match self {
            SidebarPanel::Files => ("No folder open", "Open a folder to browse its files."),
            SidebarPanel::Changes | SidebarPanel::History | SidebarPanel::Branches => (
                "Not a git repository",
                "Changes, history and branches appear for folders inside a git repository.",
            ),
            SidebarPanel::Search => ("Search", "Search across the project."),
            SidebarPanel::Outline => ("No symbols", "Symbols of the active file will appear here."),
            SidebarPanel::PullRequests => (
                "Not a git repository",
                "Pull requests appear for repositories hosted on GitHub or GitLab.",
            ),
            SidebarPanel::Agent => (
                "Agent",
                "Drive a coding agent and review each turn's changes.",
            ),
        }
    }
}

/// Events emitted by the [`Sidebar`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidebarEvent {
    /// The Files panel's Refresh button: rescan the project's files.
    RefreshFiles,
}

/// The sidebar view.
pub struct Sidebar {
    active: SidebarPanel,
    file_tree: Entity<TreeView<FsTreeDelegate>>,
    panels: HashMap<SidebarPanel, AnyView>,
    focus_handle: FocusHandle,
    /// Contains the activity bar's buttons (F6 region; not a tab stop itself).
    activity_focus: FocusHandle,
    /// Contains the active panel (header and content).
    panel_focus: FocusHandle,
    has_folder: bool,
    gear_menu: ContextMenuState,
}

impl gpui::EventEmitter<SidebarEvent> for Sidebar {}

impl Focusable for Sidebar {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.active {
            SidebarPanel::Files => self.file_tree.focus_handle(cx),
            _ => self.focus_handle.clone(),
        }
    }
}

impl Sidebar {
    /// Sidebar with a file tree over `root`.
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let file_tree = cx.new(|cx| {
            let mut tree = TreeView::new("file-tree", FsTreeDelegate::new(root.clone()), cx);
            tree.set_expanded(&root, true, cx);
            tree
        });
        Sidebar {
            active: SidebarPanel::Files,
            file_tree,
            panels: HashMap::new(),
            focus_handle: cx.focus_handle(),
            activity_focus: cx.focus_handle(),
            panel_focus: cx.focus_handle(),
            has_folder: true,
            gear_menu: ContextMenuState::default(),
        }
    }

    /// Handle that contains the focus while an activity bar button has it.
    pub fn activity_handle(&self) -> FocusHandle {
        self.activity_focus.clone()
    }

    /// Handle that contains the focus while the active panel (not the activity bar) has it.
    pub fn panel_handle(&self) -> FocusHandle {
        self.panel_focus.clone()
    }

    /// Focuses the activity bar's first button.
    pub fn focus_activity_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.activity_focus);
        window.focus_next();
        if !self.activity_focus.contains_focused(window, cx) {
            window.focus(&self.activity_focus);
        }
    }

    /// Focuses the active panel: the file tree, or the panel's first tab stop (its list or
    /// input), or the panel itself when it has none.
    pub fn focus_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active == SidebarPanel::Files && self.has_folder {
            let handle = self.file_tree.focus_handle(cx);
            window.focus(&handle);
            return;
        }
        window.focus(&self.panel_focus);
        window.focus_next();
        if !self.panel_focus.contains_focused(window, cx) {
            window.focus(&self.panel_focus);
        }
    }

    /// Up / Down move between the activity bar's buttons, Home / End jump to the ends.
    fn activity_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ks = &event.keystroke;
        if ks.modifiers.modified() {
            return;
        }
        let before = window.focused(cx);
        match ks.key.as_str() {
            "down" => window.focus_next(),
            "up" => window.focus_prev(),
            _ => return,
        }
        // Stay inside the bar: wrapping into other regions is F6's job.
        if !self.activity_focus.contains_focused(window, cx) {
            if let Some(before) = before {
                window.focus(&before);
            }
        }
        cx.stop_propagation();
    }

    /// Marks the window as having no folder open (empty states with Open
    /// Folder buttons instead of the file tree).
    pub fn set_has_folder(&mut self, has_folder: bool, cx: &mut Context<Self>) {
        self.has_folder = has_folder;
        cx.notify();
    }

    fn deploy_gear_menu(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu = super::app_menu::build_settings_menu(self.focus_handle.clone(), window, cx);
        self.gear_menu
            .deploy(menu, position, window, cx, |this: &mut Self| {
                &mut this.gear_menu
            });
        cx.notify();
    }

    /// The file tree.
    pub fn file_tree(&self) -> &Entity<TreeView<FsTreeDelegate>> {
        &self.file_tree
    }

    /// Active panel.
    pub fn active_panel(&self) -> SidebarPanel {
        self.active
    }

    /// Shows `panel`.
    pub fn set_active_panel(&mut self, panel: SidebarPanel, cx: &mut Context<Self>) {
        self.active = panel;
        cx.notify();
    }

    /// Installs a view for a panel (integration: changes, history, ...).
    pub fn set_panel_view(&mut self, panel: SidebarPanel, view: AnyView, cx: &mut Context<Self>) {
        self.panels.insert(panel, view);
        cx.notify();
    }

    fn render_activity_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let items = SidebarPanel::ALL.iter().map(|&panel| {
            let active = panel == self.active;
            let action = panel.action();
            let tooltip_action = action.boxed_clone();
            div()
                .relative()
                .flex()
                .justify_center()
                .w_full()
                .child(
                    IconButton::new(
                        SharedString::from(format!("activity-{}", panel.title())),
                        panel.icon(),
                    )
                    .icon_size(IconSize::Large)
                    .square(px(36.))
                    .color(if active {
                        ui.text.hsla()
                    } else {
                        ui.text_faint.hsla()
                    })
                    .tooltip(Tooltip::for_action(panel.title(), tooltip_action.as_ref()))
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(action.boxed_clone(), cx)
                    }),
                )
                .when(active, |el| {
                    el.child(
                        div()
                            .absolute()
                            .left_0()
                            .top(px(8.))
                            .bottom(px(8.))
                            .w(px(2.))
                            .rounded_r(px(2.))
                            .bg(ui.accent.hsla()),
                    )
                })
        });
        v_flex()
            .id("activity-bar")
            .track_focus(&self.activity_focus)
            .on_key_down(cx.listener(Self::activity_key))
            .h_full()
            .w(px(46.))
            .flex_none()
            .py(px(6.))
            .gap(px(4.))
            .items_center()
            .bg(ui.title_bar_bg.hsla())
            .border_r_1()
            .border_color(ui.border.hsla())
            .children(items)
            .child(div().flex_1())
            .child(
                IconButton::new(
                    "activity-appearance",
                    if cx.theme().appearance.is_dark() {
                        IconName::Sun
                    } else {
                        IconName::Moon
                    },
                )
                .icon_size(IconSize::Medium)
                .square(px(34.))
                .color(ui.text_faint.hsla())
                .tooltip(Tooltip::for_action("Toggle Light/Dark", &ToggleAppearance))
                .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleAppearance), cx)),
            )
            .child({
                let weak = cx.entity().downgrade();
                IconButton::new("activity-settings", IconName::Settings)
                    .icon_size(IconSize::Medium)
                    .square(px(34.))
                    .color(ui.text_faint.hsla())
                    .selected(self.gear_menu.is_open())
                    .tooltip(Tooltip::text("Settings and Help"))
                    .on_click(move |e, window, cx| {
                        let position = e.position();
                        let _ = weak.update(cx, |s, cx| s.deploy_gear_menu(position, window, cx));
                    })
            })
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let tree = self.file_tree.clone();
        let tree2 = self.file_tree.clone();
        h_flex()
            .h(metrics::tab_bar_height())
            .flex_none()
            .pl(px(14.))
            .pr(px(6.))
            .justify_between()
            .border_b_1()
            .border_color(ui.border_variant.hsla())
            .child(
                div()
                    .text_size(metrics::text_xs())
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui.text_muted.hsla())
                    .child(self.active.title().to_uppercase()),
            )
            .when(self.active == SidebarPanel::Files, |el| {
                el.child(
                    h_flex()
                        .gap(px(2.))
                        .child(
                            IconButton::new("files-open-folder", IconName::FolderPlus)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::for_action("Open Folder…", &OpenFolder))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(OpenFolder), cx)
                                }),
                        )
                        .when(self.has_folder, |el| {
                            el.child(
                                IconButton::new("files-reveal", IconName::Eye)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::for_action(
                                        "Reveal Active File",
                                        &RevealActiveFile,
                                    ))
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(Box::new(RevealActiveFile), cx)
                                    }),
                            )
                            .child(
                                IconButton::new("files-refresh", IconName::Refresh)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Refresh"))
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        tree.update(cx, |t, cx| t.refresh(cx));
                                        cx.emit(SidebarEvent::RefreshFiles);
                                    })),
                            )
                            .child(
                                IconButton::new("files-collapse", IconName::ChevronsDownUp)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Collapse All"))
                                    .on_click(move |_, _, cx| {
                                        tree2.update(cx, |t, cx| {
                                            t.collapse_all(cx);
                                            let root = t.delegate.root().to_path_buf();
                                            t.set_expanded(&root, true, cx);
                                        })
                                    }),
                            )
                        }),
                )
            })
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let (title, body) = self.active.empty_state(self.has_folder);
        let show_open = !self.has_folder && !matches!(self.active, SidebarPanel::Outline);
        v_flex()
            .size_full()
            .items_center()
            .pt(px(64.))
            .px(px(24.))
            .gap(px(8.))
            .child(
                Icon::new(self.active.icon())
                    .size(IconSize::Custom(px(28.)))
                    .color(ui.text_faint.hsla()),
            )
            .child(
                div()
                    .mt(px(6.))
                    .text_size(metrics::text())
                    .text_color(ui.text_muted.hsla())
                    .child(title),
            )
            .child(
                div()
                    .text_size(metrics::text_sm())
                    .text_color(ui.text_faint.hsla())
                    .text_center()
                    .child(body),
            )
            .when(show_open, |el| {
                el.child(
                    v_flex()
                        .mt(px(12.))
                        .w(px(200.))
                        .gap(px(6.))
                        .child(
                            Button::new("sidebar-open-folder", "Open Folder…")
                                .icon(IconName::FolderOpen)
                                .style(ButtonStyle::Accent)
                                .full_width()
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(OpenFolder), cx)
                                }),
                        )
                        .child(
                            Button::new("sidebar-open-recent", "Open Recent…")
                                .icon(IconName::History)
                                .style(ButtonStyle::Outlined)
                                .full_width()
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(OpenRecent), cx)
                                }),
                        ),
                )
            })
    }
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let content = match self.active {
            SidebarPanel::Files if self.has_folder => self.file_tree.clone().into_any_element(),
            SidebarPanel::Files => self.render_empty(cx).into_any_element(),
            SidebarPanel::Search if !self.has_folder => self.render_empty(cx).into_any_element(),
            panel => match self.panels.get(&panel) {
                Some(view) => view.clone().into_any_element(),
                None => self.render_empty(cx).into_any_element(),
            },
        };
        h_flex()
            .size_full()
            .items_start()
            .track_focus(&self.focus_handle)
            .child(self.render_activity_bar(cx))
            .child(
                v_flex()
                    .id("sidebar-panel")
                    .track_focus(&self.panel_focus)
                    .relative()
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .bg(ui.surface.hsla())
                    .child(self.render_header(cx))
                    .child(div().flex_1().min_h_0().pt(px(4.)).child(content))
                    .children(crate::a11y::region_ring(
                        self.panel_focus.contains_focused(window, cx),
                        cx,
                        px(0.),
                    )),
            )
            .children(self.gear_menu.render())
    }
}
