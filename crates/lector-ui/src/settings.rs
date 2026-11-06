//! Settings and key bindings from `lector-config`, applied to the UI:
//! editor fonts/sizes, theme selection, UI font and the keymap.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{px, App, Global, SharedString};
use lector_config::Settings;

use crate::code_view::EditorSettings;
use crate::keymap::{self, KeySpec};
use crate::theme::{ThemeMode, ThemeState};

/// The loaded settings (global). Read with [`AppSettings::get`].
#[derive(Clone, Debug, Default)]
pub struct AppSettings {
    /// Effective settings (defaults ← user file ← project file).
    pub settings: Arc<Settings>,
    /// Warnings from loading (unknown keys, parse errors).
    pub warnings: Arc<Vec<String>>,
}

impl Global for AppSettings {}

impl AppSettings {
    /// The current settings (defaults if not loaded yet).
    pub fn get(cx: &App) -> Arc<Settings> {
        cx.try_global::<AppSettings>()
            .map(|s| s.settings.clone())
            .unwrap_or_default()
    }
}

/// UI chrome font (global).
#[derive(Clone, Debug)]
pub struct UiFont {
    /// Family.
    pub family: SharedString,
    /// Base size in px.
    pub size: f32,
}

impl Default for UiFont {
    fn default() -> Self {
        UiFont {
            family: crate::assets::UI_FONT.into(),
            size: 13.,
        }
    }
}

impl Global for UiFont {}

impl UiFont {
    /// The global UI font.
    pub fn get(cx: &App) -> UiFont {
        cx.try_global::<UiFont>().cloned().unwrap_or_default()
    }
}

/// Picks the first installed family of `families`, else `fallback`.
fn first_installed(families: &[String], installed: &[String], fallback: &str) -> SharedString {
    for family in families {
        let family = family.trim();
        if family.eq_ignore_ascii_case("monospace") {
            break;
        }
        if family == crate::assets::MONO_FONT
            || family == crate::assets::UI_FONT
            || installed.iter().any(|f| f.eq_ignore_ascii_case(family))
        {
            return family.to_string().into();
        }
    }
    fallback.to_string().into()
}

/// Maps settings to the editor presentation settings.
pub fn editor_settings_from(settings: &Settings, installed: &[String]) -> EditorSettings {
    EditorSettings {
        font_family: first_installed(
            &settings.buffer_font_family,
            installed,
            crate::assets::MONO_FONT,
        ),
        font_size: px(settings.buffer_font_size.clamp(6., 48.)),
        line_height: settings.line_height.clamp(1.0, 3.0),
        tab_size: settings.tab_size.clamp(1, 16) as usize,
        show_minimap: settings.show_minimap,
        soft_wrap: settings.soft_wrap,
        indent_guides: settings.show_indent_guides,
        relative_line_numbers: false,
        sticky_headers: settings.sticky_scroll,
        ligatures: settings.ligatures,
    }
}

/// Applies `settings` (fonts, theme, colorblind palette) and stores them as
/// the [`AppSettings`] global. Refreshes windows.
pub fn apply_settings(cx: &mut App, settings: Settings, warnings: Vec<String>) {
    for w in &warnings {
        tracing::warn!("settings: {w}");
    }
    crate::scale::apply_from_settings(&settings);
    let installed = cx.text_system().all_font_names();
    cx.set_global(editor_settings_from(&settings, &installed));
    cx.set_global(UiFont {
        family: first_installed(
            std::slice::from_ref(&settings.ui_font_family),
            &installed,
            crate::assets::UI_FONT,
        ),
        size: settings.ui_font_size.clamp(9., 24.),
    });
    let theme_mode = match cx.try_global::<ThemeState>().map(|s| s.mode().clone()) {
        // A theme forced on the command line wins over settings.
        Some(ThemeMode::Named(name)) if cx.try_global::<CliTheme>().is_some() => {
            ThemeMode::Named(name)
        }
        _ => match settings.theme.mode {
            lector_config::ThemeMode::System => ThemeMode::System,
            lector_config::ThemeMode::Light => ThemeMode::Light,
            lector_config::ThemeMode::Dark => ThemeMode::Dark,
        },
    };
    if cx.has_global::<ThemeState>() {
        ThemeState::configure(
            cx,
            theme_mode,
            &settings.theme.light,
            &settings.theme.dark,
            settings.diff.colorblind,
        );
    }
    cx.set_global(AppSettings {
        settings: Arc::new(settings),
        warnings: Arc::new(warnings),
    });
    cx.refresh_windows();
}

