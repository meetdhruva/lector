//! Lector configuration: settings, keymaps and platform paths.
//!
//! Nothing here touches the user's project files. Lector's own state (sessions, recent
//! projects, review marks, notices...) is not here: it lives in one database behind
//! `lector_app::store` (ADR 0004). This crate only parses the files people edit
//! (`settings.toml`, `keymap.toml`, a project's `.lector/settings.toml`) and says where
//! Lector's folders are.
#![forbid(unsafe_code)]

mod keymap;
pub mod paths;
mod settings;
mod terminal;
mod updates;

pub use keymap::{default_keymap, load_keymap, merge_keymap, KeyBinding, Platform};
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
    should_check_now, UpdateChannel, UpdateCheck, UpdatesSettings, CHECK_INTERVAL_SECS,
};
