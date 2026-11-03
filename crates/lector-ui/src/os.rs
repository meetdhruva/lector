//! Small OS integrations (read-only): reveal a path in the system file
//! manager, open URLs.

use std::path::Path;
use std::process::Command;

/// Menu label for [`reveal_in_file_manager`] on this platform.
pub const REVEAL_LABEL: &str = if cfg!(target_os = "macos") {
    "Reveal in Finder"
} else if cfg!(target_os = "windows") {
    "Reveal in File Explorer"
} else {
    "Reveal in File Manager"
};

/// Shows `path` in the OS file manager (selects files where supported).
/// Spawns a detached process; failures are logged, never fatal.
pub fn reveal_in_file_manager(path: &Path) {
    let result = if cfg!(target_os = "macos") {
        Command::new("open").arg("-R").arg(path).spawn()
    } else if cfg!(target_os = "windows") {
        Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn()
    } else {
        let dir = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(path)
        };
        Command::new("xdg-open").arg(dir).spawn()
    };
    if let Err(err) = result {
        tracing::warn!("cannot reveal {}: {err}", path.display());
    }
}
