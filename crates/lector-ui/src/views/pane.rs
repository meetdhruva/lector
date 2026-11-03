//! Panes hold tabbed items; pane groups arrange panes in resizable splits.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{
    div, prelude::*, px, AnyElement, AnyView, App, Axis, Entity, EventEmitter, FocusHandle,
    Focusable, MouseDownEvent, ScrollHandle, SharedString, WeakEntity, Window,
};

use crate::actions::workspace::*;
use crate::code_view::CodeView;
use crate::components::context_menu::{ContextMenu, ContextMenuState};
use crate::components::split::{resizable_split, SplitState};
use crate::components::tabs::{Tab, TabBar};
use crate::components::{v_flex, IconButton, Tooltip};
use crate::icons::{FileIcon, Icon, IconName, IconSize};
use crate::theme::{ActiveTheme, ColorExt};

/// Icon shown on a tab.
#[derive(Clone, Debug, PartialEq)]
pub enum TabIcon {
    /// Material file icon for this file name.
    File(SharedString),
    /// A UI icon.
    Ui(IconName),
}

/// Something that can live in a pane tab (code view, diff view, ...).
pub trait PaneItem: 'static {
    /// Tab title.
    fn title(&self, cx: &App) -> SharedString;
    /// Tab icon.
    fn icon(&self, cx: &App) -> TabIcon;
    /// Path of the underlying file, if any.
    fn path(&self, cx: &App) -> Option<PathBuf>;
    /// The view to render.
    fn view(&self) -> AnyView;
    /// Focus handle of the view.
    fn focus_handle(&self, cx: &App) -> FocusHandle;
    /// The code view, for items that are code views (status bar, go to line).
    fn code_view(&self) -> Option<Entity<CodeView>> {
        None
    }
    /// A new item showing the same content (for splits).
    fn clone_item(&self, window: &mut Window, cx: &mut App) -> Option<Box<dyn PaneItem>>;
}

/// A file opened in a [`CodeView`].
pub struct CodeItem {
    /// The view.
    pub view: Entity<CodeView>,
    /// Absolute path.
    pub path: Option<PathBuf>,
    /// Display title (file name).
    pub title: SharedString,
}

impl CodeItem {
    /// Item for `view` showing `path`.
    pub fn new(
        view: Entity<CodeView>,
        path: Option<PathBuf>,
        title: impl Into<SharedString>,
    ) -> Self {
        CodeItem {
            view,
            path,
            title: title.into(),
        }
    }
}

impl PaneItem for CodeItem {
    fn title(&self, _: &App) -> SharedString {
        self.title.clone()
    }

    fn icon(&self, _: &App) -> TabIcon {
        TabIcon::File(self.title.clone())
    }

    fn path(&self, _: &App) -> Option<PathBuf> {
        self.path.clone()
    }

    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }

    fn code_view(&self) -> Option<Entity<CodeView>> {
        Some(self.view.clone())
    }

    fn clone_item(&self, _window: &mut Window, cx: &mut App) -> Option<Box<dyn PaneItem>> {
        let source = self.view.read(cx).source().clone();
        let selection = self.view.read(cx).selection();
        let path = self.path.clone();
        let view = cx.new(|cx| {
            let mut v = CodeView::new(source, cx);
            v.set_path(path.clone());
            v.set_selection(selection, cx);
            v
        });
        Some(Box::new(CodeItem {
            view,
            path: self.path.clone(),
            title: self.title.clone(),
        }))
    }
}

struct PaneEntry {
    id: usize,
    item: Box<dyn PaneItem>,
    preview: bool,
}

/// Events emitted by a [`Pane`].
#[derive(Clone, Debug, PartialEq)]
pub enum PaneEvent {
    /// The pane gained focus or activated an item.
    Activated,
    /// The pane has no items left.
    Empty,
    /// A tab was closed (path, for reopening).
    ItemClosed(Option<PathBuf>),
    /// "Reveal in tree" was requested from a tab menu.
    RevealPath(PathBuf),
}

/// A tabbed container of items.
pub struct Pane {
    entries: Vec<PaneEntry>,
    active: usize,
    next_id: usize,
    focus_handle: FocusHandle,
    tab_scroll: ScrollHandle,
    context_menu: ContextMenuState,
    root: Option<PathBuf>,
    pub(crate) is_active_pane: bool,
}

impl EventEmitter<PaneEvent> for Pane {}

impl Focusable for Pane {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.active_item()
            .map(|item| item.focus_handle(cx))
            .unwrap_or_else(|| self.focus_handle.clone())
    }
}

