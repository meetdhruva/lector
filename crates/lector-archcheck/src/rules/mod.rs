//! The rules. Each module exposes `check(...) -> RuleOutput` plus a pure per-file core that the
//! unit tests drive with in-memory sources.

pub mod background;
pub mod deps;
pub mod dupes;
pub mod graph;
pub mod hygiene;
pub mod purity;
pub mod size;
pub mod state;
pub mod threads;
pub mod writes;

use crate::source::SourceFile;

/// `crates/lector-ui*/src/**`: the UI crates' own sources.
pub fn is_ui_src(path: &str) -> bool {
    let mut parts = path.split('/');
    parts.next() == Some("crates")
        && parts.next().is_some_and(|c| c.starts_with("lector-ui"))
        && parts.next() == Some("src")
}

/// If a thread-spawning construct starts at token `i`, say which: `thread::spawn`,
/// `thread::Builder` or `.spawn_scoped(`.
pub fn thread_spawn_at(f: &SourceFile, i: usize) -> Option<&'static str> {
    if f.seq(i, &["thread", "::", "spawn"]) {
        Some("thread::spawn")
    } else if f.seq(i, &["thread", "::", "Builder"]) {
        Some("thread::Builder")
    } else if f.seq(i, &[".", "spawn_scoped", "("]) {
        Some(".spawn_scoped(")
    } else {
        None
    }
}

/// Text of a string token without its quotes and prefix (escapes are left as written).
pub fn str_value(text: &str) -> &str {
    let t = text
        .trim_start_matches(['b', 'c', 'r'])
        .trim_start_matches('#');
    let t = t.trim_end_matches('#');
    t.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(t)
}

#[cfg(test)]
pub(crate) fn file(path: &str, src: &str) -> SourceFile {
    SourceFile::new(path, src)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_src_paths() {
        assert!(is_ui_src("crates/lector-ui/src/a.rs"));
        assert!(is_ui_src("crates/lector-ui-git/src/x/y.rs"));
        assert!(!is_ui_src("crates/lector-ui-git/tests/y.rs"));
        assert!(!is_ui_src("crates/lector-app/src/y.rs"));
        assert!(!is_ui_src("crates/lector/src/main.rs"));
    }

    #[test]
    fn string_values() {
        assert_eq!(str_value("\"a.json\""), "a.json");
        assert_eq!(str_value("r#\"a.json\"#"), "a.json");
        assert_eq!(str_value("b\"x\""), "x");
    }
}
