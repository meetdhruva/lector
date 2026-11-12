//! Link detection: URLs and file locations in terminal output.
//!
//! Detection is purely textual and cheap (it runs on one logical line at a
//! time). Whether a detected file path exists is checked only on demand, for
//! the candidate the user hovers or clicks, by [`resolve_link`].

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use regex::Regex;

use crate::geometry::GridPoint;

/// Where a link points. Targets are untrusted data: the UI asks before opening.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LinkTarget {
    /// A URL (`http`, `https`, `ftp`, `file`, `mailto` or an OSC 8 URI).
    Url(String),
    /// A file location. Before [`resolve_link`] the path is exactly as printed;
    /// after, it is absolute and known to exist.
    File {
        /// Path of the file.
        path: PathBuf,
        /// 0-based line, if the reference carried one.
        line: Option<u32>,
        /// 0-based column, if the reference carried one.
        col: Option<u32>,
    },
}

/// How a link was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkSource {
    /// OSC 8 hyperlink set by the program.
    Hyperlink,
    /// Detected in the text.
    Detected,
}

/// A link candidate over a range of cells.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LinkCandidate {
    /// First cell.
    pub start: GridPoint,
    /// Last cell (inclusive); may be on a later line when the link wraps.
    pub end: GridPoint,
    /// Text covered by the link.
    pub text: String,
    /// Target (file targets are unresolved; see [`resolve_link`]).
    pub target: LinkTarget,
    /// How it was found.
    pub source: LinkSource,
}

impl LinkCandidate {
    /// Whether `point` lies inside the candidate.
    pub fn contains(&self, point: GridPoint) -> bool {
        (self.start.line, self.start.col) <= (point.line, point.col)
            && (point.line, point.col) <= (self.end.line, self.end.col)
    }
}

/// Resolves a candidate's target for opening.
///
/// URLs are returned unchanged. File targets are expanded (`~/`), made absolute
/// against `cwd` and returned only when the path exists; a git-diff style
/// `a/` or `b/` prefix is dropped when the path does not exist with it. Returns
/// `None` when `cancel` is set (checked before every filesystem access).
pub fn resolve_link(target: &LinkTarget, cwd: &Path, cancel: &AtomicBool) -> Option<LinkTarget> {
    let LinkTarget::File { path, line, col } = target else {
        return Some(target.clone());
    };
    let mut candidates = vec![path.clone()];
    if let Ok(rest) = path.strip_prefix("a").or_else(|_| path.strip_prefix("b")) {
        if path.to_string_lossy().starts_with("a/") || path.to_string_lossy().starts_with("b/") {
            candidates.push(rest.to_path_buf());
        }
    }
    for candidate in candidates {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let expanded = expand_home(&candidate);
        let full = if expanded.is_absolute() {
            expanded
        } else {
            cwd.join(expanded)
        };
        if std::fs::metadata(&full).is_ok() {
            return Some(LinkTarget::File {
                path: full,
                line: *line,
                col: *col,
            });
        }
    }
    None
}

fn expand_home(path: &Path) -> PathBuf {
    if let Ok(rest) = path.strip_prefix("~") {
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
        if let Some(home) = home {
            return PathBuf::from(home).join(rest);
        }
    }
    path.to_path_buf()
}

struct Patterns {
    url: Regex,
    python: Regex,
    msvc: Regex,
    path: Regex,
}

fn patterns() -> Option<&'static Patterns> {
    static PATTERNS: OnceLock<Option<Patterns>> = OnceLock::new();
    PATTERNS
        .get_or_init(|| {
            Some(Patterns {
                url: Regex::new(r#"(?:https?|ftp|file)://[^\s<>"'`{}|\\^]+|mailto:[^\s<>"'`]+"#).ok()?,
                python: Regex::new(r#"File "([^"]+)", line (\d+)"#).ok()?,
                msvc: Regex::new(r"((?:[A-Za-z]:)?[^\s()<>:]+)\((\d+)(?:,\s*(\d+))?\)").ok()?,
                path: Regex::new(
                    r"(?:[A-Za-z]:[\\/]|~/|\.{1,2}/|/)?[\w.@+~-]+(?:[\\/][\w.@+~-]+)*(?::(\d+)(?::(\d+))?)?",
                )
                .ok()?,
            })
        })
        .as_ref()
}

/// A link found in a string: byte range and target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextLink {
    pub range: Range<usize>,
    pub target: LinkTarget,
}

