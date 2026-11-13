//! Keyboard: GPUI keystrokes → the core's [`Key`]/[`Modifiers`], and key routing.
//!
//! Routing has two layers:
//! 1. **Key bindings.** The focused terminal carries the `Terminal` key context. For every
//!    binding the user's keymap makes active in `Workspace` (or globally), except the
//!    [`RESERVED_ACTIONS`], [`passthrough_bindings`] adds a `zed::NoAction` binding with the
//!    same keys in `Terminal`: the deeper context wins, the binding is disabled while a
//!    terminal has focus, and the key reaches the shell. Users rebind by adding their own
//!    `Terminal` bindings (they replace the generated ones). Bindings on the platform key stay
//!    active (a shell never sees them), except chords whose first keystroke is a `Terminal`
//!    binding itself (macOS `cmd-k` clears; `cmd-k p` must neither wait for a second key nor
//!    reach `git::Push` from a terminal).
//! 2. **Unbound keys** arrive as key-down events and are translated by [`route_keystroke`]:
//!    plain text goes through the input handler (IME, dead keys, compose), everything else is
//!    encoded by the core.

use gpui::{KeyBindingContextPredicate, KeyContext, Keystroke};
use lector_terminal::{Key, Modifiers, ScrollAmount};
use lector_ui::keymap::KeySpec;

/// Actions that keep their (Workspace/global) bindings while a terminal is focused.
pub const RESERVED_ACTIONS: &[&str] = &[
    "workspace::ToggleCommandPalette",
    "workspace::FocusNextRegion",
    "workspace::FocusPrevRegion",
    "workspace::ShowContextMenu",
    "workspace::NextTab",
    "workspace::PrevTab",
    "workspace::ZoomIn",
    "workspace::ZoomOut",
    "workspace::ZoomReset",
    "workspace::UiZoomIn",
    "workspace::UiZoomOut",
    "workspace::UiZoomReset",
    "workspace::ShowAppMenu",
];

/// Whether `action` keeps its binding inside a focused terminal.
pub fn is_reserved(action: &str) -> bool {
    action.starts_with("terminal::") || RESERVED_ACTIONS.contains(&action)
}

/// The action name GPUI uses for "unbind here".
pub const NO_ACTION: &str = "zed::NoAction";

/// Keystrokes using the platform key (cmd / super / win) never mean anything to a shell, so
/// bindings on them stay active in the terminal.
fn uses_platform_key(keys: &str) -> bool {
    keys.split_whitespace().any(|k| {
        k.split('-')
            .rev()
            .skip(1)
            .any(|m| matches!(m, "cmd" | "super" | "win" | "platform"))
    })
}

/// The first keystroke of a chord (`"cmd-k p"` → `"cmd-k"`).
fn first_keystroke(keys: &str) -> &str {
    keys.split_whitespace().next().unwrap_or("")
}

/// `Terminal`-context bindings that disable every non-reserved binding visible from a
/// focused terminal (see the module docs). Registered as a keymap extension.
pub fn passthrough_bindings(specs: &[KeySpec]) -> Vec<KeySpec> {
    let stack: Vec<KeyContext> = ["Workspace", lector_ui::keymap::contexts::TERMINAL]
        .iter()
        .filter_map(|c| KeyContext::parse(c).ok())
        .collect();
    let user_terminal_keys: Vec<&str> = specs
        .iter()
        .filter(|s| mentions_terminal(s.context.as_deref()))
        .map(|s| s.keys.as_str())
        .collect();
    // Single keystrokes bound by a real action in `Terminal`: chords starting with one would
    // make GPUI wait for the chord instead of running it.
    let terminal_single_keys: Vec<&str> = specs
        .iter()
        .filter(|s| mentions_terminal(s.context.as_deref()))
        .filter(|s| {
            let action = s.action.trim();
            !(action.is_empty() || action.eq_ignore_ascii_case("none") || action == NO_ACTION)
        })
        .map(|s| s.keys.as_str())
        .filter(|k| k.split_whitespace().count() == 1)
        .collect();
    let active = |spec: &KeySpec| match spec.context.as_deref() {
        None => true,
        Some(ctx) => KeyBindingContextPredicate::parse(ctx)
            .ok()
            .and_then(|p| p.depth_of(&stack))
            .is_some(),
    };
    // Keys a reserved action uses stay with Lector even if something else shares them.
    let reserved_keys: Vec<&str> = specs
        .iter()
        .filter(|s| is_reserved(s.action.trim()) && active(s))
        .map(|s| s.keys.as_str())
        .collect();
    let mut out: Vec<KeySpec> = Vec::new();
    for spec in specs {
        let action = spec.action.trim();
        if action.is_empty()
            || action.eq_ignore_ascii_case("none")
            || action == NO_ACTION
            || is_reserved(action)
            || mentions_terminal(spec.context.as_deref())
            || (uses_platform_key(&spec.keys)
                && !(spec.keys.split_whitespace().count() > 1
                    && terminal_single_keys.contains(&first_keystroke(&spec.keys))))
            || user_terminal_keys.contains(&spec.keys.as_str())
            || reserved_keys.contains(&spec.keys.as_str())
        {
            continue;
        }
        if active(spec) && !out.iter().any(|o| o.keys == spec.keys) {
            out.push(KeySpec {
                keys: spec.keys.clone(),
                action: NO_ACTION.to_string(),
                context: Some(lector_ui::keymap::contexts::TERMINAL.to_string()),
            });
        }
    }
    out
}

