//! Tab bar widgets: preview (italic) tabs, close buttons, middle-click close,
//! right-click menus and horizontal overflow scrolling.

use std::rc::Rc;

use gpui::{
    div, prelude::*, px, AnyElement, App, ElementId, MouseButton, MouseDownEvent, ScrollHandle,
    SharedString, Window,
};

use super::{h_flex, metrics};
use crate::icons::{Icon, IconName, IconSize};
use crate::theme::{ActiveTheme, ColorExt};

type Handler = Rc<dyn Fn(&mut Window, &mut App)>;
type MouseHandler = Rc<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>;

/// One tab.
#[derive(IntoElement)]
pub struct Tab {
    id: ElementId,
    title: SharedString,
    detail: Option<SharedString>,
    icon: Option<AnyElement>,
    active: bool,
    focused_pane: bool,
    preview: bool,
    dirty: bool,
    read_only: bool,
    on_click: Option<Handler>,
    on_close: Option<Handler>,
    on_secondary: Option<MouseHandler>,
}

impl Tab {
    /// New tab.
    pub fn new(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Self {
        Tab {
            id: id.into(),
            title: title.into(),
            detail: None,
            icon: None,
            active: false,
            focused_pane: true,
            preview: false,
            dirty: false,
            read_only: false,
            on_click: None,
            on_close: None,
            on_secondary: None,
        }
    }

    /// Disambiguating detail (e.g. parent directory) shown dimmed.
    pub fn detail(mut self, detail: Option<SharedString>) -> Self {
        self.detail = detail;
        self
    }

    /// Leading icon.
    pub fn icon(mut self, icon: impl IntoElement) -> Self {
        self.icon = Some(icon.into_any_element());
        self
    }

    /// Whether this is the pane's active tab.
    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    /// Whether the containing pane is focused (accent indicator).
    pub fn focused_pane(mut self, focused: bool) -> Self {
        self.focused_pane = focused;
        self
    }

    /// Preview tabs render in italics and are replaced by the next preview.
    pub fn preview(mut self, preview: bool) -> Self {
        self.preview = preview;
        self
    }

    /// Unsaved changes: a dot replaces the close button until hovered.
    pub fn dirty(mut self, dirty: bool) -> Self {
        self.dirty = dirty;
        self
    }

    /// Editing is blocked: a small lock after the title.
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Click (activate).
    pub fn on_click(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(f));
        self
    }

    /// Close button / middle click.
    pub fn on_close(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_close = Some(Rc::new(f));
        self
    }

    /// Right click.
    pub fn on_secondary_mouse_down(
        mut self,
        f: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_secondary = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Tab {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let ui = &cx.theme().ui;
        let fg = if self.active {
            ui.tab_active_fg.hsla()
        } else {
            ui.tab_inactive_fg.hsla()
        };
        let bg = if self.active {
            ui.tab_active_bg.hsla()
        } else {
            ui.tab_inactive_bg.hsla()
        };
        let hover = ui.hover.hsla();
        let border = ui.border.hsla();
        let indicator = if self.focused_pane {
            ui.accent.hsla()
        } else {
            ui.text_faint.hsla()
        };
        let close_id: ElementId =
            (SharedString::from(format!("{:?}-close", self.id)), 0usize).into();
        let group: SharedString = format!("tab-{:?}", self.id).into();
        let active = self.active;

        let dirty = self.dirty;
        let close = self.on_close.clone().map(|on_close| {
            let x = div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .invisible()
                .when(active && !dirty, |el| el.visible())
                .group_hover(group.clone(), |s| s.visible())
                .child(Icon::new(IconName::Close).size(IconSize::XSmall).color(fg));
            let dot = div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .when(!dirty, |el| el.invisible())
                .group_hover(group.clone(), |s| s.invisible())
                .child(div().size(px(8.)).rounded_full().bg(fg));
            div()
                .id(close_id)
                .relative()
                .flex_none()
                .size(px(18.))
                .rounded(px(4.))
                .hover(move |s| s.bg(hover))
                .child(dot)
                .child(x)
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    on_close(window, cx)
                })
        });
        let lock = self.read_only.then(|| {
            Icon::new(IconName::Lock)
                .size(IconSize::XSmall)
                .color(ui.text_faint.hsla())
        });

