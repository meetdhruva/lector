//! Buttons: text buttons and icon buttons with hover/active states and tooltips.

use std::rc::Rc;

use gpui::{div, prelude::*, px, AnyView, App, ClickEvent, ElementId, Hsla, SharedString, Window};

use super::{h_flex, metrics};
use crate::icons::{Icon, IconName, IconSize};
use crate::theme::{ActiveTheme, ColorExt};

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type TooltipBuilder = Rc<dyn Fn(&mut Window, &mut App) -> AnyView>;

/// Visual style of a button.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ButtonStyle {
    /// Transparent until hovered (toolbars).
    #[default]
    Ghost,
    /// Subtle filled background.
    Subtle,
    /// Accent-filled primary button.
    Accent,
    /// Bordered.
    Outlined,
}

/// A text button with an optional leading icon.
#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: SharedString,
    icon: Option<IconName>,
    style: ButtonStyle,
    selected: bool,
    disabled: bool,
    compact: bool,
    full_width: bool,
    color: Option<Hsla>,
    on_click: Option<ClickHandler>,
    tooltip: Option<TooltipBuilder>,
}

impl Button {
    /// New ghost button.
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Button {
            id: id.into(),
            label: label.into(),
            icon: None,
            style: ButtonStyle::Ghost,
            selected: false,
            disabled: false,
            compact: false,
            full_width: false,
            color: None,
            on_click: None,
            tooltip: None,
        }
    }

    /// Stretches to the parent's width, content centered.
    pub fn full_width(mut self) -> Self {
        self.full_width = true;
        self
    }

    /// Leading icon.
    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Visual style.
    pub fn style(mut self, style: ButtonStyle) -> Self {
        self.style = style;
        self
    }

    /// Toggled-on appearance.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Disabled (no hover, no clicks).
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Smaller padding and text (status bar).
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }

    /// Text color override.
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }

    /// Click handler.
    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(f));
        self
    }

    /// Tooltip builder (see [`super::Tooltip`]).
    pub fn tooltip(mut self, f: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        self.tooltip = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let ui = &cx.theme().ui;
        let (bg, fg, border) = match self.style {
            ButtonStyle::Ghost => (None, ui.text_muted.hsla(), None),
            ButtonStyle::Subtle => (Some(ui.hover.hsla()), ui.text.hsla(), None),
            ButtonStyle::Accent => (Some(ui.accent.hsla()), ui.accent_foreground.hsla(), None),
            ButtonStyle::Outlined => (None, ui.text.hsla(), Some(ui.border.hsla())),
        };
        let fg = if self.disabled {
            ui.text_faint.hsla()
        } else if self.selected {
            ui.text.hsla()
        } else {
            self.color.unwrap_or(fg)
        };
        let hover = ui.hover.hsla();
        let active = ui.active.hsla();
        let accent = self.style == ButtonStyle::Accent;
        let (px_x, h, text) = if self.compact {
            (px(6.), px(20.), metrics::TEXT_SM)
        } else {
            (px(8.), px(26.), metrics::TEXT)
        };
        let mut el = h_flex()
            .id(self.id)
            .flex_none()
            .gap(px(5.))
            .h(h)
            .px(px_x)
            .rounded(metrics::RADIUS)
            .text_size(text)
            .text_color(fg)
            .when(self.full_width, |el| el.w_full().justify_center())
            .when_some(bg, |el, bg| el.bg(bg))
            .when_some(border, |el, b| el.border_1().border_color(b))
            .when(self.selected, |el| el.bg(active))
            .when_some(self.icon, |el, icon| {
                el.child(Icon::new(icon).size(IconSize::Small).color(fg))
            })
            .child(self.label);
        if !self.disabled {
            el = el
                .cursor_pointer()
                .when(!accent, |el| {
                    el.hover(move |s| s.bg(hover)).active(move |s| s.bg(active))
                })
                .when(accent, |el| el.hover(|s| s.opacity(0.92)));
            if let Some(handler) = self.on_click {
                el = el.on_click(move |e, w, cx| handler(e, w, cx));
            }
        }
        if let Some(tooltip) = self.tooltip {
            el = el.tooltip(move |w, cx| tooltip(w, cx));
        }
        el
    }
}

/// A square icon-only button.
#[derive(IntoElement)]
pub struct IconButton {
    id: ElementId,
    icon: IconName,
    size: IconSize,
    selected: bool,
    disabled: bool,
    color: Option<Hsla>,
    on_click: Option<ClickHandler>,
    tooltip: Option<TooltipBuilder>,
    square: Option<gpui::Pixels>,
}

impl IconButton {
    /// New icon button.
    pub fn new(id: impl Into<ElementId>, icon: IconName) -> Self {
        IconButton {
            id: id.into(),
            icon,
            size: IconSize::Medium,
            selected: false,
            disabled: false,
            color: None,
            on_click: None,
            tooltip: None,
            square: None,
        }
    }

    /// Icon size.
    pub fn icon_size(mut self, size: IconSize) -> Self {
        self.size = size;
        self
    }

    /// Button side length (defaults to icon size + 10px).
    pub fn square(mut self, side: gpui::Pixels) -> Self {
        self.square = Some(side);
        self
    }

    /// Toggled-on appearance.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Disabled.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Icon color override.
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }

    /// Click handler.
    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(f));
        self
    }

    /// Tooltip builder.
    pub fn tooltip(mut self, f: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        self.tooltip = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for IconButton {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let ui = &cx.theme().ui;
        let color = if self.disabled {
            ui.text_faint.hsla()
        } else if self.selected {
            ui.text.hsla()
        } else {
            self.color.unwrap_or(ui.text_muted.hsla())
        };
        let hover = ui.hover.hsla();
        let active = ui.active.hsla();
        let hover_fg = ui.text.hsla();
        let side = self.square.unwrap_or(self.size.px() + px(10.));
        let mut el = div()
            .id(self.id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(side)
            .rounded(metrics::RADIUS)
            .when(self.selected, |el| el.bg(active))
            .child(Icon::new(self.icon).size(self.size).color(color));
        if !self.disabled {
            el = el
                .cursor_pointer()
                .hover(move |s| s.bg(hover).text_color(hover_fg))
                .active(move |s| s.bg(active));
            if let Some(handler) = self.on_click {
                el = el.on_click(move |e, w, cx| {
                    cx.stop_propagation();
                    handler(e, w, cx)
                });
            }
        }
        if let Some(tooltip) = self.tooltip {
            el = el.tooltip(move |w, cx| tooltip(w, cx));
        }
        el
    }
}