fn mentions_terminal(context: Option<&str>) -> bool {
    context.is_some_and(|c| {
        c.split(|ch: char| !ch.is_alphanumeric() && ch != '_')
            .any(|w| w == lector_ui::keymap::contexts::TERMINAL)
    })
}

/// Platform conventions that change how keys translate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyOptions {
    /// macOS (Option types characters unless `option_as_meta`; AltGr does not exist).
    pub mac: bool,
    /// macOS: Option sends Meta.
    pub option_as_meta: bool,
    /// The program is on the alternate screen (Shift+PageUp/Down go to the program).
    pub alt_screen: bool,
}

impl KeyOptions {
    /// Options for the running platform.
    pub fn current(option_as_meta: bool, alt_screen: bool) -> Self {
        KeyOptions {
            mac: cfg!(target_os = "macos"),
            option_as_meta,
            alt_screen,
        }
    }
}

/// What to do with a key-down event that no binding claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyRoute {
    /// Let it through: the platform input handler inserts the text (IME, dead keys).
    Text,
    /// Encode and send this key.
    Send(Key, Modifiers),
    /// Send this text as typed (AltGr characters, which the platform does not insert).
    SendText(String),
    /// Scroll the scrollback locally.
    Scroll(ScrollAmount),
    /// Not for the terminal (platform-key combinations nobody bound).
    Ignore,
}

fn named_key(key: &str) -> Option<Key> {
    Some(match key {
        "enter" => Key::Enter,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        "escape" => Key::Escape,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "insert" => Key::Insert,
        "delete" => Key::Delete,
        "space" => Key::Char(' '),
        _ => {
            let n = key.strip_prefix('f')?.parse::<u8>().ok()?;
            if (1..=20).contains(&n) {
                Key::F(n)
            } else {
                return None;
            }
        }
    })
}

fn single_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

