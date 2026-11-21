//! Finding an API token: environment → `gh` / `glab` CLI → OS keychain.
//! Tokens are never logged; [`Token`]'s `Debug` is redacted.
//!
//! **A token goes only to the host it belongs to (ADR 0010, item 2).**
//! * `github.com`: `GITHUB_TOKEN`, then `GH_TOKEN`, then `gh auth token --hostname github.com`,
//!   then the keychain entry for `github.com`.
//! * `gitlab.com`: `GITLAB_TOKEN`, then `GITLAB_PRIVATE_TOKEN`, then
//!   `glab auth status -t --hostname gitlab.com`, then the keychain entry for `gitlab.com`.
//! * **A host the user listed in `[forge]`** (GitHub Enterprise, self-hosted GitLab) never gets
//!   the variables above. Its sources, in order: the host-specific environment variable
//!   [`host_env_var`] (`LECTOR_FORGE_TOKEN_<HOST>`, e.g. `LECTOR_FORGE_TOKEN_CODE_CORP` for
//!   `code.corp`, `LECTOR_FORGE_TOKEN_GIT_CORP_8443` for `git.corp:8443`), then `gh` / `glab` for
//!   that host, run with every token variable removed from its environment (`GITHUB_TOKEN`,
//!   `GH_TOKEN`, `GH_ENTERPRISE_TOKEN`, `GITHUB_ENTERPRISE_TOKEN`, `GITLAB_TOKEN`, ...: see
//!   [`CLI_SCRUBBED_ENV`]) so it answers only from its own login for that host, then the
//!   keychain entry for that host (service `lector-forge`, account = the host).
//!
//! The token is then sent only to the remote's API base over HTTPS (or to a loopback address,
//! for tests and local proxies), never along a redirect (`Authorization` is dropped on every
//! redirect) and never to a `Link` URL on another origin.

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

/// Token variables removed from the environment of `gh` / `glab` when they are asked for a
/// user-listed host, so a github.com / gitlab.com (or shared enterprise) token in the
/// environment cannot be handed to that host.
pub const CLI_SCRUBBED_ENV: &[&str] = &[
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GITLAB_TOKEN",
    "GITLAB_PRIVATE_TOKEN",
    "GITLAB_ACCESS_TOKEN",
    "OAUTH_TOKEN",
    "GH_HOST",
    "GITLAB_HOST",
];

/// The host-specific environment variable for a user-listed `host`:
/// `LECTOR_FORGE_TOKEN_` + the host (port included) in upper case with every character other
/// than `A-Z` / `0-9` replaced by `_` (`code.corp` → `LECTOR_FORGE_TOKEN_CODE_CORP`).
pub fn host_env_var(host: &str) -> String {
    let suffix: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("LECTOR_FORGE_TOKEN_{suffix}")
}

/// The environment variables consulted for `remote`, in order: the well-known variables for
/// `github.com` / `gitlab.com`, else only [`host_env_var`] (see the [module docs](self)).
pub fn env_vars(remote: &ForgeRemote) -> Vec<String> {
    let bare = remote.host.split(':').next().unwrap_or(&remote.host);
    match (remote.kind, bare) {
        (ForgeKind::Github, "github.com") => vec!["GITHUB_TOKEN".into(), "GH_TOKEN".into()],
        (ForgeKind::Gitlab, "gitlab.com") => {
            vec!["GITLAB_TOKEN".into(), "GITLAB_PRIVATE_TOKEN".into()]
        }
        _ => vec![host_env_var(&remote.host)],
    }
}

/// Finds a token for `remote` (environment, CLI, keychain).
pub fn auth_token(remote: &ForgeRemote) -> Option<Token> {
    auth_token_with(remote, &TokenSources::default())
}

