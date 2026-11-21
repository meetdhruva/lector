//! Path confinement: the one place that decides whether a caller-supplied path lies inside a
//! root (ADR 0010, item 1). Agents (`fs/read_text_file`, `fs/write_text_file`), MCP resources
//! and the path arguments of registry commands from sandboxed sources all go through it.
//!
//! # Rules
//!
//! A path (absolute, or relative to the root) is resolved **physically**, the way the kernel
//! would walk it: every existing component is looked up, every symlink is replaced by its target
//! (relative targets against the link's directory, chains up to 40 hops) and `..` applies to the
//! directory reached so far, so `link/..` is the parent of the link's *target*. Components that
//! do not exist yet (a file or directories about to be created) are appended lexically. The
//! deepest existing directory of the result is canonicalized once more, which normalizes case on
//! case-insensitive file systems and expands Windows 8.3 short names.
//!
//! The resolved path is then checked:
//! * it must lie inside the canonical root ([`ConfineError::Outside`]);
//! * no component of the resolved path relative to the root may name a git directory: `.git`
//!   compared case-insensitively, ignoring trailing dots and spaces, an NTFS stream suffix
//!   (`.git::$INDEX_ALLOCATION`) and the code points HFS+ ignores, plus the Windows short names
//!   `git~<n>` ([`ConfineError::GitDir`]);
//! * it must not lie inside a directory the caller forbade, such as Lector's own data directory
//!   when the root contains it ([`ConfineError::Forbidden`], [`Confinement::forbid`]).
//!
//! # Symlink policy
//!
//! * **Reads** ([`Access::Read`]) may follow a link, final component included, when the target
//!   resolves inside the root and outside every git directory: `docs -> src/docs` is readable,
//!   `cfg -> .git/config` is not.
//! * **Writes** ([`Access::Write`]) never go through a final-component link: a path whose last
//!   component is a symlink is refused ([`ConfineError::FinalSymlink`]), dangling or not.
//! * **Intermediate links** are resolved and the result checked, for reads and writes alike:
//!   with `docs -> .git`, `docs/config`, `docs/hooks/pre-commit` and `docs/newdir/x` all resolve
//!   into `.git` and are refused; with `src2 -> src`, `src2/new.rs` resolves to `src/new.rs` and
//!   is allowed.
//!
//! # Check and use are not separable
//!
//! A check alone could be raced: a directory swapped for a symlink between the check and the
//! write would redirect it. [`Confined::write`] and [`Confined::read`] therefore open the
//! resolved path again in a way that cannot follow a link that was not there at check time:
//! * **Unix (Linux, macOS, the BSDs):** starting from the canonical root, every directory of the
//!   resolved path is opened with `openat(O_DIRECTORY | O_NOFOLLOW)` relative to the previous
//!   one (created with `mkdirat` first when asked to), so a link swapped in anywhere fails with
//!   `ELOOP` / `ENOTDIR` ([`ConfineError::Changed`]). A write creates its temporary file with
//!   `openat(O_CREAT | O_EXCL | O_NOFOLLOW)` and replaces the target with `renameat` in the same
//!   directory; `rename` replaces a link that appeared at the target instead of following it. A
//!   read opens the file with `O_NOFOLLOW | O_NONBLOCK` and refuses anything but a regular file.
//! * **Windows and other platforms:** std has no `openat`. Directories are created one by one and
//!   each is checked not to be a symlink or junction; the temporary file is created with
//!   `create_new`; right before the rename the parent directory is canonicalized again and must
//!   still be the resolved parent, and the target must still not be a link. Reads open the file
//!   with `FILE_FLAG_OPEN_REPARSE_POINT` (Windows), refuse a reparse point, and re-verify the
//!   resolved path after the open. A window between that re-verification and the rename (or the
//!   open) remains on these platforms.
//!
//! Nothing here writes except [`Confined::write`], which callers reach only from an explicit
//! request (an agent's `fs/write_text_file`, a registry write command), see "Write policy" in
//! `docs/ARCHITECTURE.md`.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Component, Path, PathBuf};

mod io_ops;

