//! Web permalinks to a file (and line range) at a commit on common forges.

use std::ops::Range;

use crate::error::Result;
use crate::Repo;

/// Recognised hosting services.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Forge {
    /// github.com and GitHub Enterprise (host containing `github`).
    GitHub,
    /// gitlab.com and self-hosted GitLab (host containing `gitlab`).
    GitLab,
    /// bitbucket.org.
    Bitbucket,
    /// Gitea, Forgejo and Codeberg.
    Gitea,
    /// git.sr.ht.
    SourceHut,
    /// Azure DevOps (dev.azure.com / *.visualstudio.com).
    AzureDevOps,
}

/// A remote URL reduced to its web location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteWeb {
    /// Host (with port for http(s) URLs that specify one).
    pub host: String,
    /// Repository path without leading/trailing `/` and `.git` (`owner/repo`).
    pub path: String,
    /// `http` for plain-http remotes, `https` otherwise.
    pub scheme: &'static str,
}

/// Parse a remote URL (`https://host/owner/repo.git`, `git@host:owner/repo.git`,
/// `ssh://git@host:22/owner/repo`, `git://…`) into host and repository path.
pub fn parse_remote_url(url: &str) -> Option<RemoteWeb> {
    let url = url.trim();
    let (scheme, host_and_path, keep_port) = if let Some((scheme, rest)) = url.split_once("://") {
        let scheme = scheme.to_ascii_lowercase();
        let keep_port = scheme == "http" || scheme == "https";
        let web_scheme = if scheme == "http" { "http" } else { "https" };
        if !matches!(
            scheme.as_str(),
            "http" | "https" | "ssh" | "git" | "git+ssh" | "ssh+git"
        ) {
            return None;
        }
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        (web_scheme, (host.to_owned(), path.to_owned()), keep_port)
    } else {
        // scp-like: [user@]host:path
        let (left, path) = url.split_once(':')?;
        // `C:/dir` is a Windows path, not a host.
        if left.contains('/') || left.len() == 1 || path.starts_with("//") {
            return None;
        }
        let host = left.rsplit_once('@').map_or(left, |(_, h)| h);
        ("https", (host.to_owned(), path.to_owned()), false)
    };
    let (mut host, path) = host_and_path;
    if !keep_port {
        if let Some((h, port)) = host.rsplit_once(':') {
            if port.chars().all(|c| c.is_ascii_digit()) {
                host = h.to_owned();
            }
        }
    }
    let host = host.to_ascii_lowercase();
    let path = path.trim_matches('/');
    let path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(RemoteWeb {
        host,
        path: path.to_owned(),
        scheme,
    })
}

/// Guess the forge from the host name.
pub fn detect_forge(host: &str) -> Option<Forge> {
    let host = host.to_ascii_lowercase();
    let host = host.split(':').next().unwrap_or_default();
    if host.contains("github") {
        Some(Forge::GitHub)
    } else if host.contains("gitlab") {
        Some(Forge::GitLab)
    } else if host.contains("bitbucket") {
        Some(Forge::Bitbucket)
    } else if host == "git.sr.ht" || host.ends_with(".sr.ht") {
        Some(Forge::SourceHut)
    } else if host.contains("codeberg") || host.contains("gitea") || host.contains("forgejo") {
        Some(Forge::Gitea)
    } else if host == "dev.azure.com"
        || host == "ssh.dev.azure.com"
        || host.ends_with(".visualstudio.com")
    {
        Some(Forge::AzureDevOps)
    } else {
        None
    }
}

fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Build a permalink. `lines` are 0-based, end-exclusive; URLs use 1-based inclusive lines.
pub fn build_permalink(
    remote_url: &str,
    sha: &str,
    path: &str,
    lines: Option<Range<u32>>,
) -> Option<String> {
    let web = parse_remote_url(remote_url)?;
    let forge = detect_forge(&web.host)?;
    let file = encode_path(path.trim_start_matches('/'));
    let lines = lines.map(|r| {
        let start = r.start.saturating_add(1);
        (start, r.end.max(start))
    });
    let base = format!("{}://{}/{}", web.scheme, web.host, web.path);
    let url = match forge {
        Forge::GitHub => {
            let anchor = match lines {
                Some((s, e)) if e > s => format!("#L{s}-L{e}"),
                Some((s, _)) => format!("#L{s}"),
                None => String::new(),
            };
            format!("{base}/blob/{sha}/{file}{anchor}")
        }
        Forge::GitLab => {
            let anchor = match lines {
                Some((s, e)) if e > s => format!("#L{s}-{e}"),
                Some((s, _)) => format!("#L{s}"),
                None => String::new(),
            };
            format!("{base}/-/blob/{sha}/{file}{anchor}")
        }
        Forge::Bitbucket => {
            let anchor = match lines {
                Some((s, e)) if e > s => format!("#lines-{s}:{e}"),
                Some((s, _)) => format!("#lines-{s}"),
                None => String::new(),
            };
            format!("{base}/src/{sha}/{file}{anchor}")
        }
        Forge::Gitea => {
            let anchor = match lines {
                Some((s, e)) if e > s => format!("#L{s}-L{e}"),
                Some((s, _)) => format!("#L{s}"),
                None => String::new(),
            };
            format!("{base}/src/commit/{sha}/{file}{anchor}")
        }
        Forge::SourceHut => {
            let anchor = match lines {
                Some((s, _)) => format!("#L{s}"),
                None => String::new(),
            };
            format!("{base}/tree/{sha}/item/{file}{anchor}")
        }
        Forge::AzureDevOps => azure_permalink(&web, sha, &file, lines)?,
    };
    Some(url)
}

/// Azure DevOps: `https://dev.azure.com/{org}/{project}/_git/{repo}?path=/{file}&version=GC{sha}&line=…`.
fn azure_permalink(
    web: &RemoteWeb,
    sha: &str,
    file: &str,
    lines: Option<(u32, u32)>,
) -> Option<String> {
    let segments: Vec<&str> = web.path.split('/').filter(|s| !s.is_empty()).collect();
    let (org, project, repo) = if web.host == "ssh.dev.azure.com" {
        // v3/{org}/{project}/{repo}
        match segments.as_slice() {
            ["v3", org, project, repo] => (org.to_string(), project.to_string(), repo.to_string()),
            _ => return None,
        }
    } else if web.host == "dev.azure.com" {
        match segments.as_slice() {
            [org, project, "_git", repo] => {
                (org.to_string(), project.to_string(), repo.to_string())
            }
            _ => return None,
        }
    } else {
        // {org}.visualstudio.com/[DefaultCollection/]{project}/_git/{repo}
        let org = web.host.strip_suffix(".visualstudio.com")?.to_owned();
        let segments: Vec<&str> = segments
            .into_iter()
            .filter(|s| *s != "DefaultCollection")
            .collect();
        match segments.as_slice() {
            [project, "_git", repo] => (org, project.to_string(), repo.to_string()),
            ["v3", _, project, repo] => (org, project.to_string(), repo.to_string()),
            _ => return None,
        }
    };
    let mut url =
        format!("https://dev.azure.com/{org}/{project}/_git/{repo}?path=/{file}&version=GC{sha}");
    if let Some((s, e)) = lines {
        url.push_str(&format!(
            "&line={s}&lineEnd={}&lineStartColumn=1&lineEndColumn=1&lineStyle=plain&_a=contents",
            e + 1
        ));
    }
    Some(url)
}

