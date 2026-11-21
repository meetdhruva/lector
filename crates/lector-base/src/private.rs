//! Owner-only files and directories for Lector's own data (ADR 0010, item 7).
//!
//! Everything Lector creates under its data directory (the database, shadow repositories with
//! backups of git-ignored files such as `.env`, logs, crash reports, session records, the askpass
//! directory) goes through these functions:
//! * directories are created `0700` ([`create_dir_all`]),
//! * files are created `0600` ([`write`], [`create_new`], [`open_append`]),
//!
//! whatever the process umask, on Unix. A directory that already exists with group or other
//! permissions is narrowed once by [`narrow_dir`] (callers log it). Files the user edits
//! (`settings.toml`, `keymap.toml`, themes in the config directory) do not use this module and
//! keep the user's own modes.
//!
//! On Windows nothing is changed: files under `%LOCALAPPDATA%` / `%APPDATA%` inherit the ACL of
//! the user's profile (the user, SYSTEM and Administrators), which is already owner-only for
//! other users.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Mode of directories Lector creates for itself (Unix).
pub const DIR_MODE: u32 = 0o700;
/// Mode of files Lector creates for itself (Unix).
pub const FILE_MODE: u32 = 0o600;

/// Creates `path` and every missing parent `0700` (Unix). Existing directories are left as they
/// are (see [`narrow_dir`]).
pub fn create_dir_all(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(DIR_MODE);
    }
    builder.create(path)
}

fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(FILE_MODE);
    }
    options
}

/// Creates a new file `0600` (Unix); fails with `AlreadyExists` if there is one.
pub fn create_new(path: &Path) -> io::Result<File> {
    options().write(true).create_new(true).open(path)
}

/// Opens `path` for appending, creating it `0600` (Unix) when missing. An existing file keeps
/// its mode unless it is readable by others, which is narrowed to `0600`.
pub fn open_append(path: &Path) -> io::Result<File> {
    let file = options().create(true).append(true).open(path)?;
    narrow_file(&file)?;
    Ok(file)
}

/// Writes `bytes` to `path` (created or truncated), `0600` on Unix; an existing file that others
/// could read is narrowed first.
pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    narrow_file(&file)?;
    file.write_all(bytes)
}

#[cfg(unix)]
fn narrow_file(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = file.metadata()?.permissions().mode();
    if mode & 0o077 != 0 {
        file.set_permissions(fs::Permissions::from_mode(mode & 0o700))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn narrow_file(_file: &File) -> io::Result<()> {
    Ok(())
}

/// Removes group and other permissions from the directory `path` (Unix). Returns `true` when it
/// had any (the caller logs that it narrowed it), `false` when it was already owner-only, is
/// missing, or the platform has no modes.
pub fn narrow_dir(path: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = match fs::symlink_metadata(path) {
            Ok(m) if m.is_dir() => m,
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let mode = meta.permissions().mode();
        if mode & 0o077 == 0 {
            return Ok(false);
        }
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o700))?;
        Ok(true)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(false)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn creates_owner_only_and_narrows() {
        let dir = std::env::temp_dir().join(format!("lector-private-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let deep = dir.join("a/b");
        create_dir_all(&deep).unwrap();
        assert_eq!(mode(&dir.join("a")), 0o700);
        assert_eq!(mode(&deep), 0o700);
        let f = deep.join("f");
        write(&f, b"x").unwrap();
        assert_eq!(mode(&f), 0o600);
        fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
        write(&f, b"y").unwrap();
        assert_eq!(mode(&f), 0o600);
        let log = deep.join("log");
        open_append(&log).unwrap();
        assert_eq!(mode(&log), 0o600);
        assert!(create_new(&log).is_err());
        assert_eq!(mode(&create_new_path(&deep)), 0o600);
        fs::set_permissions(&deep, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(narrow_dir(&deep).unwrap());
        assert_eq!(mode(&deep), 0o700);
        assert!(!narrow_dir(&deep).unwrap());
        assert!(!narrow_dir(&dir.join("missing")).unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    fn create_new_path(dir: &Path) -> std::path::PathBuf {
        let p = dir.join("new");
        create_new(&p).unwrap();
        p
    }
}
