//! Decisions with security weight, kept pure and tested: what a link click does, and
//! whether a paste must be confirmed first.

use std::path::{Path, PathBuf};

/// What clicking a link does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkAction {
    /// Open in the browser / mail client (http, https, mailto only).
    OpenUrl(String),
    /// Open the file in Lector at a 0-based line/column.
    OpenFile {
        /// Absolute, existing path.
        path: PathBuf,
        /// 0-based line.
        line: Option<u32>,
        /// 0-based column.
        col: Option<u32>,
        /// Outside the project: opened read-only.
        read_only: bool,
    },
    /// Nothing (with the reason shown to the user).
    Ignore(&'static str),
}

/// URL schemes opened without asking. Everything else (`file:`, `javascript:`, custom
/// app schemes...) is ignored: a program's output must not launch arbitrary handlers.
pub const OPENABLE_SCHEMES: &[&str] = &["http", "https", "mailto"];

/// Routes a URL link.
pub fn route_url(url: &str) -> LinkAction {
    let url = url.trim();
    let Some((scheme, rest)) = url.split_once(':') else {
        return LinkAction::Ignore("Not a link Lector opens");
    };
    let scheme = scheme.to_ascii_lowercase();
    let well_formed = !rest.is_empty()
        && !url.chars().any(|c| c.is_control() || c.is_whitespace())
        && (scheme == "mailto" || rest.starts_with("//"));
    if OPENABLE_SCHEMES.contains(&scheme.as_str()) && well_formed {
        LinkAction::OpenUrl(url.to_string())
    } else {
        LinkAction::Ignore("Lector only opens http, https and mailto links")
    }
}

/// Routes a file link that the core resolved (absolute, existing; `None` = not found).
pub fn route_file(
    resolved: Option<(PathBuf, Option<u32>, Option<u32>)>,
    project_root: Option<&Path>,
) -> LinkAction {
    let Some((path, line, col)) = resolved else {
        return LinkAction::Ignore("File not found");
    };
    if path.is_dir() {
        return LinkAction::Ignore("That is a folder");
    }
    let inside = project_root.is_some_and(|root| path.starts_with(root));
    LinkAction::OpenFile {
        path,
        line,
        col,
        read_only: !inside,
    }
}

/// Whether pasting `text` needs a confirmation first: the core says so (no bracketed paste and
/// a line break or a control character in the text), the setting is on, and the user did not
/// turn the question off for this run.
pub fn paste_needs_confirmation(
    text: &str,
    bracketed_paste: bool,
    setting: bool,
    suppressed: bool,
) -> bool {
    setting && !suppressed && lector_terminal::paste_needs_confirmation(text, bracketed_paste)
}

/// Title and detail of the paste confirmation for `text`.
pub fn paste_prompt(text: &str) -> (String, &'static str) {
    if text.contains(['\n', '\r']) {
        (
            format!(
                "Paste {} lines into the terminal?",
                text.lines().count().max(2)
            ),
            "The program did not enable bracketed paste, so the shell may run each line as \
             a command. (Turn this question off with terminal.confirm_multiline_paste.)",
        )
    } else {
        (
            "Paste text with control characters into the terminal?".to_owned(),
            "The clipboard holds control characters. Lector removes them before pasting, but \
             check where this text came from. (Turn this question off with \
             terminal.confirm_multiline_paste.)",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_click_routing() {
        assert_eq!(
            route_url("https://example.com/a?b=c"),
            LinkAction::OpenUrl("https://example.com/a?b=c".into())
        );
        assert_eq!(
            route_url("HTTP://example.com"),
            LinkAction::OpenUrl("HTTP://example.com".into())
        );
        assert_eq!(
            route_url("mailto:me@example.com"),
            LinkAction::OpenUrl("mailto:me@example.com".into())
        );
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "vscode://x/y",
            "ftp://example.com",
            "ssh://host",
            "https:",
            "https://exa mple.com",
            "https://a\u{1b}b",
            "no-scheme",
        ] {
            assert!(
                matches!(route_url(bad), LinkAction::Ignore(_)),
                "{bad} must not open"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(
            route_file(Some((file.clone(), Some(4), Some(2))), Some(dir.path())),
            LinkAction::OpenFile {
                path: file.clone(),
                line: Some(4),
                col: Some(2),
                read_only: false
            }
        );
        let other = tempfile::tempdir().unwrap();
        assert!(matches!(
            route_file(Some((file.clone(), None, None)), Some(other.path())),
            LinkAction::OpenFile {
                read_only: true,
                ..
            }
        ));
        assert!(matches!(
            route_file(Some((file, None, None)), None),
            LinkAction::OpenFile {
                read_only: true,
                ..
            }
        ));
        assert!(matches!(route_file(None, None), LinkAction::Ignore(_)));
        assert!(matches!(
            route_file(Some((dir.path().to_path_buf(), None, None)), None),
            LinkAction::Ignore(_)
        ));
    }

    #[test]
    fn multiline_paste_confirmation() {
        assert!(paste_needs_confirmation(
            "ls\nrm -rf x\n",
            false,
            true,
            false
        ));
        assert!(paste_needs_confirmation("echo hi\r", false, true, false));
        assert!(!paste_needs_confirmation("echo hi", false, true, false));
        assert!(
            !paste_needs_confirmation("a\nb", true, true, false),
            "bracketed"
        );
        assert!(
            !paste_needs_confirmation("a\nb", false, false, false),
            "setting off"
        );
        assert!(
            !paste_needs_confirmation("a\nb", false, true, true),
            "suppressed"
        );
        // Control characters ask even without a line break; tab alone does not.
        assert!(paste_needs_confirmation("ls\u{3}", false, true, false));
        assert!(paste_needs_confirmation("a\u{1b}[31mb", false, true, false));
        assert!(!paste_needs_confirmation("a\tb", false, true, false));
        assert!(!paste_needs_confirmation("a\u{3}b", true, true, false));
        assert!(paste_prompt("a\nb").0.contains("lines"));
        assert!(paste_prompt("a\u{3}").0.contains("control"));
    }
}
