//! lector-theme: the UI-free theme model for Lector.
//!
//! A [`Theme`] bundles UI chrome colors ([`UiColors`]), syntax styles
//! ([`SyntaxTheme`]), diff colors ([`DiffColors`]) and git status colors
//! ([`GitColors`]). [`ThemeRegistry`] holds the four built-in themes and user
//! themes loaded from TOML files.
#![forbid(unsafe_code)]

mod builtin;
mod color;
mod registry;
mod theme;

pub use builtin::{builtin_themes, LECTOR_DARK, LECTOR_HC_DARK, LECTOR_HC_LIGHT, LECTOR_LIGHT};
pub use color::{Color, ParseColorError};
pub use registry::{ThemeError, ThemeRegistry};
pub use theme::{
    Appearance, DiffColors, GitColors, SyntaxTheme, TextStyle, Theme, UiColors, CAPTURE_NAMES,
};
