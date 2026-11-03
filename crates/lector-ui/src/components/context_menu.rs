//! Right-click context menus: keyboard navigable, with separators, headers,
//! disabled items and key binding hints. Items dispatch actions.

use std::rc::Rc;

use gpui::{
    div, prelude::*, px, Action, App, Corner, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, MouseButton, Pixels, Point, SharedString, Window,
};

use super::popover::{surface, Popover};
use super::{binding_text_for_action, h_flex, metrics, v_flex};
use crate::actions::picker;
use crate::icons::{Icon, IconName, IconSize};
use crate::keymap::contexts;
use crate::theme::{ActiveTheme, ColorExt};

type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

/// One entry of a context menu.
pub enum MenuEntry {
    /// Dispatches an action (its key binding is shown).
    Action {
        /// Label.
        label: SharedString,
        /// Action to dispatch.
        action: Box<dyn Action>,
        /// Optional leading icon.
        icon: Option<IconName>,
        /// Greyed out and not selectable.
        disabled: bool,
    },
    /// Runs a closure.
    Handler {
        /// Label.
        label: SharedString,
        /// Callback.
        handler: Handler,
        /// Optional leading icon.
        icon: Option<IconName>,
        /// Greyed out and not selectable.
        disabled: bool,
    },
    /// A horizontal rule.
    Separator,
    /// A non-interactive section label.
    Header(SharedString),
}

impl MenuEntry {
    fn is_selectable(&self) -> bool {
        match self {
            MenuEntry::Action { disabled, .. } | MenuEntry::Handler { disabled, .. } => !disabled,
            _ => false,
        }
    }
}

/// A context menu view. Build with [`ContextMenu::build`].
pub struct ContextMenu {
    entries: Vec<MenuEntry>,
    selected: Option<usize>,
    focus_handle: FocusHandle,
    action_context: Option<FocusHandle>,
}

impl EventEmitter<DismissEvent> for ContextMenu {}

impl Focusable for ContextMenu {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ContextMenu {
    /// Builds a menu entity with the builder-style API.
    pub fn build(
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(ContextMenu, &mut Window, &mut Context<ContextMenu>) -> ContextMenu,
    ) -> Entity<ContextMenu> {
        cx.new(|cx| {
            let menu = ContextMenu {
                entries: Vec::new(),
                selected: None,
                focus_handle: cx.focus_handle(),
                action_context: None,
            };
            f(menu, window, cx)
        })
    }

    /// Element whose key context receives dispatched actions (defaults to
    /// whatever had focus when the menu opened, via bubbling).
    pub fn context(mut self, focus: FocusHandle) -> Self {
        self.action_context = Some(focus);
        self
    }

    /// Adds an action entry.
    pub fn action(mut self, label: impl Into<SharedString>, action: Box<dyn Action>) -> Self {
        self.entries.push(MenuEntry::Action {
            label: label.into(),
            action,
            icon: None,
            disabled: false,
        });
        self
    }

    /// Adds an action entry with an icon.
    pub fn icon_action(
        mut self,
        label: impl Into<SharedString>,
        icon: IconName,
        action: Box<dyn Action>,
    ) -> Self {
        self.entries.push(MenuEntry::Action {
            label: label.into(),
            action,
            icon: Some(icon),
            disabled: false,
        });
        self
    }

    /// Adds a disabled action entry.
    pub fn disabled_action(
        mut self,
        label: impl Into<SharedString>,
        action: Box<dyn Action>,
    ) -> Self {
        self.entries.push(MenuEntry::Action {
            label: label.into(),
            action,
            icon: None,
            disabled: true,
        });
        self
    }

    /// Adds a closure entry.
    pub fn entry(
        mut self,
        label: impl Into<SharedString>,
        handler: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        self.entries.push(MenuEntry::Handler {
            label: label.into(),
            handler: Rc::new(handler),
            icon: None,
            disabled: false,
        });
        self
    }

    /// Adds a closure entry with an icon.
    pub fn icon_entry(
        mut self,
        label: impl Into<SharedString>,
        icon: IconName,
        handler: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        self.entries.push(MenuEntry::Handler {
            label: label.into(),
            handler: Rc::new(handler),
            icon: Some(icon),
            disabled: false,
        });
        self
    }

    /// Adds a separator.
    pub fn separator(mut self) -> Self {
        self.entries.push(MenuEntry::Separator);
        self
    }

    /// Adds a header.
    pub fn header(mut self, title: impl Into<SharedString>) -> Self {
        self.entries.push(MenuEntry::Header(title.into()));
        self
    }

    /// Entries (for tests).
    pub fn entries(&self) -> &[MenuEntry] {
        &self.entries
    }

