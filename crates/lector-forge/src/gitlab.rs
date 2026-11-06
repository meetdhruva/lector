//! GitLab (gitlab.com and self-hosted): REST v4 merge requests and discussions.

use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::http::{encode_segment, Client};
use crate::time::parse_rfc3339;
use crate::{
    ChecksState, Forge, ForgeError, ForgeKind, ForgeRemote, PrFile, PrFilter, PrState, PullRequest,
    RemoteComment, Result, ReviewEvent, ReviewSubmission, ReviewThread, Side, SubmittedReview,
};

pub(crate) struct GitLab {
    remote: ForgeRemote,
    client: Client,
    viewer: OnceLock<String>,
}

fn s(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn time(v: &Value, key: &str) -> i64 {
    v.get(key)
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .unwrap_or(0)
}

fn line0(v: &Value, key: &str) -> Option<u32> {
    v.get(key)
        .and_then(Value::as_u64)
        .filter(|l| *l > 0)
        .map(|l| (l - 1) as u32)
}

pub(crate) fn mr_from_json(v: &Value) -> PullRequest {
    let state = match s(v, "state").as_str() {
        "merged" => PrState::Merged,
        "closed" | "locked" => PrState::Closed,
        _ => PrState::Open,
    };
    let refs = v.get("diff_refs");
    PullRequest {
        number: v.get("iid").and_then(Value::as_u64).unwrap_or(0),
        title: s(v, "title"),
        body: s(v, "description"),
        author: v
            .get("author")
            .map(|a| s(a, "username"))
            .unwrap_or_default(),
        state,
        draft: v.get("draft").and_then(Value::as_bool).unwrap_or(false)
            || v.get("work_in_progress")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        head_sha: refs
            .map(|r| s(r, "head_sha"))
            .filter(|x| !x.is_empty())
            .unwrap_or_else(|| s(v, "sha")),
        base_sha: refs.map(|r| s(r, "base_sha")).unwrap_or_default(),
        head_ref: s(v, "source_branch"),
        base_ref: s(v, "target_branch"),
        url: s(v, "web_url"),
        created: time(v, "created_at"),
        updated: time(v, "updated_at"),
        reviewers: v
            .get("reviewers")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|u| s(u, "username")).collect())
            .unwrap_or_default(),
        labels: v
            .get("labels")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        files: Vec::new(),
        checks: v
            .get("head_pipeline")
            .and_then(|p| match s(p, "status").as_str() {
                "success" => Some(ChecksState::Success),
                "failed" | "canceled" => Some(ChecksState::Failure),
                "" => None,
                _ => Some(ChecksState::Pending),
            }),
        comments: v
            .get("user_notes_count")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
    }
}

fn note_comment(n: &Value) -> RemoteComment {
    RemoteComment {
        id: s(n, "id"),
        author: n
            .get("author")
            .map(|a| s(a, "username"))
            .unwrap_or_default(),
        body: s(n, "body"),
        created: time(n, "created_at"),
    }
}

