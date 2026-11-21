//! Source files as the rules see them: normalised text, tokens, test-only classification.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::error::{ArchError, Result};
use crate::lexer::{scan, Scanned, Token};

/// One `.rs` file: forward-slash path relative to the repository root, LF-only text, tokens.
#[derive(Debug, Clone)]
pub struct SourceFile {
    /// Path relative to the repository root, always with `/` separators.
    pub path: String,
    /// File text with CRLF normalised to LF (line numbers and offsets refer to this text).
    pub text: String,
    /// Tokens and test regions.
    pub scanned: Scanned,
    /// The whole file is test-only (by its path or because a `#[cfg(test)] mod x;` points at it).
    pub test_only: bool,
}

impl SourceFile {
    /// Build a source file from its repository-relative path and raw text.
    pub fn new(path: impl Into<String>, raw: &str) -> Self {
        let path = path.into().replace('\\', "/");
        let text = if raw.contains('\r') {
            raw.replace("\r\n", "\n")
        } else {
            raw.to_string()
        };
        let scanned = scan(&text);
        let test_only = is_test_path(&path) || scanned.whole_file_test;
        SourceFile {
            path,
            text,
            scanned,
            test_only,
        }
    }

    /// The tokens of the file.
    pub fn tokens(&self) -> &[Token] {
        &self.scanned.tokens
    }

    /// Text of token `i` (`""` when out of range).
    pub fn tok(&self, i: usize) -> &str {
        self.scanned.text(&self.text, i)
    }

    /// Whether token `i` is test-only code.
    pub fn is_test(&self, i: usize) -> bool {
        self.test_only || self.scanned.in_test.get(i).copied().unwrap_or(false)
    }

    /// Line of token `i` (1 when out of range).
    pub fn line(&self, i: usize) -> u32 {
        self.scanned.tokens.get(i).map_or(1, |t| t.line)
    }

    /// True when the tokens starting at `i` have exactly the texts in `pattern`.
    pub fn seq(&self, i: usize, pattern: &[&str]) -> bool {
        pattern
            .iter()
            .enumerate()
            .all(|(k, p)| self.tok(i + k) == *p)
    }

    /// Number of lines (a trailing newline does not start a new line).
    pub fn line_count(&self) -> usize {
        self.text.lines().count()
    }

    /// Index of the bracket closing the `(`, `[` or `{` at `open` (last token when unbalanced).
    pub fn close_of(&self, open: usize) -> usize {
        let (o, c) = match self.tok(open) {
            "(" => ("(", ")"),
            "[" => ("[", "]"),
            _ => ("{", "}"),
        };
        let mut depth = 0usize;
        for k in open..self.tokens().len() {
            let s = self.tok(k);
            if s == o {
                depth += 1;
            } else if s == c {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return k;
                }
            }
        }
        self.tokens().len().saturating_sub(1)
    }
}

/// Test-only by path, which is deliberately narrow (fail closed: a file that is merely *named*
/// like a test file is production unless a `#[cfg(test)] mod x;` declaration or an inner
/// `#![cfg(test)]` says otherwise). Only Cargo's own non-shipping targets qualify:
/// `crates/<name>/tests/`, `crates/<name>/benches/` and `crates/<name>/examples/`.
pub fn is_test_path(path: &str) -> bool {
    let mut parts = path.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (
            Some("crates"),
            Some(_),
            Some("tests" | "benches" | "examples"),
            Some(_)
        )
    )
}

/// Mark files that a `#[cfg(test)] mod name;` declaration points at (and everything below them).
pub fn mark_test_modules(files: &mut [SourceFile]) {
    let mut test_files: BTreeSet<String> = BTreeSet::new();
    let mut test_dirs: Vec<String> = Vec::new();
    for f in files.iter() {
        if f.scanned.test_mod_decls.is_empty() {
            continue;
        }
        let (dir, file) = match f.path.rsplit_once('/') {
            Some((d, n)) => (d.to_string(), n.to_string()),
            None => (String::new(), f.path.clone()),
        };
        let stem = file.strip_suffix(".rs").unwrap_or(&file);
        let module_dir = if matches!(stem, "lib" | "main" | "mod") {
            dir
        } else if dir.is_empty() {
            stem.to_string()
        } else {
            format!("{dir}/{stem}")
        };
        for name in &f.scanned.test_mod_decls {
            let base = if module_dir.is_empty() {
                name.clone()
            } else {
                format!("{module_dir}/{name}")
            };
            test_files.insert(format!("{base}.rs"));
            test_files.insert(format!("{base}/mod.rs"));
            test_dirs.push(format!("{base}/"));
        }
    }
    for f in files.iter_mut() {
        if test_files.contains(&f.path) || test_dirs.iter().any(|d| f.path.starts_with(d.as_str()))
        {
            f.test_only = true;
        }
    }
}

