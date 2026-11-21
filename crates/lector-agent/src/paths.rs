//! Path helpers for agent file access. Whether a path is allowed is decided only by
//! [`lector_fs::confine`] (ADR 0010), reached through [`confine`].

use std::path::Path;

use lector_app::AppCore;
use lector_fs::{Access, ConfineError, Confined};

/// Confines an agent-supplied `path` (absolute, or relative to the project root) for `access`
/// with the one confinement function, [`lector_fs::Confinement`], as configured for sandboxed
/// callers by [`lector_app::commands::confinement`] (Lector's data and settings directories
/// forbidden too). The rules (resolved path, git directories in every spelling, no write through
/// a final symlink) are documented in [`lector_fs::confine`].
pub fn confine(
    core: &AppCore,
    root: &Path,
    path: &str,
    access: Access,
) -> Result<Confined, ConfineError> {
    lector_app::commands::confinement(core, root)?.check(path, access)
}

/// Project-relative, `/`-separated form of an absolute path inside `root`.
pub fn relative(root: &Path, abs: &Path) -> String {
    let canonical_root = lector_fs::Confinement::new(root)
        .map(|c| c.root().to_path_buf())
        .unwrap_or_else(|_| root.to_path_buf());
    let rel = abs
        .strip_prefix(&canonical_root)
        .or_else(|_| abs.strip_prefix(root))
        .unwrap_or(abs);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// A `file://` URI for an absolute path.
pub fn file_uri(abs: &Path) -> String {
    let s = abs.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_and_uri() {
        let dir = tempfile::tempdir().unwrap();
        let root = lector_fs::Confinement::new(dir.path())
            .unwrap()
            .root()
            .to_path_buf();
        let abs = root.join("src").join("a.rs");
        assert_eq!(relative(&root, &abs), "src/a.rs");
        assert!(file_uri(&abs).starts_with("file:///"));
    }
}
