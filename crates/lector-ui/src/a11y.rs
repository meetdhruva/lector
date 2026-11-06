//! Accessibility helpers shared by the views: reduced motion and the focus ring.
//!
//! **Focus ring.** Every focusable control shows the theme's `focus_ring` color (2 px,
//! 3 px at UI scale >= 1.6) when it has keyboard focus, and nothing takes focus without one:
//!
//! * controls that own an element-state focus handle (buttons, icon buttons, tabs) get an
//!   inset border through [`focus_ring_style`] in their `.focus(..)` refinement;
//! * containers whose focus lives in a child (lists, panels, panes) draw an overlay with
//!   [`region_ring`] while `contains_focused` is true;
//! * text inputs swap their border for the ring.
//!
//! GPUI 0.2.2 draws `box-shadow` as a filled blob under the element and has no `outline`,
//! so the ring is a real border, which sits inside the control and never clips.
//!
//! **Reduced motion.** [`reduce_motion`] is `ui.reduce_motion` when set in the settings,
//! else the operating system's preference (read once in the background by
//! [`start_os_detection`]), else `false`. Every animation in the UI (caret blink, smooth
//! scrolling, toast slide-in, spinner rotation) asks it.

use std::sync::atomic::{AtomicU8, Ordering};

use gpui::{div, prelude::*, App, Div, Hsla, Pixels, StyleRefinement};

use crate::theme::{ActiveTheme, ColorExt};

/// 0 = not detected, 1 = the OS does not ask for reduced motion, 2 = it does.
static OS_REDUCE_MOTION: AtomicU8 = AtomicU8::new(0);

/// The operating system's "reduce motion" preference, if it could be read.
pub fn os_reduce_motion() -> Option<bool> {
    match OS_REDUCE_MOTION.load(Ordering::Relaxed) {
        1 => Some(false),
        2 => Some(true),
        _ => None,
    }
}

/// Records the operating system's preference (`None` = unknown).
pub fn set_os_reduce_motion(value: Option<bool>) {
    OS_REDUCE_MOTION.store(
        match value {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        },
        Ordering::Relaxed,
    );
}

/// Whether animation should be avoided: the `ui.reduce_motion` setting, else the OS
/// preference, else `false`.
pub fn reduce_motion(cx: &App) -> bool {
    crate::settings::AppSettings::get(cx)
        .ui
        .reduce_motion
        .or_else(os_reduce_motion)
        .unwrap_or(false)
}

/// Reads the OS preference. Blocking (spawns a tiny CLI); call from a background thread.
///
/// * `LECTOR_REDUCE_MOTION=1|0` overrides everything (also useful in containers).
/// * macOS: `defaults read com.apple.universalaccess reduceMotion`.
/// * Linux: GNOME/GTK `org.gnome.desktop.interface enable-animations` through `gsettings`
///   (animations disabled means reduced motion).
/// * Windows and other desktops: unknown (`None`); GPUI 0.2.2 does not expose the setting.
pub fn detect_os_reduce_motion() -> Option<bool> {
    if let Ok(v) = std::env::var("LECTOR_REDUCE_MOTION") {
        match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => return Some(true),
            "0" | "false" | "no" | "off" => return Some(false),
            _ => {}
        }
    }
    let run = |program: &str, args: &[&str]| -> Option<String> {
        let out = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    if cfg!(target_os = "macos") {
        let v = run(
            "defaults",
            &["read", "com.apple.universalaccess", "reduceMotion"],
        )?;
        return Some(v == "1");
    }
    if cfg!(target_os = "linux") {
        let v = run(
            "gsettings",
            &["get", "org.gnome.desktop.interface", "enable-animations"],
        )?;
        return match v.as_str() {
            "false" => Some(true),
            "true" => Some(false),
            _ => None,
        };
    }
    None
}

/// Detects the OS preference on a background thread (never blocks startup) and stores it
/// for [`reduce_motion`].
pub fn start_os_detection() {
    std::thread::Builder::new()
        .name("lector-os-motion".into())
        .spawn(|| set_os_reduce_motion(detect_os_reduce_motion()))
        .ok();
}

