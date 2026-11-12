//! UI-free terminal sessions for Lector: PTY, VT emulation, scrollback, damage
//! tracking, shell integration.
//!
//! * [`Terminal`] — a running session (PTY + shell + threads); see
//!   [`session`] for the threading model.
//! * [`Emulator`] — the headless core (bytes in, [`Snapshot`]s and
//!   [`TerminalEvent`]s out); `Terminal::lock()` gives access to it.
//! * [`Snapshot`] — an immutable frame: rows of [`Cell`]s, [`Damage`],
//!   cursor, selection, modes; [`SnapshotRow::runs`] coalesces styled runs.
//! * [`input`] — key/paste/mouse/focus encoders.
//! * [`links`] — URL and file-reference detection and resolution.
//! * [`shell`] — profiles, spawn options, opt-in shell integration.
//!
//! No `alacritty_terminal` or `portable-pty` type appears in the public API.
//!
//! Security rules: nothing received from the program is executed; OSC 52
//! clipboard *reads* are denied (copies are reported as events for the UI to
//! confirm or apply); titles are sanitised; link targets are plain data the UI
//! must confirm before opening; pasted text cannot terminate bracketed paste.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod emulator;
pub mod error;
pub mod geometry;
pub mod input;
pub mod links;
mod osc;
#[cfg(unix)]
mod process_tree;
mod scan;
pub mod session;
pub mod shell;
pub mod snapshot;

use std::path::PathBuf;

pub use emulator::{
    CommandBlock, Emulator, EmulatorConfig, QueryColors, SearchMatch, SearchQuery, SelectionKind,
    DEFAULT_WORD_SEPARATORS,
};
pub use error::TerminalError;
pub use geometry::{
    grid_to_viewport, viewport_to_grid, Direction, GridPoint, ScrollAmount, Side, TermSize,
};
pub use input::{Key, Modifiers, MouseButton, MouseEvent, MouseEventKind};
pub use links::{resolve_link, LinkCandidate, LinkSource, LinkTarget};
pub use osc::{sanitize_title, MAX_TITLE_CHARS};
pub use session::{EmulatorGuard, ExitInfo, Terminal, WakeFn};
pub use shell::{ShellKind, ShellProfile, SpawnOptions};
pub use snapshot::{
    Attrs, Cell, CellStyle, CellWidth, CursorShape, CursorState, Damage, Hyperlink, MouseMode,
    SelectionSpan, Snapshot, SnapshotRow, TermColor, TerminalModes, TextRun, UnderlineStyle,
};

/// Default scrollback length in lines.
pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// Something the owner of a terminal should react to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// Content changed: take a [`Snapshot`]. Coalesced: not sent again until
    /// [`Terminal::snapshot`] is called.
    Wakeup,
    /// The program set (`Some`, sanitised) or reset (`None`) the title.
    TitleChanged(Option<String>),
    /// BEL.
    Bell,
    /// The shell reported a new working directory (OSC 7).
    CwdChanged(PathBuf),
    /// OSC 133 `A`: a prompt starts.
    PromptStarted,
    /// OSC 133 `B`: the user is typing a command.
    CommandStarted,
    /// OSC 133 `C`: the command runs; output follows.
    CommandOutputStarted,
    /// OSC 133 `D`: the command finished.
    CommandFinished {
        /// Exit code, when the shell reported one.
        exit_code: Option<i32>,
    },
    /// The program asked to copy text to the clipboard (OSC 52). The UI
    /// decides whether to honour it.
    ClipboardCopy {
        /// Text to copy.
        text: String,
        /// Target is the primary selection rather than the clipboard.
        primary_selection: bool,
    },
    /// The program tried to read the clipboard (OSC 52 `?`); it was denied.
    ClipboardReadDenied,
    /// The child exited; the session stays readable.
    Exited(ExitInfo),
}
