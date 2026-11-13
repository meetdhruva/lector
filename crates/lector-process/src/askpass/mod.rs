//! Askpass: answering git / ssh prompts from a GUI that has no terminal.
//!
//! The app starts an [`AskpassServer`] (a Unix domain socket in a private `0700` directory;
//! loopback TCP on Windows) guarded by a random per-session token, and adds
//! [`AskpassServer::env`] to the environment of git commands. git and ssh then run the
//! helper program made by [`AskpassServer::helper_program`] with the prompt as its last
//! argument; that process detects the mode with [`askpass_prompt_from_args`] first thing in
//! `main` and calls [`run_askpass_client`], which forwards the prompt to the server, prints
//! the answer on stdout and exits `0`, or exits non-zero when the user cancels.
//!
//! **Helper-mode trigger.** The environment alone never selects the helper mode (a git hook
//! that runs `lector <path>` during a sync inherits git's environment and must open a
//! window as usual). The mode is selected by an explicit marker ordinary invocations cannot
//! carry:
//! * Unix: the helper program is a tiny generated shell script `lector-askpass` (mode
//!   `0700`) in the server's private `0700` directory: `exec '<lector>'
//!   --lector-askpass-helper "$@"`. The hidden first argument [`HELPER_FLAG`] selects the
//!   mode.
//! * Windows: OpenSSH runs `SSH_ASKPASS` as an executable without extra arguments, so the
//!   helper is a hard link (a copy when linking fails) of the console binary named
//!   `lector-askpass.exe` in the server's private directory; the mode is selected when the
//!   executable's own file name is [`HELPER_NAME`] (works for git, which runs
//!   `GIT_ASKPASS` through Git for Windows' `sh`, and for OpenSSH).
//!
//! The client then still needs a well-formed endpoint and a 256-bit token from the
//! environment ([`ENV_ENDPOINT`], [`ENV_TOKEN`]); without them it exits `2`.
//! Processes that are not git network operations (terminals, language servers, agents)
//! must not inherit the askpass variables: [`SCRUB_ENV`] lists them and
//! [`crate::ProcessCommand`] removes them for those [`crate::ProcessKind`]s.
//!
//! Wire protocol, length-prefixed frames (`u32` big endian + bytes, at most [`MAX_FRAME`]):
//! client → server `"LECTOR-ASKPASS/2"`, token, operation id (ASCII digits from
//! [`ENV_OP`], or empty), prompt (UTF-8); version 1 (`"LECTOR-ASKPASS/1"`, token, prompt) is
//! still accepted. Server → client one frame: `b'A'` + answer, or `b'C'` (cancelled).
//! Connections without the right token are closed without a reply; the magic and token must
//! arrive within [`HANDSHAKE_TIMEOUT`](server::HANDSHAKE_TIMEOUT) in total, and only
//! authenticated connections count against the limit of prompts handled at once. Answers
//! are never logged and the buffers holding them are zeroed after use.
//!
//! **Prompt text is untrusted**: parts of it can come from the SSH server
//! (keyboard-interactive). [`parse_prompt`] gives a structured [`PromptKind`] only for exact
//! OpenSSH / git templates; see the `prompt` module documentation.

mod client;
mod prompt;
mod protocol;
mod server;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

pub use client::{
    run_askpass_client, run_askpass_client_for_op, run_askpass_client_with, AskpassClientError,
};
pub use prompt::{parse_prompt, sanitize_prompt_text, ParsedPrompt, MAX_PROMPT_CHARS};
pub use protocol::{Endpoint, MAX_FRAME};
pub use server::{AskpassError, AskpassServer, HANDSHAKE_TIMEOUT};

/// Hidden first argument that selects the helper mode (passed by the Unix shim script).
pub const HELPER_FLAG: &str = "--lector-askpass-helper";
/// File name (without extension) of the helper executable; an executable with this name
/// runs in helper mode (the Windows hard link).
pub const HELPER_NAME: &str = "lector-askpass";
/// The server endpoint (`unix:/path` or `tcp:127.0.0.1:port`).
pub const ENV_ENDPOINT: &str = "LECTOR_ASKPASS_ENDPOINT";
/// The per-session token.
pub const ENV_TOKEN: &str = "LECTOR_ASKPASS_TOKEN";
/// The id of the operation whose git process may prompt (set per operation with
/// [`op_env`]); the helper echoes it so the UI can tell which operation asked.
pub const ENV_OP: &str = "LECTOR_ASKPASS_OP";

/// The environment entry that tags one operation's git process with `op` (add it next to
/// [`AskpassServer::env`] when running that operation). Prompts from it arrive with
/// [`PromptRequest::op`] = `Some(op)`.
pub fn op_env(op: u64) -> (OsString, OsString) {
    (ENV_OP.into(), op.to_string().into())
}

