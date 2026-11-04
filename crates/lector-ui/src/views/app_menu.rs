//! The in-window menu bar (Linux and Windows): the shared [`crate::menus`]
//! model drawn in the title bar, VS Code/Zed style.
//!
//! * Click a title to open its dropdown; while one is open, hovering another
//!   title switches to it. `workspace::ShowAppMenu` (alt-f) opens the first menu.
//! * Keyboard: up/down/home/end move, right opens a submenu (or the next menu),
//!   left closes it (or goes to the previous menu), enter/space activates,
//!   escape closes.
//! * Items show the live key binding of their action and are disabled when no
//!   handler is available for the element that had focus when the menu opened.
//!   Activating an item refocuses that element and dispatches the action there.
//! * Narrow windows collapse the titles into a single "☰" menu of submenus.

use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    anchored, deferred, div, prelude::*, px, AnyElement, App, Corner, EventEmitter, FocusHandle,
    Focusable, KeyDownEvent, MouseButton, SharedString, Window,
};

use crate::components::context_menu::ContextMenu;
use crate::components::{binding_text_for_action, h_flex, metrics, v_flex};
use crate::icons::{Icon, IconName, IconSize};
use crate::menus::{menu_model, MenuEntrySpec, MenuSpec};
use crate::theme::{ActiveTheme, ColorExt};

/// Maximum number of recent projects listed in File → Open Recent.
const MAX_RECENT_IN_MENU: usize = 10;

/// Events emitted by the [`AppMenuBar`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppMenuEvent {
    /// A recent project was chosen in File → Open Recent.
    OpenRecent(PathBuf),
}

/// What an item does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuCommand {
    /// Dispatch the action with this name.
    Action(&'static str),
    /// Open a recent project.
    OpenRecent(PathBuf),
}

/// One rendered row of a dropdown (the model resolved against the live
/// keymap, focus and recent projects).
#[derive(Clone, Debug)]
pub enum MenuRow {
    /// A clickable item.
    Item {
        /// Label.
        label: SharedString,
        /// Secondary text (recent project paths).
        detail: Option<SharedString>,
        /// Display key binding.
        binding: Option<SharedString>,
        /// Whether it can be activated.
        enabled: bool,
        /// What it does.
        command: MenuCommand,
    },
    /// A horizontal rule.
    Separator,
    /// A nested menu.
    Submenu {
        /// Label.
        label: SharedString,
        /// Its rows.
        rows: Rc<Vec<MenuRow>>,
    },
    /// A disabled placeholder ("No Recent Projects").
    Placeholder(SharedString),
}

impl MenuRow {
    fn is_selectable(&self) -> bool {
        match self {
            MenuRow::Item { enabled, .. } => *enabled,
            MenuRow::Submenu { .. } => true,
            _ => false,
        }
    }

    fn submenu(&self) -> Option<&Rc<Vec<MenuRow>>> {
        match self {
            MenuRow::Submenu { rows, .. } => Some(rows),
            _ => None,
        }
    }
}

struct OpenState {
    /// Index into `titles` (always 0 in compact mode).
    menu: usize,
    /// Rows of every menu (resolved when the bar opened).
    all_rows: Vec<Rc<Vec<MenuRow>>>,
    /// Selected row per open level (level 0 = the dropdown).
    selection: Vec<Option<usize>>,
    /// Open levels (≥ 1). Level `l + 1` shows the submenu selected at level `l`.
    depth: usize,
    /// Focus to restore and dispatch actions to.
    previous_focus: Option<FocusHandle>,
}

/// The menu bar view.
pub struct AppMenuBar {
    model: Vec<MenuSpec>,
    open: Option<OpenState>,
    compact: bool,
    focus_handle: FocusHandle,
    /// Pointer over the titles or an open dropdown (outside clicks close).
    hovered_titles: bool,
    hovered_levels: Vec<bool>,
}