fn one_based(s: Option<regex::Match<'_>>) -> Option<u32> {
    s.and_then(|m| m.as_str().parse::<u32>().ok())
        .map(|n| n.saturating_sub(1))
}

/// Trims trailing punctuation from a URL, keeping balanced closing brackets.
fn trim_url(s: &str) -> &str {
    let mut end = s.len();
    loop {
        let t = &s[..end];
        let Some(last) = t.chars().last() else { break };
        let strip = match last {
            '.' | ',' | ':' | ';' | '!' | '?' | '\'' | '"' => true,
            ')' => t.matches('(').count() < t.matches(')').count(),
            ']' => t.matches('[').count() < t.matches(']').count(),
            _ => false,
        };
        if !strip {
            break;
        }
        end -= last.len_utf8();
    }
    &s[..end]
}

fn looks_like_path(p: &str, has_line: bool) -> bool {
    if p.chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
    {
        return false;
    }
    if p.contains('/') || p.contains('\\') {
        // Reject things like `a/b` that are only separators or `//`.
        return p.chars().any(|c| c.is_alphanumeric());
    }
    // Bare names need an extension (`main.rs`) or a line number (`Makefile:3`).
    let ext = p.rsplit_once('.').map(|(stem, ext)| {
        !stem.is_empty()
            && !ext.is_empty()
            && ext.len() <= 12
            && ext.chars().any(|c| c.is_ascii_alphabetic())
    });
    ext == Some(true) || (has_line && p.chars().any(|c| c.is_ascii_alphabetic()))
}

fn overlaps(taken: &[Range<usize>], r: &Range<usize>) -> bool {
    taken.iter().any(|t| t.start < r.end && r.start < t.end)
}