/// Translates an unbound key-down event (see the module docs).
pub fn route_keystroke(ks: &Keystroke, opts: KeyOptions) -> KeyRoute {
    let m = &ks.modifiers;
    if m.platform {
        return KeyRoute::Ignore;
    }
    // Local scrollback paging, like every terminal emulator (not on the alternate screen,
    // where the program owns the keys).
    if m.shift && !m.control && !m.alt && !opts.alt_screen {
        match ks.key.as_str() {
            "pageup" => return KeyRoute::Scroll(ScrollAmount::PageUp),
            "pagedown" => return KeyRoute::Scroll(ScrollAmount::PageDown),
            _ => {}
        }
    }
    // Option on macOS types characters unless it is Meta.
    let alt_is_meta = m.alt && (!opts.mac || opts.option_as_meta);
    let mods = Modifiers {
        shift: m.shift,
        alt: alt_is_meta,
        ctrl: m.control,
        meta: false,
    };
    if let Some(key) = named_key(&ks.key) {
        if key == Key::Char(' ') && !m.control && !alt_is_meta {
            return KeyRoute::Text;
        }
        let mods = Modifiers {
            // Named keys keep Option as a modifier (Option+Left = word left in shells).
            alt: m.alt,
            ..mods
        };
        return KeyRoute::Send(key, mods);
    }
    let typed = ks
        .key_char
        .as_deref()
        .and_then(single_char)
        .filter(|c| !c.is_control());
    // AltGr (Windows reports it as Ctrl+Alt): the layout produced a character different
    // from the key itself — type it. (X11/Wayland report AltGr as no modifier at all, so
    // those characters arrive as plain text.)
    if !opts.mac && m.alt && m.control {
        if let Some(c) = typed {
            let base = single_char(&ks.key);
            let shifted_base = base.map(|b| b.to_uppercase().next().unwrap_or(b));
            if Some(c) != base && Some(c) != shifted_base {
                return KeyRoute::SendText(c.to_string());
            }
        }
    }
    if !m.control && !alt_is_meta {
        // Plain or shifted characters, and Option-characters on macOS. Unknown named keys
        // (F30, media keys) produce nothing.
        return if typed.is_some() || single_char(&ks.key).is_some() {
            KeyRoute::Text
        } else {
            KeyRoute::Ignore
        };
    }
    let Some(base) = single_char(&ks.key) else {
        return KeyRoute::Ignore;
    };
    // Ctrl / Meta combinations: the character the layout produced when it is printable and
    // not changed by Option (e.g. Meta+Shift+a = "A"), else the key itself.
    let ch = match typed {
        // macOS Option-as-Meta: the layout's Option character is not what Meta means.
        Some(c) if !m.control && !opts.mac => c,
        _ if m.shift && !m.control => base.to_uppercase().next().unwrap_or(base),
        _ => base,
    };
    KeyRoute::Send(Key::Char(ch), mods)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ks(mods: &str, key: &str, key_char: Option<&str>) -> Keystroke {
        let m = gpui::Modifiers {
            control: mods.contains('c'),
            alt: mods.contains('a'),
            shift: mods.contains('s'),
            platform: mods.contains('p'),
            function: false,
        };
        Keystroke {
            modifiers: m,
            key: key.to_string(),
            key_char: key_char.map(str::to_string),
        }
    }

    const LINUX: KeyOptions = KeyOptions {
        mac: false,
        option_as_meta: false,
        alt_screen: false,
    };
    const MAC: KeyOptions = KeyOptions {
        mac: true,
        option_as_meta: false,
        alt_screen: false,
    };

    fn send(key: Key, shift: bool, alt: bool, ctrl: bool) -> KeyRoute {
        KeyRoute::Send(
            key,
            Modifiers {
                shift,
                alt,
                ctrl,
                meta: false,
            },
        )
    }

    #[test]
    fn translation_table() {
        let cases: Vec<(Keystroke, KeyOptions, KeyRoute)> = vec![
            (ks("", "a", Some("a")), LINUX, KeyRoute::Text),
            (ks("s", "a", Some("A")), LINUX, KeyRoute::Text),
            (ks("", "space", Some(" ")), LINUX, KeyRoute::Text),
            (
                ks("c", "space", None),
                LINUX,
                send(Key::Char(' '), false, false, true),
            ),
            (
                ks("c", "c", None),
                LINUX,
                send(Key::Char('c'), false, false, true),
            ),
            (
                ks("c", "r", Some("r")),
                LINUX,
                send(Key::Char('r'), false, false, true),
            ),
            (
                ks("c", "w", None),
                LINUX,
                send(Key::Char('w'), false, false, true),
            ),
            (
                ks("c", "d", None),
                LINUX,
                send(Key::Char('d'), false, false, true),
            ),
            (
                ks("c", "z", None),
                LINUX,
                send(Key::Char('z'), false, false, true),
            ),
            (
                ks("", "tab", None),
                LINUX,
                send(Key::Tab, false, false, false),
            ),
            (
                ks("s", "tab", None),
                LINUX,
                send(Key::Tab, true, false, false),
            ),
            (
                ks("", "escape", None),
                LINUX,
                send(Key::Escape, false, false, false),
            ),
            (
                ks("", "enter", None),
                LINUX,
                send(Key::Enter, false, false, false),
            ),
            (
                ks("", "backspace", None),
                LINUX,
                send(Key::Backspace, false, false, false),
            ),
            (
                ks("c", "left", None),
                LINUX,
                send(Key::Left, false, false, true),
            ),
            (
                ks("", "f5", None),
                LINUX,
                send(Key::F(5), false, false, false),
            ),
            (
                ks("s", "f12", None),
                LINUX,
                send(Key::F(12), true, false, false),
            ),
            (ks("", "f30", None), LINUX, KeyRoute::Ignore),
            // Alt is Meta on Linux/Windows.
            (
                ks("a", "b", Some("b")),
                LINUX,
                send(Key::Char('b'), false, true, false),
            ),
            (
                ks("as", "b", Some("B")),
                LINUX,
                send(Key::Char('B'), true, true, false),
            ),
            // AltGr: Ctrl+Alt (Windows) or Alt producing another character types it.
            (
                ks("ca", "q", Some("@")),
                LINUX,
                KeyRoute::SendText("@".into()),
            ),
            (
                ks("ca", "e", Some("€")),
                LINUX,
                KeyRoute::SendText("€".into()),
            ),
            // Alt+Shift+1 is Meta+"!" (not AltGr).
            (
                ks("as", "1", Some("!")),
                LINUX,
                send(Key::Char('!'), true, true, false),
            ),
            // Ctrl+Alt+letter without a layout character is Meta+Ctrl.
            (
                ks("ca", "x", None),
                LINUX,
                send(Key::Char('x'), false, true, true),
            ),
            // Shift+PageUp scrolls locally, but not on the alternate screen.
            (
                ks("s", "pageup", None),
                LINUX,
                KeyRoute::Scroll(ScrollAmount::PageUp),
            ),
            (
                ks("s", "pageup", None),
                KeyOptions {
                    alt_screen: true,
                    ..LINUX
                },
                send(Key::PageUp, true, false, false),
            ),
            // Platform-key combinations are not for the shell.
            (ks("p", "k", None), LINUX, KeyRoute::Ignore),
            // macOS: Option types characters by default...
            (ks("a", "e", Some("´")), MAC, KeyRoute::Text),
            (ks("a", "2", Some("€")), MAC, KeyRoute::Text),
            // ...but Option+arrows stay modified keys (word motion).
            (
                ks("a", "left", None),
                MAC,
                send(Key::Left, false, true, false),
            ),
            // ...and Option is Meta when asked.
            (
                ks("a", "b", Some("∫")),
                KeyOptions {
                    option_as_meta: true,
                    ..MAC
                },
                send(Key::Char('b'), false, true, false),
            ),
            (
                ks("c", "c", Some("c")),
                MAC,
                send(Key::Char('c'), false, false, true),
            ),
            (ks("p", "c", Some("c")), MAC, KeyRoute::Ignore),
        ];
        for (k, opts, want) in cases {
            assert_eq!(route_keystroke(&k, opts), want, "{k:?} {opts:?}");
        }
    }

    fn spec(keys: &str, action: &str, context: Option<&str>) -> KeySpec {
        KeySpec {
            keys: keys.into(),
            action: action.into(),
            context: context.map(Into::into),
        }
    }

    #[test]
    fn reserved_key_routing() {
        let specs = vec![
            spec("ctrl-w", "workspace::CloseTab", Some("Workspace")),
            spec("ctrl-p", "workspace::ToggleFileFinder", Some("Workspace")),
            spec("ctrl-r", "some::Global", None),
            spec("tab", "workspace::FocusNextControl", Some("Workspace")),
            spec(
                "ctrl-k ctrl-c",
                "git::FocusCommitMessage",
                Some("Workspace"),
            ),
            spec(
                "ctrl-shift-p",
                "workspace::ToggleCommandPalette",
                Some("Workspace"),
            ),
            spec("f6", "workspace::FocusNextRegion", Some("Workspace")),
            spec("ctrl-=", "workspace::ZoomIn", Some("Workspace")),
            spec("ctrl-`", "terminal::TogglePanel", Some("Workspace")),
            spec("ctrl-shift-`", "terminal::NewTerminal", Some("Workspace")),
            spec("cmd-k", "workspace::Something", Some("Workspace")),
            spec("ctrl-f", "editor::Find", Some("CodeView")),
            spec("escape", "editor::ClearSelection", Some("CodeView")),
            spec("ctrl-z", "x::Y", Some("Workspace && !TextInput")),
            spec("ctrl-shift-c", "terminal::Copy", Some("Terminal")),
            // A user binding in Terminal replaces the generated pass-through.
            spec("ctrl-l", "workspace::Foo", Some("Workspace")),
            spec("ctrl-l", "workspace::Bar", Some("Terminal")),
        ];
        let out = passthrough_bindings(&specs);
        let blocked: Vec<&str> = out.iter().map(|s| s.keys.as_str()).collect();
        for k in [
            "ctrl-w",
            "ctrl-p",
            "ctrl-r",
            "tab",
            "ctrl-k ctrl-c",
            "ctrl-z",
        ] {
            assert!(
                blocked.contains(&k),
                "{k} must reach the shell: {blocked:?}"
            );
        }
        for k in [
            "ctrl-shift-p",
            "f6",
            "ctrl-=",
            "ctrl-`",
            "ctrl-shift-`",
            "cmd-k",
            "ctrl-f",
            "escape",
            "ctrl-shift-c",
            "ctrl-l",
        ] {
            assert!(
                !blocked.contains(&k),
                "{k} must stay with Lector: {blocked:?}"
            );
        }
        assert!(out
            .iter()
            .all(|s| s.action == NO_ACTION && s.context.as_deref() == Some("Terminal")));
    }
}