impl EventEmitter<AppMenuEvent> for AppMenuBar {}

impl Focusable for AppMenuBar {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl AppMenuBar {
    /// A closed menu bar over [`menu_model`].
    pub fn new(cx: &mut Context<Self>) -> Self {
        AppMenuBar {
            model: menu_model(),
            open: None,
            compact: false,
            focus_handle: cx.focus_handle(),
            hovered_titles: false,
            hovered_levels: Vec::new(),
        }
    }

    /// Whether a dropdown is open.
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Index of the open menu, if any.
    pub fn open_menu_index(&self) -> Option<usize> {
        self.open.as_ref().map(|o| o.menu)
    }

    /// Collapses the titles into one "☰" button (narrow windows).
    pub fn set_compact(&mut self, compact: bool, cx: &mut Context<Self>) {
        if self.compact != compact {
            self.compact = compact;
            self.open = None;
            cx.notify();
        }
    }

    /// Rows of the open dropdown (tests).
    pub fn open_rows(&self) -> Option<Rc<Vec<MenuRow>>> {
        let open = self.open.as_ref()?;
        open.all_rows.get(open.menu).cloned()
    }

    fn title_count(&self) -> usize {
        if self.compact {
            1
        } else {
            self.model.len()
        }
    }

    /// Opens menu `ix` (`select_first` for keyboard opening). Availability of
    /// every item is resolved now, against the currently focused element.
    pub fn open_menu(
        &mut self,
        ix: usize,
        select_first: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous_focus = match self.open.take() {
            Some(open) => open.previous_focus,
            None => window.focused(cx).filter(|f| *f != self.focus_handle),
        };
        let mut all_rows: Vec<Rc<Vec<MenuRow>>> = self
            .model
            .iter()
            .map(|menu| Rc::new(resolve_rows(&menu.entries, window, cx)))
            .collect();
        if self.compact {
            let rows = self
                .model
                .iter()
                .zip(all_rows.iter())
                .map(|(menu, rows)| MenuRow::Submenu {
                    label: menu.name.into(),
                    rows: rows.clone(),
                })
                .collect();
            all_rows = vec![Rc::new(rows)];
        }
        let ix = ix.min(all_rows.len().saturating_sub(1));
        let mut state = OpenState {
            menu: ix,
            all_rows,
            selection: vec![None],
            depth: 1,
            previous_focus,
        };
        if select_first {
            state.selection[0] = first_selectable(&state.all_rows[ix], 0, 1);
        }
        self.open = Some(state);
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Closes the open menu and restores focus.
    pub fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(open) = self.open.take() {
            if let Some(focus) = open.previous_focus {
                window.focus(&focus);
            }
            self.hovered_levels.clear();
            cx.notify();
        }
    }

    fn switch_to(&mut self, ix: usize, select_first: bool, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        if open.menu == ix && !select_first {
            return;
        }
        open.menu = ix;
        open.depth = 1;
        open.selection = vec![if select_first {
            first_selectable(&open.all_rows[ix], 0, 1)
        } else {
            None
        }];
        cx.notify();
    }

    /// Rows shown at `level` of the open menu.
    fn level_rows(&self, level: usize) -> Option<Rc<Vec<MenuRow>>> {
        let open = self.open.as_ref()?;
        let mut rows = open.all_rows.get(open.menu)?.clone();
        for l in 0..level {
            let sel = (*open.selection.get(l)?)?;
            rows = rows.get(sel)?.submenu()?.clone();
        }
        Some(rows)
    }