/// A diff discussion → thread; `None` for general / system discussions.
pub(crate) fn thread_from_discussion(d: &Value) -> Option<ReviewThread> {
    let notes: Vec<&Value> = d
        .get("notes")?
        .as_array()?
        .iter()
        .filter(|n| n.get("system").and_then(Value::as_bool) != Some(true))
        .collect();
    let first = notes.first()?;
    let pos = first.get("position").filter(|p| !p.is_null())?;
    let (side, line, path) = match line0(pos, "new_line") {
        Some(l) => (Side::New, Some(l), s(pos, "new_path")),
        None => (Side::Old, line0(pos, "old_line"), s(pos, "old_path")),
    };
    let start_line = pos
        .pointer("/line_range/start")
        .and_then(|st| line0(st, "new_line").or_else(|| line0(st, "old_line")))
        .filter(|st| Some(*st) != line);
    Some(ReviewThread {
        id: s(d, "id"),
        path,
        line,
        start_line,
        original_line: line,
        side,
        resolved: first
            .get("resolved")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        outdated: false,
        can_resolve: first
            .get("resolvable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        comments: notes.iter().map(|n| note_comment(n)).collect(),
    })
}

impl GitLab {
    pub(crate) fn new(remote: ForgeRemote, client: Client) -> GitLab {
        GitLab {
            remote,
            client,
            viewer: OnceLock::new(),
        }
    }

    fn project(&self) -> String {
        format!("/projects/{}", encode_segment(&self.remote.path))
    }
}

impl Forge for GitLab {
    fn kind(&self) -> ForgeKind {
        ForgeKind::Gitlab
    }

    fn remote(&self) -> &ForgeRemote {
        &self.remote
    }

    fn viewer(&self) -> Result<String> {
        if let Some(v) = self.viewer.get() {
            return Ok(v.clone());
        }
        let login = s(&self.client.get_json("/user")?, "username");
        if login.is_empty() {
            return Err(ForgeError::Decode("/user without username".into()));
        }
        Ok(self.viewer.get_or_init(|| login).clone())
    }

    fn list_pull_requests(&self, filter: PrFilter) -> Result<Vec<PullRequest>> {
        let mut path = format!(
            "{}/merge_requests?state=opened&order_by=updated_at&sort=desc&per_page=50",
            self.project()
        );
        match filter {
            PrFilter::All => {}
            PrFilter::Mine => path.push_str(&format!(
                "&author_username={}",
                encode_segment(&self.viewer()?)
            )),
            PrFilter::ReviewRequested => path.push_str(&format!(
                "&reviewer_username={}",
                encode_segment(&self.viewer()?)
            )),
        }
        let items = self.client.get_paged(&path)?;
        Ok(items.iter().map(mr_from_json).collect())
    }

    fn pull_request(&self, number: u64) -> Result<PullRequest> {
        let base = format!("{}/merge_requests/{number}", self.project());
        let mut pr = mr_from_json(&self.client.get_json(&base)?);
        // `/diffs` (GitLab 15.7+); older servers: `/changes`.
        let files: Vec<Value> = match self.client.get_paged(&format!("{base}/diffs?per_page=100")) {
            Ok(v) => v,
            Err(ForgeError::NotFound(_)) => self
                .client
                .get_json(&format!("{base}/changes"))?
                .get("changes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            Err(e) => return Err(e),
        };
        pr.files = files
            .iter()
            .map(|f| {
                let diff = s(f, "diff");
                let (mut add, mut del) = (0, 0);
                for l in diff.lines() {
                    if l.starts_with('+') && !l.starts_with("+++") {
                        add += 1;
                    } else if l.starts_with('-') && !l.starts_with("---") {
                        del += 1;
                    }
                }
                let status = if f.get("new_file") == Some(&json!(true)) {
                    "added"
                } else if f.get("deleted_file") == Some(&json!(true)) {
                    "removed"
                } else if f.get("renamed_file") == Some(&json!(true)) {
                    "renamed"
                } else {
                    "modified"
                };
                let old = s(f, "old_path");
                let new = s(f, "new_path");
                PrFile {
                    old_path: (old != new && !old.is_empty()).then_some(old),
                    path: new,
                    status: status.into(),
                    additions: add,
                    deletions: del,
                }
            })
            .collect();
        Ok(pr)
    }

    fn review_threads(&self, number: u64) -> Result<Vec<ReviewThread>> {
        let items = self.client.get_paged(&format!(
            "{}/merge_requests/{number}/discussions?per_page=100",
            self.project()
        ))?;
        Ok(items.iter().filter_map(thread_from_discussion).collect())
    }

    fn submit_review(&self, number: u64, review: &ReviewSubmission) -> Result<SubmittedReview> {
        let base = format!("{}/merge_requests/{number}", self.project());
        let mut out = SubmittedReview::default();
        for c in &review.comments {
            let mut position = json!({
                "position_type": "text",
                "base_sha": review.base_sha,
                "start_sha": if review.start_sha.is_empty() { &review.base_sha } else { &review.start_sha },
                "head_sha": review.head_sha,
                "new_path": c.path,
                "old_path": c.old_path.as_deref().unwrap_or(&c.path),
            });
            match c.side {
                Side::New => position["new_line"] = json!(c.line + 1),
                Side::Old => position["old_line"] = json!(c.line + 1),
            }
            let d = self.client.send(
                "POST",
                &format!("{base}/discussions"),
                Some(&json!({ "body": c.body, "position": position })),
            )?;
            out.threads
                .push(Some(s(&d, "id")).filter(|x| !x.is_empty()));
        }
        let summary = match review.event {
            ReviewEvent::RequestChanges if review.body.trim().is_empty() => {
                "Changes requested.".to_string()
            }
            _ => review.body.clone(),
        };
        if !summary.trim().is_empty() {
            self.client.send(
                "POST",
                &format!("{base}/notes"),
                Some(&json!({ "body": summary })),
            )?;
        }
        match review.event {
            ReviewEvent::Approve => {
                let mut body = json!({});
                if !review.head_sha.is_empty() {
                    body["sha"] = json!(review.head_sha);
                }
                self.client
                    .send("POST", &format!("{base}/approve"), Some(&body))?;
            }
            ReviewEvent::RequestChanges => out.notes.push(
                "GitLab has no \"request changes\" verdict through the API: the summary and comments were posted, the merge request was not approved.".into(),
            ),
            ReviewEvent::Comment => {}
        }
        Ok(out)
    }

    fn reply(&self, number: u64, thread: &str, body: &str) -> Result<RemoteComment> {
        let v = self.client.send(
            "POST",
            &format!(
                "{}/merge_requests/{number}/discussions/{}/notes",
                self.project(),
                encode_segment(thread)
            ),
            Some(&json!({ "body": body })),
        )?;
        Ok(note_comment(&v))
    }

    fn resolve_thread(&self, number: u64, thread: &str, resolved: bool) -> Result<()> {
        self.client.send(
            "PUT",
            &format!(
                "{}/merge_requests/{number}/discussions/{}?resolved={resolved}",
                self.project(),
                encode_segment(thread)
            ),
            None,
        )?;
        Ok(())
    }
}
