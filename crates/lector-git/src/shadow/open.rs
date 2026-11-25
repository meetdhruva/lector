//! Opening a shadow repository: object directory, alternates entry and the process-wide registry of
//! shadows that were opened (for resolving blobs that only the shadow holds).

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::{Oid, Repo, Side};

use super::command::exec;
use super::{ShadowRepo, ShadowSnapshot};

impl ShadowRepo {
    /// Creates the shadow if needed and makes it private. `user_objects` is the user
    /// repository's object directory (alternates); `None` for a folder without git, where no
    /// git command may run in the folder, so the object-format probe and alternates are skipped
    /// (the shadow Lector creates is SHA-1).
    pub(super) fn ensure_initialized(&self, user_objects: Option<&Path>) -> Result<()> {
        // Refuse SHA-256 repositories up front.
        if let Some(Ok(out)) = user_objects.map(|_| {
            self.user_git(&["rev-parse", "--show-object-format"])
                .output()
        }) {
            let format = String::from_utf8_lossy(&out.stdout);
            if out.status.success() && !format.trim().is_empty() && format.trim() != "sha1" {
                return Err(Error::Git(format!(
                    "unsupported object format {:?} (the shadow repository needs sha1)",
                    format.trim()
                )));
            }
        }
        // Private (ADR 0010): the shadow holds backups of git-ignored files such as `.env`.
        // Directories are created `0700`, and `core.sharedRepository=0600` makes git itself
        // create every later file `0600` and directory `0700`, whatever the umask.
        if !self.git_dir.join("HEAD").is_file() {
            let mut cmd = self.base_command();
            if let Some(parent) = self.git_dir.parent() {
                lector_base::private::create_dir_all(parent)?;
                // Not in the work tree: `init` names its target, so nothing is read from where it runs.
                cmd.current_dir(parent);
            }
            cmd.args(["init", "--bare", "-q", "--template=", "--shared=0600"])
                .arg(&self.git_dir);
            exec(cmd, "git init", None)?;
        }
        self.ensure_private()?;
        if let Some(user_objects) = user_objects {
            self.sync_alternates(user_objects)?;
        }
        let marker = self.git_dir.join("lector-workdir");
        let workdir_line = format!("{}\n", self.workdir.display());
        if fs::read_to_string(&marker).ok().as_deref() != Some(workdir_line.as_str()) {
            lector_base::private::write(&marker, workdir_line.as_bytes())?;
        }
        Ok(())
    }

    fn sync_alternates(&self, user_objects: &Path) -> Result<()> {
        // Alternates: the user's objects are read-only here; keep the file in sync with
        // where the objects are now (the repository may have moved).
        let alternates = self.git_dir.join("objects").join("info").join("alternates");
        // Canonical spelling (no `\\?\` on Windows): the entry keeps working if the repo
        // was opened through a symlinked path (macOS /var -> /private/var).
        let user_objects =
            dunce::canonicalize(user_objects).unwrap_or_else(|_| user_objects.to_path_buf());
        let wanted = format!("{}\n", alternates_entry(&user_objects));
        if fs::read_to_string(&alternates).ok().as_deref() != Some(wanted.as_str()) {
            if let Some(parent) = alternates.parent() {
                lector_base::private::create_dir_all(parent)?;
            }
            lector_base::private::write(&alternates, wanted.as_bytes())?;
        }
        Ok(())
    }
}

impl ShadowRepo {
    /// Makes a shadow repository created by an older Lector private: sets
    /// `core.sharedRepository=0600` when it is missing and narrows the repository directory
    /// (and its parent, `<data dir>/shadow`) to `0700`. Objects written before keep their mode
    /// but are unreachable for other users through the narrowed directory.
    fn ensure_private(&self) -> Result<()> {
        let config = fs::read_to_string(self.git_dir.join("config")).unwrap_or_default();
        if !config.contains("sharedRepository = 0600")
            && !config.contains("sharedrepository = 0600")
        {
            let mut cmd = self.shadow_git();
            cmd.args(["config", "core.sharedRepository", "0600"]);
            exec(cmd, "git config", None)?;
        }
        for dir in [Some(self.git_dir.as_path()), self.git_dir.parent()]
            .into_iter()
            .flatten()
        {
            lector_base::private::narrow_dir(dir)?;
        }
        Ok(())
    }
}

/// The object directory of a repository (honours linked worktrees' `commondir`).
pub(super) fn objects_dir(repo: &Repo) -> PathBuf {
    let git_dir = repo.git_dir();
    let common = match fs::read_to_string(git_dir.join("commondir")) {
        Ok(rel) => {
            let joined = git_dir.join(rel.trim());
            fs::canonicalize(&joined).unwrap_or(joined)
        }
        Err(_) => git_dir.to_path_buf(),
    };
    common.join("objects")
}

/// One line of `objects/info/alternates` naming `path`. Git reads a line starting with
/// `"` as a C-quoted string and skips lines starting with `#`, and a newline would split
/// the entry, so such paths are written quoted.
fn alternates_entry(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let needs_quotes =
        raw.starts_with('"') || raw.starts_with('#') || raw.chars().any(|c| c.is_ascii_control());
    if !needs_quotes {
        return raw.into_owned();
    }
    let mut out = String::from("\"");
    for c in raw.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_ascii_control() => out.push_str(&format!("\\{:03o}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Shadow repositories opened in this process: (canonical workdir, shadow), newest last.
static KNOWN_SHADOWS: std::sync::RwLock<Vec<(PathBuf, ShadowRepo)>> =
    std::sync::RwLock::new(Vec::new());

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(super) fn remember_shadow(shadow: &ShadowRepo) {
    let key = canonical(&shadow.workdir);
    let mut known = KNOWN_SHADOWS.write().unwrap_or_else(|e| e.into_inner());
    known.retain(|(_, s)| s.git_dir != shadow.git_dir);
    known.push((key, shadow.clone()));
}

pub(super) fn known_shadows(workdir: &Path) -> Vec<ShadowRepo> {
    let key = canonical(workdir);
    let known = KNOWN_SHADOWS.read().unwrap_or_else(|e| e.into_inner());
    known
        .iter()
        .rev()
        .filter(|(w, _)| *w == key)
        .map(|(_, s)| s.clone())
        .collect()
}

/// The known shadow repository of `workdir` that contains the snapshot commit `commit`.
pub(crate) fn known_shadow_with(workdir: &Path, commit: Oid) -> Option<ShadowRepo> {
    known_shadows(workdir).into_iter().find(|s| {
        s.contains(&ShadowSnapshot {
            commit,
            time: 0,
            note: String::new(),
        })
    })
}

/// [`Repo::load`]'s fallback for blobs that are not in the user's repository.
pub(crate) fn load_from_known_shadows(workdir: &Path, side: &Side) -> Result<Option<Vec<u8>>> {
    for shadow in known_shadows(workdir) {
        if let Ok(Some(bytes)) = shadow.load(side) {
            return Ok(Some(bytes));
        }
    }
    Ok(None)
}
