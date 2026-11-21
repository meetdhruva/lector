//! Reading and writing a [`Confined`] path without following a link that appeared after the
//! check. Unix walks the path with `openat(O_NOFOLLOW)`; other platforms re-verify (see the
//! module documentation of [`super`]).

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{ConfineError, Confined, CreateDirs};

/// `.<name>.lector-<pid>-<n>.tmp`, the same shape [`crate::write_atomic`] uses.
fn temp_name(name: &std::ffi::OsStr) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        ".{}.lector-{}-{n}.tmp",
        name.to_string_lossy(),
        std::process::id()
    )
}

fn read_only() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "file is read-only")
}

fn not_regular() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "not a regular file")
}

#[cfg(unix)]
mod imp {
    use super::*;
    use rustix::fs::{
        fstat, mkdirat, openat, renameat, statat, unlinkat, AtFlags, FileType, Mode, OFlags,
    };
    use rustix::io::Errno;
    use std::os::fd::OwnedFd;
    use std::path::Component;

    /// A link (or a non-directory) where the check saw a plain directory.
    fn swapped(e: Errno) -> bool {
        e == Errno::LOOP || e == Errno::NOTDIR || e == Errno::MLINK
    }

    const DIR_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);

    /// Opens the directory that holds the confined path's last component, from the root down,
    /// never following a link. Returns it with the last component's name.
    fn open_parent(
        c: &Confined,
        create: CreateDirs,
    ) -> Result<(OwnedFd, std::ffi::OsString), ConfineError> {
        let mut names: Vec<&std::ffi::OsStr> = Vec::new();
        for comp in c.rel.components() {
            match comp {
                Component::Normal(n) => names.push(n),
                _ => return Err(c.changed()),
            }
        }
        let last = names.pop().ok_or_else(|| c.io(not_regular()))?;
        let mut dir = rustix::fs::open(&c.root, DIR_FLAGS, Mode::empty()).map_err(|e| {
            if swapped(e) {
                c.changed()
            } else {
                c.io(e.into())
            }
        })?;
        for name in names {
            let opened = match openat(&dir, name, DIR_FLAGS, Mode::empty()) {
                Err(e) if e == Errno::NOENT && create == CreateDirs::Yes => {
                    match mkdirat(&dir, name, Mode::from_raw_mode(0o777)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(e) => return Err(c.io(e.into())),
                    }
                    openat(&dir, name, DIR_FLAGS, Mode::empty())
                }
                other => other,
            };
            dir = opened.map_err(|e| {
                if swapped(e) {
                    c.changed()
                } else {
                    c.io(e.into())
                }
            })?;
        }
        Ok((dir, last.to_os_string()))
    }

    pub(in crate::confine) fn read(c: &Confined) -> Result<Vec<u8>, ConfineError> {
        let (dir, name) = open_parent(c, CreateDirs::No)?;
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        let fd = openat(&dir, &name, flags, Mode::empty()).map_err(|e| {
            if e == Errno::LOOP || e == Errno::MLINK {
                c.changed()
            } else {
                c.io(e.into())
            }
        })?;
        let st = fstat(&fd).map_err(|e| c.io(e.into()))?;
        if FileType::from_raw_mode(st.st_mode as _) != FileType::RegularFile {
            return Err(c.io(not_regular()));
        }
        let mut file = std::fs::File::from(fd);
        let mut out = Vec::new();
        file.read_to_end(&mut out).map_err(|e| c.io(e))?;
        Ok(out)
    }

    pub(in crate::confine) fn write(
        c: &Confined,
        bytes: &[u8],
        create: CreateDirs,
    ) -> Result<(), ConfineError> {
        let (dir, name) = open_parent(c, create)?;
        let existing_mode = match statat(&dir, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => {
                let kind = FileType::from_raw_mode(st.st_mode as _);
                if kind == FileType::Symlink {
                    return Err(ConfineError::FinalSymlink { path: c.path() });
                }
                if kind != FileType::RegularFile {
                    return Err(c.io(not_regular()));
                }
                // `st_mode` is u16 on macOS, u32 on Linux.
                #[allow(clippy::unnecessary_cast)]
                let mode = (st.st_mode as u32) & 0o7777;
                if mode & 0o222 == 0 {
                    return Err(c.io(read_only()));
                }
                Some(mode)
            }
            Err(Errno::NOENT) => None,
            Err(e) => return Err(c.io(e.into())),
        };
        let tmp = temp_name(&name);
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = openat(&dir, tmp.as_str(), flags, Mode::from_raw_mode(0o666))
            .map_err(|e| c.io(e.into()))?;
        let result = (|| -> io::Result<()> {
            if let Some(mode) = existing_mode {
                rustix::fs::fchmod(&fd, Mode::from_raw_mode(mode as _))?;
            }
            let mut file = std::fs::File::from(fd);
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            // Replaces whatever is at `name` now, a link included, without following it.
            renameat(&dir, tmp.as_str(), &dir, &name)?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = unlinkat(&dir, tmp.as_str(), AtFlags::empty());
            return Err(c.io(e));
        }
        let _ = rustix::fs::fsync(&dir);
        Ok(())
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::path::{Path, PathBuf};

    fn is_link(path: &Path) -> bool {
        fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink() || is_reparse(&m))
    }

    #[cfg(windows)]
    fn is_reparse(m: &fs::Metadata) -> bool {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    #[cfg(not(windows))]
    fn is_reparse(_m: &fs::Metadata) -> bool {
        false
    }

    /// The parent still resolves to where the check put it.
    fn verify_parent(c: &Confined, parent: &Path) -> Result<(), ConfineError> {
        let now = dunce::canonicalize(parent).map_err(|e| c.io(e))?;
        if now != parent {
            return Err(c.changed());
        }
        Ok(())
    }

    fn parent_of(c: &Confined) -> Result<(PathBuf, std::ffi::OsString), ConfineError> {
        let path = c.path();
        let name = path
            .file_name()
            .ok_or_else(|| c.io(not_regular()))?
            .to_os_string();
        let parent = path
            .parent()
            .ok_or_else(|| c.io(not_regular()))?
            .to_path_buf();
        Ok((parent, name))
    }

    pub(in crate::confine) fn read(c: &Confined) -> Result<Vec<u8>, ConfineError> {
        let path = c.path();
        let (parent, _) = parent_of(c)?;
        let mut opts = OpenOptions::new();
        opts.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let mut file = opts.open(&path).map_err(|e| c.io(e))?;
        let meta = file.metadata().map_err(|e| c.io(e))?;
        // The handle itself (Windows opens a reparse point instead of following it), and the
        // path once more after the open (platforms without that flag).
        if meta.file_type().is_symlink() || is_reparse(&meta) || is_link(&path) {
            return Err(c.changed());
        }
        if !meta.is_file() {
            return Err(c.io(not_regular()));
        }
        verify_parent(c, &parent)?;
        let mut out = Vec::new();
        file.read_to_end(&mut out).map_err(|e| c.io(e))?;
        Ok(out)
    }

    pub(in crate::confine) fn write(
        c: &Confined,
        bytes: &[u8],
        create: CreateDirs,
    ) -> Result<(), ConfineError> {
        let (parent, name) = parent_of(c)?;
        // Create missing directories one by one, refusing a link or junction on the way.
        let mut dir = c.root.clone();
        for comp in c.rel.parent().into_iter().flat_map(Path::components) {
            dir.push(comp.as_os_str());
            match fs::symlink_metadata(&dir) {
                Ok(m) if m.is_dir() && !m.file_type().is_symlink() && !is_reparse(&m) => {}
                Ok(_) => return Err(c.changed()),
                Err(e) if e.kind() == io::ErrorKind::NotFound && create == CreateDirs::Yes => {
                    match fs::create_dir(&dir) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(c.io(e)),
                    }
                    if is_link(&dir) {
                        return Err(c.changed());
                    }
                }
                Err(e) => return Err(c.io(e)),
            }
        }
        let target = parent.join(&name);
        let existing = match fs::symlink_metadata(&target) {
            Ok(m) if m.file_type().is_symlink() || is_reparse(&m) => {
                return Err(ConfineError::FinalSymlink { path: c.path() })
            }
            Ok(m) if !m.is_file() => return Err(c.io(not_regular())),
            Ok(m) if m.permissions().readonly() => return Err(c.io(read_only())),
            Ok(m) => Some(m),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(c.io(e)),
        };
        let tmp = parent.join(temp_name(&name));
        let result = (|| -> Result<(), ConfineError> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(|e| c.io(e))?;
            if let Some(meta) = &existing {
                fs::set_permissions(&tmp, meta.permissions()).map_err(|e| c.io(e))?;
            }
            file.write_all(bytes).map_err(|e| c.io(e))?;
            file.sync_all().map_err(|e| c.io(e))?;
            drop(file);
            // Re-verify right before every rename attempt (std has no `renameat`); the shared
            // helper retries a transient `PermissionDenied` on Windows like `write_atomic`.
            crate::write::replace_checked(
                &tmp,
                &target,
                || {
                    verify_parent(c, &parent)?;
                    if is_link(&target) {
                        return Err(ConfineError::FinalSymlink { path: c.path() });
                    }
                    Ok(())
                },
                |e| c.io(e),
            )
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }
}

pub(in crate::confine) use imp::{read, write};