    fn select(&mut self, level: usize, ix: usize, open_sub: bool, cx: &mut Context<Self>) {
        let Some(rows) = self.level_rows(level) else {
            return;
        };
        let is_sub = rows.get(ix).is_some_and(|r| r.submenu().is_some());
        let Some(open) = self.open.as_mut() else {
            return;
        };
        open.selection.truncate(level + 1);
        open.selection.resize(level + 1, None);
        open.selection[level] = Some(ix);
        open.depth = level + 1;
        if is_sub && open_sub {
            open.selection.push(None);
            open.depth = level + 2;
        }
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else {
            return;
        };
        let level = open.depth - 1;
        let Some(rows) = self.level_rows(level) else {
            return;
        };
        let current = self
            .open
            .as_ref()
            .and_then(|o| o.selection.get(level).copied().flatten());
        let n = rows.len() as isize;
        if n == 0 {
            return;
        }
        let mut ix = match current {
            Some(ix) => ix as isize,
            None if delta > 0 => -1,
            None => n,
        };
        for _ in 0..n {
            ix = (ix + delta).rem_euclid(n);
            if rows[ix as usize].is_selectable() {
                self.select(level, ix as usize, false, cx);
                return;
            }
        }
    }

    fn select_edge(&mut self, last: bool, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else {
            return;
        };
        let level = open.depth - 1;
        let Some(rows) = self.level_rows(level) else {
            return;
        };
        let found = if last {
            (0..rows.len()).rev().find(|&i| rows[i].is_selectable())
        } else {
            (0..rows.len()).find(|&i| rows[i].is_selectable())
        };
        if let Some(ix) = found {
            self.select(level, ix, false, cx);
        }
    }

    fn enter_submenu_or_next(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else {
            return;
        };
        let level = open.depth - 1;
        let selected = open.selection.get(level).copied().flatten();
        let rows = self.level_rows(level);
        let sub = selected.and_then(|ix| rows.as_ref()?.get(ix)?.submenu().cloned());
        match (selected, sub) {
            (Some(ix), Some(sub_rows)) => {
                self.select(level, ix, true, cx);
                if let Some(open) = self.open.as_mut() {
                    let first = first_selectable(&sub_rows, 0, 1);
                    if let Some(slot) = open.selection.get_mut(level + 1) {
                        *slot = first;
                    }
                }
                cx.notify();
            }
            _ => {
                let n = self.title_count();
                let next = (self.open.as_ref().map_or(0, |o| o.menu) + 1) % n;
                self.switch_to(next, true, cx);
            }
        }
    }

    fn leave_submenu_or_prev(&mut self, cx: &mut Context<Self>) {
        let n = self.title_count();
        let Some(open) = self.open.as_mut() else {
            return;
        };
        if open.depth > 1 {
            open.depth -= 1;
            open.selection.truncate(open.depth);
            cx.notify();
        } else {
            let prev = (open.menu + n - 1) % n;
            self.switch_to(prev, true, cx);
        }
    }

    fn activate_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else {
            return;
        };
        let level = open.depth - 1;
        let Some(ix) = open.selection.get(level).copied().flatten() else {
            return;
        };
        self.activate(level, ix, window, cx);
    }

