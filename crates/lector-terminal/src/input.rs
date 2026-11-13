//! Keyboard, paste, mouse and focus encoding, independent of any UI toolkit.
//!
//! The UI translates its key events into [`Key`] + [`Modifiers`] and the
//! encoder produces the bytes xterm would send, honouring the modes in
//! [`TerminalModes`] (DECCKM, DECKPAM, LNM, modifyOtherKeys, mouse modes and
//! encodings, focus reporting, bracketed paste).

use crate::snapshot::{MouseMode, TerminalModes};

/// Keyboard modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers {
    /// Shift.
    pub shift: bool,
    /// Alt / Option (sent as an ESC prefix).
    pub alt: bool,
    /// Control.
    pub ctrl: bool,
    /// Super / Command / Windows key (rarely forwarded; reported as xterm "meta").
    pub meta: bool,
}

impl Modifiers {
    /// No modifiers.
    pub const NONE: Modifiers = Modifiers {
        shift: false,
        alt: false,
        ctrl: false,
        meta: false,
    };

    /// Whether no modifier is held.
    pub fn is_empty(&self) -> bool {
        *self == Self::NONE
    }

    /// xterm modifier parameter (`1 + bits`), 1 when none.
    pub fn xterm_param(&self) -> u8 {
        1 + u8::from(self.shift)
            + 2 * u8::from(self.alt)
            + 4 * u8::from(self.ctrl)
            + 8 * u8::from(self.meta)
    }
}

/// A key, already mapped by the UI.
///
/// For printable keys pass the character the keyboard layout produced
/// (including Shift), e.g. `Key::Char('A')` for Shift+a.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// A printable character.
    Char(char),
    /// Return / Enter.
    Enter,
    /// Tab.
    Tab,
    /// Backspace.
    Backspace,
    /// Escape.
    Escape,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Insert.
    Insert,
    /// Delete (forward).
    Delete,
    /// Function key F1..=F20.
    F(u8),
    /// Keypad digit 0..=9.
    KeypadDigit(u8),
    /// Keypad Enter.
    KeypadEnter,
    /// Keypad `.`.
    KeypadDecimal,
    /// Keypad `+`.
    KeypadPlus,
    /// Keypad `-`.
    KeypadMinus,
    /// Keypad `*`.
    KeypadMultiply,
    /// Keypad `/`.
    KeypadDivide,
    /// Keypad `=`.
    KeypadEquals,
}