/// Marker global: a theme was chosen on the command line.
pub struct CliTheme;
impl Global for CliTheme {}

/// The full key map: UI plumbing defaults, then `lector-config` defaults and
/// the user's `keymap.toml` on top.
pub fn effective_keymap() -> (Vec<KeySpec>, Vec<String>) {
    let ui_defaults: Vec<lector_config::KeyBinding> = keymap::default_keymap()
        .into_iter()
        .map(|k| lector_config::KeyBinding {
            keys: k.keys,
            action: k.action,
            context: k.context,
        })
        .collect();
    let (configured, warnings) = lector_config::load_keymap();
    let merged = lector_config::merge_keymap(ui_defaults, configured);
    let specs = merged
        .into_iter()
        .map(|k| KeySpec {
            keys: k.keys,
            action: k.action,
            context: k.context,
        })
        .collect();
    (specs, warnings)
}

/// (Re)binds all keys from [`effective_keymap`]. Returns problems found.
pub fn bind_effective_keymap(cx: &mut App) -> Vec<String> {
    let (specs, mut warnings) = effective_keymap();
    cx.clear_key_bindings();
    warnings.extend(keymap::bind_keymap(cx, &specs));
    for w in &warnings {
        tracing::warn!("keymap: {w}");
    }
    warnings
}

/// Lector's own settings file, created with the commented defaults when
/// missing (this is Lector's state, never the user's project).
pub fn ensure_settings_file() -> PathBuf {
    let path = lector_config::paths::settings_file();
    ensure_file(&path, lector_config::default_settings_toml());
    path
}

/// Lector's own keymap file, created with a commented template when missing.
pub fn ensure_keymap_file() -> PathBuf {
    let path = lector_config::paths::keymap_file();
    let mut text = String::from(
        "# Lector key bindings. Entries here override the defaults listed below.\n\
         # Each binding: keys (GPUI syntax, e.g. \"ctrl-shift-p\", chords separated by a space),\n\
         # action (\"namespace::Name\"; \"none\" unbinds) and an optional context\n\
         # (Workspace, CodeView, DiffView, Picker, TextInput, TreeView).\n\
         #\n\
         # [[bindings]]\n\
         # keys = \"ctrl-alt-p\"\n\
         # action = \"workspace::ToggleFileFinder\"\n\
         # context = \"Workspace\"\n\
         #\n# Defaults:\n",
    );
    let defaults = lector_config::default_keymap(lector_config::Platform::current());
    for b in defaults {
        text.push_str(&format!(
            "#   {:<22} {:<40} {}\n",
            b.keys,
            b.action,
            b.context.unwrap_or_default()
        ));
    }
    ensure_file(&path, &text);
    path
}

fn ensure_file(path: &std::path::Path, contents: &str) {
    if path.exists() {
        return;
    }
    if let Some(parent) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            tracing::warn!("cannot create {}: {err}", parent.display());
            return;
        }
    }
    if let Err(err) = std::fs::write(path, contents) {
        tracing::warn!("cannot write {}: {err}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_installed_font() {
        let installed = vec!["DejaVu Sans Mono".to_string()];
        let fams = vec!["Nope".to_string(), "dejavu sans mono".to_string()];
        assert_eq!(
            first_installed(&fams, &installed, "X").as_ref(),
            "dejavu sans mono"
        );
        assert_eq!(first_installed(&[], &installed, "X").as_ref(), "X");
        let s = editor_settings_from(&Settings::default(), &[]);
        assert_eq!(s.font_family.as_ref(), crate::assets::MONO_FONT);
        assert_eq!(s.tab_size, 4);
    }

    #[test]
    fn keymap_merges_without_errors_on_known_actions() {
        let (specs, _) = effective_keymap();
        assert!(specs
            .iter()
            .any(|s| s.action == "workspace::ToggleFileFinder"));
        assert!(specs.iter().any(|s| s.action == "editor::MoveUp"));
    }
}