    fn activate(&mut self, level: usize, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rows) = self.level_rows(level) else {
            return;
        };
        match rows.get(ix) {
            Some(MenuRow::Item {
                enabled: true,
                command,
                ..
            }) => {
                let command = command.clone();
                let target = self.open.as_ref().and_then(|o| o.previous_focus.clone());
                self.close(window, cx);
                match command {
                    MenuCommand::Action(name) => {
                        let Ok(action) = cx.build_action(name, None) else {
                            return;
                        };
                        window.defer(cx, move |window, cx| match target {
                            Some(focus) => focus.dispatch_action(action.as_ref(), window, cx),
                            None => window.dispatch_action(action, cx),
                        });
                    }
                    MenuCommand::OpenRecent(path) => cx.emit(AppMenuEvent::OpenRecent(path)),
                }
            }
            Some(MenuRow::Submenu { rows: sub, .. }) => {
                let sub = sub.clone();
                self.select(level, ix, true, cx);
                if let Some(open) = self.open.as_mut() {
                    if let Some(slot) = open.selection.get_mut(level + 1) {
                        *slot = first_selectable(&sub, 0, 1);
                    }
                }
            }
            _ => {}
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.open.is_none() {
            return;
        }
        let ks = &event.keystroke;
        if ks.modifiers.control || ks.modifiers.platform || ks.modifiers.alt {
            if ks.key == "f" && ks.modifiers.alt && !self.compact {
                // alt-f again toggles the menu off.
                self.close(window, cx);
                cx.stop_propagation();
            }
            return;
        }
        match ks.key.as_str() {
            "down" => self.move_selection(1, cx),
            "up" => self.move_selection(-1, cx),
            "home" => self.select_edge(false, cx),
            "end" => self.select_edge(true, cx),
            "right" => self.enter_submenu_or_next(cx),
            "left" => self.leave_submenu_or_prev(cx),
            "enter" | "space" => self.activate_selected(window, cx),
            "escape" => {
                let depth = self.open.as_ref().map_or(1, |o| o.depth);
                if depth > 1 {
                    self.leave_submenu_or_prev(cx);
                } else {
                    self.close(window, cx);
                }
            }
            "tab" => {
                let delta = if ks.modifiers.shift { -1 } else { 1 };
                let n = self.title_count() as isize;
                let cur = self.open.as_ref().map_or(0, |o| o.menu) as isize;
                self.switch_to((cur + delta).rem_euclid(n) as usize, true, cx);
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn pointer_inside(&self) -> bool {
        self.hovered_titles || self.hovered_levels.iter().any(|h| *h)
    }

    fn set_level_hovered(&mut self, level: usize, hovered: bool) {
        if self.hovered_levels.len() <= level {
            self.hovered_levels.resize(level + 1, false);
        }
        self.hovered_levels[level] = hovered;
    }

    fn render_level(&self, level: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.open.as_ref()?;
        let rows = self.level_rows(level)?;
        let ui = cx.theme().ui.clone();
        let selected = open.selection.get(level).copied().flatten();
        let items: Vec<AnyElement> = rows
            .iter()
            .enumerate()
            .map(|(ix, row)| {
                let is_selected = selected == Some(ix);
                match row {
                    MenuRow::Separator => div()
                        .mx(px(6.))
                        .my(px(4.))
                        .h(px(1.))
                        .bg(ui.border_variant.hsla())
                        .into_any_element(),
                    MenuRow::Placeholder(label) => h_flex()
                        .h(px(26.))
                        .px(px(10.))
                        .text_color(ui.text_faint.hsla())
                        .child(label.clone())
                        .into_any_element(),
                    MenuRow::Item {
                        label,
                        detail,
                        binding,
                        enabled,
                        ..
                    } => {
                        let enabled = *enabled;
                        let fg = if enabled {
                            ui.text.hsla()
                        } else {
                            ui.text_faint.hsla()
                        };
                        h_flex()
                            .id(("app-menu-item", level * 1000 + ix))
                            .h(px(26.))
                            .pl(px(10.))
                            .pr(px(10.))
                            .gap(px(10.))
                            .rounded(metrics::RADIUS)
                            .text_color(fg)
                            .when(is_selected && enabled, |el| el.bg(ui.active.hsla()))
                            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                                let current = this
                                    .open
                                    .as_ref()
                                    .and_then(|o| o.selection.get(level).copied().flatten());
                                let depth = this.open.as_ref().map_or(1, |o| o.depth);
                                if current != Some(ix) || depth > level + 1 {
                                    this.select(level, ix, false, cx);
                                }
                            }))
                            .when(enabled, |el| {
                                el.cursor_pointer().on_mouse_up(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        this.activate(level, ix, window, cx)
                                    }),
                                )
                            })
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(px(8.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(label.clone())
                                    .when_some(detail.clone(), |el, d| {
                                        el.child(
                                            div()
                                                .min_w_0()
                                                .overflow_hidden()
                                                .text_ellipsis()
                                                .text_size(metrics::TEXT_SM)
                                                .text_color(ui.text_faint.hsla())
                                                .child(d),
                                        )
                                    }),
                            )
                            .when_some(binding.clone(), |el, kb| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .pl(px(24.))
                                        .text_size(metrics::TEXT_SM)
                                        .text_color(ui.text_faint.hsla())
                                        .child(kb),
                                )
                            })
                            .into_any_element()
                    }
                    MenuRow::Submenu { label, .. } => h_flex()
                        .id(("app-menu-sub", level * 1000 + ix))
                        .h(px(26.))
                        .pl(px(10.))
                        .pr(px(6.))
                        .gap(px(10.))
                        .rounded(metrics::RADIUS)
                        .text_color(ui.text.hsla())
                        .cursor_pointer()
                        .when(is_selected, |el| el.bg(ui.active.hsla()))
                        .on_mouse_move(cx.listener(move |this, _, _, cx| {
                            let depth = this.open.as_ref().map_or(1, |o| o.depth);
                            let current = this
                                .open
                                .as_ref()
                                .and_then(|o| o.selection.get(level).copied().flatten());
                            if current != Some(ix) || depth != level + 2 {
                                this.select(level, ix, true, cx);
                            }
                        }))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| this.select(level, ix, true, cx)),
                        )
                        .child(div().flex_1().whitespace_nowrap().child(label.clone()))
                        .child(
                            Icon::new(IconName::ChevronRight)
                                .size(IconSize::XSmall)
                                .color(ui.text_muted.hsla()),
                        )
                        .into_any_element(),
                }
            })
            .collect();
        Some(
            menu_surface(cx)
                .id(("app-menu-level", level))
                .occlude()
                .min_w(px(if level == 0 { 240. } else { 220. }))
                .max_w(px(520.))
                .text_size(metrics::TEXT)
                .on_hover(cx.listener(move |this, hovered: &bool, _, _| {
                    this.set_level_hovered(level, *hovered)
                }))
                .child(v_flex().p(px(4.)).children(items))
                .into_any_element(),
        )
    }

    /// The open dropdown and its open submenus, side by side; each submenu
    /// is offset so its first row lines up with the row that opened it.
    fn render_dropdown(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.open.as_ref()?;
        let depth = open.depth;
        let mut offsets = vec![px(0.)];
        for level in 0..depth.saturating_sub(1) {
            let rows = self.level_rows(level)?;
            let sel = open.selection.get(level).copied().flatten()?;
            let above: f32 = rows[..sel.min(rows.len())].iter().map(row_height).sum();
            offsets.push(offsets[level] + px(above));
        }
        let mut levels = Vec::new();
        for (level, offset) in offsets.into_iter().enumerate() {
            let surface = self.render_level(level, cx)?;
            levels.push(
                div()
                    .mt(offset)
                    .when(level > 0, |el| el.ml(px(-3.)))
                    .child(surface)
                    .into_any_element(),
            );
        }
        Some(h_flex().items_start().children(levels).into_any_element())
    }

    fn render_title(
        &self,
        ix: usize,
        label: SharedString,
        icon: Option<IconName>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ui = cx.theme().ui.clone();
        let is_open = self.open.as_ref().is_some_and(|o| o.menu == ix);
        let dropdown = if is_open {
            self.render_dropdown(cx)
        } else {
            None
        };
        div()
            .relative()
            .child(
                h_flex()
                    .id(("app-menu-title", ix))
                    .occlude()
                    .h(px(26.))
                    .px(px(if icon.is_some() { 6. } else { 8. }))
                    .rounded(metrics::RADIUS)
                    .text_size(metrics::TEXT)
                    .text_color(if is_open {
                        ui.text.hsla()
                    } else {
                        ui.title_bar_fg.hsla().opacity(0.82)
                    })
                    .when(is_open, |el| el.bg(ui.active.hsla()))
                    .when(!is_open, |el| {
                        el.hover(|s| s.bg(ui.hover.hsla()).text_color(ui.text.hsla()))
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            if this.open.as_ref().is_some_and(|o| o.menu == ix) {
                                this.close(window, cx);
                            } else if this.open.is_some() {
                                this.switch_to(ix, false, cx);
                            } else {
                                this.open_menu(ix, false, window, cx);
                            }
                        }),
                    )
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered && this.open.as_ref().is_some_and(|o| o.menu != ix) {
                            this.switch_to(ix, false, cx);
                        }
                    }))
                    .map(|el| match icon {
                        Some(icon) => el.child(
                            Icon::new(icon)
                                .size(IconSize::Medium)
                                .color(ui.title_bar_fg.hsla()),
                        ),
                        None => el.child(label),
                    }),
            )
            .when_some(dropdown, |el, dropdown| {
                el.child(
                    div().absolute().top_full().left_0().mt(px(3.)).child(
                        deferred(
                            anchored()
                                .anchor(Corner::TopLeft)
                                .snap_to_window_with_margin(px(4.))
                                .child(dropdown),
                        )
                        .with_priority(3),
                    ),
                )
            })
            .into_any_element()
    }
}

