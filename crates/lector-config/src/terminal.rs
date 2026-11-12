//! Integrated terminal settings (v0.7, `[terminal]`).
//!
//! The shell program, its arguments, profiles and environment are **user-only**: a project's
//! `.lector/settings.toml` cannot set them (opening a project must never choose a program to
//! run), see `strip_user_only` in the settings module.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A program and its arguments (`terminal.shell`, profiles).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TerminalShell {
    /// Program to run (absolute, or looked up on `PATH`).
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
}

/// A named shell profile (`[[terminal.profiles]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TerminalProfile {
    /// Name shown in menus and on tabs.
    pub name: String,
    /// Program to run.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Extra environment variables for this profile.
    pub env: BTreeMap<String, String>,
}

/// Cursor shape of the terminal (programs may still change it with DECSCUSR).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TerminalCursorStyle {
    /// Filled block.
    #[default]
    Block,
    /// Vertical bar.
    Beam,
    /// Underline.
    Underline,
}

/// Where "New Terminal" starts when no folder was picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NewTerminalCwd {
    /// The project root (the home directory without a project).
    #[default]
    Project,
    /// The folder of the active file (the project root when no file is active).
    ActiveFile,
}

/// `[terminal]` settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSettings {
    /// Shell to run; unset = the platform default (`$SHELL`, pwsh/powershell/cmd). User-only.
    pub shell: Option<TerminalShell>,
    /// Named profiles offered by "New Terminal with Profile". User-only.
    pub profiles: Vec<TerminalProfile>,
    /// Profile used by "New Terminal" (by name); unset = `shell` / the platform default.
    pub default_profile: Option<String>,
    /// Extra environment for every terminal. User-only.
    pub env: BTreeMap<String, String>,
    /// Font family; unset = the code font.
    pub font_family: Option<String>,
    /// Font size in px; unset = the code font size.
    pub font_size: Option<f32>,
    /// Line height multiplier; unset = 1.2 (terminals pack lines tighter than code).
    pub line_height: Option<f32>,
    /// Lines of scrollback kept per terminal.
    pub scrollback: usize,
    /// Default cursor shape.
    pub cursor_style: TerminalCursorStyle,
    /// Blink the cursor (honours reduced motion).
    pub cursor_blink: bool,
    /// Copy the selection to the clipboard as soon as it is made.
    pub copy_on_select: bool,
    /// Ctrl+C copies when there is a selection (Linux/Windows); otherwise it always goes to the
    /// shell (interrupt). Ctrl+Shift+C always copies.
    pub ctrl_c_copies_selection: bool,
    /// macOS: Option sends Meta (ESC prefix) instead of typing special characters.
    pub option_as_meta: bool,
    /// Ask before pasting several lines when the program did not enable bracketed paste.
    pub confirm_multiline_paste: bool,
    /// Where "New Terminal" starts: "project" or "active_file".
    pub new_terminal_cwd: NewTerminalCwd,
    /// Load Lector's shell integration (working directory and command marks; bash, zsh, fish).
    pub shell_integration: bool,
    /// Reopen terminal tabs in their last folder (fresh shells, no scrollback).
    pub restore: bool,
    /// Close the tab as soon as the shell exits (otherwise it stays until a key or close).
    pub close_on_exit: bool,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            shell: None,
            profiles: Vec::new(),
            default_profile: None,
            env: BTreeMap::new(),
            font_family: None,
            font_size: None,
            line_height: None,
            scrollback: 10_000,
            cursor_style: TerminalCursorStyle::Block,
            cursor_blink: true,
            copy_on_select: false,
            ctrl_c_copies_selection: false,
            option_as_meta: false,
            confirm_multiline_paste: true,
            new_terminal_cwd: NewTerminalCwd::Project,
            shell_integration: true,
            restore: true,
            close_on_exit: false,
        }
    }
}

/// Keys of `[terminal]` only the user's own settings file may set.
pub(crate) const USER_ONLY_TERMINAL_KEYS: &[&str] =
    &["shell", "profiles", "default_profile", "env"];
