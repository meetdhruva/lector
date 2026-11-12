//! Seam for the integrated terminal (v0.7, `lector-ui-terminal`): context menus in
//! `lector-ui` (file tree, editor tabs) offer "Open in Terminal" when a handler is
//! installed, without depending on the terminal crate.

use std::path::PathBuf;
use std::rc::Rc;

use gpui::{App, Global, Window};

type OpenInTerminal = Rc<dyn Fn(PathBuf, &mut Window, &mut App)>;

struct TerminalHook {
    open: OpenInTerminal,
}

impl Global for TerminalHook {}

/// Installs the handler run by "Open in Terminal" (`path` is a folder, or a file whose
/// folder the terminal starts in). Called by `lector_ui_terminal::init`.
pub fn set_open_in_terminal(cx: &mut App, f: impl Fn(PathBuf, &mut Window, &mut App) + 'static) {
    cx.set_global(TerminalHook { open: Rc::new(f) });
}

/// Whether "Open in Terminal" entries should be shown.
pub fn has_terminal(cx: &App) -> bool {
    cx.has_global::<TerminalHook>()
}

/// Opens a terminal in `path` (a folder, or a file's folder). Does nothing without a
/// handler. Never runs anything in the terminal: only the starting folder is set.
pub fn open_in_terminal(path: PathBuf, window: &mut Window, cx: &mut App) {
    let Some(open) = cx.try_global::<TerminalHook>().map(|h| h.open.clone()) else {
        return;
    };
    open(path, window, cx);
}
