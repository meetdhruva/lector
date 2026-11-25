//! The shadow repository of a folder that is not a git repository (ADR 0011): created at the
//! first backup, found without creating anything, no alternates, path-scoped operations only.

use std::fs;
use std::io;
use std::path::Path;

use crate::error::Result;

use super::open::remember_shadow;
use super::ShadowRepo;

impl ShadowRepo {
    /// Opens (creating on first use) the shadow repository of `folder`, a folder that is not a
    /// git repository, below `data_dir`: at [`ShadowRepo::path_for`] (keyed by the folder's
    /// canonical path), owner-only like every shadow, with the folder as work tree and no
    /// alternates. Only path-scoped operations work on it ([`ShadowRepo::is_folder`]). Nothing
    /// is written into `folder` and no git command runs there.
    pub fn open_folder(data_dir: &Path, folder: &Path) -> Result<ShadowRepo> {
        let workdir = dunce::canonicalize(folder)?;
        if !workdir.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a folder", workdir.display()),
            )
            .into());
        }
        let shadow = ShadowRepo {
            git_dir: Self::path_for(data_dir, &workdir),
            workdir,
            folder: true,
        };
        shadow.ensure_initialized(None)?;
        remember_shadow(&shadow);
        Ok(shadow)
    }

    /// The shadow repository of the folder `folder` if one exists below `data_dir` (a backup was
    /// taken there before); `None` otherwise. Never creates or changes anything, so looking for
    /// backups (opening a folder, starting an agent, listing the activity log) leaves no trace.
    /// The handle is path-scoped like one from [`ShadowRepo::open_folder`].
    pub fn existing_folder(data_dir: &Path, folder: &Path) -> Option<ShadowRepo> {
        let workdir = dunce::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
        let git_dir = Self::path_for(data_dir, &workdir);
        let head = fs::symlink_metadata(git_dir.join("HEAD")).ok()?;
        if !head.is_file() {
            return None;
        }
        let shadow = ShadowRepo {
            git_dir,
            workdir,
            folder: true,
        };
        remember_shadow(&shadow);
        Some(shadow)
    }

    /// `true` for the shadow of a folder without git: whole-tree and index operations
    /// ([`ShadowRepo::snapshot_worktree`], [`ShadowRepo::snapshot_index`],
    /// [`ShadowRepo::index_fingerprints`], [`ShadowRepo::restore_index`]) refuse, and nothing is
    /// ignored ([`ShadowRepo::is_ignored`]).
    pub fn is_folder(&self) -> bool {
        self.folder
    }
}
