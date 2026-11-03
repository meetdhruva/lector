//! Platform directories.
//!
//! Resolution order for both the config and the data directory:
//! 1. `LECTOR_CONFIG_DIR` / `LECTOR_DATA_DIR` environment variables;
//! 2. **portable mode**: a `lector-data` directory next to the executable
//!    (config in `lector-data/config`, state in `lector-data/data`);
//! 3. the platform's standard directories.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Name of the directory that, when present next to the executable, enables portable mode.
pub const PORTABLE_DIR_NAME: &str = "lector-data";

/// The portable data root (`<exe dir>/lector-data`) if portable mode is active.
pub fn portable_root() -> Option<&'static Path> {
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        let exe = dunce::canonicalize(&exe).unwrap_or(exe);
        portable_root_for(exe.parent()?)
    })
    .as_deref()
}

/// Portable root for an executable living in `exe_dir`, if `exe_dir/lector-data` is a directory.
pub fn portable_root_for(exe_dir: &Path) -> Option<PathBuf> {
    let candidate = exe_dir.join(PORTABLE_DIR_NAME);
    candidate.is_dir().then_some(candidate)
}

/// Whether Lector runs in portable mode.
pub fn is_portable() -> bool {
    portable_root().is_some()
}

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
    if let Some(dir) = env_dir("LECTOR_CONFIG_DIR") {
        return dir;
    }
    if let Some(root) = portable_root() {
        return root.join("config");
    }
    dirs::config_dir()
        .unwrap_or_else(fallback)
        .join(app_dir_name())
}

/// Directory for Lector-owned state (sessions, review state, logs).
///
/// macOS: `~/Library/Application Support/Lector`; Linux: `$XDG_DATA_HOME/lector`;
/// Windows: `%APPDATA%\Lector`.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = env_dir("LECTOR_DATA_DIR") {
        return dir;
    }
    if let Some(root) = portable_root() {
        return root.join("data");
    }
    let base = if cfg!(target_os = "windows") {
        dirs::config_dir()
    } else {
        dirs::data_dir()
    };
    base.unwrap_or_else(fallback).join(app_dir_name())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_root_requires_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(portable_root_for(dir.path()), None);
        std::fs::write(dir.path().join(PORTABLE_DIR_NAME), b"").unwrap();
        assert_eq!(
            portable_root_for(dir.path()),
            None,
            "a plain file is not a portable root"
        );
        std::fs::remove_file(dir.path().join(PORTABLE_DIR_NAME)).unwrap();
        std::fs::create_dir(dir.path().join(PORTABLE_DIR_NAME)).unwrap();
        assert_eq!(
            portable_root_for(dir.path()),
            Some(dir.path().join(PORTABLE_DIR_NAME))
        );
    }
}