impl Pane {
    /// Empty pane. `root` is used for relative paths in menus.
    pub fn new(root: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        Pane {
            entries: Vec::new(),
            active: 0,
            next_id: 0,
            focus_handle: cx.focus_handle(),
            tab_scroll: ScrollHandle::new(),
            context_menu: ContextMenuState::default(),
            root,
            is_active_pane: false,
        }
    }

    /// Number of items.
    pub fn item_count(&self) -> usize {
        self.entries.len()
    }

    /// The active item.
    pub fn active_item(&self) -> Option<&dyn PaneItem> {
        self.entries.get(self.active).map(|e| e.item.as_ref())
    }

    /// Index of the active item.
    pub fn active_index(&self) -> usize {
        self.active
    }

    /// Titles of all items (tests, menus).
    pub fn titles(&self, cx: &App) -> Vec<SharedString> {
        self.entries.iter().map(|e| e.item.title(cx)).collect()
    }

    /// Whether item `ix` is a preview tab.
    pub fn is_preview(&self, ix: usize) -> bool {
        self.entries.get(ix).is_some_and(|e| e.preview)
    }

    /// Index of the item showing `path`.
    pub fn index_for_path(&self, path: &Path, cx: &App) -> Option<usize> {
        self.entries
            .iter()
            .position(|e| e.item.path(cx).as_deref() == Some(path))
    }

    /// Adds an item (replacing the existing preview tab when `preview`) and
    /// activates it.
    pub fn add_item(
        &mut self,
        item: Box<dyn PaneItem>,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        let entry = PaneEntry { id, item, preview };
        let ix = if preview {
            if let Some(ix) = self.entries.iter().position(|e| e.preview) {
                self.entries[ix] = entry;
                ix
            } else {
                let ix = (self.active + 1).min(self.entries.len());
                self.entries.insert(ix, entry);
                ix
            }
        } else {
            let ix = (self.active + 1).min(self.entries.len());
            self.entries.insert(ix, entry);
            ix
        };
        self.activate(ix, true, window, cx);
    }

    /// Makes a preview tab permanent.
    pub fn pin(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(e) = self.entries.get_mut(ix) {
            e.preview = false;
            cx.notify();
        }
    }

    /// Activates item `ix`, optionally focusing it.
    pub fn activate(
        &mut self,
        ix: usize,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ix >= self.entries.len() {
            return;
        }
        self.active = ix;
        if focus {
            let handle = self.entries[ix].item.focus_handle(cx);
            window.focus(&handle);
        }
        // Keep the active tab visible.
        self.tab_scroll.scroll_to_item(ix);
        cx.emit(PaneEvent::Activated);
        cx.notify();
    }

    /// Activates the next (or previous) tab, wrapping.
    pub fn activate_relative(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.entries.len() as isize;
        if n == 0 {
            return;
        }
        let ix = (self.active as isize + delta).rem_euclid(n) as usize;
        self.activate(ix, true, window, cx);
    }

    /// Closes item `ix`.
    pub fn close(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.entries.len() {
            return;
        }
        let entry = self.entries.remove(ix);
        cx.emit(PaneEvent::ItemClosed(entry.item.path(cx)));
        if self.entries.is_empty() {
            self.active = 0;
            cx.emit(PaneEvent::Empty);
        } else {
            if self.active > ix || self.active >= self.entries.len() {
                self.active = self.active.saturating_sub(1);
            }
            let handle = self.entries[self.active].item.focus_handle(cx);
            window.focus(&handle);
        }
        cx.notify();
    }

