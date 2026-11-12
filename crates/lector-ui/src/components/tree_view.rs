//! A virtualized, lazily-loaded tree (file explorer, outline, change lists).
//!
//! Data comes from a [`TreeDelegate`]; the view keeps expansion state,
//! selection and a flattened list of visible rows rendered by `uniform_list`.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::ops::Range;

use crate::scale::px;
use gpui::{
    div, prelude::*, AnyElement, App, ClickEvent, EventEmitter, FocusHandle, Focusable, Hsla,
    MouseButton, MouseDownEvent, SharedString, Window,
};

use super::context_menu::{ContextMenu, ContextMenuState};
use super::list::{virtual_list, VirtualListState};
use super::{h_flex, metrics};
use crate::actions::picker::{Confirm, SelectNext, SelectPrev};
use crate::actions::tree::{CollapseSelected, ExpandSelected};
use crate::icons::{FileIcon, Icon, IconName, IconSize};
use crate::keymap::contexts;
use crate::theme::{ActiveTheme, ColorExt};

/// Icon of a tree node.
#[derive(Clone, Debug, PartialEq)]
pub enum TreeIcon {
    /// Material file icon for this file name.
    File(SharedString),
    /// Material folder icon for this folder name (open variant when expanded).
    Folder(SharedString),
    /// Material root-folder icon.
    RootFolder,
    /// A tinted UI icon.
    Ui(IconName, Option<Hsla>),
    /// No icon.
    None,
}

/// One node as supplied by the delegate.
#[derive(Clone, Debug)]
pub struct TreeNode<Id> {
    /// Stable identity (e.g. a path).
    pub id: Id,
    /// Display label.
    pub label: SharedString,
    /// Whether the node can have children (shows a chevron).
    pub is_branch: bool,
    /// Icon.
    pub icon: TreeIcon,
    /// Dimmed trailing text.
    pub detail: Option<SharedString>,
    /// Label color override (e.g. git status).
    pub color: Option<Hsla>,
    /// Small trailing badge (e.g. "M"), with its color.
    pub badge: Option<(SharedString, Hsla)>,
}

impl<Id> TreeNode<Id> {
    /// A leaf node.
    pub fn leaf(id: Id, label: impl Into<SharedString>, icon: TreeIcon) -> Self {
        TreeNode {
            id,
            label: label.into(),
            is_branch: false,
            icon,
            detail: None,
            color: None,
            badge: None,
        }
    }

    /// A branch node.
    pub fn branch(id: Id, label: impl Into<SharedString>, icon: TreeIcon) -> Self {
        TreeNode {
            is_branch: true,
            ..Self::leaf(id, label, icon)
        }
    }
}

