//! Blocking JSON-over-HTTP client (ureq) with error mapping, pagination and
//! no token in logs.

use std::time::Duration;

use serde_json::Value;

use crate::{auth::Token, ForgeError, ForgeKind, Result};

/// Pages followed at most by [`Client::get_paged`].
pub(crate) const MAX_PAGES: usize = 10;

pub(crate) struct Client {
    agent: ureq::Agent,
    base: String,
    host: String,
    kind: ForgeKind,
    token: Token,
}

/// Whether a request to `url` may carry a token: HTTPS, or plain HTTP only to the local
/// machine ([`is_loopback_url`]: tests, local mirrors and proxies). The one rule for the API
/// client's transport guard and the pull-request fetch header.
pub fn may_carry_token(url: &str) -> bool {
    let url = url.trim();
    let scheme = url.split_once("://").map(|(s, _)| s.to_ascii_lowercase());
    match scheme.as_deref() {
        Some("https") => true,
        Some("http") => is_loopback_url(url),
        _ => false,
    }
}

/// Whether `url`'s host is the local machine: `localhost`, an address in `127.0.0.0/8` or
/// `::1` (bracketed), after removing any `user[:password]@` and `:port`. The one rule for the
/// proxy bypass, the "token only over HTTPS or to loopback" guard and the pull-request fetch
/// header. Anything that does not parse is not
/// loopback: `127.0.0.1.evil.example`, `localhost.evil.example`, `127.0.0.1@evil.example`, a
/// non-numeric port, a URL without a scheme.
pub fn is_loopback_url(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Userinfo ends at the last `@`: what follows is the host.
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
        let Some((host, after)) = bracketed.split_once(']') else {
            return false;
        };
        let port = match after {
            "" => None,
            p => match p.strip_prefix(':') {
                Some(p) => Some(p),
                None => return false,
            },
        };
        let Ok(ip) = host.parse::<std::net::Ipv6Addr>() else {
            return false;
        };
        if !ip.is_loopback() {
            return false;
        }
        ("", port)
    } else {
        match host_port.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (host_port, None),
        }
    };
    if port.is_some_and(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    if host_port.starts_with('[') {
        return true;
    }
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
}

pub(crate) struct Response {
    pub json: Value,
    pub next: Option<String>,
}

