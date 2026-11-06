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
    // Follow symlinks of the deepest existing ancestor.
    let mut existing = normalized.as_path();
    while !existing.exists() {
        match existing.parent() {
            Some(p) => existing = p,
            None => break,
        }
    }
    let real = dunce_canonical(existing);
    if !real.starts_with(&canonical_root) {
        return Err(format!("{path} is outside the project"));
    }
    let rel = normalized
        .strip_prefix(&lexical_root)
        .or_else(|_| normalized.strip_prefix(&canonical_root))
        .map_err(|_| format!("{path} is outside the project"))?;
    if rel.components().any(|c| c.as_os_str() == ".git") {
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

fn dunce_canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p)
        .map(|c| {
            // Strip the Windows verbatim prefix the same way dunce does, without the dependency.
            let s = c.to_string_lossy();
            match s.strip_prefix(r"\\?\") {
                Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
                _ => c,
            }
        })
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
        assert_eq!(relative(&root, &ok), "src/a.rs");
        assert!(file_uri(&ok).starts_with("file:///"));
    }
}