/// Resolves the model's entries against the keymap, the focused element and
/// the recent projects list.
fn resolve_rows(entries: &[MenuEntrySpec], window: &mut Window, cx: &mut App) -> Vec<MenuRow> {
    let mut rows: Vec<MenuRow> = Vec::new();
    for entry in entries {
        match entry {
            MenuEntrySpec::Action { label, action } => {
                let Ok(built) = cx.build_action(action, None) else {
                    continue;
                };
                let enabled = window.is_action_available(built.as_ref(), cx)
                    || cx.is_action_available(built.as_ref());
                rows.push(MenuRow::Item {
                    label: (*label).into(),
                    detail: None,
                    binding: binding_text_for_action(built.as_ref(), cx),
                    enabled,
                    command: MenuCommand::Action(action),
                });
            }
            MenuEntrySpec::Separator => {
                if !rows.is_empty() && !matches!(rows.last(), Some(MenuRow::Separator)) {
                    rows.push(MenuRow::Separator);
                }
            }
            MenuEntrySpec::Submenu { label, entries } => {
                let sub = resolve_rows(entries, window, cx);
                if !sub.is_empty() {
                    rows.push(MenuRow::Submenu {
                        label: (*label).into(),
                        rows: Rc::new(sub),
                    });
                }
            }
            MenuEntrySpec::RecentProjects => {
                rows.extend(recent_rows(window, cx));
            }
        }
    }
    while matches!(rows.last(), Some(MenuRow::Separator)) {
        rows.pop();
    }
    rows
}

