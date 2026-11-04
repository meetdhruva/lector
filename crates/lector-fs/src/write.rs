//! Atomic file writes and change-detection stamps.
//!
//! These are the only functions in this crate that write to the user's files.
//! Callers must only invoke [`write_atomic`] as the direct result of an
//! explicit user action (save), see "Write policy" in `docs/ARCHITECTURE.md`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// Size and modification time of a file, for "changed on disk since load" checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    /// Length in bytes.
    pub len: u64,
    /// Last modification time (`UNIX_EPOCH` when the platform cannot tell).
    pub modified: SystemTime,
}

/// The current [`FileStamp`] of `path` (symlinks are followed).
pub fn stamp(path: &Path) -> io::Result<FileStamp> {
    let meta = fs::metadata(path)?;
    Ok(FileStamp {
        len: meta.len(),
        modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
    })
}

/// Maximum symlink hops followed before giving up.
const MAX_LINK_HOPS: usize = 40;

/// Resolves symlinks of `path` itself (not of its parents), also for dangling
/// links, so a save writes through the link to the file it points at.
fn resolve_target(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_LINK_HOPS {
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = fs::read_link(&current)?;
                current = match current.parent() {
                    Some(parent) if target.is_relative() => parent.join(target),
                    _ => target,
                };
            }
            _ => return Ok(current),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "too many levels of symbolic links",
    ))
}

fn temp_path(target: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = format!(".{name}.lector-{}-{n}.tmp", std::process::id());
    target.with_file_name(tmp)
}

/// Writes `bytes` to `path` atomically: the data goes to a temporary file in
/// the same directory, is fsynced, and replaces `path` with a rename, so a
/// crash leaves either the old or the new content.
///
/// * Permissions of an existing file are kept; a file that is read-only is
///   refused with `PermissionDenied` instead of being replaced.
/// * A symlink is written through: its target is replaced, the link stays.
/// * A new file is created with the default permissions.
/// * The temporary file is removed on failure.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let target = resolve_target(path)?;
    let existing = match fs::metadata(&target) {
        Ok(meta) => {
            if !meta.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "not a regular file",
                ));
            }
            if meta.permissions().readonly() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "file is read-only",
                ));
            }
            Some(meta)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let tmp = temp_path(&target);
    let result = write_and_replace(&tmp, &target, bytes, existing.as_ref());
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn write_and_replace(
    tmp: &Path,
    target: &Path,
    bytes: &[u8],
    existing: Option<&fs::Metadata>,
) -> io::Result<()> {
    let mut file: File = OpenOptions::new().write(true).create_new(true).open(tmp)?;
    if let Some(meta) = existing {
        fs::set_permissions(tmp, meta.permissions())?;
    }
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    replace(tmp, target)?;
    sync_parent(target);
    Ok(())
}

#[cfg(not(windows))]
fn replace(tmp: &Path, target: &Path) -> io::Result<()> {
    fs::rename(tmp, target)
}

/// Windows: `rename` replaces an existing file, but fails transiently with
/// `PermissionDenied` while an indexer, antivirus or another reader holds the
/// target open without share-delete; retry briefly.
#[cfg(windows)]
fn replace(tmp: &Path, target: &Path) -> io::Result<()> {
    let mut delay = std::time::Duration::from_millis(10);
    let mut last = None;
    for _ in 0..8 {
        match fs::rename(tmp, target) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                last = Some(e);
                std::thread::sleep(delay);
                delay *= 2;
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("replace failed")))
}

/// Best effort: make the rename itself durable.
#[cfg(unix)]
fn sync_parent(target: &Path) {
    if let Some(dir) = target.parent() {
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
    }
}

#[cfg(not(unix))]
fn sync_parent(_target: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    fn creates_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        write_atomic(&p, b"one").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"one");
        write_atomic(&p, b"two two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two two");
        write_atomic(&p, b"").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"");
        assert_eq!(dir_entries(dir.path()), ["a.txt"], "no temp files left");
    }

    #[test]
    fn large_content() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.bin");
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        write_atomic(&p, &data).unwrap();
        assert_eq!(fs::read(&p).unwrap(), data);
    }

    #[test]
    fn missing_parent_is_an_error_and_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nope").join("a.txt");
        assert!(write_atomic(&p, b"x").is_err());
        assert!(dir_entries(dir.path()).is_empty());
    }

    #[test]
    fn refuses_directories() {
        let dir = tempfile::tempdir().unwrap();
        assert!(write_atomic(dir.path(), b"x").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("script.sh");
        fs::write(&p, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o750)).unwrap();
        write_atomic(&p, b"#!/bin/sh\necho hi\n").unwrap();
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o750);
        assert_eq!(fs::read(&p).unwrap(), b"#!/bin/sh\necho hi\n");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_read_only_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ro.txt");
        fs::write(&p, "keep").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o444)).unwrap();
        let err = write_atomic(&p, b"new").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read(&p).unwrap(), b"keep");
        assert_eq!(dir_entries(dir.path()), ["ro.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn writes_through_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("real");
        fs::create_dir(&sub).unwrap();
        let target = sub.join("t.txt");
        fs::write(&target, "old").unwrap();
        let link = dir.path().join("link.txt");
        symlink("real/t.txt", &link).unwrap();
        write_atomic(&link, b"new").unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(fs::read(&link).unwrap(), b"new");
        assert_eq!(dir_entries(&sub), ["t.txt"]);
        // a chain of links
        let link2 = dir.path().join("link2.txt");
        symlink(&link, &link2).unwrap();
        write_atomic(&link2, b"chain").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"chain");
        assert!(fs::symlink_metadata(&link2)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_creates_target() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("l");
        symlink("made.txt", &link).unwrap();
        write_atomic(&link, b"hello").unwrap();
        assert_eq!(fs::read(dir.path().join("made.txt")).unwrap(), b"hello");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loop_is_an_error() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        symlink(&b, &a).unwrap();
        symlink(&a, &b).unwrap();
        assert!(write_atomic(&a, b"x").is_err());
    }

    #[test]
    fn stamp_tracks_changes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.txt");
        write_atomic(&p, b"abc").unwrap();
        let a = stamp(&p).unwrap();
        assert_eq!(a.len, 3);
        assert_eq!(stamp(&p).unwrap(), a);
        write_atomic(&p, b"abcdef").unwrap();
        assert_ne!(stamp(&p).unwrap(), a);
        assert!(stamp(&dir.path().join("missing")).is_err());
    }
}
