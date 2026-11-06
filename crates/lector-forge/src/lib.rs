//! Code host adapters for Lector (v0.6, Agent FORGE). See docs/ARCHITECTURE.md, "lector-forge".
//!
//! UI-free and blocking (call from a background thread): HTTP goes through `ureq`
//! (rustls with the platform's certificate verifier, proxies from `HTTPS_PROXY` /
//! `HTTP_PROXY` / `ALL_PROXY` / `NO_PROXY`), no async runtime.
//!
//! * [`detect`] / [`detect_with`] recognise a git remote URL as a GitHub (github.com or
//!   Enterprise) or GitLab (gitlab.com or self-hosted) repository → [`ForgeRemote`].
//! * [`auth_token`] finds a token: environment, the `gh` / `glab` CLIs, the OS keychain.
//!   Tokens never appear in logs or `Debug` output.
//! * [`connect`] returns a [`Forge`] (GitHub REST v3 + GraphQL for review threads,
//!   GitLab REST v4): list pull/merge requests, details, review threads mapped to
//!   (path, 0-based line, side), submit a review, reply, resolve.
//! * [`mock`] is a tiny `std::net` HTTP server serving recorded JSON (tests, screenshots).
#![forbid(unsafe_code)]

mod auth;
mod github;
mod gitlab;
mod http;
pub mod mock;
mod remote;
mod time;

use serde::{Deserialize, Serialize};

pub use crate::auth::{
    auth_token, auth_token_with, delete_token, store_token, token_help, Token, TokenSource,
    TokenSources,
};
pub use crate::remote::{detect, detect_with, parse_remote_url, ForgeHosts, ForgeRemote};
pub use crate::time::parse_rfc3339;

/// Which code host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeKind {
    /// github.com or GitHub Enterprise Server.
    Github,
    /// gitlab.com or a self-hosted GitLab.
    Gitlab,
}

impl ForgeKind {
    /// "GitHub" / "GitLab".
    pub fn label(self) -> &'static str {
        match self {
            ForgeKind::Github => "GitHub",
            ForgeKind::Gitlab => "GitLab",
        }
    }

    /// "pull request" / "merge request".
    pub fn request_noun(self) -> &'static str {
        match self {
            ForgeKind::Github => "pull request",
            ForgeKind::Gitlab => "merge request",
        }
    }
}

/// Errors from a code host.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ForgeError {
    /// No or invalid token (HTTP 401) or missing scope.
    #[error("{host}: authentication failed ({message}). {help}")]
    Unauthorized {
        /// Host.
        host: String,
        /// Server message.
        message: String,
        /// How to set up a token.
        help: String,
    },
    /// Rate limit exhausted.
    #[error("{host}: API rate limit exceeded{}", reset_label(*.reset))]
    RateLimited {
        /// Host.
        host: String,
        /// Unix seconds when the limit resets, if known.
        reset: Option<i64>,
    },
    /// HTTP 404 (or a private repository without access).
    #[error("{0}: not found (or no access)")]
    NotFound(String),
    /// Any other HTTP error status.
    #[error("HTTP {status}: {message}")]
    Http {
        /// Status code.
        status: u16,
        /// Server message.
        message: String,
    },
    /// Connection / TLS / proxy failure.
    #[error("network: {0}")]
    Network(String),
    /// Unexpected response shape.
    #[error("unexpected response: {0}")]
    Decode(String),
    /// Not supported by this host.
    #[error("{0}")]
    Unsupported(String),
}

fn reset_label(reset: Option<i64>) -> String {
    match reset {
        Some(t) => {
            let now = time::now_secs();
            let mins = ((t - now).max(0) + 59) / 60;
            format!(" (resets in {mins} min)")
        }
        None => String::new(),
    }
}

/// Result alias.
pub type Result<T> = std::result::Result<T, ForgeError>;

/// Pull request state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    /// Open.
    #[default]
    Open,
    /// Closed without merging.
    Closed,
    /// Merged.
    Merged,
}

/// Which pull requests to list (always open ones).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PrFilter {
    /// Every open one.
    #[default]
    All,
    /// Authored by the signed-in user.
    Mine,
    /// The signed-in user is a requested reviewer.
    ReviewRequested,
}

impl PrFilter {
    /// Parses "all" / "mine" / "review_requested" (settings).
    pub fn parse(s: &str) -> Option<PrFilter> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "all" => Some(PrFilter::All),
            "mine" => Some(PrFilter::Mine),
            "review_requested" | "review" | "requested" => Some(PrFilter::ReviewRequested),
            _ => None,
        }
    }
}

