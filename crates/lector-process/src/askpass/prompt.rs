//! Classification of askpass prompts by exact OpenSSH / git templates.
//!
//! The prompt text reaches Lector from git and ssh, but parts of it can come from the
//! remote: an SSH server's keyboard-interactive prompts are passed to `SSH_ASKPASS` as they
//! are (OpenSSH ≥ 8.3 prefixes them with `(user@host) `). So the text alone must never earn a
//! trusted-looking dialog by containing the right words: only prompts that match one of
//! the templates below *exactly* (anchored, nothing before or after) get a structured
//! kind; everything else is [`PromptKind::Other`], shown verbatim (sanitized with
//! [`sanitize_prompt_text`]) as something "the server asks".
//!
//! Templates (OpenSSH `sshconnect.c` / `sshconnect2.c` / `ssh-add.c`, git `credential.c`):
//! * `Username for '<url>': ` / `Password for '<url>': ` (git; `<url>` has a scheme);
//! * `<user>@<host>'s password: ` (ssh password authentication) and a bare `Password: `;
//! * `Enter passphrase for key '<path>': ` and `Enter passphrase for <path>: `
//!   (optionally ` (will confirm each use)`), where `<path>` is absolute or `~/`;
//! * the unknown-host-key confirmation: `The authenticity of host '<host> (<ip>)' can't be
//!   established.` / `<TYPE> key fingerprint is <fp>.` / zero or more lines about other
//!   names, addresses or the visual host key / `Are you sure you want to continue
//!   connecting (yes/no[/[fingerprint]])? `.
//!
//! Residual risk: an old ssh (< 8.3) passes keyboard-interactive prompts without the
//! prefix, so a hostile server could still type a template verbatim. The passphrase dialog
//! additionally requires the key file to exist (checked by the UI, which has the file
//! system), and the host-key dialog only ever answers the literal `yes`.

use super::PromptKind;

/// Longest prompt text (in characters) [`sanitize_prompt_text`] keeps.
pub const MAX_PROMPT_CHARS: usize = 2000;

/// What a prompt asks for and the fields its template carries. Fields are only filled
/// for an exact template match; for [`PromptKind::Other`] they are all `None`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedPrompt {
    /// The classification.
    pub kind: PromptKind,
    /// git prompts: the URL in quotes (may still contain a user name; redact before showing).
    pub url: Option<String>,
    /// ssh password prompts: `user@host`; host-key prompts: `host (ip)`.
    pub host: Option<String>,
    /// Passphrase prompts: the key file as ssh printed it.
    pub key_path: Option<String>,
    /// Host-key prompts: the key type (`ED25519`).
    pub key_type: Option<String>,
    /// Host-key prompts: the fingerprint (`SHA256:...`).
    pub fingerprint: Option<String>,
}

/// Characters no template contains: C0/C1 controls (newlines are handled by the
/// multi-line template) and bidirectional overrides/isolates/marks.
fn is_forbidden(c: char) -> bool {
    (c.is_control() && c != '\n') || is_bidi_control(c)
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}' | '\u{061c}'
    )
}

/// One line without a newline and without forbidden characters.
fn clean_line(s: &str) -> bool {
    !s.contains('\n') && !s.chars().any(is_forbidden)
}

/// `prefix '<inner>': ` exactly (the trailing space is optional); `inner` has no quote.
fn quoted_template<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let inner = text
        .strip_prefix(prefix)?
        .strip_prefix('\'')?
        .trim_end_matches(' ')
        .strip_suffix("':")?;
    (!inner.is_empty() && !inner.contains('\'')).then_some(inner)
}

fn looks_like_url(s: &str) -> bool {
    match s.split_once("://") {
        Some((scheme, rest)) => {
            !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
                && !rest.is_empty()
                && !s.contains(char::is_whitespace)
        }
        None => false,
    }
}

fn looks_like_key_path(s: &str) -> bool {
    let b = s.as_bytes();
    let absolute = s.starts_with('/')
        || s.starts_with("~/")
        || (b.len() > 2
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && matches!(b[2], b'/' | b'\\'));
    absolute && !s.contains('\'')
}

/// Classifies `text` by the exact templates of the module documentation and extracts their
/// fields. Pure (no file system); the UI additionally checks that a passphrase prompt's key
/// file exists before showing the passphrase dialog.
pub fn parse_prompt(text: &str) -> ParsedPrompt {
    parse_single_line(text)
        .or_else(|| parse_host_key(text))
        .unwrap_or_default()
}