/// Read every `.rs` file under `<root>/crates` (skipping `target` and hidden directories),
/// sorted by path.
pub fn load_sources(root: &Path) -> Result<Vec<SourceFile>> {
    let mut found: Vec<String> = Vec::new();
    walk(root, "crates", &mut found)?;
    found.sort();
    let mut files = Vec::with_capacity(found.len());
    for rel in found {
        let full = root.join(&rel);
        let bytes = fs::read(&full).map_err(|e| ArchError::io(rel.clone(), e))?;
        let raw = String::from_utf8_lossy(&bytes);
        files.push(SourceFile::new(rel, &raw));
    }
    mark_test_modules(&mut files);
    Ok(files)
}

fn walk(root: &Path, rel_dir: &str, out: &mut Vec<String>) -> Result<()> {
    let dir = root.join(rel_dir);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(ArchError::io(rel_dir.to_string(), e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| ArchError::io(rel_dir.to_string(), e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = format!("{rel_dir}/{name}");
        let kind = entry
            .file_type()
            .map_err(|e| ArchError::io(rel.clone(), e))?;
        if kind.is_dir() {
            if name == "target" || name.starts_with('.') {
                continue;
            }
            walk(root, &rel, out)?;
        } else if kind.is_file() && name.ends_with(".rs") {
            out.push(rel);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paths() {
        for p in [
            "crates/a/tests/x.rs",
            "crates/a/tests/common/mod.rs",
            "crates/a/benches/b.rs",
            "crates/a/examples/e.rs",
        ] {
            assert!(is_test_path(p), "{p}");
        }
        // Named like tests but not Cargo test targets: production unless declared otherwise.
        for p in [
            "crates/a/src/lib.rs",
            "crates/a/src/foo_tests.rs",
            "crates/a/src/tests.rs",
            "crates/a/src/test_support.rs",
            "crates/a/src/test_support/mod.rs",
            "crates/a/src/health/tests.rs",
            "crates/a/src/tests/x.rs",
            "crates/a/src/examples/x.rs",
            "crates/a/src/testing.rs",
            "crates/a/build.rs",
            "crates/a/tests",
            "tests/x.rs",
            "other/a/tests/x.rs",
        ] {
            assert!(!is_test_path(p), "{p}");
        }
    }

    #[test]
    fn crlf_is_normalised_and_lines_are_counted() {
        let f = SourceFile::new("crates/a/src/x.rs", "fn a() {}\r\nfn b() {}\r\n");
        assert_eq!(f.line_count(), 2);
        assert!(!f.text.contains('\r'));
        assert_eq!(f.line(6), 2);
    }

    #[test]
    fn cfg_test_mod_declaration_marks_the_target_file() {
        let mut files = vec![
            SourceFile::new(
                "crates/a/src/lib.rs",
                "#[cfg(test)]\nmod checks;\nmod real;\n",
            ),
            SourceFile::new("crates/a/src/checks.rs", "fn t() {}"),
            SourceFile::new("crates/a/src/real.rs", "fn r() {}"),
        ];
        mark_test_modules(&mut files);
        assert!(files[1].test_only);
        assert!(!files[2].test_only);
    }

    #[test]
    fn cfg_test_mod_in_a_non_root_file_and_directory_modules() {
        let mut files = vec![
            SourceFile::new("crates/a/src/db/mod.rs", "#[cfg(test)]\nmod harness;\n"),
            SourceFile::new("crates/a/src/db/harness/mod.rs", "mod inner;"),
            SourceFile::new("crates/a/src/db/harness/inner.rs", "fn i() {}"),
            SourceFile::new("crates/a/src/db/other.rs", "fn o() {}"),
            SourceFile::new("crates/a/src/view.rs", "#[cfg(test)]\nmod fixtures;\n"),
            SourceFile::new("crates/a/src/view/fixtures.rs", "fn f() {}"),
        ];
        mark_test_modules(&mut files);
        let flags: Vec<bool> = files.iter().map(|f| f.test_only).collect();
        assert_eq!(flags, [false, true, true, false, false, true]);
    }

    #[test]
    fn token_helpers() {
        let f = SourceFile::new(
            "crates/a/src/x.rs",
            "fn f() { thread::spawn(g); }\n#[cfg(test)]\nmod t { fn h() { thread::spawn(g); } }",
        );
        let hits: Vec<usize> = (0..f.tokens().len())
            .filter(|i| f.seq(*i, &["thread", "::", "spawn"]))
            .collect();
        assert_eq!(hits.len(), 2);
        assert!(!f.is_test(hits[0]));
        assert!(f.is_test(hits[1]));
    }
}