impl Client {
    pub(crate) fn new(base: String, host: String, kind: ForgeKind, token: Token) -> Client {
        let mut cfg = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            .user_agent(concat!("lector/", env!("CARGO_PKG_VERSION")))
            // Never carry the token along a redirect (to any host). This is ureq's default;
            // stated here so an upgrade cannot change it silently (pinned by a test).
            .redirect_auth_headers(ureq::config::RedirectAuthHeaders::Never)
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            );
        if is_loopback_url(&base) {
            cfg = cfg.proxy(None);
        }
        Client {
            agent: cfg.build().into(),
            base: base.trim_end_matches('/').to_string(),
            host,
            kind,
            token,
        }
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            path.to_string()
        } else {
            format!("{}{}", self.base, path)
        }
    }

    fn with_headers<B>(&self, req: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        let req = req.header("Authorization", format!("Bearer {}", self.token.secret()));
        match self.kind {
            ForgeKind::Github => req
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"),
            ForgeKind::Gitlab => req.header("Accept", "application/json"),
        }
    }

    fn finish(
        &self,
        what: &str,
        result: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<Response> {
        let mut resp = result.map_err(|e| ForgeError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let next = header("link").and_then(|l| next_link(&l));
        let remaining = header("x-ratelimit-remaining").or_else(|| header("ratelimit-remaining"));
        let reset = header("x-ratelimit-reset")
            .or_else(|| header("ratelimit-reset"))
            .and_then(|v| v.parse::<i64>().ok());
        let text = resp
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_string()
            .map_err(|e| ForgeError::Network(e.to_string()))?;
        tracing::debug!(target: "lector_forge", status, what, "forge request");
        if (200..300).contains(&status) {
            let json = if text.trim().is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&text)
                    .map_err(|e| ForgeError::Decode(format!("{what}: {e}")))?
            };
            return Ok(Response { json, next });
        }
        let message = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("message")
                    .or_else(|| v.get("error_description"))
                    .or_else(|| v.get("error"))
                    .map(|m| match m {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
            })
            .unwrap_or_else(|| text.chars().take(200).collect());
        let rate_limited = status == 429
            || (status == 403
                && (remaining.as_deref() == Some("0")
                    || message.to_ascii_lowercase().contains("rate limit")));
        Err(match status {
            _ if rate_limited => ForgeError::RateLimited {
                host: self.host.clone(),
                reset,
            },
            401 => ForgeError::Unauthorized {
                host: self.host.clone(),
                message,
                help: match self.kind {
                    _ if !crate::remote::is_public_forge_host(&self.host) => format!(
                        "Set {} or run `{} auth login --hostname {}`.",
                        crate::auth::host_env_var(&self.host),
                        match self.kind {
                            ForgeKind::Github => "gh",
                            ForgeKind::Gitlab => "glab",
                        },
                        self.host.split(':').next().unwrap_or(&self.host)
                    ),
                    ForgeKind::Github => "Set GITHUB_TOKEN or run `gh auth login`.".into(),
                    ForgeKind::Gitlab => "Set GITLAB_TOKEN or run `glab auth login`.".into(),
                },
            },
            404 => ForgeError::NotFound(what.to_string()),
            _ => ForgeError::Http { status, message },
        })
    }

    /// The token travels only over HTTPS, or to a loopback address (tests, local proxies).
    fn check_transport(&self, url: &str) -> Result<()> {
        if may_carry_token(url) {
            return Ok(());
        }
        Err(ForgeError::Unsupported(format!(
            "refusing to send a token over plain HTTP ({})",
            origin(url)
        )))
    }

    pub(crate) fn get(&self, path: &str) -> Result<Response> {
        // Absolute URLs come from `Link` headers: never send the token to another origin.
        if path.contains("://") && origin(path) != origin(&self.base) {
            return Err(ForgeError::Decode(format!(
                "refusing to follow a link to another host ({})",
                origin(path)
            )));
        }
        let url = self.url(path);
        self.check_transport(&url)?;
        let req = self.with_headers(self.agent.get(&url));
        self.finish(path, req.call())
    }

    pub(crate) fn get_json(&self, path: &str) -> Result<Value> {
        Ok(self.get(path)?.json)
    }

    /// GETs a JSON array, following `Link: rel="next"` up to [`MAX_PAGES`].
    pub(crate) fn get_paged(&self, path: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut next = Some(path.to_string());
        let mut pages = 0;
        while let Some(p) = next.take() {
            let r = self.get(&p)?;
            match r.json {
                Value::Array(items) => out.extend(items),
                other => {
                    return Err(ForgeError::Decode(format!(
                        "{path}: expected an array, got {other}"
                    )))
                }
            }
            pages += 1;
            if pages < MAX_PAGES {
                next = r.next;
            }
        }
        Ok(out)
    }

    pub(crate) fn send(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let url = self.url(path);
        self.check_transport(&url)?;
        let payload = body.map(Value::to_string).unwrap_or_default();
        let result = match method {
            "POST" => self
                .with_headers(self.agent.post(&url))
                .header("Content-Type", "application/json")
                .send(payload.as_str()),
            "PUT" => self
                .with_headers(self.agent.put(&url))
                .header("Content-Type", "application/json")
                .send(payload.as_str()),
            "PATCH" => self
                .with_headers(self.agent.patch(&url))
                .header("Content-Type", "application/json")
                .send(payload.as_str()),
            _ => return Err(ForgeError::Unsupported(format!("HTTP method {method}"))),
        };
        Ok(self.finish(path, result)?.json)
    }

    /// Runs a GitHub GraphQL query; GraphQL-level errors become [`ForgeError::Http`].
    pub(crate) fn graphql(&self, url: &str, query: &str, variables: Value) -> Result<Value> {
        let body = serde_json::json!({ "query": query, "variables": variables });
        let v = self.send("POST", url, Some(&body))?;
        if let Some(errors) = v.get("errors").and_then(Value::as_array) {
            if !errors.is_empty() {
                let msg = errors
                    .iter()
                    .filter_map(|e| e.get("message").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("; ");
                if msg.to_ascii_lowercase().contains("rate limit") {
                    return Err(ForgeError::RateLimited {
                        host: self.host.clone(),
                        reset: None,
                    });
                }
                return Err(ForgeError::Http {
                    status: 200,
                    message: msg,
                });
            }
        }
        v.get("data")
            .cloned()
            .ok_or_else(|| ForgeError::Decode("GraphQL response without data".into()))
    }
}

/// `scheme://host[:port]` of a URL, lowercased (no userinfo, path or query).
pub(crate) fn origin(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    format!("{}://{}", scheme, host).to_ascii_lowercase()
}

/// The `rel="next"` URL of a `Link` header.
pub(crate) fn next_link(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        params
            .split(';')
            .any(|p| p.trim().replace(' ', "") == "rel=\"next\"")
            .then(|| {
                url.trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string()
            })
    })
}