/// Encodes a key press; `None` when the key produces no input (e.g. F99).
pub fn encode_key(key: Key, mods: Modifiers, modes: &TerminalModes) -> Option<Vec<u8>> {
    let m = mods.xterm_param();
    let mut out = Vec::new();
    let csi_mod = |out: &mut Vec<u8>, num: &str, fin: u8| {
        out.extend_from_slice(b"\x1b[");
        out.extend_from_slice(num.as_bytes());
        if m > 1 {
            out.extend_from_slice(format!(";{m}").as_bytes());
        }
        out.push(fin);
    };
    // Cursor-type keys: SS3 in application mode without modifiers, CSI otherwise.
    let cursor = |out: &mut Vec<u8>, fin: u8, app: bool| {
        if m > 1 {
            out.extend_from_slice(format!("\x1b[1;{m}").as_bytes());
            out.push(fin);
        } else if app {
            out.extend_from_slice(&[0x1b, b'O', fin]);
        } else {
            out.extend_from_slice(&[0x1b, b'[', fin]);
        }
    };
    let other_keys = |code: u32, out: &mut Vec<u8>| {
        out.extend_from_slice(format!("\x1b[27;{m};{code}~").as_bytes());
    };
    match key {
        Key::Up => cursor(&mut out, b'A', modes.app_cursor),
        Key::Down => cursor(&mut out, b'B', modes.app_cursor),
        Key::Right => cursor(&mut out, b'C', modes.app_cursor),
        Key::Left => cursor(&mut out, b'D', modes.app_cursor),
        Key::Home => cursor(&mut out, b'H', modes.app_cursor),
        Key::End => cursor(&mut out, b'F', modes.app_cursor),
        Key::Insert => csi_mod(&mut out, "2", b'~'),
        Key::Delete => csi_mod(&mut out, "3", b'~'),
        Key::PageUp => csi_mod(&mut out, "5", b'~'),
        Key::PageDown => csi_mod(&mut out, "6", b'~'),
        Key::F(n @ 1..=4) => {
            let fin = b"PQRS"[usize::from(n - 1)];
            if m > 1 {
                out.extend_from_slice(format!("\x1b[1;{m}").as_bytes());
                out.push(fin);
            } else {
                out.extend_from_slice(&[0x1b, b'O', fin]);
            }
        }
        Key::F(n @ 5..=20) => {
            const CODES: [u8; 16] = [
                15, 17, 18, 19, 20, 21, 23, 24, 25, 26, 28, 29, 31, 32, 33, 34,
            ];
            csi_mod(&mut out, &CODES[usize::from(n - 5)].to_string(), b'~');
        }
        Key::F(_) => return None,
        Key::KeypadEnter if modes.app_keypad && mods.is_empty() => {
            out.extend_from_slice(b"\x1bOM");
        }
        Key::Enter | Key::KeypadEnter => {
            if modes.modify_other_keys >= 1 && (mods.ctrl || mods.shift) {
                other_keys(13, &mut out);
            } else {
                if mods.alt {
                    out.push(0x1b);
                }
                out.push(b'\r');
                if modes.line_feed_new_line {
                    out.push(b'\n');
                }
            }
        }
        Key::Tab => {
            if mods.shift && !mods.ctrl && !mods.alt {
                out.extend_from_slice(b"\x1b[Z");
            } else if modes.modify_other_keys >= 1 && (mods.ctrl || mods.shift) {
                other_keys(9, &mut out);
            } else {
                if mods.alt {
                    out.push(0x1b);
                }
                out.push(b'\t');
            }
        }
        Key::Backspace => {
            if modes.modify_other_keys >= 2
                && !mods.is_empty()
                && !(mods.alt && !mods.ctrl && !mods.shift)
            {
                other_keys(127, &mut out);
            } else {
                if mods.alt {
                    out.push(0x1b);
                }
                out.push(if mods.ctrl { 0x08 } else { 0x7f });
            }
        }
        Key::Escape => {
            if modes.modify_other_keys >= 2 && !mods.is_empty() {
                other_keys(27, &mut out);
            } else {
                if mods.alt {
                    out.push(0x1b);
                }
                out.push(0x1b);
            }
        }
        Key::KeypadDigit(d) if d <= 9 => keypad(&mut out, b'0' + d, b'p' + d, modes, mods),
        Key::KeypadDigit(_) => return None,
        Key::KeypadDecimal => keypad(&mut out, b'.', b'n', modes, mods),
        Key::KeypadPlus => keypad(&mut out, b'+', b'k', modes, mods),
        Key::KeypadMinus => keypad(&mut out, b'-', b'm', modes, mods),
        Key::KeypadMultiply => keypad(&mut out, b'*', b'j', modes, mods),
        Key::KeypadDivide => keypad(&mut out, b'/', b'o', modes, mods),
        Key::KeypadEquals => keypad(&mut out, b'=', b'X', modes, mods),
        Key::Char(c) => encode_char(c, mods, modes, &mut out),
    }
    Some(out)
}

fn keypad(out: &mut Vec<u8>, normal: u8, app: u8, modes: &TerminalModes, mods: Modifiers) {
    if modes.app_keypad && mods.is_empty() {
        out.extend_from_slice(&[0x1b, b'O', app]);
    } else {
        out.push(normal);
    }
}

/// The C0 control a Ctrl+`c` chord produces, if it has one.
fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        '@' | ' ' | '2' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' | '~' => 0x1e,
        '_' | '7' | '/' | '-' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