/// Thickness of the focus ring: 2 px, 3 px once the UI is scaled to 160 % or more.
pub fn ring_width() -> Pixels {
    gpui::px(if crate::scale::scale() >= 1.6 { 3. } else { 2. })
}

/// The theme's focus ring color.
pub fn ring_color(cx: &App) -> Hsla {
    cx.theme().ui.focus_ring.hsla()
}

/// Adds the ring to the focused style of a control: a border in the ring color. The
/// control's padding must shrink by the border (see [`ring_padding`]) when its width is
/// automatic, so focusing does not move its neighbors.
pub fn focus_ring_style(style: StyleRefinement, ring: Hsla) -> StyleRefinement {
    let style = style.border_color(ring);
    if crate::scale::scale() >= 1.6 {
        style.border_3()
    } else {
        style.border_2()
    }
}

/// `padding` minus the ring border, for the focused style of auto-width controls that
/// have no border of their own (`existing_border` = the border they always have).
pub fn ring_padding(padding: Pixels, existing_border: f32) -> Pixels {
    let ring = f32::from(ring_width());
    gpui::px((f32::from(padding) - (ring - existing_border).max(0.)).max(0.))
}

/// An overlay that draws the ring just inside its (relatively positioned) parent. Add it as
/// the parent's last child while the parent or a descendant has focus. It has no hitbox, so
/// pointer events go through it.
pub fn ring_overlay(cx: &App, radius: Pixels) -> Div {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .rounded(radius)
        .border_color(ring_color(cx))
        .map(|el| {
            if crate::scale::scale() >= 1.6 {
                el.border_3()
            } else {
                el.border_2()
            }
        })
}

/// The ring around a text field's text, reaching a few pixels past it into the field's
/// padding (fields draw their own border; the ring sits over it). Nothing when unfocused.
pub fn outset_ring(focused: bool, cx: &App) -> Option<Div> {
    focused.then(|| {
        ring_overlay(cx, gpui::px(4.))
            .top(crate::scale::px(-3.))
            .bottom(crate::scale::px(-3.))
            .left(crate::scale::px(-5.))
            .right(crate::scale::px(-5.))
            .size_auto()
    })
}

/// The focused style of a control: `.focus(ring_focus(cx))`.
pub fn ring_focus(cx: &App) -> impl FnOnce(StyleRefinement) -> StyleRefinement + 'static {
    let ring = ring_color(cx);
    move |s| focus_ring_style(s, ring)
}

/// Makes a clickable element keyboard-operable: a tab stop whose focused style is the ring
/// (Enter and Space then click it). Use it on `div().id(..).on_click(..)` controls.
pub fn focusable<E: gpui::StatefulInteractiveElement>(el: E, cx: &App) -> E {
    let ring = ring_color(cx);
    el.tab_index(0).focus(move |s| focus_ring_style(s, ring))
}

/// [`ring_overlay`] when `focused`, nothing otherwise: `el.children(region_ring(..))`.
pub fn region_ring(focused: bool, cx: &App, radius: Pixels) -> Option<Div> {
    focused.then(|| ring_overlay(cx, radius))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_preference_round_trips() {
        let before = os_reduce_motion();
        set_os_reduce_motion(Some(true));
        assert_eq!(os_reduce_motion(), Some(true));
        set_os_reduce_motion(Some(false));
        assert_eq!(os_reduce_motion(), Some(false));
        set_os_reduce_motion(None);
        assert_eq!(os_reduce_motion(), None);
        set_os_reduce_motion(before);
    }

    #[test]
    fn ring_is_two_pixels_and_grows_with_the_scale() {
        crate::scale::set_scale(1.0);
        assert_eq!(f32::from(ring_width()), 2.0);
        crate::scale::set_scale(2.0);
        assert_eq!(f32::from(ring_width()), 3.0);
        crate::scale::set_scale(1.0);
    }

    #[test]
    fn padding_compensates_the_ring() {
        crate::scale::set_scale(1.0);
        assert_eq!(f32::from(ring_padding(gpui::px(8.), 0.)), 6.0);
        assert_eq!(f32::from(ring_padding(gpui::px(8.), 1.)), 7.0);
    }
}
