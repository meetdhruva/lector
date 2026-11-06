//! Recognising GitHub / GitLab remotes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ForgeKind;

/// Known hosts (from settings `[forge]`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ForgeHosts {
    /// GitHub Enterprise hosts (github.com is always known).
    pub github: Vec<String>,
    /// Self-hosted GitLab hosts (gitlab.com is always known).
    pub gitlab: Vec<String>,
    /// API base URL overrides by host (e.g. a proxy, or a mock server in tests).
    pub api_urls: BTreeMap<String, String>,
}

/// A repository on a code host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeRemote {
    /// Host kind.
    pub kind: ForgeKind,
    /// Host name (lowercase, with `:port` when not default).
    pub host: String,
    /// Repository path: `owner/repo` (GitHub) or `group/sub/project` (GitLab).
    pub path: String,
    /// Owner / namespace (everything before the last segment).
    pub owner: String,
    /// Repository name (last segment).
    pub name: String,
    /// REST API base URL (`https://api.github.com`, `https://host/api/v3`, `https://host/api/v4`).
    pub api_base: String,
    /// Web base URL (`https://host`).
    pub web_base: String,
    /// The git remote URL as configured (used to fetch pull requests).
    pub remote_url: String,
}

impl ForgeRemote {
    /// Web URL of the repository.
    pub fn web_url(&self) -> String {
        format!("{}/{}", self.web_base, self.path)
    }

    /// Short label: `github.com/owner/repo`.
    pub fn label(&self) -> String {
        format!("{}/{}", self.host, self.path)
    }

    /// GitHub GraphQL endpoint.
    pub fn graphql_url(&self) -> String {
        match self.api_base.strip_suffix("/api/v3") {
            Some(base) => format!("{base}/api/graphql"),
            None => format!("{}/graphql", self.api_base.trim_end_matches('/')),
        }
    }
}

/// Splits a git remote URL into (scheme, host incl. port, path without `.git`).
/// Supports `https://`, `http://`, `ssh://`, `git://`, `git+ssh://` and scp-like
/// `user@host:path`. Local paths and `file://` give `None`.
pub fn parse_remote_url(url: &str) -> Option<(String, String, String)> {
    let url = url.trim();
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => {
            // scp-like: [user@]host:path (not a Windows drive, not a local path)
            if url.starts_with('/') || url.starts_with('.') {
                return None;
            }
            let (host, path) = url.split_once(':')?;
            if host.len() <= 1 || host.contains('/') || path.starts_with("//") {
                return None;
            }
            let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
            return Some(("ssh".into(), host.to_ascii_lowercase(), clean_path(path)?));
        }
    };
    if scheme == "file" {
        return None;
    }
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host_port = host_port.to_ascii_lowercase();
    // Drop default / ssh ports: the API lives on the default port.
    let host = match host_port.split_once(':') {
        Some((h, port)) if scheme.contains("ssh") || port == "443" || port == "22" => h.to_string(),
        Some((h, "80")) if scheme == "http" => h.to_string(),
        _ => host_port,
    };
    if host.is_empty() {
        return None;
    }
    Some((scheme, host, clean_path(path)?))
}

fn clean_path(path: &str) -> Option<String> {
    let p = path.trim_matches('/');
    let p = p.strip_suffix(".git").unwrap_or(p).trim_matches('/');
    let p = p.strip_prefix('~').unwrap_or(p);
    (!p.is_empty() && p.contains('/')).then(|| p.to_string())
}

/// Detects github.com and gitlab.com remotes (plus `github.*` / `gitlab.*` hosts).
pub fn detect(remote_url: &str) -> Option<ForgeRemote> {
    detect_with(remote_url, &ForgeHosts::default())
}