/// Parses an operation id as the helper reads it from [`ENV_OP`]: 1 to 20 ASCII digits.
pub(crate) fn parse_op(text: &str) -> Option<u64> {
    if text.is_empty() || text.len() > 20 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Environment variables the askpass setup adds to git network commands. Children that are
/// not git network operations (integrated terminal, language servers, agents) must not
/// inherit them: an interactive `git push` in Lector's terminal should prompt on the
/// terminal, and nothing else should be able to reach the askpass server. (`LECTOR_ASKPASS`
/// is the pre-0.7.1 helper flag, scrubbed in case an older Lector launched this one.)
pub const SCRUB_ENV: &[&str] = &[
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "SSH_ASKPASS_REQUIRE",
    "GIT_TERMINAL_PROMPT",
    "LECTOR_ASKPASS",
    ENV_ENDPOINT,
    ENV_TOKEN,
    ENV_OP,
];

/// Whether `token` looks like a server token (64 lowercase hex digits).
pub fn is_valid_token(token: &str) -> bool {
    token.len() == 64
        && token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// What a prompt asks for, classified from its text by exact templates ([`parse_prompt`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PromptKind {
    /// `Username for 'https://host': ` (git).
    Username,
    /// `Password for 'https://user@host': ` (git), `user@host's password: ` (ssh).
    Password,
    /// `Enter passphrase for key '/home/u/.ssh/id_ed25519': ` (ssh).
    Passphrase,
    /// `Are you sure you want to continue connecting (yes/no/[fingerprint])?` (ssh). The
    /// answer must be the literal text `yes` (or `no`, or the fingerprint).
    HostKeyConfirm,
    /// Anything else (OTP codes, PINs, keyboard-interactive prompts of the server, text that
    /// only resembles a template, ...); show the sanitized text as something the server asks
    /// and take free, masked input.
    #[default]
    Other,
}

impl PromptKind {
    /// Whether the input should be masked.
    pub fn is_secret(self) -> bool {
        matches!(
            self,
            PromptKind::Password | PromptKind::Passphrase | PromptKind::Other
        )
    }
}

/// Classifies a prompt by exact OpenSSH / git templates (`parse_prompt(text).kind`); text
/// that merely contains the right words is [`PromptKind::Other`].
pub fn classify_prompt(text: &str) -> PromptKind {
    parse_prompt(text).kind
}

/// A prompt delivered to the [`AskpassServer`] handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptRequest {
    /// The prompt text exactly as git / ssh passed it (may span several lines).
    pub text: String,
    /// Its classification.
    pub kind: PromptKind,
    /// The operation id the helper found in [`ENV_OP`] (`None`: not set, or an older helper).
    pub op: Option<u64>,
}

/// A string that is zeroed when dropped and never shown by `Debug`.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(zeroize::Zeroizing<String>);

impl Secret {
    /// Wraps a value.
    pub fn new(value: impl Into<String>) -> Self {
        Secret(zeroize::Zeroizing::new(value.into()))
    }

    /// The value (do not log it).
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// The handler's reply to a [`PromptRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptReply {
    /// The user's answer (printed to git / ssh).
    Answer(Secret),
    /// The user cancelled: the helper exits non-zero, git reports an authentication failure.
    Cancel,
}

/// If this process was started as an askpass helper, returns the prompt. `args` includes
/// the program name. Helper mode means: the first argument is [`HELPER_FLAG`] (Unix shim;
/// the prompt follows), or the executable's own file name is [`HELPER_NAME`] (Windows hard
/// link; the prompt is the only argument). The environment alone never selects it. Call it
/// first thing in `main`, before any other argument parsing.
pub fn askpass_prompt_from_args(args: &[OsString]) -> Option<String> {
    let exe = std::env::current_exe().ok();
    prompt_for_invocation(exe.as_deref(), args)
}

fn is_helper_name(path: &Path) -> bool {
    path.file_stem()
        .is_some_and(|s| s.eq_ignore_ascii_case(OsStr::new(HELPER_NAME)))
}

fn prompt_for_invocation(exe: Option<&Path>, args: &[OsString]) -> Option<String> {
    let lossy = |s: &OsString| s.to_string_lossy().into_owned();
    if let [_, flag, rest @ ..] = args {
        if flag == HELPER_FLAG {
            // ssh and git pass exactly one prompt; anything else is still a helper call.
            return Some(rest.first().map(lossy).unwrap_or_default());
        }
    }
    let named = exe.is_some_and(is_helper_name)
        || args.first().is_some_and(|a| is_helper_name(Path::new(a)));
    if !named {
        return None;
    }
    match args {
        [] | [_] => Some(String::new()),
        [_, prompt] => Some(lossy(prompt)),
        _ => None,
    }
}