fn encode_char(c: char, mods: Modifiers, modes: &TerminalModes, out: &mut Vec<u8>) {
    let plain_shift = !mods.ctrl && !mods.alt && !mods.meta;
    if plain_shift {
        let mut buf = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        return;
    }
    let level = modes.modify_other_keys;
    if mods.ctrl {
        match ctrl_byte(c) {
            // Level 2 reports every modified key; letters with plain Ctrl stay C0 like xterm.
            Some(b) if level < 2 || (c.is_ascii_lowercase() && !mods.shift && !mods.alt) => {
                if mods.alt {
                    out.push(0x1b);
                }
                out.push(b);
                return;
            }
            Some(_) => {}
            None if level == 0 => {
                // No control code: send the character itself (with ESC for Alt).
                if mods.alt {
                    out.push(0x1b);
                }
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                return;
            }
            None => {}
        }
        out.extend_from_slice(format!("\x1b[27;{};{}~", mods.xterm_param(), c as u32).as_bytes());
        return;
    }
    if level >= 2 && (mods.alt || mods.meta) {
        out.extend_from_slice(format!("\x1b[27;{};{}~", mods.xterm_param(), c as u32).as_bytes());
        return;
    }
    // Alt (and Meta, which most terminals treat like Alt) prefixes ESC.
    out.push(0x1b);
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Whether pasted text keeps `c`: everything except control characters (C0, DEL, C1),
/// with tab and line endings allowed.
fn paste_keeps(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r') || !c.is_control()
}

/// Removes the control characters a paste must not deliver: every C0 control except
/// tab, line feed and carriage return, plus DEL and C1 controls. Without bracketed paste a
/// pasted `\x0f` (Ctrl+O) or `\x03` would act as a key press in the shell; with bracketed
/// paste ESC is what could end the paste early, and the rest are never legitimate text.
/// The same filter applies in both modes.
pub fn sanitize_paste(text: &str) -> String {
    text.chars().filter(|&c| paste_keeps(c)).collect()
}

/// Whether pasting `text` should be confirmed by the user before it is sent: the program
/// did not enable bracketed paste (`bracketed` is false, so the shell sees the paste as
/// typing) and the text contains a line ending (each line would run at once) or a control
/// character that [`sanitize_paste`] removes (the user should see that the clipboard held
/// more than what will be pasted). With bracketed paste nothing runs before the program
/// decides, so no confirmation is needed. User settings (`terminal.confirm_multiline_paste`,
/// "don't ask again") are the caller's to apply on top.
pub fn paste_needs_confirmation(text: &str, bracketed: bool) -> bool {
    !bracketed
        && text
            .chars()
            .any(|c| matches!(c, '\n' | '\r') || !paste_keeps(c))
}

/// Encodes pasted text.
///
/// Control characters are removed first ([`sanitize_paste`]). Line endings become CR
/// (what the Enter key sends). With bracketed paste the text is wrapped in `ESC[200~` /
/// `ESC[201~`; since ESC is removed no embedded marker can survive, and every embedded
/// start/end marker is still removed (repeatedly, so nested fragments cannot reassemble
/// into a marker): otherwise pasted text could end the paste early and have the rest
/// executed as typed commands.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let normalized = sanitize_paste(text)
        .replace("\r\n", "\r")
        .replace('\n', "\r");
    let mut bytes = normalized.into_bytes();
    if !bracketed {
        return bytes;
    }
    loop {
        let before = bytes.len();
        bytes = remove_all(&bytes, PASTE_END);
        bytes = remove_all(&bytes, PASTE_START);
        if bytes.len() == before {
            break;
        }
    }
    let mut out = Vec::with_capacity(bytes.len() + 12);
    out.extend_from_slice(PASTE_START);
    out.extend_from_slice(&bytes);
    out.extend_from_slice(PASTE_END);
    out
}