fn recent_rows(window: &mut Window, cx: &mut App) -> Vec<MenuRow> {
    let recent = lector_config::RecentProjects::new().list();
    let mut rows: Vec<MenuRow> = recent
        .into_iter()
        .take(MAX_RECENT_IN_MENU)
        .map(|path| {
            let exists = path.is_dir();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            MenuRow::Item {
                label: name.into(),
                detail: Some(tildify(&path).into()),
                binding: None,
                enabled: exists,
                command: MenuCommand::OpenRecent(path),
            }
        })
        .collect();
    if rows.is_empty() {
        rows.push(MenuRow::Placeholder("No Recent Projects".into()));
    }
    rows.push(MenuRow::Separator);
    if let Ok(more) = cx.build_action("workspace::OpenRecent", None) {
        let enabled =
            window.is_action_available(more.as_ref(), cx) || cx.is_action_available(more.as_ref());
        rows.push(MenuRow::Item {
            label: "More…".into(),
            detail: None,
            binding: binding_text_for_action(more.as_ref(), cx),
            enabled,
            command: MenuCommand::Action("workspace::OpenRecent"),
        });
    }
    rows
}

/// The "Settings and Help" menu of the title bar and activity bar gear
/// buttons. Actions are dispatched from `context` (an element inside the
/// workspace).
pub fn build_settings_menu(
    context: FocusHandle,
    window: &mut Window,
    cx: &mut App,
) -> gpui::Entity<ContextMenu> {
    use crate::actions::workspace::*;
    ContextMenu::build(window, cx, move |menu, _, _| {
        menu.context(context)
            .icon_action("Settings", IconName::Settings, Box::new(OpenSettings))
            .icon_action(
                "Keyboard Shortcuts",
                IconName::Keyboard,
                Box::new(ShowKeybindings),
            )
            .icon_action("Select Theme…", IconName::Palette, Box::new(SelectTheme))
            .action("Toggle Light/Dark", Box::new(ToggleAppearance))
            .separator()
            .icon_action(
                "Command Palette",
                IconName::Command,
                Box::new(ToggleCommandPalette),
            )
            .icon_action("Welcome", IconName::Sparkles, Box::new(ShowWelcome))
            .icon_action(
                "Documentation",
                IconName::BookOpen,
                Box::new(OpenDocumentation),
            )
            .icon_action("Report an Issue…", IconName::Bug, Box::new(ReportIssue))
            .separator()
            .icon_action("About Lector", IconName::Info, Box::new(About))
    })
}