/// The executable the helper program runs for the running executable `exe`
/// (`std::env::current_exe()`). On Windows, the GUI `lector.exe` has no console; when the
/// console build `lector-cli.exe` sits next to it, that one is used (its stdout is a normal
/// console-subsystem handle). Elsewhere, `exe` itself. [`AskpassServer::helper_program`]
/// wraps it in the shim / hard link that selects the helper mode.
pub fn askpass_helper_path(exe: &Path) -> PathBuf {
    if cfg!(windows) {
        let gui = exe
            .file_stem()
            .is_some_and(|s| s.eq_ignore_ascii_case(OsStr::new("lector")));
        if gui {
            if let Some(cli) = exe.parent().map(|d| d.join("lector-cli.exe")) {
                if cli.is_file() {
                    return cli;
                }
            }
        }
    }
    exe.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_git_and_ssh_prompts() {
        let cases = [
            ("Username for 'https://github.com': ", PromptKind::Username),
            ("Password for 'https://ann@github.com': ", PromptKind::Password),
            ("ann@host's password: ", PromptKind::Password),
            (
                "Enter passphrase for key '/home/ann/.ssh/id_ed25519': ",
                PromptKind::Passphrase,
            ),
            (
                "The authenticity of host 'h (1.2.3.4)' can't be established.\nED25519 key fingerprint is SHA256:abc.\nAre you sure you want to continue connecting (yes/no/[fingerprint])? ",
                PromptKind::HostKeyConfirm,
            ),
            ("Verification code: ", PromptKind::Other),
            (
                "(git@evil) Enter passphrase for key '/home/ann/.ssh/id_ed25519': ",
                PromptKind::Other,
            ),
            ("Type yes (yes/no) to go on: ", PromptKind::Other),
        ];
        for (text, kind) in cases {
            assert_eq!(classify_prompt(text), kind, "{text}");
        }
        assert!(PromptKind::Password.is_secret());
        assert!(!PromptKind::Username.is_secret());
    }

    #[test]
    fn helper_mode_needs_the_flag_or_the_dedicated_name_never_env_alone() {
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        let gui = Some(Path::new("/opt/lector/lector"));
        // An ordinary invocation (a hook running `lector <path>`) is never the helper, even
        // with the askpass environment inherited from git.
        assert_eq!(
            prompt_for_invocation(gui, &args(&["lector", "src/main.rs"])),
            None
        );
        assert_eq!(prompt_for_invocation(gui, &args(&["lector"])), None);
        assert_eq!(
            prompt_for_invocation(gui, &args(&["lector", "Password: "])),
            None
        );
        // The shim's hidden flag.
        assert_eq!(
            prompt_for_invocation(gui, &args(&["lector", HELPER_FLAG, "Password: "])).as_deref(),
            Some("Password: ")
        );
        assert_eq!(
            prompt_for_invocation(gui, &args(&["lector", HELPER_FLAG])).as_deref(),
            Some("")
        );
        // The flag is only recognized as the first argument.
        assert_eq!(
            prompt_for_invocation(gui, &args(&["lector", "x", HELPER_FLAG])),
            None
        );
        // The dedicated executable name (Windows hard link), by exe path or argv[0].
        let link = Some(Path::new("/tmp/d/lector-askpass.exe"));
        assert_eq!(
            prompt_for_invocation(link, &args(&["x", "Username: "])).as_deref(),
            Some("Username: ")
        );
        assert_eq!(
            prompt_for_invocation(None, &args(&["C:/d/LECTOR-ASKPASS.EXE", "Username: "]))
                .as_deref(),
            Some("Username: ")
        );
        assert_eq!(prompt_for_invocation(link, &args(&["x", "a", "b"])), None);
    }

    #[test]
    fn operation_ids_are_digits_only() {
        assert_eq!(parse_op("42"), Some(42));
        assert_eq!(parse_op(""), None);
        assert_eq!(parse_op("4 2"), None);
        assert_eq!(parse_op("-1"), None);
        assert_eq!(parse_op(&"9".repeat(21)), None);
        let (k, v) = op_env(7);
        assert_eq!((k.to_str(), v.to_str()), (Some(ENV_OP), Some("7")));
    }

    #[test]
    fn tokens_are_validated() {
        assert!(is_valid_token(&"ab".repeat(32)));
        assert!(!is_valid_token("abc"));
        assert!(!is_valid_token(&"AB".repeat(32)));
        assert!(!is_valid_token(&"zz".repeat(32)));
    }

    #[test]
    fn the_scrub_list_names_every_askpass_variable() {
        let server = AskpassServer::start_tcp(|_| PromptReply::Cancel).expect("server");
        for (name, _) in server.env(Path::new("/x/lector-askpass")) {
            let name = name.to_string_lossy().into_owned();
            assert!(SCRUB_ENV.contains(&name.as_str()), "{name} not scrubbed");
        }
    }

    #[test]
    fn secrets_do_not_leak_through_debug() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s:?}"), "Secret(***)");
        assert_eq!(s.expose(), "hunter2");
        let r = PromptReply::Answer(s);
        assert!(!format!("{r:?}").contains("hunter2"));
    }

    #[test]
    fn helper_path_is_the_executable_off_windows() {
        let exe = Path::new("/opt/lector/lector");
        if !cfg!(windows) {
            assert_eq!(askpass_helper_path(exe), exe);
        }
    }
}