    /// Closes the active item.
    pub fn close_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.active;
        self.close(ix, window, cx);
    }

    /// Closes all items except `keep`.
    pub fn close_others(&mut self, keep: usize, window: &mut Window, cx: &mut Context<Self>) {
        if keep >= self.entries.len() {
            return;
        }
        let keep_id = self.entries[keep].id;
        while let Some(ix) = self.entries.iter().position(|e| e.id != keep_id) {
            self.close(ix, window, cx);
        }
    }

    /// Closes all items right of `ix`.
    pub fn close_to_right(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        while self.entries.len() > ix + 1 {
            let last = self.entries.len() - 1;
            self.close(last, window, cx);
        }
    }

    fn relative_path(&self, path: &Path) -> String {
        match &self.root {
            Some(root) => path
                .strip_prefix(root)
                .unwrap_or(path)
                .display()
                .to_string(),
            None => path.display().to_string(),
        }
    }

    fn deploy_tab_menu(
        &mut self,
        ix: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = cx.entity().downgrade();
        let path = self.entries.get(ix).and_then(|e| e.item.path(cx));
        let rel = path.as_ref().map(|p| self.relative_path(p));
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let p1 = pane.clone();
            let p2 = pane.clone();
            let p3 = pane.clone();
            let mut menu = menu
                .entry("Close", move |window, cx| {
                    let _ = p1.update(cx, |pane, cx| pane.close(ix, window, cx));
                })
                .entry("Close Others", move |window, cx| {
                    let _ = p2.update(cx, |pane, cx| pane.close_others(ix, window, cx));
                })
                .entry("Close to the Right", move |window, cx| {
                    let _ = p3.update(cx, |pane, cx| pane.close_to_right(ix, window, cx));
                });
            if let Some(path) = path.clone() {
                let abs = path.display().to_string();
                let rel = rel.clone().unwrap_or_else(|| abs.clone());
                let reveal = pane.clone();
                menu = menu
                    .separator()
                    .entry("Copy Path", move |_, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(abs.clone()))
                    })
                    .entry("Copy Relative Path", move |_, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(rel.clone()))
                    })
                    .separator()
                    .entry("Reveal in File Tree", move |_, cx| {
                        let path = path.clone();
                        let _ = reveal.update(cx, |_, cx| cx.emit(PaneEvent::RevealPath(path)));
                    });
            }
            menu
        });
        self.context_menu
            .deploy(menu, event.position, window, cx, |this: &mut Self| {
                &mut this.context_menu
            });
        cx.notify();
    }

    fn tab_detail(&self, ix: usize, cx: &App) -> Option<SharedString> {
        // Disambiguate equal titles with the parent directory.
        let title = self.entries[ix].item.title(cx);
        let dupes = self
            .entries
            .iter()
            .filter(|e| e.item.title(cx) == title)
            .count();
        if dupes < 2 {
            return None;
        }
        let path = self.entries[ix].item.path(cx)?;
        let parent = path.parent()?.file_name()?.to_string_lossy().to_string();
        Some(parent.into())
    }
}

impl Render for Pane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let this = cx.entity().downgrade();
        let tabs: Vec<AnyElement> = (0..self.entries.len())
            .map(|ix| {
                let entry = &self.entries[ix];
                let title = entry.item.title(cx);
                let icon: AnyElement = match entry.item.icon(cx) {
                    TabIcon::File(name) => FileIcon::file(name).size(px(15.)).into_any_element(),
                    TabIcon::Ui(icon) => Icon::new(icon).size(IconSize::Small).into_any_element(),
                };
                let (t1, t2, t3) = (this.clone(), this.clone(), this.clone());
                Tab::new(("tab", entry.id), title)
                    .detail(self.tab_detail(ix, cx))
                    .icon(icon)
                    .active(ix == self.active)
                    .focused_pane(self.is_active_pane)
                    .preview(entry.preview)
                    .on_click(move |window, cx| {
                        let _ = t1.update(cx, |pane, cx| pane.activate(ix, true, window, cx));
                    })
                    .on_close(move |window, cx| {
                        let _ = t2.update(cx, |pane, cx| pane.close(ix, window, cx));
                    })
                    .on_secondary_mouse_down(move |event, window, cx| {
                        let _ =
                            t3.update(cx, |pane, cx| pane.deploy_tab_menu(ix, event, window, cx));
                    })
                    .into_any_element()
            })
            .collect();

        let content: AnyElement = match self.active_item() {
            Some(item) => item.view().into_any_element(),
            None => empty_pane(cx).into_any_element(),
        };

        v_flex()
            .size_full()
            .bg(ui.background.hsla())
            .track_focus(&self.focus_handle)
            .when(!self.entries.is_empty(), |el| {
                el.child(
                    TabBar::new("pane-tabs", self.tab_scroll.clone())
                        .tabs(tabs)
                        .end_child(
                            IconButton::new("split-right", IconName::Columns)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::for_action("Split Right", &SplitRight))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(SplitRight), cx)
                                }),
                        )
                        .end_child(
                            IconButton::new("pane-more", IconName::MoreHorizontal)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("More"))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(ToggleCommandPalette), cx)
                                }),
                        ),
                )
            })
            .child(div().flex_1().min_h_0().relative().child(content))
            .children(self.context_menu.render())
    }
}

