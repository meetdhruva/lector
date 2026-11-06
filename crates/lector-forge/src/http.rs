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

fn is_loopback(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split(['/', ':']).next().unwrap_or("");
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
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
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            );
        if is_loopback(&base) {
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
                    ForgeKind::Github => "Set GITHUB_TOKEN or run `gh auth login`.".into(),
                    ForgeKind::Gitlab => "Set GITLAB_TOKEN or run `glab auth login`.".into(),
                },
            },
            404 => ForgeError::NotFound(what.to_string()),
            _ => ForgeError::Http { status, message },
        })
    }

    pub(crate) fn get(&self, path: &str) -> Result<Response> {
        let req = self.with_headers(self.agent.get(&self.url(path)));
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
        assert!(is_loopback("http://127.0.0.1:8080/api"));
        assert!(!is_loopback("https://api.github.com"));
    }
}