/// Supplies tree data and per-node menus.
pub trait TreeDelegate: 'static {
    /// Node identity.
    type Id: Clone + Eq + Hash + 'static;

    /// Children of `parent` (`None` = roots). Called lazily on expansion and
    /// cached until [`TreeView::refresh`].
    fn children(&mut self, parent: Option<&Self::Id>, cx: &mut App) -> Vec<TreeNode<Self::Id>>;

    /// Optional context menu for a node, typically built with
    /// [`ContextMenu::build`].
    fn context_menu(
        &mut self,
        _node: &TreeNode<Self::Id>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<gpui::Entity<ContextMenu>> {
        None
    }

    /// Optional context menu for the empty area below the nodes (e.g. actions on the
    /// project root).
    fn background_context_menu(
        &mut self,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<gpui::Entity<ContextMenu>> {
        None
    }
}

/// Events emitted by [`TreeView`].
#[derive(Clone, Debug)]
pub enum TreeViewEvent<Id> {
    /// Selection changed.
    Selected(Id),
    /// A leaf was opened: `preview` for single click, `false` for double
    /// click / Enter.
    Confirmed {
        /// The node.
        id: Id,
        /// Single-click (preview) open.
        preview: bool,
    },
}

#[derive(Clone)]
struct FlatEntry<Id> {
    node: TreeNode<Id>,
    depth: usize,
    expanded: bool,
    parent: Option<usize>,
}

/// The tree view.
pub struct TreeView<D: TreeDelegate> {
    /// The delegate.
    pub delegate: D,
    children_cache: HashMap<Option<D::Id>, Vec<TreeNode<D::Id>>>,
    expanded: HashSet<D::Id>,
    flat: Vec<FlatEntry<D::Id>>,
    selected: Option<usize>,
    list: VirtualListState,
    focus_handle: FocusHandle,
    context_menu: ContextMenuState,
    id: SharedString,
    focused: bool,
}

impl<D: TreeDelegate> EventEmitter<TreeViewEvent<D::Id>> for TreeView<D> {}

impl<D: TreeDelegate> Focusable for TreeView<D> {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

const INDENT: f32 = 14.;
const ROW_PAD: f32 = 8.;

impl<D: TreeDelegate> TreeView<D> {
    /// Creates the tree and loads its roots.
    pub fn new(id: impl Into<SharedString>, delegate: D, cx: &mut Context<Self>) -> Self {
        let mut this = TreeView {
            delegate,
            children_cache: HashMap::new(),
            expanded: HashSet::new(),
            flat: Vec::new(),
            selected: None,
            list: VirtualListState::new(),
            focus_handle: cx.focus_handle().tab_stop(true),
            context_menu: ContextMenuState::default(),
            id: id.into(),
            focused: false,
        };
        this.rebuild(cx);
        this
    }

    /// Number of visible rows.
    pub fn visible_count(&self) -> usize {
        self.flat.len()
    }

    /// Labels of visible rows with their depth (for tests and debugging).
    pub fn visible_labels(&self) -> Vec<(usize, SharedString)> {
        self.flat
            .iter()
            .map(|e| (e.depth, e.node.label.clone()))
            .collect()
    }

    /// Selected node id.
    pub fn selected_id(&self) -> Option<&D::Id> {
        self.selected
            .and_then(|ix| self.flat.get(ix))
            .map(|e| &e.node.id)
    }

    /// Whether `id` is expanded.
    pub fn is_expanded(&self, id: &D::Id) -> bool {
        self.expanded.contains(id)
    }

    /// Drops cached children and reloads (expansion state is kept).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let selected = self.selected_id().cloned();
        self.children_cache.clear();
        self.rebuild(cx);
        if let Some(id) = selected {
            self.selected = self.flat.iter().position(|e| e.node.id == id);
        }
        cx.notify();
    }

    fn children_of(&mut self, parent: Option<&D::Id>, cx: &mut App) -> Vec<TreeNode<D::Id>> {
        let key = parent.cloned();
        if let Some(children) = self.children_cache.get(&key) {
            return children.clone();
        }
        let children = self.delegate.children(parent, cx);
        self.children_cache.insert(key, children.clone());
        children
    }

    fn rebuild(&mut self, cx: &mut App) {
        let mut flat = Vec::new();
        let roots = self.children_of(None, cx);
        let mut stack: Vec<(TreeNode<D::Id>, usize, Option<usize>)> =
            roots.into_iter().rev().map(|n| (n, 0, None)).collect();
        while let Some((node, depth, parent)) = stack.pop() {
            let expanded = node.is_branch && self.expanded.contains(&node.id);
            let ix = flat.len();
            let children = if expanded {
                self.children_of(Some(&node.id), cx)
            } else {
                Vec::new()
            };
            flat.push(FlatEntry {
                node,
                depth,
                expanded,
                parent,
            });
            for child in children.into_iter().rev() {
                stack.push((child, depth + 1, Some(ix)));
            }
        }
        self.flat = flat;
        if let Some(sel) = self.selected {
            if sel >= self.flat.len() {
                self.selected = self.flat.len().checked_sub(1);
            }
        }
    }

    /// Expands or collapses `id`.
    pub fn set_expanded(&mut self, id: &D::Id, expanded: bool, cx: &mut Context<Self>) {
        let selected = self.selected_id().cloned();
        if expanded {
            self.expanded.insert(id.clone());
        } else {
            self.expanded.remove(id);
        }
        self.rebuild(cx);
        if let Some(sel) = selected {
            self.selected = self.flat.iter().position(|e| e.node.id == sel);
        }
        cx.notify();
    }

    /// Toggles expansion of `id`.
    pub fn toggle_expanded(&mut self, id: &D::Id, cx: &mut Context<Self>) {
        let expanded = self.expanded.contains(id);
        self.set_expanded(id, !expanded, cx);
    }

    /// Collapses everything.
    pub fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.expanded.clear();
        self.rebuild(cx);
        self.selected = None;
        cx.notify();
    }

    /// Expands `ancestors` (outermost first) and selects `target`, scrolling
    /// it into view.
    pub fn reveal(&mut self, ancestors: &[D::Id], target: &D::Id, cx: &mut Context<Self>) {
        for a in ancestors {
            self.expanded.insert(a.clone());
        }
        self.rebuild(cx);
        if let Some(ix) = self.flat.iter().position(|e| &e.node.id == target) {
            self.select_index(ix, cx);
            self.list.center(ix);
        }
        cx.notify();
    }

    /// Selects row `ix`.
    pub fn select_index(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.flat.len() {
            self.selected = Some(ix);
            self.list.reveal(ix);
            cx.emit(TreeViewEvent::Selected(self.flat[ix].node.id.clone()));
            cx.notify();
        }
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let next = match self.selected {
            Some(ix) => (ix + 1).min(self.flat.len().saturating_sub(1)),
            None => 0,
        };
        self.select_index(next, cx);
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        let prev = self.selected.map(|ix| ix.saturating_sub(1)).unwrap_or(0);
        self.select_index(prev, cx);
    }

    fn expand_selected(&mut self, _: &ExpandSelected, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.selected else { return };
        let entry = &self.flat[ix];
        if !entry.node.is_branch {
            return;
        }
        if entry.expanded {
            if ix + 1 < self.flat.len() && self.flat[ix + 1].parent == Some(ix) {
                self.select_index(ix + 1, cx);
            }
        } else {
            let id = entry.node.id.clone();
            self.set_expanded(&id, true, cx);
        }
    }

    fn collapse_selected(&mut self, _: &CollapseSelected, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.selected else { return };
        let entry = &self.flat[ix];
        if entry.node.is_branch && entry.expanded {
            let id = entry.node.id.clone();
            self.set_expanded(&id, false, cx);
        } else if let Some(parent) = entry.parent {
            self.select_index(parent, cx);
        }
    }

    fn confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.selected {
            self.activate(ix, false, cx);
        }
    }

    fn activate(&mut self, ix: usize, preview: bool, cx: &mut Context<Self>) {
        let Some(entry) = self.flat.get(ix) else {
            return;
        };
        if entry.node.is_branch {
            let id = entry.node.id.clone();
            self.toggle_expanded(&id, cx);
        } else {
            cx.emit(TreeViewEvent::Confirmed {
                id: entry.node.id.clone(),
                preview,
            });
        }
    }

    fn on_row_click(
        &mut self,
        ix: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        self.select_index(ix, cx);
        let double = event.click_count() >= 2;
        let is_branch = self.flat.get(ix).is_some_and(|e| e.node.is_branch);
        if is_branch {
            if !double {
                self.activate(ix, true, cx);
            }
        } else {
            self.activate(ix, !double, cx);
        }
    }

    fn deploy_context_menu(
        &mut self,
        ix: usize,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_index(ix, cx);
        let Some(node) = self.flat.get(ix).map(|e| e.node.clone()) else {
            return;
        };
        let Some(menu) = self.delegate.context_menu(&node, window, cx) else {
            return;
        };
        self.context_menu
            .deploy(menu, position, window, cx, |this: &mut Self| {
                &mut this.context_menu
            });
        cx.notify();
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let focused = self.focused;
        let selected_parent = self
            .selected
            .and_then(|s| self.flat.get(s))
            .and_then(|e| e.parent);
        let mut rows = Vec::with_capacity(range.len());
        for ix in range {
            let Some(entry) = self.flat.get(ix) else {
                continue;
            };
            let node = &entry.node;
            let selected = self.selected == Some(ix);
            let hover = ui.hover.hsla();
            let indent = px(ROW_PAD + INDENT * entry.depth as f32);

            // Indent guides: one line per ancestor level, highlighting the
            // level that contains the selection.
            let mut guides = Vec::new();
            let mut ancestor = entry.parent;
            let mut level = entry.depth;
            while let Some(a) = ancestor {
                level -= 1;
                let active = selected_parent == Some(a) && !selected || self.selected == Some(a);
                let color = if active {
                    ui.indent_guide_active.hsla()
                } else {
                    ui.indent_guide.hsla()
                };
                guides.push(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(ROW_PAD + INDENT * level as f32 + 7.))
                        .w(px(1.))
                        .bg(color),
                );
                ancestor = self.flat.get(a).and_then(|e| e.parent);
            }

            let chevron = if node.is_branch {
                Icon::new(if entry.expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(IconSize::XSmall)
                .color(ui.text_faint.hsla())
                .into_any_element()
            } else {
                div().into_any_element()
            };
            let icon: AnyElement = match &node.icon {
                TreeIcon::File(name) => FileIcon::file(name.clone()).into_any_element(),
                TreeIcon::Folder(name) => {
                    FileIcon::folder(name.clone(), entry.expanded, false).into_any_element()
                }
                TreeIcon::RootFolder => {
                    FileIcon::folder("", entry.expanded, true).into_any_element()
                }
                TreeIcon::Ui(icon, color) => Icon::new(*icon)
                    .size(IconSize::Small)
                    .color(color.unwrap_or(ui.text_muted.hsla()))
                    .into_any_element(),
                TreeIcon::None => div().into_any_element(),
            };
            let label_color = node.color.unwrap_or(ui.text.hsla());
            let row = h_flex()
                .id(("tree-row", ix))
                .relative()
                .h(metrics::row_height())
                .w_full()
                .pl(indent)
                .pr(px(8.))
                .gap(px(4.))
                .text_size(metrics::text())
                .text_color(label_color)
                .cursor_pointer()
                .when(selected, |el| {
                    el.bg(if focused {
                        ui.active.hsla()
                    } else {
                        ui.selection_inactive.hsla()
                    })
                })
                .when(!selected, |el| el.hover(move |s| s.bg(hover)))
                .children(guides)
                .children(crate::a11y::region_ring(selected && focused, cx, px(3.)))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .w(px(14.))
                        .child(chevron),
                )
                .child(icon)
                .child(
                    div()
                        .ml(px(2.))
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(node.label.clone()),
                )
                .when_some(node.detail.clone(), |el, d| {
                    el.child(
                        div()
                            .flex_none()
                            .text_size(metrics::text_sm())
                            .text_color(ui.text_faint.hsla())
                            .child(d),
                    )
                })
                .when_some(node.badge.clone(), |el, (badge, color)| {
                    el.child(
                        div()
                            .flex_none()
                            .w(px(14.))
                            .text_size(metrics::text_xs())
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(color)
                            .child(badge),
                    )
                })
                .on_click(cx.listener(move |this, event, window, cx| {
                    this.on_row_click(ix, event, window, cx)
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.deploy_context_menu(ix, event.position, window, cx)
                    }),
                );
            rows.push(row.into_any_element());
        }
        rows
    }
}

impl<D: TreeDelegate> Render for TreeView<D> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.focused = self.focus_handle.contains_focused(window, cx);
        let count = self.flat.len();
        let list = virtual_list(
            self.id.clone(),
            count,
            &self.list,
            cx,
            cx.processor(|this, range, window, cx| this.render_rows(range, window, cx)),
        );
        div()
            .key_context(contexts::TREE_VIEW)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::expand_selected))
            .on_action(cx.listener(Self::collapse_selected))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(
                |this, _: &crate::actions::workspace::ShowContextMenu, window, cx| {
                    match this.selected {
                        Some(ix) => {
                            let position = this.list.row_anchor(ix, metrics::row_height());
                            this.deploy_context_menu(ix, position, window, cx)
                        }
                        None => cx.propagate(),
                    }
                },
            ))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    let Some(menu) = this.delegate.background_context_menu(window, cx) else {
                        return;
                    };
                    this.context_menu.deploy(
                        menu,
                        event.position,
                        window,
                        cx,
                        |this: &mut Self| &mut this.context_menu,
                    );
                    cx.notify();
                }),
            )
            .size_full()
            .child(list)
            .children(self.context_menu.render())
    }
}
