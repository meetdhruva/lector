//! Window caption buttons (minimize, maximize/restore, close) for the custom
//! title bar.
//!
//! * **macOS**: none; the native traffic lights are kept (the title bar leaves
//!   room for them).
//! * **Windows**: GPUI hides the native caption (`appears_transparent`), so
//!   Lector draws Windows-style buttons and tags them with
//!   [`WindowControlArea`]s; the OS then handles them natively (hover,
//!   Snap Layouts on maximize, close). The title bar itself is a `Drag` area
//!   (move, double-click maximize, system menu).
//! * **Linux**: drawn when the window uses client-side decorations, honoring
//!   the compositor's supported controls (`window.window_controls()`).
//!   `LECTOR_WINDOW_CONTROLS=1` forces them (window managers without
//!   decorations, headless screenshots).

use gpui::{
    div, prelude::*, px, rgb, AnyElement, App, Decorations, Hsla, Window, WindowControlArea,
};

use crate::actions::workspace::CloseWindow;
use crate::components::{h_flex, metrics, Tooltip};
use crate::icons::{IconName, IconSize};
use crate::theme::{ActiveTheme, ColorExt};

/// Which caption buttons the title bar draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptionStyle {
    /// No buttons (macOS traffic lights, or server-side decorations).
    None,
    /// Rounded GNOME/KDE-like buttons (Linux client-side decorations).
    Linux,
    /// Full-height Windows 11 caption buttons.
    Windows,
}

/// The caption style for `window` on this platform.
pub fn caption_style(window: &Window) -> CaptionStyle {
    if cfg!(target_os = "macos") {
        return CaptionStyle::None;
    }
    if cfg!(target_os = "windows") {
        return CaptionStyle::Windows;
    }
    let forced = std::env::var_os("LECTOR_WINDOW_CONTROLS").is_some_and(|v| v != "0");
    match window.window_decorations() {
        Decorations::Client { .. } => CaptionStyle::Linux,
        Decorations::Server if forced => CaptionStyle::Linux,
        Decorations::Server => CaptionStyle::None,
    }
}

/// The close button's hover color (Windows uses #c42b1c).
fn close_red(style: CaptionStyle) -> Hsla {
    match style {
        CaptionStyle::Windows => rgb(0xc42b1c).into(),
        _ => rgb(0xe0443e).into(),
    }
}

/// Caption buttons, or `None` when the platform draws its own.
pub fn window_controls(window: &mut Window, cx: &mut App) -> Option<AnyElement> {
    match caption_style(window) {
        CaptionStyle::None => None,
        CaptionStyle::Linux => Some(linux_controls(window, cx)),
        CaptionStyle::Windows => Some(windows_controls(window, cx)),
    }
}

fn linux_controls(window: &mut Window, cx: &mut App) -> AnyElement {
    let ui = cx.theme().ui.clone();
    let supported = window.window_controls();
    let maximized = window.is_maximized();
    let fg = ui.title_bar_fg.hsla();
    let hover = ui.hover.hsla();
    let active = ui.active.hsla();
    let red = close_red(CaptionStyle::Linux);
    let button = |id: &'static str, icon: IconName, close: bool| {
        let group = format!("{id}-group");
        div()
            .id(id)
            .group(group.clone())
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .w(px(30.))
            .h(px(26.))
            .rounded(px(6.))
            .cursor_pointer()
            .when(!close, |el| {
                el.hover(move |s| s.bg(hover)).active(move |s| s.bg(active))
            })
            .when(close, |el| {
                el.hover(move |s| s.bg(red))
                    .active(move |s| s.bg(red.opacity(0.85)))
            })
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(svg_icon(icon).text_color(fg).when(close, |el| {
                el.group_hover(group, |s| s.text_color(gpui::white()))
            }))
    };
    h_flex()
        .id("window-controls")
        .flex_none()
        .gap(px(6.))
        .pl(px(6.))
        .when(supported.minimize, |el| {
            el.child(
                button("win-minimize", IconName::WindowMinimize, false)
                    .tooltip(Tooltip::text("Minimize"))
                    .on_click(|_, window, _| window.minimize_window()),
            )
        })
        .when(supported.maximize, |el| {
            el.child(
                button(
                    "win-maximize",
                    if maximized {
                        IconName::WindowRestore
                    } else {
                        IconName::WindowMaximize
                    },
                    false,
                )
                .tooltip(Tooltip::text(if maximized {
                    "Restore"
                } else {
                    "Maximize"
                }))
                .on_click(|_, window, _| window.zoom_window()),
            )
        })
        .child(
            button("win-close", IconName::WindowClose, true)
                .tooltip(Tooltip::for_action("Close Window", &CloseWindow))
                .on_click(|_, window, cx| window.dispatch_action(Box::new(CloseWindow), cx)),
        )
        .into_any_element()
}

