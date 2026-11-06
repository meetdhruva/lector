//! GitHub (github.com and Enterprise Server): REST v3, plus GraphQL for review
//! threads (resolution state, replies, resolving are GraphQL-only).

use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::http::Client;
use crate::time::parse_rfc3339;
use crate::{
    ChecksState, Forge, ForgeError, ForgeKind, ForgeRemote, PrFile, PrFilter, PrState, PullRequest,
    RemoteComment, Result, ReviewEvent, ReviewSubmission, ReviewThread, Side, SubmittedReview,
};

pub(crate) struct GitHub {
    remote: ForgeRemote,
    client: Client,
    viewer: OnceLock<String>,
}

fn s(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn time(v: &Value, key: &str) -> i64 {
    v.get(key)
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .unwrap_or(0)
}

/// 1-based API line → 0-based.
fn line0(v: &Value, key: &str) -> Option<u32> {
    v.get(key)
        .and_then(Value::as_u64)
        .filter(|l| *l > 0)
        .map(|l| (l - 1) as u32)
}

pub(crate) fn pull_from_json(v: &Value) -> PullRequest {
    let state = if v.get("merged_at").is_some_and(|m| !m.is_null())
        || v.get("merged") == Some(&json!(true))
    {
        PrState::Merged
    } else if s(v, "state") == "closed" {
        PrState::Closed
    } else {
        PrState::Open
    };
    PullRequest {
        number: v.get("number").and_then(Value::as_u64).unwrap_or(0),
        title: s(v, "title"),
        body: s(v, "body"),
        author: v.get("user").map(|u| s(u, "login")).unwrap_or_default(),
        state,
        draft: v.get("draft").and_then(Value::as_bool).unwrap_or(false),
        head_sha: v.get("head").map(|h| s(h, "sha")).unwrap_or_default(),
        base_sha: v.get("base").map(|h| s(h, "sha")).unwrap_or_default(),
        head_ref: v.get("head").map(|h| s(h, "ref")).unwrap_or_default(),
        base_ref: v.get("base").map(|h| s(h, "ref")).unwrap_or_default(),
        url: s(v, "html_url"),
        created: time(v, "created_at"),
        updated: time(v, "updated_at"),
        reviewers: v
            .get("requested_reviewers")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|u| s(u, "login")).collect())
            .unwrap_or_default(),
        labels: v
            .get("labels")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|l| s(l, "name")).collect())
            .unwrap_or_default(),
        files: Vec::new(),
        checks: None,
        comments: (v.get("comments").and_then(Value::as_u64).unwrap_or(0)
            + v.get("review_comments")
                .and_then(Value::as_u64)
                .unwrap_or(0)) as u32,
    }
}

const THREADS_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id isResolved isOutdated viewerCanResolve path diffSide
          line startLine originalLine originalStartLine
          comments(first: 100) { nodes { id databaseId body createdAt author { login } } }
        }
      }
    }
  }
}"#;

const REPLY_MUTATION: &str = r#"mutation($thread: ID!, $body: String!) {
  addPullRequestReviewThreadReply(input: { pullRequestReviewThreadId: $thread, body: $body }) {
    comment { id databaseId body createdAt author { login } }
  }
}"#;

const RESOLVE_MUTATION: &str = r#"mutation($thread: ID!) {
  resolveReviewThread(input: { threadId: $thread }) { thread { id isResolved } }
}"#;

const UNRESOLVE_MUTATION: &str = r#"mutation($thread: ID!) {
  unresolveReviewThread(input: { threadId: $thread }) { thread { id isResolved } }
}"#;

fn comment_from_graphql(c: &Value) -> RemoteComment {
    RemoteComment {
        id: c
            .get("databaseId")
            .and_then(Value::as_u64)
            .map(|d| d.to_string())
            .unwrap_or_else(|| s(c, "id")),
        author: c.get("author").map(|a| s(a, "login")).unwrap_or_default(),
        body: s(c, "body"),
        created: time(c, "createdAt"),
    }
}

pub(crate) fn thread_from_graphql(t: &Value) -> ReviewThread {
    let outdated = t
        .get("isOutdated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    ReviewThread {
        id: s(t, "id"),
        path: s(t, "path"),
        line: line0(t, "line"),
        start_line: line0(t, "startLine").or_else(|| line0(t, "originalStartLine")),
        original_line: line0(t, "originalLine"),
        side: if s(t, "diffSide") == "LEFT" {
            Side::Old
        } else {
            Side::New
        },
        resolved: t
            .get("isResolved")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        outdated,
        can_resolve: t
            .get("viewerCanResolve")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        comments: t
            .pointer("/comments/nodes")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(comment_from_graphql).collect())
            .unwrap_or_default(),
    }
}

impl GitHub {
    pub(crate) fn new(remote: ForgeRemote, client: Client) -> GitHub {
        GitHub {
            remote,
            client,
            viewer: OnceLock::new(),
        }
    }

    fn repo_path(&self) -> String {
        format!("/repos/{}/{}", self.remote.owner, self.remote.name)
    }
}

impl Forge for GitHub {
    fn kind(&self) -> ForgeKind {
        ForgeKind::Github
    }

    fn remote(&self) -> &ForgeRemote {
        &self.remote
    }

