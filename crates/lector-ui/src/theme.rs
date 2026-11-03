//! Theme integration: the active [`Theme`] as a GPUI global, color
//! conversion, and following the OS light/dark appearance.

use std::sync::Arc;

use gpui::{App, Global, Hsla, Rgba, SharedString, Window, WindowAppearance};
use lector_theme::{Appearance, Color, Theme, ThemeRegistry};

/// Converts theme colors into GPUI colors.
pub trait ColorExt {
    /// As GPUI `Hsla`.
    fn hsla(self) -> Hsla;
    /// As GPUI `Rgba`.
    fn rgba(self) -> Rgba;
}

impl ColorExt for Color {
    fn hsla(self) -> Hsla {
        self.rgba().into()
    }

    fn rgba(self) -> Rgba {
        Rgba {
            r: self.r as f32 / 255.0,
            g: self.g as f32 / 255.0,
            b: self.b as f32 / 255.0,
            a: self.a as f32 / 255.0,
        }
    }
}

/// How the active theme is chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThemeMode {
    /// Follow the OS appearance, using the configured light/dark themes.
    System,
    /// Always use the configured light theme.
    Light,
    /// Always use the configured dark theme.
    Dark,
    /// A specific theme by name, regardless of appearance.
    Named(SharedString),
}

/// Global theme state. Access the active theme with [`ActiveTheme::theme`].
pub struct ThemeState {
    registry: ThemeRegistry,
    mode: ThemeMode,
    light_theme: SharedString,
    dark_theme: SharedString,
    colorblind_diff: bool,
    os_appearance: Appearance,
    active: Arc<Theme>,
}

impl Global for ThemeState {}

/// Access to the active theme from any GPUI context.
pub trait ActiveTheme {
    /// The active theme.
    fn theme(&self) -> &Arc<Theme>;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Arc<Theme> {
        &self.global::<ThemeState>().active
    }
}

/// Maps a GPUI window appearance to a theme appearance.
pub fn appearance_from_window(appearance: WindowAppearance) -> Appearance {
    match appearance {
        WindowAppearance::Light | WindowAppearance::VibrantLight => Appearance::Light,
        WindowAppearance::Dark | WindowAppearance::VibrantDark => Appearance::Dark,
    }
}

impl ThemeState {
    /// Installs the theme global with `registry` (built-ins plus any user
    /// themes already loaded) and the initial OS appearance.
    pub fn init(cx: &mut App, registry: ThemeRegistry, mode: ThemeMode, os_appearance: Appearance) {
        let active = Arc::new(registry.default_for(os_appearance).clone());
        let mut state = ThemeState {
            registry,
            mode,
            light_theme: lector_theme::LECTOR_LIGHT.into(),
            dark_theme: lector_theme::LECTOR_DARK.into(),
            colorblind_diff: false,
            os_appearance,
            active,
        };
        state.recompute();
        cx.set_global(state);
    }

    fn recompute(&mut self) {
        let name: SharedString = match &self.mode {
            ThemeMode::System => match self.os_appearance {
                Appearance::Light => self.light_theme.clone(),
                Appearance::Dark => self.dark_theme.clone(),
            },
            ThemeMode::Light => self.light_theme.clone(),
            ThemeMode::Dark => self.dark_theme.clone(),
            ThemeMode::Named(name) => name.clone(),
        };
        let theme = self
            .registry
            .get(&name)
            .cloned()
            .unwrap_or_else(|| self.registry.default_for(self.os_appearance).clone());
        let theme = if self.colorblind_diff {
            theme.with_colorblind_diff()
        } else {
            theme
        };
        if *self.active != theme {
            self.active = Arc::new(theme);
        }
    }

    /// The theme registry.
    pub fn registry(&self) -> &ThemeRegistry {
        &self.registry
    }

    /// Whether the colorblind-safe diff palette is enabled.
    pub fn colorblind_diff(&self) -> bool {
        self.colorblind_diff
    }

    /// Current mode.
    pub fn mode(&self) -> &ThemeMode {
        &self.mode
    }

    /// Updates the global and refreshes all windows.
    pub fn update(cx: &mut App, f: impl FnOnce(&mut ThemeState)) {
        let state = cx.global_mut::<ThemeState>();
        f(state);
        state.recompute();
        cx.refresh_windows();
    }

    /// Sets the mode (system/light/dark/named).
    pub fn set_mode(cx: &mut App, mode: ThemeMode) {
        Self::update(cx, |s| s.mode = mode);
    }

    /// Selects a theme by name; it also becomes the configured light or dark
    /// theme for its appearance, so OS switching keeps working.
    pub fn select_theme(cx: &mut App, name: &str) {
        Self::update(cx, |s| {
            if let Some(theme) = s.registry.get(name) {
                let name: SharedString = theme.name.clone().into();
                match theme.appearance {
                    Appearance::Light => s.light_theme = name.clone(),
                    Appearance::Dark => s.dark_theme = name.clone(),
                }
                s.mode = ThemeMode::Named(name);
            }
        });
    }

    /// Configures the themes used for light and dark appearance.
    pub fn set_light_dark(cx: &mut App, light: &str, dark: &str) {
        Self::update(cx, |s| {
            s.light_theme = light.to_string().into();
            s.dark_theme = dark.to_string().into();
        });
    }

    /// A copy of the theme registry (to reload user themes off the global).
    pub fn registry_clone(cx: &App) -> ThemeRegistry {
        cx.global::<ThemeState>().registry.clone()
    }

    /// Replaces the registry (e.g. after reloading the themes folder).
    pub fn replace_registry(cx: &mut App, registry: ThemeRegistry) {
        Self::update(cx, |s| s.registry = registry);
    }

    /// Applies a full configuration (from settings) in one refresh.
    pub fn configure(cx: &mut App, mode: ThemeMode, light: &str, dark: &str, colorblind: bool) {
        Self::update(cx, |s| {
            s.mode = mode;
            if s.registry.get(light).is_some() {
                s.light_theme = light.to_string().into();
            }
            if s.registry.get(dark).is_some() {
                s.dark_theme = dark.to_string().into();
            }
            s.colorblind_diff = colorblind;
        });
    }

    /// Toggles between the configured light and dark themes.
    pub fn toggle_appearance(cx: &mut App) {
        Self::update(cx, |s| {
            s.mode = if s.active.appearance.is_dark() {
                ThemeMode::Light
            } else {
                ThemeMode::Dark
            };
        });
    }

    /// Enables the blue/orange colorblind-safe diff palette.
    pub fn set_colorblind_diff(cx: &mut App, enabled: bool) {
        Self::update(cx, |s| s.colorblind_diff = enabled);
    }

    /// Adds or replaces a theme in the registry.
    pub fn insert_theme(cx: &mut App, theme: Theme) {
        Self::update(cx, |s| s.registry.insert(theme));
    }

    /// Records the OS appearance of `window` and switches theme if following
    /// the system.
    pub fn sync_with_window(window: &Window, cx: &mut App) {
        let appearance = appearance_from_window(window.appearance());
        if cx.global::<ThemeState>().os_appearance != appearance {
            Self::update(cx, |s| s.os_appearance = appearance);
        }
    }
}