/// A caption glyph (an `svg()` so `group_hover` can recolor it).
fn svg_icon(icon: IconName) -> gpui::Svg {
    gpui::svg()
        .path(crate::icons::ui_icon_path(icon))
        .size(IconSize::Medium.px())
        .flex_none()
}

fn windows_controls(window: &mut Window, cx: &mut App) -> AnyElement {
    let ui = cx.theme().ui.clone();
    let maximized = window.is_maximized();
    let dark = cx.theme().appearance.is_dark();
    let fg = ui.title_bar_fg.hsla();
    let hover: Hsla = if dark {
        gpui::hsla(0., 0., 1., 0.07)
    } else {
        gpui::hsla(0., 0., 0., 0.05)
    };
    let pressed: Hsla = if dark {
        gpui::hsla(0., 0., 1., 0.04)
    } else {
        gpui::hsla(0., 0., 0., 0.03)
    };
    let red = close_red(CaptionStyle::Windows);
    // Segoe Fluent Icons (Windows 11) or Segoe MDL2 Assets (Windows 10) share
    // these code points.
    let font = if cx
        .text_system()
        .all_font_names()
        .iter()
        .any(|n| n == "Segoe Fluent Icons")
    {
        "Segoe Fluent Icons"
    } else {
        "Segoe MDL2 Assets"
    };
    let button = |id: &'static str, glyph: &'static str, area: WindowControlArea| {
        let close = area == WindowControlArea::Close;
        let group = format!("{id}-group");
        div()
            .id(id)
            .group(group.clone())
            .occlude()
            .window_control_area(area)
            .flex()
            .items_center()
            .justify_center()
            .w(px(46.))
            .h(metrics::TITLE_BAR_HEIGHT)
            .font_family(font)
            .text_size(px(10.))
            .text_color(fg)
            .when(!close, |el| {
                el.hover(move |s| s.bg(hover))
                    .active(move |s| s.bg(pressed))
            })
            .when(close, |el| {
                el.hover(move |s| s.bg(red).text_color(gpui::white()))
                    .active(move |s| s.bg(red.opacity(0.9)).text_color(gpui::white()))
            })
            .child(glyph)
    };
    h_flex()
        .id("window-controls")
        .flex_none()
        .h(metrics::TITLE_BAR_HEIGHT)
        .ml(px(6.))
        .child(button("win-minimize", "\u{e921}", WindowControlArea::Min))
        .child(button(
            "win-maximize",
            if maximized { "\u{e923}" } else { "\u{e922}" },
            WindowControlArea::Max,
        ))
        .child(button("win-close", "\u{e8bb}", WindowControlArea::Close))
        .into_any_element()
}

/// `workspace::ToggleMaximize`.
pub fn toggle_maximize(window: &mut Window) {
    // macOS and Linux toggle; on Windows GPUI can only maximize (the caption
    // button and a title-bar double-click restore natively).
    if cfg!(target_os = "windows") && window.is_maximized() {
        return;
    }
    window.zoom_window();
}