    fn viewer(&self) -> Result<String> {
        if let Some(v) = self.viewer.get() {
            return Ok(v.clone());
        }
        let login = s(&self.client.get_json("/user")?, "login");
        if login.is_empty() {
            return Err(ForgeError::Decode("/user without login".into()));
        }
        Ok(self.viewer.get_or_init(|| login).clone())
    }

    fn list_pull_requests(&self, filter: PrFilter) -> Result<Vec<PullRequest>> {
        let items = self.client.get_paged(&format!(
            "{}/pulls?state=open&sort=updated&direction=desc&per_page=50",
            self.repo_path()
        ))?;
        let mut prs: Vec<PullRequest> = items.iter().map(pull_from_json).collect();
        match filter {
            PrFilter::All => {}
            PrFilter::Mine => {
                let me = self.viewer()?;
                prs.retain(|p| p.author.eq_ignore_ascii_case(&me));
            }
            PrFilter::ReviewRequested => {
                let me = self.viewer()?;
                prs.retain(|p| p.reviewers.iter().any(|r| r.eq_ignore_ascii_case(&me)));
            }
        }
        prs.sort_by_key(|p| std::cmp::Reverse(p.updated));
        Ok(prs)
    }

    fn pull_request(&self, number: u64) -> Result<PullRequest> {
        let base = self.repo_path();
        let mut pr = pull_from_json(&self.client.get_json(&format!("{base}/pulls/{number}"))?);
        let files = self
            .client
            .get_paged(&format!("{base}/pulls/{number}/files?per_page=100"))?;
        pr.files = files
            .iter()
            .map(|f| PrFile {
                path: s(f, "filename"),
                old_path: f
                    .get("previous_filename")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                status: s(f, "status"),
                additions: f.get("additions").and_then(Value::as_u64).unwrap_or(0) as u32,
                deletions: f.get("deletions").and_then(Value::as_u64).unwrap_or(0) as u32,
            })
            .collect();
        // Combined commit status: one cheap request; absent checks are fine.
        if !pr.head_sha.is_empty() {
            if let Ok(st) = self
                .client
                .get_json(&format!("{base}/commits/{}/status", pr.head_sha))
            {
                let total = st.get("total_count").and_then(Value::as_u64).unwrap_or(0);
                pr.checks = (total > 0).then(|| match s(&st, "state").as_str() {
                    "success" => ChecksState::Success,
                    "failure" | "error" => ChecksState::Failure,
                    _ => ChecksState::Pending,
                });
            }
        }
        Ok(pr)
    }

    fn review_threads(&self, number: u64) -> Result<Vec<ReviewThread>> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..crate::http::MAX_PAGES {
            let data = self.client.graphql(
                &self.remote.graphql_url(),
                THREADS_QUERY,
                json!({
                    "owner": self.remote.owner, "name": self.remote.name,
                    "number": number, "after": after,
                }),
            )?;
            let threads = data
                .pointer("/repository/pullRequest/reviewThreads")
                .ok_or_else(|| ForgeError::NotFound(format!("pull request #{number}")))?;
            if let Some(nodes) = threads.get("nodes").and_then(Value::as_array) {
                out.extend(nodes.iter().map(thread_from_graphql));
            }
            let more = threads
                .pointer("/pageInfo/hasNextPage")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            after = threads
                .pointer("/pageInfo/endCursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            if !more || after.is_none() {
                break;
            }
        }
        Ok(out)
    }

    fn submit_review(&self, number: u64, review: &ReviewSubmission) -> Result<SubmittedReview> {
        let event = match review.event {
            ReviewEvent::Approve => "APPROVE",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
            ReviewEvent::Comment => "COMMENT",
        };
        let comments: Vec<Value> = review
            .comments
            .iter()
            .map(|c| {
                let side = match c.side {
                    Side::Old => "LEFT",
                    Side::New => "RIGHT",
                };
                let mut v = json!({
                    "path": c.path, "line": c.line + 1, "side": side, "body": c.body,
                });
                if let Some(start) = c.start_line.filter(|s| *s < c.line) {
                    v["start_line"] = json!(start + 1);
                    v["start_side"] = json!(side);
                }
                v
            })
            .collect();
        let mut body = json!({ "event": event, "body": review.body, "comments": comments });
        if !review.head_sha.is_empty() {
            body["commit_id"] = json!(review.head_sha);
        }
        let v = self.client.send(
            "POST",
            &format!("{}/pulls/{number}/reviews", self.repo_path()),
            Some(&body),
        )?;
        Ok(SubmittedReview {
            id: v
                .get("id")
                .map(|i| i.to_string().trim_matches('"').to_string())
                .unwrap_or_default(),
            threads: vec![None; review.comments.len()],
            notes: Vec::new(),
        })
    }

    fn reply(&self, _number: u64, thread: &str, body: &str) -> Result<RemoteComment> {
        let data = self.client.graphql(
            &self.remote.graphql_url(),
            REPLY_MUTATION,
            json!({ "thread": thread, "body": body }),
        )?;
        data.pointer("/addPullRequestReviewThreadReply/comment")
            .map(comment_from_graphql)
            .ok_or_else(|| ForgeError::Decode("reply without comment".into()))
    }

    fn resolve_thread(&self, _number: u64, thread: &str, resolved: bool) -> Result<()> {
        let q = if resolved {
            RESOLVE_MUTATION
        } else {
            UNRESOLVE_MUTATION
        };
        self.client
            .graphql(&self.remote.graphql_url(), q, json!({ "thread": thread }))?;
        Ok(())
    }
}