/// Detects URLs and file references in `text`.
pub(crate) fn detect_links(text: &str) -> Vec<TextLink> {
    let Some(p) = patterns() else {
        return Vec::new();
    };
    let mut out: Vec<TextLink> = Vec::new();
    let mut taken: Vec<Range<usize>> = Vec::new();
    let push =
        |out: &mut Vec<TextLink>, taken: &mut Vec<Range<usize>>, range: Range<usize>, target| {
            if range.is_empty() || overlaps(taken, &range) {
                return;
            }
            taken.push(range.clone());
            out.push(TextLink { range, target });
        };
    for m in p.url.find_iter(text) {
        let s = trim_url(m.as_str());
        push(
            &mut out,
            &mut taken,
            m.start()..m.start() + s.len(),
            LinkTarget::Url(s.to_owned()),
        );
    }
    for c in p.python.captures_iter(text) {
        let (Some(all), Some(path)) = (c.get(0), c.get(1)) else {
            continue;
        };
        let target = LinkTarget::File {
            path: PathBuf::from(path.as_str()),
            line: one_based(c.get(2)),
            col: None,
        };
        push(&mut out, &mut taken, all.range(), target);
    }
    for c in p.msvc.captures_iter(text) {
        let (Some(all), Some(path)) = (c.get(0), c.get(1)) else {
            continue;
        };
        if !looks_like_path(path.as_str(), true) {
            continue;
        }
        let target = LinkTarget::File {
            path: PathBuf::from(path.as_str()),
            line: one_based(c.get(2)),
            col: one_based(c.get(3)),
        };
        push(&mut out, &mut taken, all.range(), target);
    }
    for c in p.path.captures_iter(text) {
        let Some(all) = c.get(0) else { continue };
        let line = c.get(1);
        let path_end = line.map_or(all.end(), |l| l.start() - 1);
        let mut path = &text[all.start()..path_end];
        // A trailing dot is sentence punctuation, not part of the name.
        let mut range = all.range();
        if line.is_none() {
            let trimmed = path.trim_end_matches(['.', ',']);
            range.end -= path.len() - trimmed.len();
            path = trimmed;
        }
        // Must not be glued to a preceding word character (e.g. inside `foo:bar.rs`).
        if !looks_like_path(path, line.is_some()) {
            continue;
        }
        let target = LinkTarget::File {
            path: PathBuf::from(path),
            line: one_based(line),
            col: one_based(c.get(2)),
        };
        push(&mut out, &mut taken, range, target);
    }
    out.sort_by_key(|l| l.range.start);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, line: Option<u32>, col: Option<u32>) -> LinkTarget {
        LinkTarget::File {
            path: PathBuf::from(path),
            line,
            col,
        }
    }

    fn find(text: &str) -> Vec<(&str, LinkTarget)> {
        detect_links(text)
            .into_iter()
            .map(|l| (&text[l.range.clone()], l.target))
            .collect()
    }

    #[test]
    fn table() {
        let cases: Vec<(&str, Vec<(&str, LinkTarget)>)> = vec![
            (
                "see https://example.com/a_(b).",
                vec![(
                    "https://example.com/a_(b)",
                    LinkTarget::Url("https://example.com/a_(b)".into()),
                )],
            ),
            (
                "(https://x.org/y)",
                vec![("https://x.org/y", LinkTarget::Url("https://x.org/y".into()))],
            ),
            (
                "  --> src/main.rs:10:5",
                vec![("src/main.rs:10:5", file("src/main.rs", Some(9), Some(4)))],
            ),
            (
                "error at lib.rs:3",
                vec![("lib.rs:3", file("lib.rs", Some(2), None))],
            ),
            (
                r#"  File "/tmp/x y.py", line 3, in <module>"#,
                vec![(
                    r#"File "/tmp/x y.py", line 3"#,
                    file("/tmp/x y.py", Some(2), None),
                )],
            ),
            (
                "    at foo (/app/index.js:12:7)",
                vec![(
                    "/app/index.js:12:7",
                    file("/app/index.js", Some(11), Some(6)),
                )],
            ),
            (
                r"C:\src\a.cs(10,4): error",
                vec![(r"C:\src\a.cs(10,4)", file(r"C:\src\a.cs", Some(9), Some(3)))],
            ),
            (
                "Main.java(42)",
                vec![("Main.java(42)", file("Main.java", Some(41), None))],
            ),
            (
                "open ./build/out.txt.",
                vec![("./build/out.txt", file("./build/out.txt", None, None))],
            ),
            (
                "~/notes/todo.md",
                vec![("~/notes/todo.md", file("~/notes/todo.md", None, None))],
            ),
            (
                "Makefile:12: recipe",
                vec![("Makefile:12", file("Makefile", Some(11), None))],
            ),
            ("version 1.2.3 and 10:30", vec![]),
            ("hello world", vec![]),
        ];
        for (text, want) in cases {
            assert_eq!(find(text), want, "{text}");
        }
    }

    #[test]
    fn resolves_existing_files_only() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        std::fs::create_dir_all(dir.path().join("src")).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(dir.path().join("src/x.rs"), "").unwrap_or_else(|e| panic!("{e}"));
        let cancel = AtomicBool::new(false);
        let got = resolve_link(&file("src/x.rs", Some(1), None), dir.path(), &cancel);
        assert_eq!(
            got,
            Some(file(
                &dir.path().join("src/x.rs").to_string_lossy(),
                Some(1),
                None
            ))
        );
        let diff = resolve_link(&file("b/src/x.rs", None, None), dir.path(), &cancel);
        assert!(diff.is_some());
        assert_eq!(
            resolve_link(&file("nope.rs", None, None), dir.path(), &cancel),
            None
        );
        let url = LinkTarget::Url("https://a".into());
        assert_eq!(resolve_link(&url, dir.path(), &cancel), Some(url.clone()));
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            resolve_link(&file("src/x.rs", None, None), dir.path(), &cancel),
            None
        );
    }
}
