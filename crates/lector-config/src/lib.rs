//! Lector configuration: settings, keymaps, platform paths, and persisted
//! session / review state.
//!
//! Nothing here touches the user's project files; all state lives under
//! [`paths::config_dir`] and [`paths::data_dir`].
#![forbid(unsafe_code)]

mod atomic;
mod keymap;
pub mod paths;
mod recent;
mod review;
mod session;
mod settings;
mod terminal;
mod updates;

pub use keymap::{default_keymap, load_keymap, merge_keymap, KeyBinding, Platform};
pub use recent::{RecentProjects, MAX_RECENT_PROJECTS};
pub use review::{content_hash, ReviewBackend, ReviewStore};
pub use session::{
    session_key, Axis, PaneState, Session, SessionStore, TabKind, TabState, WindowBounds,
};
pub use settings::{
    default_settings_toml, load_settings, load_settings_for, AgentServer, AgentSettings, AutoFetch,
    AutoSave, CompletionInsertMode, CompletionSettings, DiffLayout, DiffSettings, EditorSettings,
    FileSettings, ForgeSettings, GitSettings, LspSettings, ReviewSettings, ServerOverride,
    SessionSettings, Settings, ShowWhitespace, SignatureHelpSettings, ThemeMode, ThemeSettings,
    UiSettings,
};
pub use terminal::{
    NewTerminalCwd, TerminalCursorStyle, TerminalProfile, TerminalSettings, TerminalShell,
};
pub use updates::{
    should_check_now, update_state_file, UpdateChannel, UpdateCheck, UpdatesSettings,
    CHECK_INTERVAL_SECS,
};
