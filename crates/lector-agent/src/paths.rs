//! Path checks for agent file access: everything must stay inside the project root.

use std::path::{Component, Path, PathBuf};

/// Resolves `path` (absolute, or relative to `root`) to an absolute path inside `root`.
/// Refuses paths that leave the root (`..`, symlinked directories pointing outside) and
/// anything inside a `.git` directory.
pub fn resolve_in_root(root: &Path, path: &str) -> Result<PathBuf, String> {
    if path.trim().is_empty() {
        return Err("empty path".into());
    }
    let raw = Path::new(path);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
    let normalized = normalize(&joined).ok_or_else(|| format!("invalid path {path}"))?;
    let canonical_root = dunce_canonical(root);
    let lexical_root = normalize(root).unwrap_or_else(|| root.to_path_buf());
    let inside = normalized.starts_with(&lexical_root) || normalized.starts_with(&canonical_root);
    if !inside {
        return Err(format!("{path} is outside the project"));
    }
    // Follow symlinks of the deepest existing ancestor. `symlink_metadata` (not `exists`)
    // so a dangling symlink counts as existing: writing through it would create its target,
    // which may be outside the project; such paths fail to canonicalize and are refused.
    let mut existing = normalized.as_path();
    while std::fs::symlink_metadata(existing).is_err() {
        match existing.parent() {
            Some(p) => existing = p,
            None => break,
        }
    }
    let real = std::fs::canonicalize(existing)
        .map(|c| strip_verbatim(&c))
        .map_err(|_| format!("{path} is outside the project (unresolvable link)"))?;
    if !real.starts_with(&canonical_root) {
        return Err(format!("{path} is outside the project"));
    }
    let rel = normalized
        .strip_prefix(&lexical_root)
        .or_else(|_| normalized.strip_prefix(&canonical_root))
        .map_err(|_| format!("{path} is outside the project"))?;
    if rel
        .components()
        .any(|c| is_git_dir_name(&c.as_os_str().to_string_lossy()))
    {
        return Err(format!("{path} is inside .git"));
    }
    Ok(canonical_root.join(rel))
}

/// Project-relative, `/`-separated form of an absolute path inside `root`.
pub fn relative(root: &Path, abs: &Path) -> String {
    let canonical_root = dunce_canonical(root);
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

/// Whether a path component names a `.git` directory on some file system: compared
/// case-insensitively (macOS, Windows), ignoring trailing dots/spaces (Windows strips
/// them) and the code points HFS+ ignores (as git's own `is_hfs_dotgit` does).
fn is_git_dir_name(component: &str) -> bool {
    let cleaned: String = component
        .chars()
        .filter(|c| {
            !matches!(
                *c as u32,
                0x200C..=0x200F | 0x202A..=0x202E | 0x206A..=0x206F | 0xFEFF
            )
        })
        .collect();
    let cleaned = cleaned.trim_end_matches(['.', ' ']);
    cleaned.eq_ignore_ascii_case(".git") || cleaned.eq_ignore_ascii_case("git~1")
}

/// Strips the Windows verbatim prefix the same way dunce does, without the dependency.
fn strip_verbatim(c: &Path) -> PathBuf {
    let s = c.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => c.to_path_buf(),
    }
}

fn dunce_canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p)
        .map(|c| strip_verbatim(&c))
        .unwrap_or_else(|_| p.to_path_buf())
}

fn normalize(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stays_inside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let ok = resolve_in_root(&root, "src/a.rs").unwrap();
        assert!(ok.ends_with("src/a.rs"));
        let abs = root.join("src").join("new").join("b.rs");
        assert!(resolve_in_root(&root, abs.to_str().unwrap()).is_ok());
        assert!(resolve_in_root(&root, "../x").is_err());
        assert!(resolve_in_root(&root, "src/../../x").is_err());
        assert!(resolve_in_root(&root, "/etc/passwd").is_err());
        assert!(resolve_in_root(&root, ".git/config").is_err());
        assert!(resolve_in_root(&root, "").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path(), root.join("escape")).unwrap();
            assert!(resolve_in_root(&root, "escape/secret").is_err());
        }
        assert!(resolve_in_root(&root, ".GIT/config").is_err());
        assert!(resolve_in_root(&root, "sub/.Git./hooks/pre-commit").is_err());
        assert!(resolve_in_root(&root, ".git\u{200c}/config").is_err());
        assert!(resolve_in_root(&root, "src/.gitignore").is_ok());
        assert_eq!(relative(&root, &ok), "src/a.rs");
        assert!(file_uri(&ok).starts_with("file:///"));
    }

    /// Regression: a dangling symlink inside the project pointing outside used to pass
    /// (it does not "exist"), and `write_atomic` then created its target outside the root.
    #[cfg(unix)]
    #[test]
    fn dangling_symlinks_leading_out_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let outside = dir.path().join("outside.txt");
        std::os::unix::fs::symlink(&outside, root.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("nodir"), root.join("dlink")).unwrap();
        assert!(resolve_in_root(&root, "link.txt").is_err());
        assert!(resolve_in_root(&root, "dlink/x.rs").is_err());
        // A dangling link to a missing file inside the project is refused too (cannot resolve).
        std::os::unix::fs::symlink(root.join("missing"), root.join("inner")).unwrap();
        assert!(resolve_in_root(&root, "inner").is_err());
        assert!(!outside.exists());
    }
}