/// Standard base64 (for the git `Authorization: Basic` header).
pub(crate) fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Percent-encodes a path segment (GitLab project ids `group%2Fproject`).
pub(crate) fn encode_segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: a `Link: rel="next"` to another host received the `Authorization` header.
    #[test]
    fn pagination_never_leaves_the_api_origin() {
        assert_eq!(
            origin("https://API.github.com/repos?page=2"),
            "https://api.github.com"
        );
        assert_eq!(
            origin("https://x@evil.example:8443/a"),
            "https://evil.example:8443"
        );
        let c = Client::new(
            "http://127.0.0.1:9".into(),
            "github.com".into(),
            ForgeKind::Github,
            Token::new("secret"),
        );
        match c.get("http://evil.example/steal?page=2") {
            Err(ForgeError::Decode(m)) => {
                assert!(m.contains("another host") && !m.contains("secret"))
            }
            other => panic!("expected a refusal, got {:?}", other.map(|r| r.json)),
        }
    }

    /// ADR 0010, item 2: a redirect never carries the `Authorization` header (ureq 3.5's
    /// `RedirectAuthHeaders::Never`, set explicitly in [`Client::new`]), neither to another
    /// server nor back to the same one.
    #[test]
    fn redirects_drop_the_authorization_header() {
        use crate::mock::{MockServer, Route};
        let other = MockServer::start(0, vec![Route::json("GET", "/landing", "{\"ok\":true}")])
            .expect("mock");
        let api = MockServer::start(
            0,
            vec![
                Route::json("GET", "/away", "")
                    .status(302)
                    .header("Location", &format!("{}/landing", other.url())),
                Route::json("GET", "/here", "")
                    .status(307)
                    .header("Location", "/same"),
                Route::json("GET", "/same", "{\"ok\":true}"),
            ],
        )
        .expect("mock");
        let c = Client::new(
            api.url(),
            "github.com".into(),
            ForgeKind::Github,
            Token::new("s3cret"),
        );
        assert_eq!(c.get_json("/away").expect("followed")["ok"], true);
        assert_eq!(c.get_json("/here").expect("followed")["ok"], true);
        let first = api.requests();
        assert!(first
            .iter()
            .filter(|r| r.target != "/same")
            .all(|r| r.header("authorization") == Some("Bearer s3cret")));
        let landed = other.requests();
        assert_eq!(landed.len(), 1);
        assert_eq!(landed[0].header("authorization"), None);
        let same = first.iter().find(|r| r.target == "/same").expect("same");
        assert_eq!(same.header("authorization"), None);
    }

    /// The token is never sent over plain HTTP to a non-loopback host.
    #[test]
    fn tokens_need_https() {
        let c = Client::new(
            "http://code.corp/api/v3".into(),
            "code.corp".into(),
            ForgeKind::Github,
            Token::new("secret"),
        );
        match c.get("/user") {
            Err(ForgeError::Unsupported(m)) => assert!(m.contains("plain HTTP"), "{m}"),
            other => panic!("expected a refusal, got {:?}", other.map(|r| r.json)),
        }
        assert!(matches!(
            c.send("POST", "/x", None),
            Err(ForgeError::Unsupported(_))
        ));
        // A bracketed IPv6 loopback API (`[forge.api_urls]`) is allowed; a look-alike is not.
        assert!(c.check_transport("http://[::1]:9000/api/v4/user").is_ok());
        assert!(c
            .check_transport("http://127.0.0.1@evil.example/user")
            .is_err());
    }

    #[test]
    fn tokens_go_over_https_or_to_this_machine() {
        for url in [
            "https://github.com/a/b.git",
            "HTTPS://code.corp/x",
            "http://127.0.0.1:9/a/b.git",
            "http://[::1]:9000/a",
            "http://localhost/a",
        ] {
            assert!(may_carry_token(url), "{url}");
        }
        for url in [
            "http://code.corp/a/b.git",
            "http://127.0.0.1@evil.example/a",
            "http://localhost.evil.example/a",
            "git@github.com:a/b.git",
            "ssh://git@github.com/a/b",
            "ftp://127.0.0.1/a",
            "",
        ] {
            assert!(!may_carry_token(url), "{url}");
        }
    }

    /// Step 8 review: authorities are parsed (userinfo, bracketed IPv6, port); look-alikes are
    /// not loopback.
    #[test]
    fn loopback_authorities() {
        for url in [
            "http://127.0.0.1:8080/api",
            "http://127.0.0.1",
            "http://127.1.2.3:9/x",
            "http://localhost:9000/api/v4",
            "http://LOCALHOST/",
            "http://[::1]:9000/api",
            "http://[::1]/",
            "http://user:pw@127.0.0.1:9/",
            "http://127.0.0.1?x",
            "http://localhost#f",
        ] {
            assert!(is_loopback_url(url), "{url}");
        }
        for url in [
            "https://api.github.com",
            "http://127.0.0.1.evil.example/",
            "http://localhost.evil.example:80/",
            "http://127.0.0.1@evil.example/",
            "http://localhost@evil.example/",
            "http://[::1]@evil.example/",
            "http://evil.example/127.0.0.1",
            "http://evil.example:127/",
            "http://[::2]:9000/",
            "http://[::1/",
            "http://[::1]x/",
            "http://127.0.0.1:port/",
            "http://128.0.0.1/",
            "http:///127.0.0.1",
            "127.0.0.1",
            "",
        ] {
            assert!(!is_loopback_url(url), "{url}");
        }
    }

    #[test]
    fn links_and_encoding() {
        let l = r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=5>; rel="last""#;
        assert_eq!(
            next_link(l).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
        assert_eq!(next_link(r#"<a>; rel="prev""#), None);
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(encode_segment("group/sub proj"), "group%2Fsub%20proj");
        assert!(is_loopback_url("http://127.0.0.1:8080/api"));
        assert!(!is_loopback_url("https://api.github.com"));
    }
}
