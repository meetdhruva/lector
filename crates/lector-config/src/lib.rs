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

pub use keymap::{default_keymap, load_keymap, merge_keymap, KeyBinding, Platform};
pub use recent::{RecentProjects, MAX_RECENT_PROJECTS};
pub use review::{content_hash, ReviewBackend, ReviewStore};
pub use session::{
    session_key, Axis, PaneState, Session, SessionStore, TabKind, TabState, WindowBounds,
};
pub use settings::{
    default_settings_toml, load_settings, load_settings_for, AutoSave, DiffLayout, DiffSettings,
    EditorSettings, FileSettings, GitSettings, LspSettings, ServerOverride, SessionSettings,
    Settings, ShowWhitespace, ThemeMode, ThemeSettings,
};
