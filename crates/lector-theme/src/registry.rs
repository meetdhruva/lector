//! Theme registry: built-ins plus user themes loaded from TOML files.

use std::path::{Path, PathBuf};

use crate::builtin::{builtin_themes, LECTOR_DARK, LECTOR_LIGHT};
use crate::{Appearance, Color, TextStyle, Theme};

/// Error loading a user theme.
#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    /// The file could not be read.
    #[error("failed to read {path}: {source}")]
    Io {
        /// File that failed.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// The TOML is malformed or has invalid values.
    #[error("invalid theme{}: {message}", path.as_ref().map(|p| format!(" {}", p.display())).unwrap_or_default())]
    Invalid {
        /// File that failed, if loaded from disk.
        path: Option<PathBuf>,
        /// Human-readable reason.
        message: String,
    },
}

fn invalid(message: impl Into<String>) -> ThemeError {
    ThemeError::Invalid {
        path: None,
        message: message.into(),
    }
}

/// Holds all known themes by name, in insertion order.
#[derive(Clone, Debug)]
pub struct ThemeRegistry {
    themes: Vec<Theme>,
}

impl Default for ThemeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ThemeRegistry {
    /// Registry containing the four built-in themes.
    pub fn new() -> Self {
        ThemeRegistry {
            themes: builtin_themes(),
        }
    }

    /// All theme names in display order.
    pub fn names(&self) -> Vec<&str> {
        self.themes.iter().map(|t| t.name.as_str()).collect()
    }

    /// All themes in display order.
    pub fn themes(&self) -> &[Theme] {
        &self.themes
    }

    /// Theme by exact name, falling back to a case-insensitive match.
    pub fn get(&self, name: &str) -> Option<&Theme> {
        self.themes.iter().find(|t| t.name == name).or_else(|| {
            self.themes
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case(name))
        })
    }

    /// The default built-in theme for an appearance.
    pub fn default_for(&self, appearance: Appearance) -> &Theme {
        let name = match appearance {
            Appearance::Dark => LECTOR_DARK,
            Appearance::Light => LECTOR_LIGHT,
        };
        // Built-ins are never removed, so this lookup always succeeds; fall
        // back to the first theme defensively.
        self.get(name).unwrap_or(&self.themes[0])
    }

    /// Adds or replaces (by name) a theme.
    pub fn insert(&mut self, theme: Theme) {
        if let Some(existing) = self.themes.iter_mut().find(|t| t.name == theme.name) {
            *existing = theme;
        } else {
            self.themes.push(theme);
        }
    }

    /// Parses a TOML theme and registers it. See [`ThemeRegistry::parse_toml`].
    pub fn load_str(&mut self, toml_src: &str) -> Result<&Theme, ThemeError> {
        let theme = self.parse_toml(toml_src)?;
        let name = theme.name.clone();
        self.insert(theme);
        self.get(&name)
            .ok_or_else(|| invalid("theme vanished after insert"))
    }

    /// Loads every `*.toml` file in `dir` (sorted by file name). A missing
    /// directory is not an error. Returns the names loaded and per-file errors.
    pub fn load_dir(&mut self, dir: &Path) -> (Vec<String>, Vec<ThemeError>) {
        let mut loaded = Vec::new();
        let mut errors = Vec::new();
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (loaded, errors),
            Err(source) => {
                errors.push(ThemeError::Io {
                    path: dir.to_path_buf(),
                    source,
                });
                return (loaded, errors);
            }
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("toml"))
            })
            .collect();
        paths.sort();
        for path in paths {
            let src = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(source) => {
                    errors.push(ThemeError::Io { path, source });
                    continue;
                }
            };
            match self.load_str(&src) {
                Ok(theme) => loaded.push(theme.name.clone()),
                Err(ThemeError::Invalid { message, .. }) => errors.push(ThemeError::Invalid {
                    path: Some(path),
                    message,
                }),
                Err(e) => errors.push(e),
            }
        }
        (loaded, errors)
    }

    /// Parses a user theme without registering it.
    ///
    /// Format: top-level `name`, `appearance` ("light"/"dark") and optional
    /// `base` (name of a registered theme; defaults to the built-in for the
    /// appearance), then `[ui]`, `[syntax]`, `[diff]`, `[git]` tables that
    /// override individual colors of the base. Syntax entries are either a
    /// color string or `{ color = "#..", bold = true, italic = false }`.
    pub fn parse_toml(&self, src: &str) -> Result<Theme, ThemeError> {
        let table: toml::Table = toml::from_str(src).map_err(|e| invalid(e.to_string()))?;
        for key in table.keys() {
            if !matches!(
                key.as_str(),
                "name" | "appearance" | "base" | "ui" | "syntax" | "diff" | "git"
            ) {
                return Err(invalid(format!("unknown top-level key `{key}`")));
            }
        }
        let name = table
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| invalid("missing string `name`"))?
            .to_string();
        let appearance = match table.get("appearance").and_then(|v| v.as_str()) {
            Some(a) if a.eq_ignore_ascii_case("light") => Appearance::Light,
            Some(a) if a.eq_ignore_ascii_case("dark") => Appearance::Dark,
            Some(other) => return Err(invalid(format!("invalid appearance `{other}`"))),
            None => return Err(invalid("missing `appearance` (\"light\" or \"dark\")")),
        };
        let base = match table.get("base").and_then(|v| v.as_str()) {
            Some(base) => self
                .get(base)
                .ok_or_else(|| invalid(format!("unknown base theme `{base}`")))?,
            None => self.default_for(appearance),
        };

        let mut theme = base.clone();
        theme.name = name;
        theme.appearance = appearance;
        theme.ui = merge_section(&base.ui, table.get("ui"), "ui")?;
        theme.diff = merge_section(&base.diff, table.get("diff"), "diff")?;
        theme.git = merge_section(&base.git, table.get("git"), "git")?;
        if let Some(syntax) = table.get("syntax") {
            let syntax = syntax
                .as_table()
                .ok_or_else(|| invalid("`syntax` must be a table"))?;
            for (capture, value) in syntax {
                let style =
                    parse_style(value).map_err(|m| invalid(format!("syntax.{capture}: {m}")))?;
                // A user style for `a` also replaces inherited `a.*` styles
                // unless the user set those explicitly.
                let prefix = format!("{capture}.");
                theme
                    .syntax
                    .styles
                    .retain(|k, _| !k.starts_with(&prefix) || syntax.contains_key(k));
                theme.syntax.styles.insert(capture.clone(), style);
            }
        }
        Ok(theme)
    }
}

