//! Glob helpers: gitignore-like pattern expansion on top of `globset`.

use globset::{Glob, GlobSet, GlobSetBuilder};

/// Builds a set from user patterns. A pattern matches a path (relative, `/`-separated)
/// when it matches the path itself, any descendant of it, and — if it has no `/` —
/// at any depth (so `node_modules` and `*.lock` behave like gitignore entries).
/// Invalid patterns are returned in the second element and otherwise ignored.
pub(crate) fn build_set(patterns: &[String]) -> (GlobSet, Vec<String>) {
    let mut builder = GlobSetBuilder::new();
    let mut invalid = Vec::new();
    for raw in patterns {
        let p = raw.trim();
        let p = p.strip_prefix("./").unwrap_or(p);
        let p = p.strip_prefix('/').unwrap_or(p);
        let p = p.trim_end_matches('/');
        if p.is_empty() {
            continue;
        }
        let mut variants = vec![p.to_string(), format!("{p}/**")];
        if !p.contains('/') {
            variants.push(format!("**/{p}"));
            variants.push(format!("**/{p}/**"));
        }
        let mut ok = true;
        let mut globs = Vec::new();
        for v in variants {
            match Glob::new(&v) {
                Ok(g) => globs.push(g),
                Err(_) => ok = false,
            }
        }
        if ok {
            for g in globs {
                builder.add(g);
            }
        } else {
            invalid.push(raw.clone());
        }
    }
    match builder.build() {
        Ok(set) => (set, invalid),
        Err(_) => (GlobSet::empty(), invalid),
    }
}