/// Combined status of CI checks of the head commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksState {
    /// All passed.
    Success,
    /// Some failed.
    Failure,
    /// Still running / queued.
    Pending,
}

/// A changed file of a pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PrFile {
    /// New path.
    pub path: String,
    /// Previous path for renames.
    pub old_path: Option<String>,
    /// added / modified / removed / renamed.
    pub status: String,
    /// Added lines.
    pub additions: u32,
    /// Removed lines.
    pub deletions: u32,
}

/// A pull request (GitHub) or merge request (GitLab).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PullRequest {
    /// Number (GitHub) / iid (GitLab).
    pub number: u64,
    /// Title.
    pub title: String,
    /// Description (markdown).
    pub body: String,
    /// Author login.
    pub author: String,
    /// State.
    pub state: PrState,
    /// Draft / work in progress.
    pub draft: bool,
    /// Head commit.
    pub head_sha: String,
    /// Base commit (tip of the target branch the host knows about).
    pub base_sha: String,
    /// Source branch.
    pub head_ref: String,
    /// Target branch.
    pub base_ref: String,
    /// Web URL.
    pub url: String,
    /// Unix seconds.
    pub created: i64,
    /// Unix seconds.
    pub updated: i64,
    /// Requested reviewers (logins).
    pub reviewers: Vec<String>,
    /// Labels.
    pub labels: Vec<String>,
    /// Changed files (filled by [`Forge::pull_request`]).
    pub files: Vec<PrFile>,
    /// CI summary of the head commit (filled by [`Forge::pull_request`] when cheap).
    pub checks: Option<ChecksState>,
    /// Comment count, when listed.
    pub comments: u32,
}

/// Diff side of a review comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// Old / removed lines ("LEFT").
    Old,
    /// New / context lines ("RIGHT").
    #[default]
    New,
}

/// One comment of a review thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RemoteComment {
    /// Host id.
    pub id: String,
    /// Author login.
    pub author: String,
    /// Markdown body.
    pub body: String,
    /// Unix seconds.
    pub created: i64,
}

/// An inline review thread (GitHub review thread / GitLab diff discussion).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ReviewThread {
    /// Host id (GitHub GraphQL node id / GitLab discussion id).
    pub id: String,
    /// Repo-relative path.
    pub path: String,
    /// 0-based line on `side` in the current head, `None` when outdated.
    pub line: Option<u32>,
    /// 0-based first line of a multi-line comment.
    pub start_line: Option<u32>,
    /// 0-based line in the commit the comment was written on.
    pub original_line: Option<u32>,
    /// Side.
    pub side: Side,
    /// Resolved.
    pub resolved: bool,
    /// The diff moved on (GitHub `isOutdated`).
    pub outdated: bool,
    /// The viewer may resolve it.
    pub can_resolve: bool,
    /// Comments, oldest first.
    pub comments: Vec<RemoteComment>,
}

/// Review verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReviewEvent {
    /// Approve.
    Approve,
    /// Request changes.
    RequestChanges,
    /// Comment only.
    #[default]
    Comment,
}

impl ReviewEvent {
    /// Human label.
    pub fn label(self) -> &'static str {
        match self {
            ReviewEvent::Approve => "Approve",
            ReviewEvent::RequestChanges => "Request changes",
            ReviewEvent::Comment => "Comment",
        }
    }
}

/// An inline comment of a review being submitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DraftComment {
    /// Repo-relative path.
    pub path: String,
    /// Path on the old side, when renamed (GitLab positions need it).
    pub old_path: Option<String>,
    /// 0-based (last) line.
    pub line: u32,
    /// 0-based first line of a range.
    pub start_line: Option<u32>,
    /// Side.
    pub side: Side,
    /// Markdown body.
    pub body: String,
}

/// A review to submit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReviewSubmission {
    /// Verdict.
    pub event: ReviewEvent,
    /// Summary body.
    pub body: String,
    /// Inline comments.
    pub comments: Vec<DraftComment>,
    /// Head commit reviewed (comments are positioned against it).
    pub head_sha: String,
    /// Base commit (GitLab positions).
    pub base_sha: String,
    /// Merge-base / start commit (GitLab positions).
    pub start_sha: String,
}

