//! Embedded assets: UI icons (served to `svg()` through [`Assets`]) and fonts.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{App, AssetSource, SharedString};
use lector_icons::{FileIcons, UiIcon};

/// Path prefix of UI icons served by [`Assets`]: `icons/ui/<name>.svg`.
pub const UI_ICON_PREFIX: &str = "icons/ui/";
/// Path prefix of file icons served by [`Assets`]: `icons/file/<name>.svg`.
pub const FILE_ICON_PREFIX: &str = "icons/file/";

/// UI monospace font family (embedded).
pub const MONO_FONT: &str = "JetBrains Mono";
/// UI sans-serif font family (embedded).
pub const UI_FONT: &str = "Inter";

macro_rules! font {
    ($path:literal) => {
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/fonts/",
            $path
        ))
    };
}

static FONTS: &[&[u8]] = &[
    font!("jetbrains-mono/JetBrainsMono-Regular.ttf"),
    font!("jetbrains-mono/JetBrainsMono-Bold.ttf"),
    font!("jetbrains-mono/JetBrainsMono-Italic.ttf"),
    font!("jetbrains-mono/JetBrainsMono-BoldItalic.ttf"),
    font!("inter/Inter-Regular.ttf"),
    font!("inter/Inter-Medium.ttf"),
    font!("inter/Inter-SemiBold.ttf"),
];

/// The GPUI asset source for Lector.
#[derive(Clone, Copy, Debug, Default)]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(name) = path
            .strip_prefix(UI_ICON_PREFIX)
            .and_then(|p| p.strip_suffix(".svg"))
        {
            return Ok(UiIcon::from_name(name).map(|icon| Cow::Borrowed(icon.svg())));
        }
        if let Some(name) = path
            .strip_prefix(FILE_ICON_PREFIX)
            .and_then(|p| p.strip_suffix(".svg"))
        {
            return Ok(FileIcons::get()
                .by_name(name)
                .map(|icon| Cow::Borrowed(icon.svg)));
        }
        Ok(None)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        if path.trim_end_matches('/') == UI_ICON_PREFIX.trim_end_matches('/') {
            return Ok(UiIcon::ALL
                .iter()
                .map(|i| format!("{UI_ICON_PREFIX}{}.svg", i.name()).into())
                .collect());
        }
        Ok(Vec::new())
    }
}

/// Registers the embedded fonts with the text system.
pub fn load_fonts(cx: &mut App) {
    let fonts = FONTS.iter().map(|f| Cow::Borrowed(*f)).collect();
    if let Err(err) = cx.text_system().add_fonts(fonts) {
        tracing::warn!("failed to load embedded fonts: {err:#}");
    }
}
