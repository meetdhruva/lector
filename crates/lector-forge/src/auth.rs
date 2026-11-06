//! Finding an API token: environment → `gh` / `glab` CLI → OS keychain.
//! Tokens are never logged; [`Token`]'s `Debug` is redacted.

use std::process::{Command, Stdio};

use crate::{ForgeKind, ForgeRemote};

/// Keychain service name.
const SERVICE: &str = "lector-forge";

/// An API token. `Debug` / `Display` never show it.
#[derive(Clone, PartialEq, Eq)]
pub struct Token {
    secret: String,
    /// Where it came from.
    pub source: TokenSource,
}

/// Where a token was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// An environment variable.
    Env,
    /// `gh auth token` / `glab auth status -t`.
    Cli,
    /// The OS keychain.
    Keychain,
    /// Given directly (tests, settings dialog).
    Explicit,
}

impl Token {
    /// A token given directly.
    pub fn new(secret: impl Into<String>) -> Token {
        Token {
            secret: secret.into(),
            source: TokenSource::Explicit,
        }
    }

    /// The secret (only for the `Authorization` header).
    pub fn secret(&self) -> &str {
        &self.secret
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Token(<redacted>, {:?})", self.source)
    }
}

/// Which sources [`auth_token_with`] consults (all by default).
pub struct TokenSources<'a> {
    /// Environment lookup.
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// Ask `gh` / `glab`.
    pub cli: bool,
    /// Ask the OS keychain.
    pub keychain: bool,
}

impl Default for TokenSources<'_> {
    fn default() -> Self {
        TokenSources {
            env: &|k| std::env::var(k).ok(),
            cli: true,
            keychain: true,
        }
    }
}

/// The environment variables consulted for `remote`, in order.
pub fn env_vars(remote: &ForgeRemote) -> &'static [&'static str] {
    match (remote.kind, remote.host.as_str()) {
        (ForgeKind::Github, "github.com") => &["GITHUB_TOKEN", "GH_TOKEN"],
        (ForgeKind::Github, _) => &[
            "GH_ENTERPRISE_TOKEN",
            "GITHUB_ENTERPRISE_TOKEN",
            "GITHUB_TOKEN",
            "GH_TOKEN",
        ],
        (ForgeKind::Gitlab, _) => &["GITLAB_TOKEN", "GITLAB_PRIVATE_TOKEN"],
    }
}

/// Finds a token for `remote` (environment, CLI, keychain).
pub fn auth_token(remote: &ForgeRemote) -> Option<Token> {
    auth_token_with(remote, &TokenSources::default())
}

/// [`auth_token`] with explicit sources.
pub fn auth_token_with(remote: &ForgeRemote, sources: &TokenSources<'_>) -> Option<Token> {
    for var in env_vars(remote) {
        if let Some(v) = (sources.env)(var).map(|v| v.trim().to_string()) {
            if !v.is_empty() {
                return Some(Token {
                    secret: v,
                    source: TokenSource::Env,
                });
            }
        }
    }
    if sources.cli {
        if let Some(t) = cli_token(remote) {
            return Some(Token {
                secret: t,
                source: TokenSource::Cli,
            });
        }
    }
    if sources.keychain {
        if let Some(t) = keychain_token(&remote.host) {
            return Some(Token {
                secret: t,
                source: TokenSource::Keychain,
            });
        }
    }
    None
}

fn run(program: &str, args: &[&str]) -> Option<std::process::Output> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .output()
        .ok()
}

fn cli_token(remote: &ForgeRemote) -> Option<String> {
    let host = remote.host.split(':').next().unwrap_or(&remote.host);
    match remote.kind {
        ForgeKind::Github => {
            let out = run("gh", &["auth", "token", "--hostname", host])?;
            if !out.status.success() {
                return None;
            }
            let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (!t.is_empty()).then_some(t)
        }
        ForgeKind::Gitlab => {
            let out = run("glab", &["auth", "status", "-t", "--hostname", host])?;
            // glab prints the status (with "Token: <value>") on stderr.
            let text = format!(
                "{}\n{}",
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&out.stdout)
            );
            parse_glab_token(&text)
        }
    }
}

/// Extracts the token from `glab auth status -t` output.
pub(crate) fn parse_glab_token(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let l = l.trim().trim_start_matches(['✓', '!', 'x', '✗', ' ']);
        let rest = l.strip_prefix("Token:")?.trim();
        (!rest.is_empty() && !rest.starts_with('*')).then(|| rest.to_string())
    })
}

fn keychain_token(host: &str) -> Option<String> {
    let entry = keyring::Entry::new(SERVICE, host).ok()?;
    entry.get_password().ok().filter(|t| !t.is_empty())
}

/// Stores a token for `host` in the OS keychain.
pub fn store_token(host: &str, token: &str) -> Result<(), String> {
    keyring::Entry::new(SERVICE, host)
        .and_then(|e| e.set_password(token))
        .map_err(|e| e.to_string())
}

/// Removes the keychain token of `host`.
pub fn delete_token(host: &str) -> Result<(), String> {
    match keyring::Entry::new(SERVICE, host).and_then(|e| e.delete_credential()) {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// How to give Lector a token for `remote` (markdown).
pub fn token_help(remote: &ForgeRemote) -> String {
    let vars = env_vars(remote).join("` or `");
    match remote.kind {
        ForgeKind::Github => format!(
            "Lector reads a token from `{vars}`, then from the GitHub CLI (`gh auth login --hostname {host}`), \
             then from the OS keychain (service `{SERVICE}`, account `{host}`). \
             Classic tokens need the `repo` scope; fine-grained tokens need *Pull requests: read & write* and *Contents: read*.",
            host = remote.host
        ),
        ForgeKind::Gitlab => format!(
            "Lector reads a token from `{vars}`, then from the GitLab CLI (`glab auth login --hostname {host}`), \
             then from the OS keychain (service `{SERVICE}`, account `{host}`). The token needs the `api` scope.",
            host = remote.host
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_order_and_redaction() {
        let r = crate::detect("https://github.com/a/b").expect("gh");
        let env = |k: &str| match k {
            "GH_TOKEN" => Some("from-gh".to_string()),
            "GITHUB_TOKEN" => Some("  ".to_string()),
            _ => None,
        };
        let src = TokenSources {
            env: &env,
            cli: false,
            keychain: false,
        };
        let t = auth_token_with(&r, &src).expect("token");
        assert_eq!(t.secret(), "from-gh");
        assert_eq!(t.source, TokenSource::Env);
        assert!(!format!("{t:?}").contains("from-gh"));
        let none = TokenSources {
            env: &|_| None,
            cli: false,
            keychain: false,
        };
        assert!(auth_token_with(&r, &none).is_none());
        let gl = crate::detect("https://gitlab.com/a/b").expect("gl");
        let env = |k: &str| (k == "GITLAB_TOKEN").then(|| "glpat".to_string());
        let src = TokenSources {
            env: &env,
            cli: false,
            keychain: false,
        };
        assert_eq!(
            auth_token_with(&gl, &src).map(|t| t.secret().to_string()),
            Some("glpat".into())
        );
        assert!(token_help(&gl).contains("GITLAB_TOKEN"));
    }

    #[test]
    fn glab_output() {
        let out = "gitlab.com\n  ✓ Logged in to gitlab.com as ada\n  ✓ Token: glpat-123\n";
        assert_eq!(parse_glab_token(out).as_deref(), Some("glpat-123"));
        assert_eq!(parse_glab_token("  ✓ Token: **************"), None);
    }
}
