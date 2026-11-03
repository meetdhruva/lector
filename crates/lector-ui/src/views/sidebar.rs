//! The sidebar: an activity bar (Files, Changes, History, Branches, Search,
//! Outline) and the active panel.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{
    div, prelude::*, px, Action, AnyView, App, Entity, FocusHandle, Focusable, FontWeight,
    SharedString, Window,
};

use super::file_tree::FsTreeDelegate;
use crate::actions::workspace::*;
use crate::components::tree_view::TreeView;
use crate::components::{h_flex, metrics, v_flex, IconButton, Tooltip};
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
}

impl SidebarPanel {
    /// All panels in activity-bar order.
    pub const ALL: [SidebarPanel; 6] = [
        SidebarPanel::Files,
        SidebarPanel::Changes,
        SidebarPanel::History,
        SidebarPanel::Branches,
        SidebarPanel::Search,
        SidebarPanel::Outline,
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
        }
    }

    fn empty_state(self) -> (&'static str, &'static str) {
        match self {
            SidebarPanel::Files => ("No folder open", "Open a folder to browse its files."),
            SidebarPanel::Changes => (
                "No changes",
                "Uncommitted changes and reviews will appear here.",
            ),
            SidebarPanel::History => ("History", "Commits of the current branch will appear here."),
            SidebarPanel::Branches => ("Branches", "Local and remote branches, tags and stashes."),
            SidebarPanel::Search => ("Search", "Search across the project (ctrl-shift-f)."),
            SidebarPanel::Outline => ("No symbols", "Symbols of the active file will appear here."),
        }
    }
}

/// The sidebar view.
pub struct Sidebar {
    active: SidebarPanel,
    file_tree: Entity<TreeView<FsTreeDelegate>>,
    panels: HashMap<SidebarPanel, AnyView>,
    focus_handle: FocusHandle,
}

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
        }
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
            .child(
                IconButton::new("activity-settings", IconName::Settings)
                    .icon_size(IconSize::Medium)
                    .square(px(34.))
                    .color(ui.text_faint.hsla())
                    .tooltip(Tooltip::for_action("Settings", &OpenSettings))
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenSettings), cx)),
            )
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let tree = self.file_tree.clone();
        let tree2 = self.file_tree.clone();
        h_flex()
            .h(metrics::TAB_BAR_HEIGHT)
            .flex_none()
            .pl(px(14.))
            .pr(px(6.))
            .justify_between()
            .border_b_1()
            .border_color(ui.border_variant.hsla())
            .child(
                div()
                    .text_size(metrics::TEXT_XS)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui.text_muted.hsla())
                    .child(self.active.title().to_uppercase()),
            )
            .when(self.active == SidebarPanel::Files, |el| {
                el.child(
                    h_flex()
                        .gap(px(2.))
                        .child(
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
                                .on_click(move |_, _, cx| tree.update(cx, |t, cx| t.refresh(cx))),
                        )
                        .child(
                            IconButton::new("files-collapse", IconName::ChevronsDownUp)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Collapse All"))
                                .on_click(move |_, _, cx| {
                                    tree2.update(cx, |t, cx| t.collapse_all(cx))
                                }),
                        ),
                )
            })
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let (title, body) = self.active.empty_state();
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
                    .text_size(metrics::TEXT)
                    .text_color(ui.text_muted.hsla())
                    .child(title),
            )
            .child(
                div()
                    .text_size(metrics::TEXT_SM)
                    .text_color(ui.text_faint.hsla())
                    .text_center()
                    .child(body),
            )
    }
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let content = match self.active {
            SidebarPanel::Files => self.file_tree.clone().into_any_element(),
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
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .bg(ui.surface.hsla())
                    .child(self.render_header(cx))
                    .child(div().flex_1().min_h_0().pt(px(4.)).child(content)),
            )
    }
}