impl Repo {
    /// A web permalink for `path` (repository-relative) at `rev` (default: the
    /// `HEAD` commit), optionally anchored to 0-based `lines`. Uses the remote of
    /// `HEAD`'s upstream, else `origin`, else the first remote. `Ok(None)` when
    /// there is no remote or the host isn't a recognised forge.
    pub fn permalink(
        &self,
        path: &str,
        lines: Option<Range<u32>>,
        rev: Option<&str>,
    ) -> Result<Option<String>> {
        let remotes = self.remotes()?;
        let preferred = self.head_remote_name();
        let remote = preferred
            .and_then(|name| remotes.iter().find(|r| r.name == name))
            .or_else(|| remotes.iter().find(|r| r.name == "origin"))
            .or_else(|| remotes.first());
        let Some(remote) = remote else {
            return Ok(None);
        };
        let sha = self.resolve(rev.unwrap_or("HEAD"))?.to_string();
        Ok(build_permalink(&remote.url, &sha, path, lines))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn parses_url_forms() {
        let cases = [
            (
                "https://github.com/owner/repo.git",
                "github.com",
                "owner/repo",
            ),
            ("https://github.com/owner/repo", "github.com", "owner/repo"),
            (
                "https://user:tok@github.com/owner/repo.git/",
                "github.com",
                "owner/repo",
            ),
            ("git@github.com:owner/repo.git", "github.com", "owner/repo"),
            ("github.com:owner/repo", "github.com", "owner/repo"),
            (
                "ssh://git@gitlab.com:2222/group/sub/repo.git",
                "gitlab.com",
                "group/sub/repo",
            ),
            (
                "ssh://git@codeberg.org/owner/repo.git",
                "codeberg.org",
                "owner/repo",
            ),
            ("git://git.example.org/proj.git", "git.example.org", "proj"),
            (
                "https://gitea.local:3000/o/r.git",
                "gitea.local:3000",
                "o/r",
            ),
            ("git@git.sr.ht:~user/repo", "git.sr.ht", "~user/repo"),
        ];
        for (url, host, path) in cases {
            let web = parse_remote_url(url).unwrap_or_else(|| panic!("failed: {url}"));
            assert_eq!(web.host, host, "{url}");
            assert_eq!(web.path, path, "{url}");
        }
        assert!(parse_remote_url("/local/path/repo").is_none());
        assert!(parse_remote_url("file:///tmp/repo").is_none());
        assert!(parse_remote_url("").is_none());
        assert_eq!(parse_remote_url("http://h.com/o/r").unwrap().scheme, "http");
    }

    #[test]
    fn github() {
        assert_eq!(
            build_permalink("git@github.com:o/r.git", SHA, "src/main.rs", Some(9..20)).unwrap(),
            format!("https://github.com/o/r/blob/{SHA}/src/main.rs#L10-L20")
        );
        assert_eq!(
            build_permalink("https://github.com/o/r", SHA, "a b.rs", Some(4..5)).unwrap(),
            format!("https://github.com/o/r/blob/{SHA}/a%20b.rs#L5")
        );
        assert_eq!(
            build_permalink("https://github.com/o/r", SHA, "x.rs", None).unwrap(),
            format!("https://github.com/o/r/blob/{SHA}/x.rs")
        );
    }

    #[test]
    fn gitlab() {
        assert_eq!(
            build_permalink("git@gitlab.com:g/s/r.git", SHA, "f.rs", Some(9..20)).unwrap(),
            format!("https://gitlab.com/g/s/r/-/blob/{SHA}/f.rs#L10-20")
        );
    }

    #[test]
    fn bitbucket() {
        assert_eq!(
            build_permalink("git@bitbucket.org:o/r.git", SHA, "f.rs", Some(9..20)).unwrap(),
            format!("https://bitbucket.org/o/r/src/{SHA}/f.rs#lines-10:20")
        );
    }

    #[test]
    fn gitea_family() {
        for host in ["codeberg.org", "gitea.example.com", "forgejo.example.com"] {
            assert_eq!(
                build_permalink(&format!("https://{host}/o/r.git"), SHA, "f.rs", Some(9..20))
                    .unwrap(),
                format!("https://{host}/o/r/src/commit/{SHA}/f.rs#L10-L20")
            );
        }
    }

    #[test]
    fn sourcehut() {
        assert_eq!(
            build_permalink("https://git.sr.ht/~u/r", SHA, "f.rs", Some(9..20)).unwrap(),
            format!("https://git.sr.ht/~u/r/tree/{SHA}/item/f.rs#L10")
        );
    }

    #[test]
    fn azure() {
        let expected = format!(
            "https://dev.azure.com/org/proj/_git/repo?path=/f.rs&version=GC{SHA}&line=10&lineEnd=21&lineStartColumn=1&lineEndColumn=1&lineStyle=plain&_a=contents"
        );
        assert_eq!(
            build_permalink(
                "https://org@dev.azure.com/org/proj/_git/repo",
                SHA,
                "f.rs",
                Some(9..20)
            )
            .unwrap(),
            expected
        );
        assert_eq!(
            build_permalink(
                "git@ssh.dev.azure.com:v3/org/proj/repo",
                SHA,
                "f.rs",
                Some(9..20)
            )
            .unwrap(),
            expected
        );
        assert_eq!(
            build_permalink(
                "https://org.visualstudio.com/DefaultCollection/proj/_git/repo",
                SHA,
                "f.rs",
                Some(9..20)
            )
            .unwrap(),
            expected
        );
    }

    #[test]
    fn unknown_host() {
        assert!(build_permalink("https://example.com/o/r", SHA, "f", None).is_none());
    }
}
