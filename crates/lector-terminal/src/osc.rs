//! Parsing of the shell-integration OSC payloads and title sanitising.

use std::path::PathBuf;

/// Longest title (in characters) passed on to the UI.
pub const MAX_TITLE_CHARS: usize = 256;

/// A parsed OSC 133 / 633 shell-integration mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellMark {
    /// `A`: the prompt starts.
    PromptStart,
    /// `B`: the prompt ended, the user types the command.
    CommandStart,
    /// `C`: the command was submitted, output follows.
    OutputStart,
    /// `D[;exit]`: the command finished.
    CommandEnd(Option<i32>),
}

/// Parses `133;X[;...]` or `633;X[;...]` (VS Code's variant shares A-D).
pub(crate) fn parse_shell_mark(payload: &[u8]) -> Option<ShellMark> {
    let rest = payload
        .strip_prefix(b"133;")
        .or_else(|| payload.strip_prefix(b"633;"))?;
    let mut parts = rest.split(|&b| b == b';');
    let kind = parts.next()?;
    match kind {
        b"A" => Some(ShellMark::PromptStart),
        b"B" => Some(ShellMark::CommandStart),
        b"C" => Some(ShellMark::OutputStart),
        b"D" => {
            let code = parts
                .next()
                .and_then(|p| std::str::from_utf8(p).ok())
                .and_then(|s| s.trim().parse::<i32>().ok());
            Some(ShellMark::CommandEnd(code))
        }
        _ => None,
    }
}

/// Parses an OSC 7 payload (`7;file://host/path`) into a local path.
///
/// The path is percent-decoded. Reports from another host (e.g. a shell on an
/// ssh server) are ignored: their paths mean nothing locally. `local_host` is
/// this machine's host name (compared case-insensitively); empty hosts and
/// `localhost` are always accepted.
pub(crate) fn parse_osc7(payload: &[u8], local_host: Option<&str>) -> Option<PathBuf> {
    let uri = payload.strip_prefix(b"7;")?;
    let rest = uri
        .strip_prefix(b"file://")
        .or_else(|| uri.strip_prefix(b"kitty-shell-cwd://"))?;
    let slash = rest.iter().position(|&b| b == b'/')?;
    let host = std::str::from_utf8(&rest[..slash]).ok()?;
    let host_ok = host.is_empty()
        || host.eq_ignore_ascii_case("localhost")
        || local_host.is_some_and(|h| {
            h.eq_ignore_ascii_case(host)
                || h.split('.')
                    .next()
                    .is_some_and(|short| short.eq_ignore_ascii_case(host))
                || host
                    .split('.')
                    .next()
                    .is_some_and(|short| short.eq_ignore_ascii_case(h))
        });
    if !host_ok {
        return None;
    }
    let raw = &rest[slash..];
    // Drop a query or fragment, which file URIs should not carry.
    let raw = raw.split(|&b| b == b'?' || b == b'#').next().unwrap_or(raw);
    let decoded = percent_decode(raw)?;
    if decoded.contains(&0) {
        return None;
    }
    bytes_to_path(decoded)
}

fn percent_decode(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' {
            let hi = hex(*input.get(i + 1)?)?;
            let lo = hex(*input.get(i + 2)?)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    Some(out)
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(unix)]
fn bytes_to_path(bytes: Vec<u8>) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: Vec<u8>) -> Option<PathBuf> {
    let s = String::from_utf8(bytes).ok()?;
    // `file:///C:/Users/x` decodes to `/C:/Users/x`.
    let b = s.as_bytes();
    let s = if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        &s[1..]
    } else {
        &s[..]
    };
    Some(PathBuf::from(s.replace('/', "\\")))
}

/// Sanitises a title set by a program: removes control characters and
/// bidirectional overrides, trims, and caps it at [`MAX_TITLE_CHARS`].
pub fn sanitize_title(title: &str) -> String {
    title
        .chars()
        .filter(|&c| {
            !c.is_control()
                && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}')
        })
        .take(MAX_TITLE_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// This machine's host name, used to accept OSC 7 reports.
pub(crate) fn local_hostname() -> Option<String> {
    #[cfg(unix)]
    {
        let uname = rustix::system::uname();
        let name = uname.nodename().to_string_lossy().into_owned();
        if !name.is_empty() {
            return Some(name);
        }
    }
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks() {
        assert_eq!(parse_shell_mark(b"133;A"), Some(ShellMark::PromptStart));
        assert_eq!(
            parse_shell_mark(b"133;A;aid=12"),
            Some(ShellMark::PromptStart)
        );
        assert_eq!(parse_shell_mark(b"133;B"), Some(ShellMark::CommandStart));
        assert_eq!(parse_shell_mark(b"633;C"), Some(ShellMark::OutputStart));
        assert_eq!(
            parse_shell_mark(b"133;D;127"),
            Some(ShellMark::CommandEnd(Some(127)))
        );
        assert_eq!(
            parse_shell_mark(b"133;D"),
            Some(ShellMark::CommandEnd(None))
        );
        assert_eq!(
            parse_shell_mark(b"133;D;x"),
            Some(ShellMark::CommandEnd(None))
        );
        assert_eq!(parse_shell_mark(b"133;Z"), None);
        assert_eq!(parse_shell_mark(b"7;file:///"), None);
    }

    #[cfg(unix)]
    #[test]
    fn osc7_paths() {
        assert_eq!(
            parse_osc7(b"7;file:///tmp/a%20b", None),
            Some(PathBuf::from("/tmp/a b"))
        );
        assert_eq!(
            parse_osc7(b"7;file://localhost/x", None),
            Some(PathBuf::from("/x"))
        );
        assert_eq!(
            parse_osc7(b"7;file://box/x", Some("BOX.local")),
            Some(PathBuf::from("/x"))
        );
        assert_eq!(parse_osc7(b"7;file://other/x", Some("box")), None);
        assert_eq!(parse_osc7(b"7;http://h/x", None), None);
        assert_eq!(parse_osc7(b"7;file:///x%zz", None), None);
        assert_eq!(parse_osc7(b"7;file:///x%00y", None), None);
        assert_eq!(
            parse_osc7(b"7;file:///caf%C3%A9", None),
            Some(PathBuf::from("/caf\u{e9}"))
        );
    }

    #[test]
    fn titles() {
        assert_eq!(
            sanitize_title(" vim \x1b[31m\x07main.rs\u{202e} "),
            "vim [31mmain.rs"
        );
        assert_eq!(sanitize_title(&"x".repeat(1000)).len(), MAX_TITLE_CHARS);
    }
}
