//! Reusable, themed widgets.

pub mod anchor;
pub mod button;
pub mod context_menu;
pub mod list;
pub mod modal;
pub mod picker;
pub mod popover;
pub mod prompt;
pub mod scrollbar;
pub mod split;
pub mod tabs;
pub mod text_area;
pub mod text_input;
pub mod tooltip;
pub mod tree_view;

pub use button::{Button, ButtonStyle, IconButton};
pub use context_menu::{ContextMenu, ContextMenuState};
pub use modal::ModalLayer;
pub use picker::{Picker, PickerDelegate};
pub use tooltip::Tooltip;

use crate::scale::px;
use gpui::{div, Action, App, Div, KeyBinding, Pixels, SharedString, Styled};

/// Horizontal flex row, items centered vertically.
pub fn h_flex() -> Div {
    div().flex().flex_row().items_center()
}

/// Vertical flex column.
pub fn v_flex() -> Div {
    div().flex().flex_col()
}

/// Spacing and typography tokens shared by all widgets.
///
/// Use the functions: they follow the interface scale (`ui.scale`, see [`crate::scale`]).
/// The upper-case constants are the unscaled base values, kept for code that must not scale.
pub mod metrics {
    use super::*;

    /// Default UI text size.
    pub const TEXT: Pixels = gpui::px(13.);
    /// Small UI text (secondary labels, status bar).
    pub const TEXT_SM: Pixels = gpui::px(12.);
    /// Extra-small UI text (badges, section headers).
    pub const TEXT_XS: Pixels = gpui::px(11.);
    /// Corner radius of controls.
    pub const RADIUS: Pixels = gpui::px(5.);
    /// Corner radius of floating surfaces.
    pub const RADIUS_LG: Pixels = gpui::px(8.);
    /// Height of list rows.
    pub const ROW_HEIGHT: Pixels = gpui::px(24.);
    /// Height of the tab bar.
    pub const TAB_BAR_HEIGHT: Pixels = gpui::px(34.);
    /// Height of the title bar.
    pub const TITLE_BAR_HEIGHT: Pixels = gpui::px(38.);
    /// Height of the status bar.
    pub const STATUS_BAR_HEIGHT: Pixels = gpui::px(26.);

    /// Default UI text size at the current interface scale.
    pub fn text() -> Pixels {
        px(13.)
    }
    /// Small UI text at the current interface scale.
    pub fn text_sm() -> Pixels {
        px(12.)
    }
    /// Extra-small UI text at the current interface scale.
    pub fn text_xs() -> Pixels {
        px(11.)
    }
    /// Corner radius of controls at the current interface scale.
    pub fn radius() -> Pixels {
        px(5.)
    }
    /// Corner radius of floating surfaces at the current interface scale.
    pub fn radius_lg() -> Pixels {
        px(8.)
    }
    /// Height of list rows at the current interface scale.
    pub fn row_height() -> Pixels {
        px(24.)
    }
    /// Height of the tab bar at the current interface scale.
    pub fn tab_bar_height() -> Pixels {
        px(34.)
    }
    /// Height of the title bar at the current interface scale.
    pub fn title_bar_height() -> Pixels {
        px(38.)
    }
    /// Height of the status bar at the current interface scale.
    pub fn status_bar_height() -> Pixels {
        px(26.)
    }
}

/// Formats a key binding for display: `⌘⇧P` on macOS, `Ctrl+Shift+P` elsewhere.
pub fn format_binding(binding: &KeyBinding) -> SharedString {
    let parts: Vec<String> = binding
        .keystrokes()
        .iter()
        .map(|ks| format_keystroke(ks.modifiers(), ks.key()))
        .collect();
    parts.join(" ").into()
}

/// Formats one keystroke for display.
pub fn format_keystroke(m: &gpui::Modifiers, key: &str) -> String {
    let mac = cfg!(target_os = "macos");
    let key = match key {
        "up" => "↑".to_string(),
        "down" => "↓".to_string(),
        "left" => "←".to_string(),
        "right" => "→".to_string(),
        "enter" => if mac { "↩" } else { "Enter" }.to_string(),
        "escape" => "Esc".to_string(),
        "backspace" => if mac { "⌫" } else { "Backspace" }.to_string(),
        "tab" => if mac { "⇥" } else { "Tab" }.to_string(),
        "space" => "Space".to_string(),
        "pageup" => "PgUp".to_string(),
        "pagedown" => "PgDn".to_string(),
        k if k.len() == 1 => k.to_uppercase(),
        k => {
            let mut c = k.chars();
            match c.next() {
                Some(f) => f.to_uppercase().chain(c).collect(),
                None => String::new(),
            }
        }
    };
    if mac {
        let mut s = String::new();
        if m.control {
            s.push('⌃');
        }
        if m.alt {
            s.push('⌥');
        }
        if m.shift {
            s.push('⇧');
        }
        if m.platform {
            s.push('⌘');
        }
        s.push_str(&key);
        s
    } else {
        let mut parts: Vec<&str> = Vec::new();
        if m.control {
            parts.push("Ctrl");
        }
        if m.alt {
            parts.push("Alt");
        }
        if m.shift {
            parts.push("Shift");
        }
        if m.platform {
            parts.push("Super");
        }
        parts.push(&key);
        parts.join("+")
    }
}

/// Display text of the highest-precedence binding of `action`, ignoring
/// key contexts (good enough for menus, tooltips and the palette).
pub fn binding_text_for_action(action: &dyn Action, cx: &App) -> Option<SharedString> {
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    let binding = keymap.bindings_for_action(action).last()?;
    Some(format_binding(binding))
}