        let mut tab = h_flex()
            .id(self.id)
            .group(group)
            .relative()
            .flex_none()
            .h_full()
            .pl(px(12.))
            .pr(px(6.))
            .gap(px(6.))
            .bg(bg)
            .border_r_1()
            // Inactive tabs carry the bar's bottom border; the active tab
            // omits it so it merges with the editor below.
            .when(!active, |el| el.border_b_1())
            .border_color(border)
            .text_size(metrics::TEXT)
            .text_color(fg)
            .cursor_pointer()
            .when(!active, |el| el.hover(move |s| s.bg(hover)))
            .when(active, |el| {
                el.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(2.))
                        .bg(indicator),
                )
            })
            .when_some(self.icon, |el, icon| el.child(icon))
            .child(
                div()
                    .whitespace_nowrap()
                    .when(self.preview, |el| el.italic())
                    .child(self.title),
            )
            .children(lock)
            .when_some(self.detail, |el, detail| {
                el.child(
                    div()
                        .whitespace_nowrap()
                        .text_size(metrics::TEXT_SM)
                        .text_color(ui.text_faint.hsla())
                        .child(detail),
                )
            })
            .children(close);

        if let Some(on_click) = self.on_click {
            tab = tab.on_mouse_down(MouseButton::Left, move |_, window, cx| on_click(window, cx));
        }
        if let Some(on_close) = self.on_close {
            tab = tab.on_mouse_down(MouseButton::Middle, move |_, window, cx| {
                cx.stop_propagation();
                on_close(window, cx)
            });
        }
        if let Some(on_secondary) = self.on_secondary {
            tab = tab.on_mouse_down(MouseButton::Right, move |e, window, cx| {
                on_secondary(e, window, cx)
            });
        }
        tab
    }
}

/// A horizontally scrolling tab strip followed by trailing controls.
#[derive(IntoElement)]
pub struct TabBar {
    id: ElementId,
    tabs: Vec<AnyElement>,
    end: Vec<AnyElement>,
    scroll: ScrollHandle,
}

impl TabBar {
    /// New tab bar using `scroll` for its overflow offset.
    pub fn new(id: impl Into<ElementId>, scroll: ScrollHandle) -> Self {
        TabBar {
            id: id.into(),
            tabs: Vec::new(),
            end: Vec::new(),
            scroll,
        }
    }

    /// Adds a tab.
    pub fn tab(mut self, tab: impl IntoElement) -> Self {
        self.tabs.push(tab.into_any_element());
        self
    }

    /// Adds tabs.
    pub fn tabs(mut self, tabs: impl IntoIterator<Item = impl IntoElement>) -> Self {
        self.tabs
            .extend(tabs.into_iter().map(IntoElement::into_any_element));
        self
    }

    /// Trailing control (e.g. split buttons).
    pub fn end_child(mut self, el: impl IntoElement) -> Self {
        self.end.push(el.into_any_element());
        self
    }
}

impl RenderOnce for TabBar {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let ui = &cx.theme().ui;
        h_flex()
            .relative()
            .h(metrics::TAB_BAR_HEIGHT)
            .w_full()
            .flex_none()
            .bg(ui.tab_bar_bg.hsla())
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .h(px(1.))
                    .bg(ui.border.hsla()),
            )
            .child(
                div()
                    .id(self.id)
                    .flex()
                    .flex_row()
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .overflow_x_scroll()
                    .track_scroll(&self.scroll)
                    .children(self.tabs),
            )
            .when(!self.end.is_empty(), |el| {
                el.child(
                    h_flex()
                        .flex_none()
                        .px(px(6.))
                        .gap(px(2.))
                        .children(self.end),
                )
            })
    }
}