/// Symlink hops followed at most while resolving one path.
const MAX_LINK_HOPS: usize = 40;

/// What the caller is going to do with the path: decides the [symlink policy](self#symlink-policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Read it: a final-component link is followed when its target is allowed.
    Read,
    /// Write it: a final-component link is refused.
    Write,
}

/// Why a path was refused. Each variant names the reason; none of them wrote anything.
#[derive(Debug, thiserror::Error)]
pub enum ConfineError {
    /// The path was empty.
    #[error("empty path")]
    Empty,
    /// The root cannot be resolved (missing, unreadable).
    #[error("the folder {root} cannot be resolved: {source}")]
    Root {
        /// The root as given.
        root: PathBuf,
        /// What failed.
        #[source]
        source: io::Error,
    },
    /// The path resolves outside the root (`..`, an absolute path elsewhere, a link leading out).
    #[error("{path} is outside the project")]
    Outside {
        /// The path as given.
        path: PathBuf,
    },
    /// The path resolves into a git directory (`.git`, any case, or a Windows short name).
    #[error("{path} is inside a git directory")]
    GitDir {
        /// The path as given.
        path: PathBuf,
    },
    /// The path resolves into a directory the caller forbade (Lector's data directory).
    #[error("{path} is inside Lector's own data")]
    Forbidden {
        /// The path as given.
        path: PathBuf,
    },
    /// A write whose final component is a symlink.
    #[error("{path} is a symbolic link; writes do not follow links")]
    FinalSymlink {
        /// The path as given.
        path: PathBuf,
    },
    /// Too many links, or a link that cannot be read.
    #[error("{path} cannot be resolved: {source}")]
    Unresolvable {
        /// The path as given.
        path: PathBuf,
        /// What failed.
        #[source]
        source: io::Error,
    },
    /// Between the check and the read or write, a component became a link or changed type.
    #[error("{path} changed while it was being opened (a link appeared)")]
    Changed {
        /// The resolved path.
        path: PathBuf,
    },
    /// The read or write itself failed.
    #[error("{path}: {source}")]
    Io {
        /// The resolved path.
        path: PathBuf,
        /// What failed.
        #[source]
        source: io::Error,
    },
}

/// A root with the directories under it that callers may not reach. Build it once per root and
/// check many paths with [`Confinement::check`].
#[derive(Debug, Clone)]
pub struct Confinement {
    root: PathBuf,
    forbidden: Vec<PathBuf>,
}

impl Confinement {
    /// Confinement to `root` (canonicalized; a root that is itself a link is resolved).
    pub fn new(root: &Path) -> Result<Confinement, ConfineError> {
        let canonical = dunce::canonicalize(root).map_err(|source| ConfineError::Root {
            root: root.to_path_buf(),
            source,
        })?;
        Ok(Confinement {
            root: canonical,
            forbidden: Vec::new(),
        })
    }

    /// Also refuses every path inside `dir` (resolved the same way). Callers pass Lector's data
    /// directory so a project that contains it (portable mode, a data directory override) cannot
    /// reach the shadow repositories, logs or the database. A `dir` outside the root changes
    /// nothing.
    pub fn forbid(mut self, dir: &Path) -> Confinement {
        let resolved = dunce::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        if resolved.starts_with(&self.root) && !self.forbidden.contains(&resolved) {
            self.forbidden.push(resolved);
        }
        self
    }