fn merge_section<T>(
    base: &T,
    overrides: Option<&toml::Value>,
    section: &str,
) -> Result<T, ThemeError>
where
    T: Clone + serde::Serialize + serde::de::DeserializeOwned,
{
    let Some(overrides) = overrides else {
        return Ok(base.clone());
    };
    let overrides = overrides
        .as_table()
        .ok_or_else(|| invalid(format!("`{section}` must be a table")))?;
    let mut merged = match toml::Value::try_from(base) {
        Ok(toml::Value::Table(t)) => t,
        Ok(_) => return Err(invalid(format!("{section}: base is not a table"))),
        Err(e) => return Err(invalid(e.to_string())),
    };
    for (key, value) in overrides {
        if !merged.contains_key(key) {
            return Err(invalid(format!("unknown key `{section}.{key}`")));
        }
        let color = value
            .as_str()
            .ok_or_else(|| invalid(format!("`{section}.{key}` must be a color string")))?;
        Color::parse(color).map_err(|e| invalid(format!("{section}.{key}: {e}")))?;
        merged.insert(key.clone(), value.clone());
    }
    toml::Value::Table(merged)
        .try_into()
        .map_err(|e| invalid(format!("{section}: {e}")))
}

fn parse_style(value: &toml::Value) -> Result<TextStyle, String> {
    match value {
        toml::Value::String(s) => Color::parse(s)
            .map(TextStyle::color)
            .map_err(|e| e.to_string()),
        toml::Value::Table(t) => {
            let color = t
                .get("color")
                .and_then(|c| c.as_str())
                .ok_or("missing `color`")?;
            let color = Color::parse(color).map_err(|e| e.to_string())?;
            let flag = |k: &str| t.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
            Ok(TextStyle {
                color,
                bold: flag("bold"),
                italic: flag("italic"),
            })
        }
        _ => Err("expected a color string or a table".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_present() {
        let reg = ThemeRegistry::new();
        assert!(reg.get("Lector Dark").is_some());
        assert!(reg.get("lector light").is_some());
        assert_eq!(reg.default_for(Appearance::Light).name, "Lector Light");
    }

    #[test]
    fn user_theme_overrides_base() {
        let mut reg = ThemeRegistry::new();
        let src = r##"
            name = "Midnight"
            appearance = "dark"
            [ui]
            background = "#000010"
            [syntax]
            keyword = "#ff00ff"
            "function.method" = { color = "#00ff00", bold = true }
            [git]
            added = "#00ff00"
        "##;
        let theme = reg.load_str(src).unwrap().clone();
        assert_eq!(theme.ui.background, Color::hex(0x000010));
        let dark = reg.get("Lector Dark").unwrap();
        assert_eq!(theme.ui.text, dark.ui.text);
        assert_eq!(
            theme.syntax.style_for("keyword.control").unwrap().color,
            Color::hex(0xff00ff)
        );
        let m = theme.syntax.style_for("function.method").unwrap();
        assert!(m.bold);
        assert_eq!(theme.git.added, Color::hex(0x00ff00));
        assert!(reg.names().contains(&"Midnight"));
    }

    #[test]
    fn rejects_bad_themes() {
        let reg = ThemeRegistry::new();
        assert!(reg.parse_toml("appearance = \"dark\"").is_err());
        assert!(reg
            .parse_toml("name = \"x\"\nappearance = \"dim\"")
            .is_err());
        assert!(reg
            .parse_toml("name = \"x\"\nappearance = \"dark\"\n[ui]\nbackgrond = \"#000\"")
            .is_err());
        assert!(reg
            .parse_toml("name = \"x\"\nappearance = \"dark\"\n[ui]\nbackground = \"red\"")
            .is_err());
        assert!(reg
            .parse_toml("name = \"x\"\nappearance = \"dark\"\nbase = \"Nope\"")
            .is_err());
    }

    #[test]
    fn load_dir_reports_per_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.toml"),
            "name = \"A\"\nappearance = \"light\"\n[diff]\nadded_gutter = \"#00aa00\"",
        )
        .unwrap();
        std::fs::write(dir.path().join("b.toml"), "name = ").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        let mut reg = ThemeRegistry::new();
        let (loaded, errors) = reg.load_dir(dir.path());
        assert_eq!(loaded, vec!["A".to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].to_string().contains("b.toml"));
        let (loaded, errors) = reg.load_dir(&dir.path().join("missing"));
        assert!(loaded.is_empty() && errors.is_empty());
    }

    #[test]
    fn style_fallback() {
        let reg = ThemeRegistry::new();
        let t = reg.get("Lector Dark").unwrap();
        assert_eq!(
            t.syntax.style_for("keyword.control.flow.rust"),
            t.syntax.style_for("keyword.control")
        );
        assert!(t.syntax.style_for("nonexistent").is_none());
        let resolved = t.syntax.resolve_all(&["string", "zzz"]);
        assert!(resolved[0].is_some() && resolved[1].is_none());
    }
}
