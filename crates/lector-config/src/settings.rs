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
}

impl Default for GitSettings {
    fn default() -> Self {
        Self {
            blame_gutter: false,
            show_gutter: true,
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
    /// Session options.
    pub session: SessionSettings,
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
            session: SessionSettings::default(),
        }
    }
}

const DEFAULT_SETTINGS_TOML: &str = r#"# Lector settings. Every key is shown with its default value.
# Edit your copy (`settings.toml` in the config dir); a project may override
# values in `<project>/.lector/settings.toml`. Unknown keys only produce warnings.

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

[session]
# Reopen the tabs and layout from the last session.
restore = true
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

pub(crate) fn load_settings_from(
    user_file: &Path,
    project_root: Option<&Path>,
) -> (Settings, Vec<String>) {
    let mut warnings = Vec::new();
    let mut merged = toml::Table::new();
    let project_file = project_root.map(|r| r.join(".lector").join("settings.toml"));
    for file in std::iter::once(user_file.to_path_buf()).chain(project_file) {
        if let Some(layer) = read_layer(&file, &mut warnings) {
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
    fn default_toml_parses_to_defaults() {
        let s: Settings = toml::from_str(default_settings_toml()).unwrap();
        assert_eq!(s, Settings::default());
        let mut w = Vec::new();
        assert!(parse_layer("d", default_settings_toml(), &mut w).is_some());
        assert!(w.is_empty(), "{w:?}");
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