    /// The canonical root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves `path` (absolute, or relative to the root) and checks it for `access`; see the
    /// [module documentation](self) for the rules.
    pub fn check(&self, path: impl AsRef<Path>, access: Access) -> Result<Confined, ConfineError> {
        let given = path.as_ref();
        if given.as_os_str().is_empty() || given.to_string_lossy().trim().is_empty() {
            return Err(ConfineError::Empty);
        }
        let joined = if given.is_absolute() {
            given.to_path_buf()
        } else {
            self.root.join(given)
        };
        let resolved = resolve(&joined, access, given)?;
        let resolved = canonicalize_existing_prefix(&resolved);
        let outside = || ConfineError::Outside {
            path: given.to_path_buf(),
        };
        let rel = resolved.strip_prefix(&self.root).map_err(|_| outside())?;
        if rel.components().any(|c| match c {
            Component::Normal(name) => is_git_dir_name(name),
            _ => true,
        }) {
            return Err(ConfineError::GitDir {
                path: given.to_path_buf(),
            });
        }
        if self.forbidden.iter().any(|f| resolved.starts_with(f)) {
            return Err(ConfineError::Forbidden {
                path: given.to_path_buf(),
            });
        }
        let rel = rel.to_path_buf();
        if access == Access::Write {
            if rel.as_os_str().is_empty() {
                return Err(ConfineError::Io {
                    path: resolved,
                    source: io::Error::new(io::ErrorKind::InvalidInput, "not a file"),
                });
            }
            // `resolve` refused a final link that existed; this repeats it on the canonical
            // spelling (a case-folded name may differ from the one looked up).
            if std::fs::symlink_metadata(&resolved).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(ConfineError::FinalSymlink {
                    path: given.to_path_buf(),
                });
            }
        }
        Ok(Confined {
            root: self.root.clone(),
            rel,
            access,
        })
    }

    /// [`Confinement::check`] with [`Access::Read`].
    pub fn read(&self, path: impl AsRef<Path>) -> Result<Confined, ConfineError> {
        self.check(path, Access::Read)
    }

    /// [`Confinement::check`] with [`Access::Write`].
    pub fn write(&self, path: impl AsRef<Path>) -> Result<Confined, ConfineError> {
        self.check(path, Access::Write)
    }
}

/// One-shot form of [`Confinement::check`] without forbidden directories.
pub fn confine(
    root: &Path,
    path: impl AsRef<Path>,
    access: Access,
) -> Result<Confined, ConfineError> {
    Confinement::new(root)?.check(path, access)
}

/// A path that passed [`Confinement::check`]: the canonical root plus the resolved path relative
/// to it (no symlink in any existing component). Use [`Confined::read`] / [`Confined::write`] to
/// touch it without following a link that appeared since the check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confined {
    root: PathBuf,
    rel: PathBuf,
    access: Access,
}

/// Whether [`Confined::write`] creates missing parent directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateDirs {
    /// Create them (inside the root, never through a link).
    Yes,
    /// Fail with `NotFound` when the parent is missing.
    No,
}

impl Confined {
    /// The resolved absolute path (canonical root joined with [`Confined::relative`]).
    pub fn path(&self) -> PathBuf {
        self.root.join(&self.rel)
    }

    /// The canonical root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The resolved path relative to the root (empty for the root itself).
    pub fn relative(&self) -> &Path {
        &self.rel
    }

    /// The access it was checked for.
    pub fn access(&self) -> Access {
        self.access
    }

    /// The relative path with `/` separators (`src/lib.rs`).
    pub fn relative_slash(&self) -> String {
        self.rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/")
    }

    /// Reads the file without following a link that was not there at check time (see
    /// [the module documentation](self#check-and-use-are-not-separable)). Only regular files
    /// are read.
    pub fn read(&self) -> Result<Vec<u8>, ConfineError> {
        io_ops::read(self)
    }

    /// Writes `bytes` atomically (temporary file, fsync, rename in the same directory), never
    /// through a symlink, keeping an existing file's permissions and refusing a read-only one
    /// (`PermissionDenied`). Only for paths checked with [`Access::Write`]. See
    /// [the module documentation](self#check-and-use-are-not-separable) for what each platform
    /// does against a link swapped in after the check.
    pub fn write(&self, bytes: &[u8], create_dirs: CreateDirs) -> Result<(), ConfineError> {
        if self.access != Access::Write || self.rel.as_os_str().is_empty() {
            return Err(ConfineError::Io {
                path: self.path(),
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "path was not confined for writing",
                ),
            });
        }
        io_ops::write(self, bytes, create_dirs)
    }

    fn changed(&self) -> ConfineError {
        ConfineError::Changed { path: self.path() }
    }

    fn io(&self, source: io::Error) -> ConfineError {
        ConfineError::Io {
            path: self.path(),
            source,
        }
    }
}