fn empty_pane(cx: &App) -> impl IntoElement {
    let ui = &cx.theme().ui;
    let hint = |label: &'static str, action: &dyn gpui::Action| {
        let key = crate::components::binding_text_for_action(action, cx).unwrap_or_default();
        div()
            .flex()
            .justify_between()
            .gap(px(32.))
            .w(px(300.))
            .child(div().text_color(ui.text_muted.hsla()).child(label))
            .child(div().text_color(ui.text_faint.hsla()).child(key))
    };
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap(px(10.))
        .text_size(px(13.))
        .child(
            div()
                .mb(px(14.))
                .text_size(px(22.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(ui.text_faint.hsla())
                .child("Lector"),
        )
        .child(hint("Go to File", &ToggleFileFinder))
        .child(hint("Command Palette", &ToggleCommandPalette))
        .child(hint("Toggle Sidebar", &ToggleSidebar))
        .child(hint("Show Changes", &ShowChanges))
}

/// A tree of panes in resizable splits.
pub enum PaneNode {
    /// A single pane.
    Pane(Entity<Pane>),
    /// Children split along an axis.
    Split {
        /// Split state (axis + fractions).
        state: Entity<SplitState>,
        /// Children.
        children: Vec<PaneNode>,
    },
}

impl PaneNode {
    /// All panes in order.
    pub fn panes(&self) -> Vec<Entity<Pane>> {
        match self {
            PaneNode::Pane(p) => vec![p.clone()],
            PaneNode::Split { children, .. } => children.iter().flat_map(|c| c.panes()).collect(),
        }
    }

    /// Splits `target` along `axis`, inserting `new_pane` after it.
    pub fn split(
        &mut self,
        target: &Entity<Pane>,
        new_pane: Entity<Pane>,
        axis: Axis,
        cx: &mut App,
    ) -> bool {
        match self {
            PaneNode::Pane(p) if p == target => {
                let old = std::mem::replace(self, PaneNode::Pane(new_pane.clone()));
                let state = cx.new(|_| SplitState::new(axis, 2));
                *self = PaneNode::Split {
                    state,
                    children: vec![old, PaneNode::Pane(new_pane)],
                };
                true
            }
            PaneNode::Pane(_) => false,
            PaneNode::Split { state, children } => {
                let same_axis = state.read(cx).axis() == axis;
                if same_axis {
                    if let Some(ix) = children
                        .iter()
                        .position(|c| matches!(c, PaneNode::Pane(p) if p == target))
                    {
                        children.insert(ix + 1, PaneNode::Pane(new_pane));
                        state.update(cx, |s, _| s.insert_after(ix));
                        return true;
                    }
                }
                for child in children.iter_mut() {
                    if child.split(target, new_pane.clone(), axis, cx) {
                        return true;
                    }
                }
                false
            }
        }
    }

    /// Removes `target`; collapses single-child splits.
    pub fn remove(&mut self, target: &Entity<Pane>, cx: &mut App) -> bool {
        let PaneNode::Split { state, children } = self else {
            return false;
        };
        if let Some(ix) = children
            .iter()
            .position(|c| matches!(c, PaneNode::Pane(p) if p == target))
        {
            children.remove(ix);
            state.update(cx, |s, _| s.remove(ix));
        } else if !children.iter_mut().any(|c| c.remove(target, cx)) {
            return false;
        }
        if children.len() == 1 {
            if let Some(only) = children.pop() {
                *self = only;
            }
        }
        true
    }

    /// Renders the tree.
    pub fn render(&self, depth: usize, cx: &App) -> AnyElement {
        match self {
            PaneNode::Pane(p) => p.clone().into_any_element(),
            PaneNode::Split { state, children } => {
                let kids = children.iter().map(|c| c.render(depth + 1, cx)).collect();
                resizable_split(("pane-split", depth), state, kids, cx).into_any_element()
            }
        }
    }
}

/// Weak reference helper used by callbacks.
pub type WeakPane = WeakEntity<Pane>;

/// Shared constructor used by the workspace for new panes.
pub fn new_pane(root: Option<PathBuf>, cx: &mut App) -> Entity<Pane> {
    cx.new(|cx| Pane::new(root, cx))
}

/// Opens `item` in `pane`, or activates an existing tab for the same path.
pub fn open_in_pane(
    pane: &Entity<Pane>,
    item_path: Option<&Path>,
    make: impl FnOnce(&mut Window, &mut App) -> Box<dyn PaneItem>,
    preview: bool,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(path) = item_path {
        let existing = pane.read(cx).index_for_path(path, cx);
        if let Some(ix) = existing {
            pane.update(cx, |pane, cx| {
                if !preview {
                    pane.pin(ix, cx);
                }
                pane.activate(ix, true, window, cx);
            });
            return;
        }
    }
    let item = make(window, cx);
    pane.update(cx, |pane, cx| pane.add_item(item, preview, window, cx));
}

/// Alias kept for integration code that wants shared items.
pub type SharedItem = Rc<dyn PaneItem>;