fn parse_single_line(text: &str) -> Option<ParsedPrompt> {
    if !clean_line(text) {
        return None;
    }
    if let Some(url) = quoted_template(text, "Username for ") {
        return looks_like_url(url).then(|| ParsedPrompt {
            kind: PromptKind::Username,
            url: Some(url.to_owned()),
            ..ParsedPrompt::default()
        });
    }
    if let Some(url) = quoted_template(text, "Password for ") {
        return looks_like_url(url).then(|| ParsedPrompt {
            kind: PromptKind::Password,
            url: Some(url.to_owned()),
            ..ParsedPrompt::default()
        });
    }
    if let Some(path) = quoted_template(text, "Enter passphrase for key ") {
        return looks_like_key_path(path).then(|| passphrase(path));
    }
    let body = text.trim_end_matches(' ');
    if let Some(rest) = body.strip_prefix("Enter passphrase for ") {
        let path = rest.strip_suffix(':')?;
        let path = path
            .strip_suffix(" (will confirm each use)")
            .unwrap_or(path);
        return looks_like_key_path(path).then(|| passphrase(path));
    }
    if body == "Password:" {
        return Some(ParsedPrompt {
            kind: PromptKind::Password,
            ..ParsedPrompt::default()
        });
    }
    if let Some(user_host) = body.strip_suffix("'s password:") {
        let (user, host) = user_host.split_once('@')?;
        let valid = |s: &str| {
            !s.is_empty()
                && !s
                    .chars()
                    .any(|c| c.is_whitespace() || matches!(c, '\'' | '(' | ')' | '@'))
        };
        return (valid(user) && valid(host)).then(|| ParsedPrompt {
            kind: PromptKind::Password,
            host: Some(user_host.to_owned()),
            ..ParsedPrompt::default()
        });
    }
    None
}

fn passphrase(path: &str) -> ParsedPrompt {
    ParsedPrompt {
        kind: PromptKind::Passphrase,
        key_path: Some(path.to_owned()),
        ..ParsedPrompt::default()
    }
}

