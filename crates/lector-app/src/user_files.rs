//! Files and directories Lector creates for the user in its own directories (ADR 0012, item 2).
//!
//! UI code creates no files: when the user opens settings or the keymap, the window asks this
//! service for the file, and a window without a folder asks it for its placeholder directory.
//! Everything here is
//! * the direct result of an explicit user action (Open Settings, Open Keymap) or of opening a
//!   window without a folder (an empty placeholder Lector never scans or watches),
//! * inside Lector's own directories (`lector_config::paths::config_dir` / `data_dir`, or the
//!   system temp directory as the placeholder's last resort), never in a project,
//! * created owner-only through `lector_base::private` (directories `0700`, files `0600` on
//!   Unix), and only when missing: an existing file is never rewritten and keeps its modes.
//!
//! Failures are logged and the path is returned anyway, so the caller can still show (or fail to
//! open) it; nothing here panics.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Lector's settings file (`settings.toml` in the config directory), created with the commented
/// defaults when missing (Open Settings).
pub fn ensure_settings_file() -> PathBuf {
    let path = lector_config::paths::settings_file();
    ensure_file(&path, lector_config::default_settings_toml());
    path
}

/// Lector's keymap file (`keymap.toml` in the config directory), created with a commented
/// template listing the default bindings when missing (Open Keymap).
pub fn ensure_keymap_file() -> PathBuf {
    let path = lector_config::paths::keymap_file();
    if !path.exists() {
        ensure_file(&path, &keymap_template());
    }
    path
}

/// The commented template of a new `keymap.toml`: how a binding is written, then every default
/// binding of this platform.
pub fn keymap_template() -> String {
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
    for b in lector_config::default_keymap(lector_config::Platform::current()) {
        text.push_str(&format!(
            "#   {:<22} {:<40} {}\n",
            b.keys,
            b.action,
            b.context.unwrap_or_default()
        ));
    }
    text
}

/// Root folder of the placeholder project of a window with no folder open: `no-folder` in
/// Lector's data directory (empty, never scanned or watched), or `lector-no-folder` in the
/// system temp directory when that cannot be created.
pub fn no_folder_root() -> PathBuf {
    let dir = lector_config::paths::data_dir().join("no-folder");
    match lector_base::private::create_dir_all(&dir) {
        Ok(()) => dir,
        Err(err) => {
            tracing::warn!("cannot create {}: {err}", dir.display());
            let tmp = std::env::temp_dir().join("lector-no-folder");
            if let Err(err) = lector_base::private::create_dir_all(&tmp) {
                tracing::warn!("cannot create {}: {err}", tmp.display());
            }
            tmp
        }
    }
}

/// Creates `path` with `contents` (and its missing parents) when it does not exist yet.
fn ensure_file(path: &Path, contents: &str) {
    if path.exists() {
        return;
    }
    if let Some(parent) = path.parent() {
        if let Err(err) = lector_base::private::create_dir_all(parent) {
            tracing::warn!("cannot create {}: {err}", parent.display());
            return;
        }
    }
    let written = lector_base::private::create_new(path)
        .and_then(|mut file| file.write_all(contents.as_bytes()));
    match written {
        Ok(()) => {}
        // Created in the meantime (another window): it is the user's now, leave it.
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => tracing::warn!("cannot write {}: {err}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_created_owner_only_and_an_existing_one_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config/settings.toml");
        ensure_file(&path, "first\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
        ensure_file(&path, "second\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n");
    }

    #[test]
    fn the_keymap_template_lists_the_defaults_as_comments() {
        let text = keymap_template();
        assert!(text.lines().all(|l| l.starts_with('#')), "{text}");
        assert!(text.contains("workspace::"), "{text}");
    }
}