fn remove_all(haystack: &[u8], needle: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(needle) {
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

/// Focus change report (`CSI I` / `CSI O`) when the program enabled `?1004`.
pub fn encode_focus(focused: bool, modes: &TerminalModes) -> Option<Vec<u8>> {
    modes.focus_reporting.then(|| {
        if focused {
            b"\x1b[I".to_vec()
        } else {
            b"\x1b[O".to_vec()
        }
    })
}

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    /// Primary button.
    Left,
    /// Middle button / wheel click.
    Middle,
    /// Secondary button.
    Right,
}

impl MouseButton {
    fn code(self) -> u32 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        }
    }
}

/// What happened with the mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseEventKind {
    /// Button pressed.
    Press(MouseButton),
    /// Button released.
    Release(MouseButton),
    /// Pointer moved, with the held button if any.
    Move(Option<MouseButton>),
    /// Wheel scrolled up (towards history) by one notch.
    WheelUp,
    /// Wheel scrolled down by one notch.
    WheelDown,
}

/// A mouse event at a viewport cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MouseEvent {
    /// Kind of event.
    pub kind: MouseEventKind,
    /// Viewport row (0-based).
    pub row: u32,
    /// Column (0-based).
    pub col: u32,
    /// Modifiers held.
    pub mods: Modifiers,
}