/// [`auth_token`] with explicit sources.
pub fn auth_token_with(remote: &ForgeRemote, sources: &TokenSources<'_>) -> Option<Token> {
    for var in env_vars(remote) {
        if let Some(v) = (sources.env)(&var).map(|v| v.trim().to_string()) {
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

/// The `gh` / `glab` command for `remote`: environment tokens are scrubbed unless the host is
/// `github.com` / `gitlab.com` ([`CLI_SCRUBBED_ENV`]).
fn cli_command(remote: &ForgeRemote, program: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1");
    if !crate::remote::is_public_forge_host(&remote.host) {
        for var in CLI_SCRUBBED_ENV {
            cmd.env_remove(var);
        }
    }
    cmd
}

fn run(remote: &ForgeRemote, program: &str, args: &[&str]) -> Option<std::process::Output> {
    cli_command(remote, program, args).output().ok()
}

fn cli_token(remote: &ForgeRemote) -> Option<String> {
    let host = remote.host.split(':').next().unwrap_or(&remote.host);
    match remote.kind {
        ForgeKind::Github => {
            let out = run(remote, "gh", &["auth", "token", "--hostname", host])?;
            if !out.status.success() {
                return None;
            }
            let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (!t.is_empty()).then_some(t)
        }
        ForgeKind::Gitlab => {
            let out = run(
                remote,
                "glab",
                &["auth", "status", "-t", "--hostname", host],
            )?;
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
    let vars = format!("`{}`", env_vars(remote).join("` or `"));
    let vars = if crate::remote::is_public_forge_host(&remote.host) {
        vars
    } else {
        format!("{vars} (a token for this host only; `GITHUB_TOKEN`, `GITLAB_TOKEN` and the like are never sent here)")
    };
    match remote.kind {
        ForgeKind::Github => format!(
            "Lector reads a token from {vars}, then from the GitHub CLI (`gh auth login --hostname {host}`), \
             then from the OS keychain (service `{SERVICE}`, account `{host}`). \
             Classic tokens need the `repo` scope; fine-grained tokens need *Pull requests: read & write* and *Contents: read*.",
            host = remote.host
        ),
        ForgeKind::Gitlab => format!(
            "Lector reads a token from {vars}, then from the GitLab CLI (`glab auth login --hostname {host}`), \
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

    fn listed() -> crate::ForgeHosts {
        crate::ForgeHosts {
            github: vec!["code.corp".into()],
            gitlab: vec!["git.corp:8443".into()],
            ..Default::default()
        }
    }

    fn every_token(k: &str) -> Option<String> {
        Some(format!("value-of-{k}"))
    }

    /// ADR 0010, item 2: a user-listed host never receives the github.com / gitlab.com
    /// environment tokens, only its own host-specific variable.
    #[test]
    fn listed_hosts_get_only_their_own_token() {
        let src = TokenSources {
            env: &every_token,
            cli: false,
            keychain: false,
        };
        let ghe = crate::detect_with("git@code.corp:team/app.git", &listed()).expect("ghe");
        assert_eq!(env_vars(&ghe), ["LECTOR_FORGE_TOKEN_CODE_CORP"]);
        assert_eq!(
            auth_token_with(&ghe, &src).map(|t| t.secret().to_string()),
            Some("value-of-LECTOR_FORGE_TOKEN_CODE_CORP".into())
        );
        let gl = crate::detect_with("https://git.corp:8443/g/p.git", &listed()).expect("gl");
        assert_eq!(env_vars(&gl), ["LECTOR_FORGE_TOKEN_GIT_CORP_8443"]);
        let only_public = |k: &str| {
            matches!(
                k,
                "GITHUB_TOKEN" | "GH_TOKEN" | "GITLAB_TOKEN" | "GH_ENTERPRISE_TOKEN"
            )
            .then(|| "public".to_string())
        };
        let src = TokenSources {
            env: &only_public,
            cli: false,
            keychain: false,
        };
        assert!(auth_token_with(&ghe, &src).is_none());
        assert!(auth_token_with(&gl, &src).is_none());
        // github.com and gitlab.com still read the well-known variables.
        let gh = crate::detect("https://github.com/a/b").expect("gh");
        assert_eq!(
            auth_token_with(&gh, &src).map(|t| t.secret().to_string()),
            Some("public".into())
        );
        assert!(token_help(&ghe).contains("LECTOR_FORGE_TOKEN_CODE_CORP"));
        assert!(token_help(&ghe)
            .starts_with("Lector reads a token from `LECTOR_FORGE_TOKEN_CODE_CORP`"));
    }

    /// `gh` / `glab` asked about a listed host run without any token variable.
    #[test]
    fn cli_for_listed_hosts_runs_without_token_variables() {
        let ghe = crate::detect_with("git@code.corp:team/app.git", &listed()).expect("ghe");
        let cmd = cli_command(&ghe, "gh", &["auth", "token"]);
        let removed: Vec<String> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        for var in CLI_SCRUBBED_ENV {
            assert!(removed.iter().any(|r| r == var), "{var} not removed");
        }
        let gh = crate::detect("https://github.com/a/b").expect("gh");
        let cmd = cli_command(&gh, "gh", &["auth", "token"]);
        assert!(cmd.get_envs().all(|(_, v)| v.is_some()));
    }

    #[test]
    fn glab_output() {
        let out = "gitlab.com\n  ✓ Logged in to gitlab.com as ada\n  ✓ Token: glpat-123\n";
        assert_eq!(parse_glab_token(out).as_deref(), Some("glpat-123"));
        assert_eq!(parse_glab_token("  ✓ Token: **************"), None);
    }
}