/// `~/src/lector` for paths under the home directory.
pub fn tildify(path: &std::path::Path) -> String {
    let home =
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    if let Some(home) = home.filter(|h| !h.as_os_str().is_empty()) {
        if let Ok(rest) = path.strip_prefix(&home) {
            let sep = std::path::MAIN_SEPARATOR;
            return if rest.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~{sep}{}", rest.display())
            };
        }
    }
    path.display().to_string()
}

/// Rendered height of a row (keep in sync with `render_level`).
fn row_height(row: &MenuRow) -> f32 {
    match row {
        MenuRow::Separator => 9.,
        _ => 26.,
    }
}

/// The dropdown surface (like `popover::surface`, without clipping).
fn menu_surface(cx: &App) -> gpui::Div {
    let ui = &cx.theme().ui;
    div()
        .bg(ui.elevated.hsla())
        .border_1()
        .border_color(ui.border.hsla())
        .rounded(metrics::RADIUS_LG)
        .shadow_lg()
}

fn first_selectable(rows: &[MenuRow], from: usize, step: usize) -> Option<usize> {
    (from..rows.len())
        .step_by(step.max(1))
        .find(|&i| rows[i].is_selectable())
}

impl Render for AppMenuBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let titles: Vec<AnyElement> = if self.compact {
            vec![self.render_title(0, "Menu".into(), Some(IconName::Menu), cx)]
        } else {
            let names: Vec<&'static str> = self.model.iter().map(|m| m.name).collect();
            names
                .into_iter()
                .enumerate()
                .map(|(ix, name)| self.render_title(ix, name.into(), None, cx))
                .collect()
        };
        h_flex()
            .id("app-menu-bar")
            .key_context("AppMenu")
            .track_focus(&self.focus_handle)
            .flex_none()
            .gap(px(1.))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_hover(cx.listener(|this, hovered: &bool, _, _| this.hovered_titles = *hovered))
            .when(self.open.is_some(), |el| {
                el.on_mouse_down_out(cx.listener(|this, _, window, cx| {
                    if !this.pointer_inside() {
                        this.close(window, cx);
                    }
                }))
            })
            .children(titles)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tildify_home_paths() {
        if let Some(home) = std::env::var_os("HOME").filter(|_| !cfg!(windows)) {
            let home = PathBuf::from(home);
            assert_eq!(tildify(&home.join("src/x")), "~/src/x");
        }
        assert_eq!(tildify(std::path::Path::new("/opt/x")), "/opt/x");
    }
}