    /// Currently selected entry index.
    pub fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    fn select_next(&mut self, _: &picker::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn select_prev(&mut self, _: &picker::SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    /// Moves the selection by `delta` selectable entries, wrapping around.
    pub fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.entries.len() as isize;
        if n == 0 {
            return;
        }
        let mut ix = match self.selected {
            Some(ix) => ix as isize,
            None if delta > 0 => -1,
            None => n,
        };
        for _ in 0..n {
            ix = (ix + delta).rem_euclid(n);
            if self.entries[ix as usize].is_selectable() {
                self.selected = Some(ix as usize);
                cx.notify();
                return;
            }
        }
    }

    fn confirm(&mut self, _: &picker::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.selected {
            self.activate(ix, window, cx);
        }
    }

    fn cancel(&mut self, _: &picker::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    /// Runs entry `ix` and dismisses the menu.
    pub fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(ix) else {
            return;
        };
        if !entry.is_selectable() {
            return;
        }
        cx.emit(DismissEvent);
        match entry {
            MenuEntry::Action { action, .. } => {
                let action = action.boxed_clone();
                if let Some(context) = self.action_context.clone() {
                    window.focus(&context);
                    context.dispatch_action(action.as_ref(), window, cx);
                } else {
                    window.dispatch_action(action, cx);
                }
            }
            MenuEntry::Handler { handler, .. } => {
                let handler = handler.clone();
                handler(window, cx);
            }
            _ => {}
        }
    }
}

impl Render for ContextMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let entries: Vec<gpui::AnyElement> = self
            .entries
            .iter()
            .enumerate()
            .map(|(ix, entry)| match entry {
                MenuEntry::Separator => div()
                    .my(px(4.))
                    .h(px(1.))
                    .bg(ui.border_variant.hsla())
                    .into_any_element(),
                MenuEntry::Header(title) => div()
                    .px(px(10.))
                    .pt(px(6.))
                    .pb(px(2.))
                    .text_size(metrics::TEXT_XS)
                    .text_color(ui.text_faint.hsla())
                    .child(title.clone())
                    .into_any_element(),
                MenuEntry::Action {
                    label,
                    icon,
                    disabled,
                    action,
                } => {
                    let binding = binding_text_for_action(action.as_ref(), cx);
                    self.render_item(ix, label.clone(), *icon, *disabled, binding, &ui, cx)
                }
                MenuEntry::Handler {
                    label,
                    icon,
                    disabled,
                    ..
                } => self.render_item(ix, label.clone(), *icon, *disabled, None, &ui, cx),
            })
            .collect();
        surface(cx)
            .key_context(contexts::CONTEXT_MENU)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            .min_w(px(200.))
            .max_w(px(360.))
            .child(v_flex().p(px(4.)).children(entries))
    }
}

impl ContextMenu {
    #[allow(clippy::too_many_arguments)]
    fn render_item(
        &self,
        ix: usize,
        label: SharedString,
        icon: Option<IconName>,
        disabled: bool,
        binding: Option<SharedString>,
        ui: &lector_theme::UiColors,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let selected = self.selected == Some(ix);
        let fg = if disabled {
            ui.text_faint.hsla()
        } else {
            ui.text.hsla()
        };
        h_flex()
            .id(("menu-item", ix))
            .h(px(26.))
            .px(px(8.))
            .gap(px(8.))
            .rounded(metrics::RADIUS)
            .text_size(metrics::TEXT)
            .text_color(fg)
            .when(selected, |el| el.bg(ui.active.hsla()))
            .when(!disabled, |el| {
                el.cursor_pointer()
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if this.selected != Some(ix) {
                            this.selected = Some(ix);
                            cx.notify();
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| this.activate(ix, window, cx)),
                    )
            })
            .child(div().w(px(16.)).flex_none().when_some(icon, |el, icon| {
                el.child(Icon::new(icon).size(IconSize::Small).color(if disabled {
                    ui.text_faint.hsla()
                } else {
                    ui.text_muted.hsla()
                }))
            }))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(label),
            )
            .when_some(binding, |el, kb| {
                el.child(
                    div()
                        .pl(px(16.))
                        .text_size(metrics::TEXT_SM)
                        .text_color(ui.text_faint.hsla())
                        .child(kb),
                )
            })
            .into_any_element()
    }
}

/// Convenience: a [`Popover`] specialized for context menus.
pub struct ContextMenuState {
    popover: Popover<ContextMenu>,
}

impl Default for ContextMenuState {
    fn default() -> Self {
        ContextMenuState {
            popover: Popover::new(),
        }
    }
}

impl ContextMenuState {
    /// Opens `menu` at the mouse `position`.
    pub fn deploy<T: 'static>(
        &mut self,
        menu: Entity<ContextMenu>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<T>,
        state: impl Fn(&mut T) -> &mut ContextMenuState + 'static,
    ) {
        self.popover.show(
            menu,
            position,
            Corner::TopLeft,
            window,
            cx,
            move |this, _, cx| {
                state(this).popover.close();
                cx.notify();
            },
        );
    }

    /// Whether the menu is open.
    pub fn is_open(&self) -> bool {
        self.popover.is_open()
    }

    /// Element to render as a child of the owner.
    pub fn render(&self) -> Option<impl IntoElement> {
        self.popover.render()
    }
}
