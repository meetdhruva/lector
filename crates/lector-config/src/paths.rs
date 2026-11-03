//! Platform directories. `LECTOR_CONFIG_DIR` / `LECTOR_DATA_DIR` override them.

use std::path::PathBuf;

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn app_dir_name() -> &'static str {
    if cfg!(target_os = "linux") {
        "lector"
    } else {
        "Lector"
    }
}

fn fallback() -> PathBuf {
    std::env::temp_dir()
}

/// Directory holding `settings.toml`, `keymap.toml` and user themes.
///
/// macOS: `~/Library/Application Support/Lector`; Linux: `$XDG_CONFIG_HOME/lector`;
/// Windows: `%APPDATA%\Lector`.
pub fn config_dir() -> PathBuf {
    env_dir("LECTOR_CONFIG_DIR").unwrap_or_else(|| {
        dirs::config_dir()
            .unwrap_or_else(fallback)
            .join(app_dir_name())
    })
}

/// Directory for Lector-owned state (sessions, review state, logs).
///
/// macOS: `~/Library/Application Support/Lector`; Linux: `$XDG_DATA_HOME/lector`;
/// Windows: `%APPDATA%\Lector`.
pub fn data_dir() -> PathBuf {
    env_dir("LECTOR_DATA_DIR").unwrap_or_else(|| {
        let base = if cfg!(target_os = "windows") {
            dirs::config_dir()
        } else {
            dirs::data_dir()
        };
        base.unwrap_or_else(fallback).join(app_dir_name())
    })
}

/// `<config_dir>/settings.toml`
pub fn settings_file() -> PathBuf {
    config_dir().join("settings.toml")
}

/// `<config_dir>/keymap.toml`
pub fn keymap_file() -> PathBuf {
    config_dir().join("keymap.toml")
}

/// `<config_dir>/themes`
pub fn themes_dir() -> PathBuf {
    config_dir().join("themes")
}

/// `<data_dir>/lector.log`
pub fn log_file() -> PathBuf {
    data_dir().join("lector.log")
}

/// `<data_dir>/sessions`
pub fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

/// `<data_dir>/review.json`
pub fn review_file() -> PathBuf {
    data_dir().join("review.json")
}