/// Detects a remote using the configured hosts.
pub fn detect_with(remote_url: &str, hosts: &ForgeHosts) -> Option<ForgeRemote> {
    let (scheme, host, path) = parse_remote_url(remote_url)?;
    let bare_host = host.split(':').next().unwrap_or(&host).to_string();
    let listed = |list: &[String]| {
        list.iter()
            .any(|h| h.eq_ignore_ascii_case(&host) || h.eq_ignore_ascii_case(&bare_host))
    };
    let kind = if bare_host == "github.com"
        || bare_host == "www.github.com"
        || listed(&hosts.github)
    {
        ForgeKind::Github
    } else if bare_host == "gitlab.com" || listed(&hosts.gitlab) || bare_host.starts_with("gitlab.")
    {
        ForgeKind::Gitlab
    } else if bare_host.starts_with("github.") {
        ForgeKind::Github
    } else {
        return None;
    };
    let host = if bare_host == "www.github.com" {
        "github.com".to_string()
    } else {
        host
    };
    if kind == ForgeKind::Github && path.split('/').count() != 2 {
        return None;
    }
    let (owner, name) = path.rsplit_once('/')?;
    let web_scheme = if scheme == "http" { "http" } else { "https" };
    // An SSH port is not the web port.
    let web_host = if scheme.contains("ssh") {
        bare_host.clone()
    } else {
        host.clone()
    };
    let web_base = format!("{web_scheme}://{web_host}");
    let api_base = hosts
        .api_urls
        .get(&host)
        .or_else(|| hosts.api_urls.get(&bare_host))
        .map(|u| u.trim_end_matches('/').to_string())
        .unwrap_or_else(|| match kind {
            ForgeKind::Github if bare_host == "github.com" => "https://api.github.com".into(),
            ForgeKind::Github => format!("{web_base}/api/v3"),
            ForgeKind::Gitlab => format!("{web_base}/api/v4"),
        });
    Some(ForgeRemote {
        kind,
        host,
        path: path.clone(),
        owner: owner.to_string(),
        name: name.to_string(),
        api_base,
        web_base,
        remote_url: remote_url.trim().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_url_forms() {
        for (url, host, path) in [
            (
                "https://github.com/acme/widgets.git",
                "github.com",
                "acme/widgets",
            ),
            (
                "https://user@github.com/acme/widgets",
                "github.com",
                "acme/widgets",
            ),
            (
                "git@github.com:acme/widgets.git",
                "github.com",
                "acme/widgets",
            ),
            (
                "ssh://git@github.com:22/acme/widgets.git",
                "github.com",
                "acme/widgets",
            ),
            (
                "ssh://git@gitlab.corp:2222/g/sub/p.git",
                "gitlab.corp",
                "g/sub/p",
            ),
            (
                "https://gitlab.corp:8443/g/p.git/",
                "gitlab.corp:8443",
                "g/p",
            ),
            ("git://github.com/a/b", "github.com", "a/b"),
        ] {
            let (_, h, p) = parse_remote_url(url).unwrap_or_else(|| panic!("{url}"));
            assert_eq!((h.as_str(), p.as_str()), (host, path), "{url}");
        }
        assert!(parse_remote_url("/srv/git/repo.git").is_none());
        assert!(parse_remote_url("file:///srv/repo").is_none());
        assert!(parse_remote_url("C:\\repo").is_none());
        assert!(parse_remote_url("https://github.com/onlyowner").is_none());
    }

    #[test]
    fn detects_hosts() {
        let gh = detect("git@github.com:acme/widgets.git").expect("gh");
        assert_eq!(gh.kind, ForgeKind::Github);
        assert_eq!(gh.api_base, "https://api.github.com");
        assert_eq!(gh.graphql_url(), "https://api.github.com/graphql");
        assert_eq!(gh.web_url(), "https://github.com/acme/widgets");

        let gl = detect("https://gitlab.com/group/sub/proj.git").expect("gl");
        assert_eq!(gl.kind, ForgeKind::Gitlab);
        assert_eq!((gl.owner.as_str(), gl.name.as_str()), ("group/sub", "proj"));
        assert_eq!(gl.api_base, "https://gitlab.com/api/v4");

        assert!(detect("https://example.com/a/b.git").is_none());
        assert!(detect("https://github.com/a/b/c").is_none());

        let hosts = ForgeHosts {
            github: vec!["code.corp".into()],
            gitlab: vec!["git.corp".into()],
            api_urls: [(
                "git.corp".to_string(),
                "http://127.0.0.1:9/api/v4/".to_string(),
            )]
            .into_iter()
            .collect(),
        };
        let ghe = detect_with("git@code.corp:team/app.git", &hosts).expect("ghe");
        assert_eq!(ghe.kind, ForgeKind::Github);
        assert_eq!(ghe.api_base, "https://code.corp/api/v3");
        assert_eq!(ghe.graphql_url(), "https://code.corp/api/graphql");
        let sh = detect_with("https://git.corp/team/app", &hosts).expect("gitlab");
        assert_eq!(sh.kind, ForgeKind::Gitlab);
        assert_eq!(sh.api_base, "http://127.0.0.1:9/api/v4");
        assert!(detect("https://gitlab.example.org/a/b").is_some());
    }
}