/// Resolves `joined` physically (see the module documentation). `given` is only for errors.
fn resolve(joined: &Path, access: Access, given: &Path) -> Result<PathBuf, ConfineError> {
    let unresolvable = |source: io::Error| ConfineError::Unresolvable {
        path: given.to_path_buf(),
        source,
    };
    let mut out = PathBuf::new();
    let mut queue: VecDeque<OsString> = VecDeque::new();
    for c in joined.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => queue.push_back(OsString::from("..")),
            Component::Normal(n) => queue.push_back(n.to_os_string()),
        }
    }
    let mut hops = 0usize;
    // Once a component is missing, the rest cannot contain links: append lexically.
    let mut missing = false;
    while let Some(name) = queue.pop_front() {
        if name == ".." {
            out.pop();
            if missing {
                // Back above the missing part: what follows may exist (and be a link) again.
                missing = std::fs::symlink_metadata(&out).is_err();
            }
            continue;
        }
        if name == "." {
            continue;
        }
        let next = out.join(&name);
        if missing {
            out = next;
            continue;
        }
        let meta = match std::fs::symlink_metadata(&next) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                missing = true;
                out = next;
                continue;
            }
            Err(e) => return Err(unresolvable(e)),
        };
        if !meta.file_type().is_symlink() {
            out = next;
            continue;
        }
        if access == Access::Write && queue.iter().all(|n| n == ".") {
            return Err(ConfineError::FinalSymlink {
                path: given.to_path_buf(),
            });
        }
        hops += 1;
        if hops > MAX_LINK_HOPS {
            return Err(unresolvable(io::Error::new(
                io::ErrorKind::InvalidInput,
                "too many levels of symbolic links",
            )));
        }
        let target = std::fs::read_link(&next).map_err(unresolvable)?;
        let target = dunce::simplified(&target).to_path_buf();
        let mut prefix: Vec<OsString> = Vec::new();
        if target.is_absolute() {
            out = PathBuf::new();
        }
        for c in target.components() {
            match c {
                Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
                Component::CurDir => {}
                Component::ParentDir => prefix.push(OsString::from("..")),
                Component::Normal(n) => prefix.push(n.to_os_string()),
            }
        }
        for n in prefix.into_iter().rev() {
            queue.push_front(n);
        }
    }
    Ok(out)
}

/// Canonicalizes the deepest existing ancestor of `path` (case and short names) and appends the
/// missing rest. `path` contains no links in its existing part, so this changes only spelling.
fn canonicalize_existing_prefix(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest: Vec<&OsStr> = Vec::new();
    loop {
        if std::fs::symlink_metadata(existing).is_ok() {
            break;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
    let mut out = dunce::canonicalize(existing).unwrap_or_else(|_| existing.to_path_buf());
    // A final link (reads) canonicalizes to its target, which `resolve` already followed.
    for name in rest.into_iter().rev() {
        out.push(name);
    }
    out
}

/// Whether a path component names a git directory on some file system: `.git` compared
/// case-insensitively (macOS, Windows), ignoring trailing dots and spaces (Windows strips them),
/// an NTFS alternate-stream suffix after `:`, and the code points HFS+ ignores (as git's own
/// `is_hfs_dotgit` does); and the Windows 8.3 short names `git~1`, `git~2`, ...
pub fn is_git_dir_name(component: &OsStr) -> bool {
    let text = component.to_string_lossy();
    let cleaned: String = text
        .chars()
        .filter(|c| {
            !matches!(
                *c as u32,
                0x200C..=0x200F | 0x202A..=0x202E | 0x206A..=0x206F | 0xFEFF
            )
        })
        .collect();
    let cleaned = cleaned.split(':').next().unwrap_or("");
    let cleaned = cleaned.trim_end_matches(['.', ' ']);
    if cleaned.eq_ignore_ascii_case(".git") {
        return true;
    }
    let lower = cleaned.to_ascii_lowercase();
    lower
        .strip_prefix("git~")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests;