/// Encodes a mouse event for the program, or `None` when the current mouse
/// mode does not report it (the UI should then handle it itself).
///
/// Wheel events on the alternate screen without mouse reporting become arrow
/// keys when alternate-scroll mode (`?1007`) is on, like xterm.
pub fn encode_mouse(event: MouseEvent, modes: &TerminalModes) -> Option<Vec<u8>> {
    let mode = modes.mouse;
    if mode == MouseMode::None {
        if modes.alt_screen && modes.alternate_scroll {
            let key = match event.kind {
                MouseEventKind::WheelUp => Key::Up,
                MouseEventKind::WheelDown => Key::Down,
                _ => return None,
            };
            return encode_key(key, Modifiers::NONE, modes);
        }
        return None;
    }
    let (mut code, release) = match event.kind {
        MouseEventKind::Press(b) => (b.code(), false),
        MouseEventKind::Release(b) => {
            if mode == MouseMode::X10 {
                return None;
            }
            (if modes.sgr_mouse { b.code() } else { 3 }, true)
        }
        MouseEventKind::Move(held) => match (mode, held) {
            (MouseMode::Motion, None) => (3 + 32, false),
            (MouseMode::Motion | MouseMode::Drag, Some(b)) => (b.code() + 32, false),
            _ => return None,
        },
        MouseEventKind::WheelUp => (64, false),
        MouseEventKind::WheelDown => (65, false),
    };
    if mode != MouseMode::X10 {
        if event.mods.shift {
            code += 4;
        }
        if event.mods.alt || event.mods.meta {
            code += 8;
        }
        if event.mods.ctrl {
            code += 16;
        }
    } else if matches!(event.kind, MouseEventKind::Move(_)) {
        return None;
    }
    let (x, y) = (event.col.saturating_add(1), event.row.saturating_add(1));
    if modes.sgr_mouse {
        let fin = if release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{x};{y}{fin}").into_bytes());
    }
    let mut out = b"\x1b[M".to_vec();
    if modes.utf8_mouse {
        for v in [code + 32, x + 32, y + 32] {
            let c = char::from_u32(v.min(2047))?;
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    } else {
        // Classic encoding cannot address beyond column/row 223.
        if x > 223 || y > 223 {
            return None;
        }
        out.extend_from_slice(&[(code + 32) as u8, (x + 32) as u8, (y + 32) as u8]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(key: Key, mods: Modifiers) -> Vec<u8> {
        encode_key(key, mods, &TerminalModes::default()).unwrap_or_default()
    }

    const CTRL: Modifiers = Modifiers {
        shift: false,
        alt: false,
        ctrl: true,
        meta: false,
    };
    const ALT: Modifiers = Modifiers {
        shift: false,
        alt: true,
        ctrl: false,
        meta: false,
    };
    const SHIFT: Modifiers = Modifiers {
        shift: true,
        alt: false,
        ctrl: false,
        meta: false,
    };
    const NONE: Modifiers = Modifiers::NONE;

    #[test]
    fn key_table() {
        let table: &[(Key, Modifiers, &[u8])] = &[
            (Key::Char('a'), NONE, b"a"),
            (Key::Char('A'), SHIFT, b"A"),
            (Key::Char('\u{e9}'), NONE, "\u{e9}".as_bytes()),
            (Key::Char('c'), CTRL, b"\x03"),
            (Key::Char(' '), CTRL, b"\x00"),
            (Key::Char('['), CTRL, b"\x1b"),
            (Key::Char('?'), CTRL, b"\x7f"),
            (Key::Char('x'), ALT, b"\x1bx"),
            (
                Key::Char('c'),
                Modifiers {
                    ctrl: true,
                    alt: true,
                    ..NONE
                },
                b"\x1b\x03",
            ),
            (Key::Enter, NONE, b"\r"),
            (Key::Enter, ALT, b"\x1b\r"),
            (Key::Tab, NONE, b"\t"),
            (Key::Tab, SHIFT, b"\x1b[Z"),
            (Key::Backspace, NONE, b"\x7f"),
            (Key::Backspace, CTRL, b"\x08"),
            (Key::Backspace, ALT, b"\x1b\x7f"),
            (Key::Escape, NONE, b"\x1b"),
            (Key::Up, NONE, b"\x1b[A"),
            (Key::Left, CTRL, b"\x1b[1;5D"),
            (
                Key::Right,
                Modifiers {
                    shift: true,
                    alt: true,
                    ..NONE
                },
                b"\x1b[1;4C",
            ),
            (Key::Home, NONE, b"\x1b[H"),
            (Key::End, SHIFT, b"\x1b[1;2F"),
            (Key::PageUp, NONE, b"\x1b[5~"),
            (Key::PageDown, CTRL, b"\x1b[6;5~"),
            (Key::Insert, NONE, b"\x1b[2~"),
            (Key::Delete, NONE, b"\x1b[3~"),
            (Key::F(1), NONE, b"\x1bOP"),
            (Key::F(4), SHIFT, b"\x1b[1;2S"),
            (Key::F(5), NONE, b"\x1b[15~"),
            (Key::F(12), CTRL, b"\x1b[24;5~"),
            (Key::F(20), NONE, b"\x1b[34~"),
            (Key::KeypadDigit(5), NONE, b"5"),
            (Key::KeypadEnter, NONE, b"\r"),
        ];
        for (key, mods, want) in table {
            assert_eq!(k(*key, *mods), want.to_vec(), "{key:?} {mods:?}");
        }
        assert_eq!(
            encode_key(Key::F(21), NONE, &TerminalModes::default()),
            None
        );
    }

    #[test]
    fn application_modes() {
        let modes = TerminalModes {
            app_cursor: true,
            app_keypad: true,
            ..TerminalModes::default()
        };
        assert_eq!(encode_key(Key::Up, NONE, &modes), Some(b"\x1bOA".to_vec()));
        assert_eq!(
            encode_key(Key::Home, NONE, &modes),
            Some(b"\x1bOH".to_vec())
        );
        assert_eq!(
            encode_key(Key::Up, CTRL, &modes),
            Some(b"\x1b[1;5A".to_vec())
        );
        assert_eq!(
            encode_key(Key::KeypadDigit(1), NONE, &modes),
            Some(b"\x1bOq".to_vec())
        );
        assert_eq!(
            encode_key(Key::KeypadEnter, NONE, &modes),
            Some(b"\x1bOM".to_vec())
        );
        let lnm = TerminalModes {
            line_feed_new_line: true,
            ..TerminalModes::default()
        };
        assert_eq!(encode_key(Key::Enter, NONE, &lnm), Some(b"\r\n".to_vec()));
    }

    #[test]
    fn modify_other_keys() {
        let l1 = TerminalModes {
            modify_other_keys: 1,
            ..TerminalModes::default()
        };
        let l2 = TerminalModes {
            modify_other_keys: 2,
            ..TerminalModes::default()
        };
        assert_eq!(
            encode_key(Key::Char('c'), CTRL, &l1),
            Some(b"\x03".to_vec())
        );
        assert_eq!(
            encode_key(Key::Char('1'), CTRL, &l1),
            Some(b"\x1b[27;5;49~".to_vec())
        );
        assert_eq!(
            encode_key(Key::Enter, CTRL, &l1),
            Some(b"\x1b[27;5;13~".to_vec())
        );
        assert_eq!(
            encode_key(Key::Char('1'), CTRL, &TerminalModes::default()),
            Some(b"1".to_vec())
        );
        assert_eq!(
            encode_key(Key::Char('c'), CTRL, &l2),
            Some(b"\x03".to_vec())
        );
        assert_eq!(
            encode_key(
                Key::Char('C'),
                Modifiers {
                    ctrl: true,
                    shift: true,
                    ..NONE
                },
                &l2
            ),
            Some(b"\x1b[27;6;67~".to_vec())
        );
        assert_eq!(
            encode_key(Key::Char('x'), ALT, &l2),
            Some(b"\x1b[27;3;120~".to_vec())
        );
        assert_eq!(
            encode_key(Key::Tab, CTRL, &l2),
            Some(b"\x1b[27;5;9~".to_vec())
        );
    }

    #[test]
    fn paste() {
        assert_eq!(encode_paste("a\nb\r\nc", false), b"a\rb\rc".to_vec());
        assert_eq!(encode_paste("ls", true), b"\x1b[200~ls\x1b[201~".to_vec());
        let evil = "echo hi\x1b[201~rm -rf ~\n";
        let out = encode_paste(evil, true);
        assert_eq!(out, b"\x1b[200~echo hi[201~rm -rf ~\r\x1b[201~".to_vec());
        // Fragments that reassemble into a marker after one removal pass.
        let nested = "a\x1b[20\x1b[201~1~b";
        let out = encode_paste(nested, true);
        let inner = &out[PASTE_START.len()..out.len() - PASTE_END.len()];
        assert!(!inner.windows(PASTE_END.len()).any(|w| w == PASTE_END));
        assert!(!inner.windows(PASTE_START.len()).any(|w| w == PASTE_START));
    }

    #[test]
    fn paste_strips_control_characters() {
        // Ctrl+O (bash: operate-and-get-next), Ctrl+C, DEL and a C1 CSI are dropped; tab
        // and line endings are kept, with or without bracketed paste.
        let text = "ls\x0f -l\x03\x7f\u{9b}\tx\r\ny\n";
        assert_eq!(sanitize_paste(text), "ls -l\tx\r\ny\n");
        assert_eq!(encode_paste(text, false), b"ls -l\tx\ry\r".to_vec());
        assert_eq!(
            encode_paste("a\x1bb\x00c", true),
            b"\x1b[200~abc\x1b[201~".to_vec()
        );
    }

    #[test]
    fn paste_confirmation_predicate() {
        let cases = [
            ("echo hi", false, false),
            ("echo hi\n", false, true),
            ("echo hi\r", false, true),
            ("a\nb", true, false),
            ("ls\x0f", false, true),
            ("ls\x1b[A", false, true),
            ("tab\there", false, false),
            ("ls\x0f", true, false),
            ("caf\u{e9} \u{1F600}", false, false),
        ];
        for (text, bracketed, want) in cases {
            assert_eq!(
                paste_needs_confirmation(text, bracketed),
                want,
                "{text:?} bracketed={bracketed}"
            );
        }
    }

    #[test]
    fn focus() {
        let on = TerminalModes {
            focus_reporting: true,
            ..TerminalModes::default()
        };
        assert_eq!(encode_focus(true, &on), Some(b"\x1b[I".to_vec()));
        assert_eq!(encode_focus(false, &on), Some(b"\x1b[O".to_vec()));
        assert_eq!(encode_focus(true, &TerminalModes::default()), None);
    }

    fn ev(kind: MouseEventKind, row: u32, col: u32, mods: Modifiers) -> MouseEvent {
        MouseEvent {
            kind,
            row,
            col,
            mods,
        }
    }

    #[test]
    fn mouse_encodings() {
        let none = TerminalModes::default();
        let press = ev(MouseEventKind::Press(MouseButton::Left), 2, 4, NONE);
        assert_eq!(encode_mouse(press, &none), None);

        let click = TerminalModes {
            mouse: MouseMode::Click,
            ..none
        };
        assert_eq!(
            encode_mouse(press, &click),
            Some(b"\x1b[M\x20\x25\x23".to_vec())
        );
        let rel = ev(MouseEventKind::Release(MouseButton::Left), 2, 4, NONE);
        assert_eq!(
            encode_mouse(rel, &click),
            Some(b"\x1b[M\x23\x25\x23".to_vec())
        );
        let moved = ev(MouseEventKind::Move(Some(MouseButton::Left)), 0, 0, NONE);
        assert_eq!(encode_mouse(moved, &click), None);
        assert_eq!(
            encode_mouse(
                ev(MouseEventKind::Press(MouseButton::Left), 0, 300, NONE),
                &click
            ),
            None
        );

        let sgr = TerminalModes {
            mouse: MouseMode::Drag,
            sgr_mouse: true,
            ..none
        };
        assert_eq!(encode_mouse(press, &sgr), Some(b"\x1b[<0;5;3M".to_vec()));
        assert_eq!(encode_mouse(rel, &sgr), Some(b"\x1b[<0;5;3m".to_vec()));
        assert_eq!(encode_mouse(moved, &sgr), Some(b"\x1b[<32;1;1M".to_vec()));
        assert_eq!(
            encode_mouse(ev(MouseEventKind::Move(None), 0, 0, NONE), &sgr),
            None
        );
        let ctrl_right = ev(MouseEventKind::Press(MouseButton::Right), 0, 299, CTRL);
        assert_eq!(
            encode_mouse(ctrl_right, &sgr),
            Some(b"\x1b[<18;300;1M".to_vec())
        );
        assert_eq!(
            encode_mouse(ev(MouseEventKind::WheelDown, 0, 0, NONE), &sgr),
            Some(b"\x1b[<65;1;1M".to_vec())
        );

        let any = TerminalModes {
            mouse: MouseMode::Motion,
            sgr_mouse: true,
            ..none
        };
        assert_eq!(
            encode_mouse(ev(MouseEventKind::Move(None), 1, 1, NONE), &any),
            Some(b"\x1b[<35;2;2M".to_vec())
        );

        let x10 = TerminalModes {
            mouse: MouseMode::X10,
            ..none
        };
        assert_eq!(
            encode_mouse(
                ev(MouseEventKind::Press(MouseButton::Left), 0, 0, CTRL),
                &x10
            ),
            Some(b"\x1b[M\x20\x21\x21".to_vec())
        );
        assert_eq!(encode_mouse(rel, &x10), None);

        let utf8 = TerminalModes {
            mouse: MouseMode::Click,
            utf8_mouse: true,
            ..none
        };
        let far = ev(MouseEventKind::Press(MouseButton::Left), 0, 299, NONE);
        let mut want = b"\x1b[M ".to_vec();
        want.extend_from_slice("\u{14c}".as_bytes());
        want.push(b'!');
        assert_eq!(encode_mouse(far, &utf8), Some(want));
    }

    #[test]
    fn alternate_scroll_wheel() {
        let alt = TerminalModes {
            alt_screen: true,
            alternate_scroll: true,
            ..TerminalModes::default()
        };
        let up = ev(MouseEventKind::WheelUp, 0, 0, NONE);
        assert_eq!(encode_mouse(up, &alt), Some(b"\x1b[A".to_vec()));
        let app = TerminalModes {
            app_cursor: true,
            ..alt
        };
        assert_eq!(
            encode_mouse(ev(MouseEventKind::WheelDown, 0, 0, NONE), &app),
            Some(b"\x1bOB".to_vec())
        );
    }
}