fn parse_host_key(text: &str) -> Option<ParsedPrompt> {
    if text.chars().any(is_forbidden) {
        return None;
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let [first, second, middle @ .., last] = lines.as_slice() else {
        return None;
    };
    let host = first
        .strip_prefix("The authenticity of host '")?
        .strip_suffix("' can't be established.")?;
    if host.is_empty() || host.contains('\'') {
        return None;
    }
    let (key_type, fingerprint) = second.split_once(" key fingerprint is ")?;
    let fingerprint = fingerprint.strip_suffix('.')?;
    let type_ok = !key_type.is_empty()
        && key_type
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-');
    let fp_ok = !fingerprint.is_empty() && !fingerprint.contains(char::is_whitespace);
    if !type_ok || !fp_ok {
        return None;
    }
    let question = last.trim_end_matches(' ');
    if question != "Are you sure you want to continue connecting (yes/no/[fingerprint])?"
        && question != "Are you sure you want to continue connecting (yes/no)?"
    {
        return None;
    }
    // Lines ssh prints between the fingerprint and the question: other known names or
    // addresses of the key, or the visual host key art (VisualHostKey). Bounded.
    if middle.len() > 24 || middle.iter().any(|l| l.chars().count() > 200) {
        return None;
    }
    Some(ParsedPrompt {
        kind: PromptKind::HostKeyConfirm,
        host: Some(host.to_owned()),
        key_type: Some(key_type.to_owned()),
        fingerprint: Some(fingerprint.to_owned()),
        ..ParsedPrompt::default()
    })
}

/// The prompt text as a dialog may show it: control characters other than newline and tab
/// become U+FFFD, bidirectional controls are removed, at most [`MAX_PROMPT_CHARS`]
/// characters are kept (an ellipsis marks a cut) and trailing whitespace is trimmed.
pub fn sanitize_prompt_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_PROMPT_CHARS * 4));
    for (n, c) in text.chars().enumerate() {
        if n >= MAX_PROMPT_CHARS {
            out.push('\u{2026}');
            break;
        }
        if is_bidi_control(c) {
            continue;
        }
        if c.is_control() && c != '\n' && c != '\t' {
            out.push('\u{fffd}');
        } else {
            out.push(c);
        }
    }
    out.truncate(out.trim_end().len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_KEY: &str = "The authenticity of host 'github.com (140.82.121.4)' can't be established.\nED25519 key fingerprint is SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU.\nThis key is not known by any other names.\nAre you sure you want to continue connecting (yes/no/[fingerprint])? ";

    #[test]
    fn templates_are_classified_with_their_fields() {
        let p = parse_prompt("Username for 'https://github.com': ");
        assert_eq!(p.kind, PromptKind::Username);
        assert_eq!(p.url.as_deref(), Some("https://github.com"));

        let p = parse_prompt("Password for 'https://ann@github.com': ");
        assert_eq!(p.kind, PromptKind::Password);
        assert_eq!(p.url.as_deref(), Some("https://ann@github.com"));

        let p = parse_prompt("ann@build.example's password: ");
        assert_eq!(p.kind, PromptKind::Password);
        assert_eq!(p.host.as_deref(), Some("ann@build.example"));

        assert_eq!(parse_prompt("Password: ").kind, PromptKind::Password);

        let p = parse_prompt("Enter passphrase for key '/home/ann/.ssh/id_ed25519': ");
        assert_eq!(p.kind, PromptKind::Passphrase);
        assert_eq!(p.key_path.as_deref(), Some("/home/ann/.ssh/id_ed25519"));
        let p = parse_prompt("Enter passphrase for /home/ann/.ssh/id_rsa: ");
        assert_eq!(p.key_path.as_deref(), Some("/home/ann/.ssh/id_rsa"));
        let p = parse_prompt("Enter passphrase for ~/.ssh/k (will confirm each use): ");
        assert_eq!(p.key_path.as_deref(), Some("~/.ssh/k"));
        let p = parse_prompt("Enter passphrase for key 'C:\\Users\\ann\\.ssh\\id_ed25519': ");
        assert_eq!(p.kind, PromptKind::Passphrase);

        let p = parse_prompt(HOST_KEY);
        assert_eq!(p.kind, PromptKind::HostKeyConfirm);
        assert_eq!(p.host.as_deref(), Some("github.com (140.82.121.4)"));
        assert_eq!(p.key_type.as_deref(), Some("ED25519"));
        assert_eq!(
            p.fingerprint.as_deref(),
            Some("SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU")
        );
        let old = "The authenticity of host 'h (1.2.3.4)' can't be established.\nECDSA key fingerprint is SHA256:abc.\nAre you sure you want to continue connecting (yes/no)? ";
        assert_eq!(parse_prompt(old).kind, PromptKind::HostKeyConfirm);
    }

    #[test]
    fn spoofed_and_unknown_prompts_are_other_without_fields() {
        let cases = [
            // Keyboard-interactive prompts carry OpenSSH's `(user@host) ` prefix.
            "(git@evil.example) Enter passphrase for key '/home/u/.ssh/id_ed25519': ",
            "(git@evil.example) Password: ",
            "(u@h) The authenticity of host 'github.com (1.2.3.4)' can't be established.\nED25519 key fingerprint is SHA256:x.\nAre you sure you want to continue connecting (yes/no/[fingerprint])? ",
            // The right words in the wrong place.
            "Please type yes (yes/no) to continue: ",
            "Are you sure you want to continue connecting (yes/no)? ",
            "Your passphrase please: ",
            "Enter passphrase for key 'id_ed25519': ",
            "Enter passphrase for key '/k': trailing",
            "xEnter passphrase for key '/k': ",
            "Username for 'not a url': ",
            "Password for github: ",
            "Username: ",
            "Verification code: ",
            "PIN for ann@host: ",
            "ann@h o s t's password: ",
            // Control characters and bidi overrides.
            "Password for 'https://github.com\u{1b}[2K': ",
            "Enter passphrase for key '/home/u/\u{202e}k': ",
            "Password:\u{7}",
            "Username for 'https://a\nb': ",
            "",
        ];
        for text in cases {
            let p = parse_prompt(text);
            assert_eq!(p, ParsedPrompt::default(), "{text:?}");
            assert_eq!(super::super::classify_prompt(text), PromptKind::Other);
        }
        // A host-key prompt with an attacker-length tail is not the template either.
        let long = HOST_KEY.replace(
            "This key is not known by any other names.\n",
            &"x\n".repeat(30),
        );
        assert_eq!(parse_prompt(&long).kind, PromptKind::Other);
    }

    #[test]
    fn prompt_text_is_neutralized_for_display() {
        assert_eq!(
            sanitize_prompt_text("a\u{1b}[31mb\u{202e}c\td\ne\u{9b}  \n"),
            "a\u{fffd}[31mbc\td\ne\u{fffd}"
        );
        let long = "x".repeat(MAX_PROMPT_CHARS + 50);
        let out = sanitize_prompt_text(&long);
        assert_eq!(out.chars().count(), MAX_PROMPT_CHARS + 1);
        assert!(out.ends_with('\u{2026}'));
    }
}