/// Result of [`Forge::submit_review`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SubmittedReview {
    /// Review id (GitHub) or empty.
    pub id: String,
    /// Remote thread id per submitted inline comment, when the host returns it.
    pub threads: Vec<Option<String>>,
    /// Notes about parts the host does not support (shown to the user).
    pub notes: Vec<String>,
}

/// A code host API for one repository.
pub trait Forge: Send + Sync {
    /// Host kind.
    fn kind(&self) -> ForgeKind;
    /// The repository.
    fn remote(&self) -> &ForgeRemote;
    /// Login of the token's user.
    fn viewer(&self) -> Result<String>;
    /// Open pull requests matching `filter`, newest update first.
    fn list_pull_requests(&self, filter: PrFilter) -> Result<Vec<PullRequest>>;
    /// One pull request with files and a checks summary.
    fn pull_request(&self, number: u64) -> Result<PullRequest>;
    /// Inline review threads.
    fn review_threads(&self, number: u64) -> Result<Vec<ReviewThread>>;
    /// Submits a review (irreversible and public).
    fn submit_review(&self, number: u64, review: &ReviewSubmission) -> Result<SubmittedReview>;
    /// Replies to a thread.
    fn reply(&self, number: u64, thread: &str, body: &str) -> Result<RemoteComment>;
    /// Resolves (or unresolves) a thread.
    fn resolve_thread(&self, number: u64, thread: &str, resolved: bool) -> Result<()>;
    /// Refspecs fetching the pull request's head (and base) into `refs/lector/pr/<n>/*`.
    fn fetch_refspecs(&self, pr: &PullRequest) -> Vec<String> {
        let n = pr.number;
        let head = match self.kind() {
            ForgeKind::Github => format!("refs/pull/{n}/head"),
            ForgeKind::Gitlab => format!("refs/merge-requests/{n}/head"),
        };
        let mut v = vec![format!("+{head}:refs/lector/pr/{n}/head")];
        if !pr.base_ref.is_empty() {
            v.push(format!(
                "+refs/heads/{}:refs/lector/pr/{n}/base",
                pr.base_ref
            ));
        }
        v
    }
}

/// Connects to the remote's API with `token`.
pub fn connect(remote: &ForgeRemote, token: Token) -> Box<dyn Forge> {
    let client = http::Client::new(
        remote.api_base.clone(),
        remote.host.clone(),
        remote.kind,
        token,
    );
    match remote.kind {
        ForgeKind::Github => Box::new(github::GitHub::new(remote.clone(), client)),
        ForgeKind::Gitlab => Box::new(gitlab::GitLab::new(remote.clone(), client)),
    }
}

/// The `Authorization` header git should send to fetch over HTTPS with `token`
/// (`Basic` with `x-access-token` / `oauth2`), for `ShadowRepo::fetch_with`.
pub fn git_auth_header(kind: ForgeKind, token: &Token) -> String {
    let user = match kind {
        ForgeKind::Github => "x-access-token",
        ForgeKind::Gitlab => "oauth2",
    };
    format!(
        "Authorization: Basic {}",
        http::base64(format!("{user}:{}", token.secret()).as_bytes())
    )
}

/// Review key of a pull request for `lector_app::comments` (`pr:<host>/<path>#<n>`).
pub fn review_key(remote: &ForgeRemote, number: u64) -> String {
    format!("pr:{}/{}#{number}", remote.host, remote.path)
}

/// Parses a key made by [`review_key`] into (host/path, number).
pub fn parse_review_key(key: &str) -> Option<(&str, u64)> {
    let rest = key.strip_prefix("pr:")?;
    let (repo, n) = rest.rsplit_once('#')?;
    Some((repo, n.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip() {
        let r = detect("git@github.com:acme/widgets.git").expect("github");
        let k = review_key(&r, 12);
        assert_eq!(k, "pr:github.com/acme/widgets#12");
        assert_eq!(parse_review_key(&k), Some(("github.com/acme/widgets", 12)));
    }

    #[test]
    fn auth_header_is_basic() {
        let h = git_auth_header(ForgeKind::Github, &Token::new("abc"));
        assert_eq!(h, "Authorization: Basic eC1hY2Nlc3MtdG9rZW46YWJj");
    }

    #[test]
    fn filters_parse() {
        assert_eq!(
            PrFilter::parse("review-requested"),
            Some(PrFilter::ReviewRequested)
        );
        assert_eq!(PrFilter::parse("Mine"), Some(PrFilter::Mine));
        assert_eq!(PrFilter::parse("x"), None);
    }
}
