//! Review comments that survive new revisions (v0.6, Agent FORGE).
//!
//! A [`Thread`] is anchored to a line (or a range of lines) of a file on one side of a
//! review: the line's text, a few lines of context, the review key (a changeset key
//! such as `worktree` / `branch:main...feature`, or `pr:<host>/<repo>#12`) and the
//! revision of the file it was written against (git blob id when known, plus a content
//! hash). When the file changes — new commits, a rebase, an agent turn, edits in the
//! working tree — [`CommentStore::reanchor_path`] / [`CommentStore::sync_changeset`] move
//! every thread with [`reanchor_anchor`]: exact (line map through a line diff), moved
//! (moved-block detection), fuzzy (`lector_diff::remap::reanchor_with`, with a
//! confidence); below the threshold the thread becomes [`ThreadState::Outdated`] and
//! keeps its original snippet.
//!
//! Threads live in the [`Db`] (`comments` table), never in the user's repository.
//! They come from local review ([`Origin::Local`]) or a code host
//! ([`Origin::Github`] / [`Origin::Gitlab`], synced by `lector-forge`); comments can be
//! drafts until a review is submitted.
//!
//! Commands (JSON schemas, registered by [`register_commands`]): `comments.list`,
//! `comments.add`, `comments.reply`, `comments.resolve`, `comments.delete`,
//! `comments.export`. They only write Lector's own database (`writes = false`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use anyhow::{anyhow, bail};
use lector_diff::remap::{self, LineMap, Mapped, ReanchorOptions};
use lector_diff::{diff_lines, DiffOptions};
use lector_git::{Changeset, Oid, Repo, Side as GitSide};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::commands::schema::{object, prop};
use crate::commands::{
    Availability, CommandContext, CommandRegistry, CommandSpec, FnCommand, Outcome,
};
use crate::core::AppCore;
use crate::db::{Db, DbError};

/// Lines of context captured on each side of an anchor.
pub const CONTEXT_LINES: usize = 3;
/// Default minimum confidence to keep a thread attached after re-anchoring.
pub const MIN_CONFIDENCE: f32 = 0.6;

/// Errors of the comment store.
#[derive(Debug, thiserror::Error)]
pub enum CommentError {
    /// Storage failure.
    #[error(transparent)]
    Db(#[from] DbError),
    /// No such thread / comment.
    #[error("comment thread {0} not found")]
    NotFound(u64),
    /// A stored thread could not be decoded.
    #[error("unreadable comment thread: {0}")]
    Decode(#[from] serde_json::Error),
    /// Invalid input (empty body, line out of range...).
    #[error("{0}")]
    Invalid(String),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, CommentError>;

/// Which side of a diff a thread is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DiffSide {
    /// The old side (removed lines, "LEFT" on GitHub).
    Old,
    /// The new side (added / context lines, "RIGHT").
    #[default]
    New,
}

/// Life cycle of a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThreadState {
    /// Needs attention.
    #[default]
    Open,
    /// Done.
    Resolved,
    /// The anchored code is gone (re-anchoring failed); the original snippet is kept.
    Outdated,
}

/// Where a thread comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Written in Lector.
    #[default]
    Local,
    /// A GitHub pull-request review thread.
    Github,
    /// A GitLab merge-request discussion.
    Gitlab,
}

/// How the last re-anchoring found the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnchorMatch {
    /// Where it was created (or not re-anchored yet).
    #[default]
    Original,
    /// The identical line (with its context) was found.
    Exact,
    /// The line moved with a block of code.
    Moved,
    /// A similar line was found (see [`Thread::confidence`]).
    Fuzzy,
    /// Not found: the thread is outdated.
    Lost,
}

/// Where a thread is attached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Anchor {
    /// Repo-relative path ('/' separators).
    pub path: String,
    /// Diff side.
    #[serde(default)]
    pub side: DiffSide,
    /// First line (0-based) of the commented range.
    pub line: u32,
    /// Last line (0-based, inclusive) when a selection was commented; `None` = one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    /// Text of `line` (anchor for re-anchoring).
    #[serde(default)]
    pub text: String,
    /// Up to [`CONTEXT_LINES`] lines before `line`, nearest last.
    #[serde(default)]
    pub context_before: Vec<String>,
    /// Up to [`CONTEXT_LINES`] lines after the commented range, nearest first.
    #[serde(default)]
    pub context_after: Vec<String>,
    /// Review key: a changeset key (`worktree`, `branch:main...feature`, `commit:<sha>`)
    /// or a pull-request key (`pr:github.com/owner/repo#12`).
    #[serde(default)]
    pub changeset: String,
    /// Git blob id of the file revision the anchor was taken from, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    /// Content hash of that revision (see [`content_id`]).
    #[serde(default)]
    pub content: String,
}

impl Anchor {
    /// Captures an anchor on `line` (through `end_line`) of `text`.
    pub fn capture(
        path: &str,
        side: DiffSide,
        text: &str,
        line: u32,
        end_line: Option<u32>,
        changeset: &str,
        blob: Option<String>,
    ) -> Option<Anchor> {
        let base = remap::Anchor::capture(text, line, CONTEXT_LINES)?;
        let end = end_line.filter(|e| *e > line);
        let context_after = match end {
            Some(e) => remap::Anchor::capture(text, e, CONTEXT_LINES)
                .map(|a| a.context_after)
                .unwrap_or_default(),
            None => base.context_after,
        };
        Some(Anchor {
            path: path.to_string(),
            side,
            line,
            end_line: end,
            text: base.text,
            context_before: base.context_before,
            context_after,
            changeset: changeset.to_string(),
            blob,
            content: content_id(text),
        })
    }

    /// The commented lines, `[line, end]` inclusive.
    pub fn lines(&self) -> std::ops::RangeInclusive<u32> {
        self.line..=self.end_line.unwrap_or(self.line).max(self.line)
    }

    fn to_remap(&self) -> remap::Anchor {
        remap::Anchor {
            line: self.line,
            text: self.text.clone(),
            context_before: self.context_before.clone(),
            context_after: self.context_after.clone(),
        }
    }
}

/// One comment of a thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Comment {
    /// Id (unique in the database).
    pub id: u64,
    /// Author name or login.
    #[serde(default)]
    pub author: String,
    /// Markdown body.
    pub body: String,
    /// Unix seconds.
    pub created: i64,
    /// Unix seconds of the last edit.
    #[serde(default)]
    pub updated: i64,
    /// Not published yet (pending review).
    #[serde(default)]
    pub draft: bool,
    /// Id on the code host once published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,
}

/// A comment thread anchored in a file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Thread {
    /// Id (unique in the database).
    pub id: u64,
    /// Repository work directory (canonical).
    pub repo: String,
    /// Current anchor (updated by re-anchoring).
    pub anchor: Anchor,
    /// The anchor as created (its snippet is what an outdated thread shows).
    pub original: Anchor,
    /// State.
    #[serde(default)]
    pub state: ThreadState,
    /// Where it comes from.
    #[serde(default)]
    pub origin: Origin,
    /// Thread / discussion id on the code host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,
    /// Comments, oldest first (never empty for a stored thread).
    pub comments: Vec<Comment>,
    /// How the last re-anchoring went.
    #[serde(default)]
    pub matched: AnchorMatch,
    /// Confidence of the last re-anchoring (1 = exact).
    #[serde(default = "one")]
    pub confidence: f32,
    /// Unix seconds.
    pub created: i64,
    /// Unix seconds of the last change.
    #[serde(default)]
    pub updated: i64,
}

fn one() -> f32 {
    1.0
}

impl Thread {
    /// Whether every comment is a draft (the thread itself is unpublished).
    pub fn is_draft(&self) -> bool {
        !self.comments.is_empty() && self.comments.iter().all(|c| c.draft)
    }

    /// Number of draft comments.
    pub fn draft_count(&self) -> usize {
        self.comments.iter().filter(|c| c.draft).count()
    }

    /// The first comment's body, shortened to one line.
    pub fn summary(&self) -> String {
        let body = self.comments.first().map(|c| c.body.as_str()).unwrap_or("");
        let line = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let mut s: String = line.chars().take(100).collect();
        if line.chars().count() > 100 {
            s.push('…');
        }
        s
    }

    /// 1-based "path:line" (or "path:a-b") label.
    pub fn location(&self) -> String {
        let a = &self.anchor;
        match a.end_line {
            Some(e) if e > a.line => format!("{}:{}-{}", a.path, a.line + 1, e + 1),
            _ => format!("{}:{}", a.path, a.line + 1),
        }
    }

    /// Plain-text rendering for an agent prompt: location, snippet, comments.
    pub fn to_prompt(&self) -> String {
        let mut out = format!(
            "{} ({}{})\n",
            self.location(),
            match self.anchor.side {
                DiffSide::New => "new",
                DiffSide::Old => "old",
            },
            if self.state == ThreadState::Outdated {
                ", outdated"
            } else {
                ""
            }
        );
        out.push_str(&format!("> {}\n", self.original.text.trim_end()));
        for c in &self.comments {
            let who = if c.author.is_empty() {
                "reviewer"
            } else {
                &c.author
            };
            out.push_str(&format!("{who}: {}\n", c.body.trim()));
        }
        out
    }
}

/// A new thread for [`CommentStore::add`].
#[derive(Debug, Clone, Default)]
pub struct NewThread {
    /// Repository work directory.
    pub repo: PathBuf,
    /// Where.
    pub anchor: Anchor,
    /// First comment.
    pub body: String,
    /// Author.
    pub author: String,
    /// Keep as a draft (pending review).
    pub draft: bool,
    /// Origin (local for user comments).
    pub origin: Origin,
    /// Remote thread id (synced threads).
    pub remote_id: Option<String>,
    /// Remote comment id of the first comment (synced threads).
    pub remote_comment_id: Option<String>,
    /// Creation time (0 = now).
    pub created: i64,
    /// Initial state (synced threads may be resolved / outdated).
    pub state: ThreadState,
}

/// Which threads to list.
#[derive(Debug, Clone, Default)]
pub struct ThreadFilter {
    /// Only this review key.
    pub changeset: Option<String>,
    /// Only this repo-relative path.
    pub path: Option<String>,
    /// Only these states (empty = all).
    pub states: Vec<ThreadState>,
    /// Only threads with drafts.
    pub drafts_only: bool,
    /// Only this origin.
    pub origin: Option<Origin>,
}

impl ThreadFilter {
    /// Threads of one review key.
    pub fn changeset(key: impl Into<String>) -> Self {
        ThreadFilter {
            changeset: Some(key.into()),
            ..Default::default()
        }
    }

    fn matches(&self, t: &Thread) -> bool {
        self.changeset
            .as_ref()
            .is_none_or(|c| &t.anchor.changeset == c)
            && self.path.as_ref().is_none_or(|p| &t.anchor.path == p)
            && (self.states.is_empty() || self.states.contains(&t.state))
            && (!self.drafts_only || t.draft_count() > 0)
            && self.origin.is_none_or(|o| t.origin == o)
    }
}

/// What a change notification is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentEvent {
    /// Repository work directory.
    pub repo: String,
    /// The thread, or `None` for bulk changes (re-anchoring, sync).
    pub thread: Option<u64>,
}

type Callback = Arc<dyn Fn(&CommentEvent) + Send + Sync>;

struct Inner {
    db: Db,
    subscribers: Mutex<Vec<(u64, Callback)>>,
    next_sub: AtomicU64,
    write: Mutex<()>,
}

/// Keeps a [`CommentStore::subscribe`] callback alive.
#[must_use = "dropping a Subscription unsubscribes"]
pub struct Subscription {
    id: u64,
    inner: Weak<Inner>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.subscribers.lock().retain(|(id, _)| *id != self.id);
        }
    }
}

/// Outcome of re-anchoring one thread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reanchored {
    /// New line (`None` when lost).
    pub line: Option<u32>,
    /// Confidence in 0..=1.
    pub confidence: f32,
    /// How it was found.
    pub matched: AnchorMatch,
}

/// Report of one thread after [`CommentStore::reanchor_path`].
#[derive(Debug, Clone, PartialEq)]
pub struct ReanchorReport {
    /// Thread id.
    pub thread: u64,
    /// Result.
    pub result: Reanchored,
}

/// The persistent comment store (one per [`AppCore`], see [`CommentStore::for_core`]).
#[derive(Clone)]
pub struct CommentStore {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for CommentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommentStore").finish_non_exhaustive()
    }
}

/// Canonical repository key for a work directory.
pub fn repo_key(workdir: &Path) -> String {
    dunce::canonicalize(workdir)
        .unwrap_or_else(|_| workdir.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Stable content hash of a file revision (FNV-1a 64, hex).
pub fn content_id(text: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn text_cache_key(content: &str) -> String {
    format!("comments/text/{content}")
}

impl CommentStore {
    /// A store over `db`.
    pub fn new(db: Db) -> CommentStore {
        CommentStore {
            inner: Arc::new(Inner {
                db,
                subscribers: Mutex::new(Vec::new()),
                next_sub: AtomicU64::new(1),
                write: Mutex::new(()),
            }),
        }
    }

    /// The core's store (extension slot; shared by the GUI, the CLI and agents).
    pub fn for_core(core: &AppCore) -> Arc<CommentStore> {
        core.extension_or_init(|| Arc::new(CommentStore::new(core.db().clone())))
    }

    /// Calls `callback` after every change (on the thread that made it).
    pub fn subscribe(
        &self,
        callback: impl Fn(&CommentEvent) + Send + Sync + 'static,
    ) -> Subscription {
        let id = self.inner.next_sub.fetch_add(1, Ordering::Relaxed);
        self.inner.subscribers.lock().push((id, Arc::new(callback)));
        Subscription {
            id,
            inner: Arc::downgrade(&self.inner),
        }
    }

    fn notify(&self, repo: &str, thread: Option<u64>) {
        let callbacks: Vec<Callback> = self
            .inner
            .subscribers
            .lock()
            .iter()
            .map(|(_, c)| c.clone())
            .collect();
        let event = CommentEvent {
            repo: repo.to_string(),
            thread,
        };
        for cb in callbacks {
            cb(&event);
        }
    }

    fn load_all(&self, repo: &str) -> Result<Vec<Thread>> {
        self.migrate_legacy(repo)?;
        let mut out = Vec::new();
        for bytes in self.inner.db.comment_threads(repo)? {
            match serde_json::from_slice::<Thread>(&bytes) {
                Ok(t) => out.push(t),
                Err(err) => tracing::warn!("skipping unreadable comment thread: {err}"),
            }
        }
        Ok(out)
    }

    /// v0.4 single comments (`Db::add_comment`) become one-comment threads.
    fn migrate_legacy(&self, repo: &str) -> Result<()> {
        let legacy = self.inner.db.comments(repo, None)?;
        for c in legacy {
            let anchor = Anchor {
                path: c.path.clone(),
                line: c.line,
                text: c.anchor_text.clone(),
                changeset: c.changeset.clone(),
                ..Default::default()
            };
            let thread = Thread {
                id: c.id,
                repo: repo.to_string(),
                anchor: anchor.clone(),
                original: anchor,
                state: if c.resolved {
                    ThreadState::Resolved
                } else {
                    ThreadState::Open
                },
                comments: vec![Comment {
                    id: c.id,
                    body: c.body.clone(),
                    created: c.created,
                    updated: c.created,
                    ..Default::default()
                }],
                confidence: 1.0,
                created: c.created,
                updated: c.created,
                ..Default::default()
            };
            self.save(&thread)?;
            self.inner.db.delete_comment(repo, c.id)?;
        }
        Ok(())
    }

    fn save(&self, t: &Thread) -> Result<()> {
        let bytes = serde_json::to_vec(t)?;
        self.inner
            .db
            .put_comment_thread(&t.repo, t.id, Some(&bytes))?;
        Ok(())
    }

    /// Threads of a repository matching `filter`, ordered by path, line, id.
    pub fn threads(&self, repo: &Path, filter: &ThreadFilter) -> Result<Vec<Thread>> {
        let key = repo_key(repo);
        let mut v: Vec<Thread> = self
            .load_all(&key)?
            .into_iter()
            .filter(|t| filter.matches(t))
            .collect();
        v.sort_by(|a, b| {
            (a.anchor.path.as_str(), a.anchor.line, a.id).cmp(&(
                b.anchor.path.as_str(),
                b.anchor.line,
                b.id,
            ))
        });
        Ok(v)
    }

    /// One thread.
    pub fn thread(&self, repo: &Path, id: u64) -> Result<Thread> {
        let key = repo_key(repo);
        let bytes = self
            .inner
            .db
            .comment_thread(&key, id)?
            .ok_or(CommentError::NotFound(id))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn update(
        &self,
        repo: &Path,
        id: u64,
        f: impl FnOnce(&mut Thread) -> Result<()>,
    ) -> Result<Thread> {
        let _guard = self.inner.write.lock();
        let mut t = self.thread(repo, id)?;
        f(&mut t)?;
        t.updated = now_secs();
        self.save(&t)?;
        drop(_guard);
        self.notify(&t.repo, Some(t.id));
        Ok(t)
    }

    /// Creates a thread. Remembers the anchored file text (in the db cache) so
    /// later re-anchoring can diff against it.
    pub fn add(&self, new: NewThread) -> Result<Thread> {
        if new.body.trim().is_empty() {
            return Err(CommentError::Invalid("the comment is empty".into()));
        }
        let repo = repo_key(&new.repo);
        let id = self.inner.db.next_comment_id()?;
        let cid = self.inner.db.next_comment_id()?;
        let created = if new.created == 0 {
            now_secs()
        } else {
            new.created
        };
        let t = Thread {
            id,
            repo: repo.clone(),
            anchor: new.anchor.clone(),
            original: new.anchor,
            state: new.state,
            origin: new.origin,
            remote_id: new.remote_id,
            comments: vec![Comment {
                id: cid,
                author: new.author,
                body: new.body,
                created,
                updated: created,
                draft: new.draft,
                remote_id: new.remote_comment_id,
            }],
            matched: AnchorMatch::Original,
            confidence: 1.0,
            created,
            updated: created,
        };
        self.save(&t)?;
        self.notify(&repo, Some(id));
        Ok(t)
    }

    /// Remembers the full text of a file revision so threads anchored on it can be
    /// re-anchored through a line diff later (LRU cache; best effort).
    pub fn remember_text(&self, text: &str) {
        let key = text_cache_key(&content_id(text));
        if matches!(self.inner.db.cache_get(&key), Ok(Some(_))) {
            return;
        }
        if let Err(err) = self.inner.db.cache_put(&key, text.as_bytes()) {
            tracing::debug!("cannot cache anchored text: {err}");
        }
    }

    fn remembered_text(&self, content: &str) -> Option<String> {
        if content.is_empty() {
            return None;
        }
        let bytes = self.inner.db.cache_get(&text_cache_key(content)).ok()??;
        String::from_utf8(bytes).ok()
    }

    /// Adds a reply.
    pub fn reply(
        &self,
        repo: &Path,
        thread: u64,
        body: &str,
        author: &str,
        draft: bool,
    ) -> Result<Thread> {
        if body.trim().is_empty() {
            return Err(CommentError::Invalid("the reply is empty".into()));
        }
        let cid = self.inner.db.next_comment_id()?;
        self.update(repo, thread, |t| {
            let now = now_secs();
            t.comments.push(Comment {
                id: cid,
                author: author.to_string(),
                body: body.to_string(),
                created: now,
                updated: now,
                draft,
                remote_id: None,
            });
            Ok(())
        })
    }

    /// Adds a comment synced from a code host (no-op if its remote id is known).
    pub fn add_remote_comment(&self, repo: &Path, thread: u64, comment: Comment) -> Result<Thread> {
        let cid = self.inner.db.next_comment_id()?;
        self.update(repo, thread, |t| {
            if comment.remote_id.is_some()
                && t.comments.iter().any(|c| c.remote_id == comment.remote_id)
            {
                return Ok(());
            }
            t.comments.push(Comment { id: cid, ..comment });
            Ok(())
        })
    }

    /// Edits a comment's body.
    pub fn edit_comment(
        &self,
        repo: &Path,
        thread: u64,
        comment: u64,
        body: &str,
    ) -> Result<Thread> {
        self.update(repo, thread, |t| {
            let c = t
                .comments
                .iter_mut()
                .find(|c| c.id == comment)
                .ok_or(CommentError::NotFound(comment))?;
            c.body = body.to_string();
            c.updated = now_secs();
            Ok(())
        })
    }

    /// Resolves (`true`) or reopens a thread.
    pub fn set_resolved(&self, repo: &Path, thread: u64, resolved: bool) -> Result<Thread> {
        self.update(repo, thread, |t| {
            t.state = if resolved {
                ThreadState::Resolved
            } else if t.matched == AnchorMatch::Lost {
                ThreadState::Outdated
            } else {
                ThreadState::Open
            };
            Ok(())
        })
    }

    /// Sets the remote ids after publishing (drafts become published).
    pub fn mark_published(
        &self,
        repo: &Path,
        thread: u64,
        remote_thread: Option<String>,
        remote_comments: &[(u64, String)],
    ) -> Result<Thread> {
        self.update(repo, thread, |t| {
            if remote_thread.is_some() {
                t.remote_id = remote_thread;
            }
            for c in &mut t.comments {
                if c.draft {
                    c.draft = false;
                    if let Some((_, rid)) = remote_comments.iter().find(|(id, _)| *id == c.id) {
                        c.remote_id = Some(rid.clone());
                    }
                }
            }
            Ok(())
        })
    }

    /// Deletes a whole thread.
    pub fn delete(&self, repo: &Path, thread: u64) -> Result<()> {
        let key = repo_key(repo);
        if self.inner.db.comment_thread(&key, thread)?.is_none() {
            return Err(CommentError::NotFound(thread));
        }
        self.inner.db.put_comment_thread(&key, thread, None)?;
        self.notify(&key, Some(thread));
        Ok(())
    }

    /// Deletes one comment; deleting the last one deletes the thread. Returns the
    /// remaining thread, if any.
    pub fn delete_comment(&self, repo: &Path, thread: u64, comment: u64) -> Result<Option<Thread>> {
        let t = self.thread(repo, thread)?;
        if !t.comments.iter().any(|c| c.id == comment) {
            return Err(CommentError::NotFound(comment));
        }
        if t.comments.len() == 1 {
            self.delete(repo, thread)?;
            return Ok(None);
        }
        self.update(repo, thread, |t| {
            t.comments.retain(|c| c.id != comment);
            Ok(())
        })
        .map(Some)
    }

    /// Finds a synced thread by origin and remote id.
    pub fn find_remote(
        &self,
        repo: &Path,
        origin: Origin,
        remote_id: &str,
    ) -> Result<Option<Thread>> {
        Ok(self
            .load_all(&repo_key(repo))?
            .into_iter()
            .find(|t| t.origin == origin && t.remote_id.as_deref() == Some(remote_id)))
    }

    /// Re-anchors the threads on `path` (side `side`, review key `changeset` or all keys
    /// when `None`) to `new_text`, a revision identified by `blob` (git blob id, if any).
    /// `old_text` loads the text an anchor was taken from (by blob id); the store also
    /// remembers texts it anchored on. Threads already on this revision are skipped.
    #[allow(clippy::too_many_arguments)]
    pub fn reanchor_path(
        &self,
        repo: &Path,
        changeset: Option<&str>,
        path: &str,
        side: DiffSide,
        new_text: &str,
        blob: Option<String>,
        old_text: &dyn Fn(&Anchor) -> Option<String>,
    ) -> Result<Vec<ReanchorReport>> {
        let key = repo_key(repo);
        let content = content_id(new_text);
        let _guard = self.inner.write.lock();
        let mut reports = Vec::new();
        let mut remembered = false;
        for mut t in self.load_all(&key)? {
            if t.anchor.path != path
                || t.anchor.side != side
                || changeset.is_some_and(|c| t.anchor.changeset != c)
                || (t.anchor.content == content && t.matched != AnchorMatch::Lost)
            {
                continue;
            }
            // A lost thread retries from its original anchor.
            let from = if t.matched == AnchorMatch::Lost {
                t.original.clone()
            } else {
                t.anchor.clone()
            };
            let old = if from.content == content {
                Some(new_text.to_string())
            } else {
                old_text(&from).or_else(|| self.remembered_text(&from.content))
            };
            let result = reanchor_anchor(&from, old.as_deref(), new_text, MIN_CONFIDENCE);
            apply_reanchor(&mut t, &from, &result, new_text, &content, blob.clone());
            t.updated = now_secs();
            self.save(&t)?;
            if !remembered {
                self.remember_text(new_text);
                remembered = true;
            }
            reports.push(ReanchorReport {
                thread: t.id,
                result,
            });
        }
        drop(_guard);
        if !reports.is_empty() {
            self.notify(&key, None);
        }
        Ok(reports)
    }

    /// Re-anchors every thread of review key `key` against the files of `cs` in `repo`
    /// (new side for new-side threads, old side for old-side ones). Old revisions are
    /// loaded by blob id from `repo`.
    pub fn sync_changeset(
        &self,
        repo: &Repo,
        key: &str,
        cs: &Changeset,
    ) -> Result<Vec<ReanchorReport>> {
        let threads = self.threads(repo.workdir(), &ThreadFilter::changeset(key))?;
        if threads.is_empty() {
            return Ok(Vec::new());
        }
        let changes = repo
            .changes(cs)
            .map_err(|e| CommentError::Invalid(e.to_string()))?;
        let load_blob = |a: &Anchor| -> Option<String> {
            let oid: Oid = a.blob.as_deref()?.parse().ok()?;
            let bytes = repo.load(&GitSide::Blob(oid)).ok()??;
            String::from_utf8(bytes).ok()
        };
        let mut out = Vec::new();
        for side in [DiffSide::New, DiffSide::Old] {
            let mut paths: Vec<&str> = threads
                .iter()
                .filter(|t| t.anchor.side == side)
                .map(|t| t.anchor.path.as_str())
                .collect();
            paths.sort_unstable();
            paths.dedup();
            for path in paths {
                let change = changes.iter().find(|c| match side {
                    DiffSide::New => c.new_path.as_deref() == Some(path),
                    DiffSide::Old => c.old_path.as_deref() == Some(path),
                });
                let Some(change) = change else {
                    continue; // file no longer in the changeset: keep threads as they are
                };
                let git_side = match side {
                    DiffSide::New => &change.new,
                    DiffSide::Old => &change.old,
                };
                let Ok(Some(bytes)) = repo.load(git_side) else {
                    continue;
                };
                let Ok(text) = String::from_utf8(bytes) else {
                    continue;
                };
                let blob = match git_side {
                    GitSide::Blob(oid) => Some(oid.to_string()),
                    _ => None,
                };
                out.extend(self.reanchor_path(
                    repo.workdir(),
                    Some(key),
                    path,
                    side,
                    &text,
                    blob,
                    &load_blob,
                )?);
            }
        }
        Ok(out)
    }
}

fn apply_reanchor(
    t: &mut Thread,
    from: &Anchor,
    r: &Reanchored,
    new_text: &str,
    content: &str,
    blob: Option<String>,
) {
    t.matched = r.matched;
    t.confidence = r.confidence;
    match r.line {
        Some(line) => {
            let span = from.end_line.map(|e| e.saturating_sub(from.line));
            let end = span.map(|s| line + s);
            if let Some(mut a) = Anchor::capture(
                &from.path,
                from.side,
                new_text,
                line,
                end,
                &from.changeset,
                blob,
            ) {
                a.content = content.to_string();
                t.anchor = a;
            }
            if t.state == ThreadState::Outdated {
                t.state = ThreadState::Open;
            }
        }
        None => {
            if t.state == ThreadState::Open {
                t.state = ThreadState::Outdated;
            }
            // Keep the last known position; remember which revision was tried.
            t.anchor.content = content.to_string();
            t.anchor.blob = blob;
        }
    }
}

fn normalized(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Finds `anchor` (taken from `old_text`, when known) in `new_text`.
///
/// 1. With the old text: a line diff with move detection maps the line —
///    unchanged → [`AnchorMatch::Exact`], inside a moved block → [`AnchorMatch::Moved`];
///    a rewritten / deleted line falls through with the new position as a hint.
/// 2. Otherwise `lector_diff::remap::reanchor_with` searches the new text: the
///    identical line with matching context → `Exact` (`Moved` when it jumped far and
///    its context moved with it), a similar line → `Fuzzy` with its confidence.
/// 3. Below `min_confidence` → `Lost` (the caller marks the thread outdated).
pub fn reanchor_anchor(
    anchor: &Anchor,
    old_text: Option<&str>,
    new_text: &str,
    min_confidence: f32,
) -> Reanchored {
    let lost = Reanchored {
        line: None,
        confidence: 0.0,
        matched: AnchorMatch::Lost,
    };
    let new_lines: Vec<&str> = new_text.split('\n').collect();
    let line_text = |l: u32| {
        new_lines
            .get(l as usize)
            .map(|s| s.strip_suffix('\r').unwrap_or(s))
    };
    let want = normalized(&anchor.text);
    let mut hint = None;
    if let Some(old) = old_text {
        let old_lines: Vec<&str> = old.split('\n').collect();
        let consistent = old_lines
            .get(anchor.line as usize)
            .is_some_and(|l| normalized(l) == want);
        if consistent {
            let diff = diff_lines(old, new_text, &DiffOptions::default());
            let map = LineMap::new(old, new_text, &diff);
            match map.map(anchor.line) {
                Mapped::Exact(l) if line_text(l).is_some_and(|t| normalized(t) == want) => {
                    return Reanchored {
                        line: Some(l),
                        confidence: 1.0,
                        matched: AnchorMatch::Exact,
                    }
                }
                Mapped::Moved(l) if line_text(l).is_some_and(|t| normalized(t) == want) => {
                    return Reanchored {
                        line: Some(l),
                        confidence: 0.95,
                        matched: AnchorMatch::Moved,
                    }
                }
                Mapped::Exact(l) | Mapped::Moved(l) => hint = Some(l),
                Mapped::Deleted { near } => hint = Some(near),
            }
        }
    }
    let opts = ReanchorOptions {
        min_confidence,
        hint,
        ..ReanchorOptions::default()
    };
    match remap::reanchor_with(&anchor.to_remap(), new_text, &opts) {
        Some(r) if r.confidence >= min_confidence => {
            let matched = if r.fuzzy {
                AnchorMatch::Fuzzy
            } else if old_text.is_some() && hint.is_some() {
                // The diff said rewritten/deleted but the identical line exists elsewhere.
                AnchorMatch::Moved
            } else {
                AnchorMatch::Exact
            };
            Reanchored {
                line: Some(r.line),
                confidence: r.confidence,
                matched,
            }
        }
        _ => lost,
    }
}

/// Markdown export of threads (for sharing or pasting into a review).
pub fn export_markdown(title: &str, threads: &[Thread]) -> String {
    let mut out = format!("# Review comments — {title}\n\n");
    if threads.is_empty() {
        out.push_str("_No comments._\n");
        return out;
    }
    for t in threads {
        let state = match t.state {
            ThreadState::Open => "open",
            ThreadState::Resolved => "resolved",
            ThreadState::Outdated => "outdated",
        };
        let draft = if t.is_draft() { ", draft" } else { "" };
        out.push_str(&format!("## `{}` ({state}{draft})\n\n", t.location()));
        out.push_str(&format!("```\n{}\n```\n\n", t.original.text.trim_end()));
        for c in &t.comments {
            let who = if c.author.is_empty() {
                "you"
            } else {
                &c.author
            };
            out.push_str(&format!("**{who}**: {}\n\n", c.body.trim()));
        }
    }
    out
}

/// JSON form of a thread for commands (compact; 0-based lines).
pub fn thread_json(t: &Thread) -> Value {
    json!({
        "id": t.id,
        "path": t.anchor.path,
        "side": t.anchor.side,
        "line": t.anchor.line,
        "end_line": t.anchor.end_line,
        "changeset": t.anchor.changeset,
        "state": t.state,
        "origin": t.origin,
        "matched": t.matched,
        "confidence": (t.confidence * 1000.0).round() / 1000.0,
        "snippet": t.original.text,
        "draft": t.is_draft(),
        "remote_id": t.remote_id,
        "comments": t.comments.iter().map(|c| json!({
            "id": c.id, "author": c.author, "body": c.body, "created": c.created,
            "draft": c.draft,
        })).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------------------

/// Registers `comments.*` commands.
pub fn register_commands(r: &CommandRegistry) {
    r.register(list_cmd());
    r.register(add_cmd());
    r.register(reply_cmd());
    r.register(resolve_cmd());
    r.register(delete_cmd());
    r.register(export_cmd());
}

fn needs_repo(cx: &CommandContext) -> Availability {
    match &cx.project {
        None => Availability::no("No folder is open"),
        Some(p) if p.repo().is_none() => Availability::no("Not a git repository"),
        Some(_) => Availability::Available,
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn u64_arg(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

/// The review key and (when it is a changeset) the changeset of the arguments / focus.
fn review_key(cx: &CommandContext, args: &Value) -> (String, Option<Changeset>) {
    match str_arg(args, "changeset") {
        Some(key) => (
            key.to_string(),
            lector_workspace::review::parse_changeset_key(key),
        ),
        None => {
            let cs = cx.focus.changeset.clone().unwrap_or(Changeset::WorkingTree);
            (lector_workspace::review::changeset_key(&cs), Some(cs))
        }
    }
}

fn changeset_schema() -> Value {
    prop(
        "string",
        "Review key: a changeset key (worktree, staged, commit:<sha>, range:<a>..<b>, branch:<base>...<branch>) or a pull-request key (pr:<host>/<repo>#<n>); default: the focused changeset or worktree",
    )
}

/// The user's git name (read-only `git config`), or "you".
pub fn local_author(repo: &Path) -> String {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["config", "user.name"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "you".into())
}

/// Text of `path` on `side` of `cs` in `repo` (+ blob id when it is a git object);
/// falls back to the working-tree file for the new side.
pub fn side_text(
    repo: &Repo,
    cs: Option<&Changeset>,
    path: &str,
    side: DiffSide,
) -> Option<(String, Option<String>)> {
    if let Some(cs) = cs {
        if let Ok(changes) = repo.changes(cs) {
            let change = changes.iter().find(|c| match side {
                DiffSide::New => c.new_path.as_deref() == Some(path),
                DiffSide::Old => c.old_path.as_deref() == Some(path),
            });
            if let Some(change) = change {
                let s = match side {
                    DiffSide::New => &change.new,
                    DiffSide::Old => &change.old,
                };
                let blob = match s {
                    GitSide::Blob(oid) => Some(oid.to_string()),
                    _ => None,
                };
                if let Ok(Some(bytes)) = repo.load(s) {
                    return Some((String::from_utf8_lossy(&bytes).into_owned(), blob));
                }
            }
        }
    }
    match side {
        DiffSide::New => std::fs::read(repo.workdir().join(path))
            .ok()
            .map(|b| (String::from_utf8_lossy(&b).into_owned(), None)),
        DiffSide::Old => None,
    }
}

fn list_cmd() -> FnCommand {
    let spec = CommandSpec::read(
        "comments.list",
        "List Review Comments",
        "Review",
        object(
            &[
                ("changeset", changeset_schema()),
                ("path", prop("string", "Only this repo-relative path")),
                (
                    "state",
                    json!({"type": "string", "enum": ["open", "resolved", "outdated", "all"],
                           "description": "Filter by state (default all)"}),
                ),
                (
                    "drafts_only",
                    prop("boolean", "Only threads with draft comments"),
                ),
                (
                    "all_changesets",
                    prop("boolean", "Every review key of the repository"),
                ),
            ],
            &[],
        ),
    );
    FnCommand::new(spec, |cx, args| {
        let repo = cx.require_repo()?;
        let store = CommentStore::for_core(&cx.core);
        let (key, cs) = review_key(cx, args);
        let all = args.get("all_changesets").and_then(Value::as_bool) == Some(true);
        if !all {
            if let Some(cs) = &cs {
                if let Err(err) = store.sync_changeset(repo, &key, cs) {
                    tracing::debug!("re-anchoring skipped: {err}");
                }
            }
        }
        let states = match str_arg(args, "state") {
            Some("open") => vec![ThreadState::Open],
            Some("resolved") => vec![ThreadState::Resolved],
            Some("outdated") => vec![ThreadState::Outdated],
            _ => vec![],
        };
        let filter = ThreadFilter {
            changeset: (!all).then_some(key.clone()),
            path: str_arg(args, "path").map(str::to_string),
            states,
            drafts_only: args.get("drafts_only").and_then(Value::as_bool) == Some(true),
            origin: None,
        };
        let threads = store.threads(repo.workdir(), &filter)?;
        Ok(
            Outcome::new(format!("{} comment thread(s)", threads.len())).with_data(json!({
                "changeset": key,
                "threads": threads.iter().map(thread_json).collect::<Vec<_>>(),
            })),
        )
    })
    .with_available(needs_repo)
}

fn add_cmd() -> FnCommand {
    let spec = CommandSpec::read(
        "comments.add",
        "Add Review Comment",
        "Review",
        object(
            &[
                ("path", prop("string", "Repo-relative path")),
                ("line", prop("integer", "0-based line")),
                (
                    "end_line",
                    prop("integer", "0-based last line of a range (inclusive)"),
                ),
                (
                    "side",
                    json!({"type": "string", "enum": ["new", "old"], "description": "Diff side (default new)"}),
                ),
                ("body", prop("string", "Markdown body")),
                ("changeset", changeset_schema()),
                (
                    "draft",
                    prop(
                        "boolean",
                        "Keep as a draft of a pending pull-request review",
                    ),
                ),
                ("author", prop("string", "Author (default: git user.name)")),
            ],
            &["path", "line", "body"],
        ),
    );
    FnCommand::new(spec, |cx, args| {
        let repo = cx.require_repo()?;
        let path = str_arg(args, "path").ok_or_else(|| anyhow!("missing path"))?;
        let path = path.trim_start_matches("./").replace('\\', "/");
        let line = u64_arg(args, "line").unwrap_or(0) as u32;
        let end_line = u64_arg(args, "end_line").map(|v| v as u32);
        let side = match str_arg(args, "side") {
            Some("old") => DiffSide::Old,
            _ => DiffSide::New,
        };
        let body = str_arg(args, "body").ok_or_else(|| anyhow!("missing body"))?;
        let (key, cs) = review_key(cx, args);
        let (text, blob) = side_text(repo, cs.as_ref(), &path, side)
            .ok_or_else(|| anyhow!("cannot read {path} on the {side:?} side of {key}"))?;
        let anchor = Anchor::capture(&path, side, &text, line, end_line, &key, blob)
            .ok_or_else(|| anyhow!("line {} is past the end of {path}", line + 1))?;
        let store = CommentStore::for_core(&cx.core);
        store.remember_text(&text);
        let author = str_arg(args, "author")
            .map(str::to_string)
            .unwrap_or_else(|| local_author(repo.workdir()));
        let t = store.add(NewThread {
            repo: repo.workdir().to_path_buf(),
            anchor,
            body: body.to_string(),
            author,
            draft: args.get("draft").and_then(Value::as_bool) == Some(true),
            ..Default::default()
        })?;
        Ok(Outcome::new(format!("Comment added on {}", t.location())).with_data(thread_json(&t)))
    })
    .with_available(needs_repo)
}

fn reply_cmd() -> FnCommand {
    let spec = CommandSpec::read(
        "comments.reply",
        "Reply to Review Comment",
        "Review",
        object(
            &[
                ("thread", prop("integer", "Thread id")),
                ("body", prop("string", "Markdown body")),
                ("draft", prop("boolean", "Keep as a draft")),
                ("author", prop("string", "Author (default: git user.name)")),
            ],
            &["thread", "body"],
        ),
    );
    FnCommand::new(spec, |cx, args| {
        let repo = cx.require_repo()?;
        let id = u64_arg(args, "thread").ok_or_else(|| anyhow!("missing thread"))?;
        let body = str_arg(args, "body").ok_or_else(|| anyhow!("missing body"))?;
        let author = str_arg(args, "author")
            .map(str::to_string)
            .unwrap_or_else(|| local_author(repo.workdir()));
        let draft = args.get("draft").and_then(Value::as_bool) == Some(true);
        let t = CommentStore::for_core(&cx.core).reply(repo.workdir(), id, body, &author, draft)?;
        Ok(Outcome::new(format!("Replied on {}", t.location())).with_data(thread_json(&t)))
    })
    .with_available(needs_repo)
}

fn resolve_cmd() -> FnCommand {
    let spec = CommandSpec::read(
        "comments.resolve",
        "Resolve Review Comment",
        "Review",
        object(
            &[
                ("thread", prop("integer", "Thread id")),
                ("resolved", prop("boolean", "false reopens (default true)")),
            ],
            &["thread"],
        ),
    );
    FnCommand::new(spec, |cx, args| {
        let repo = cx.require_repo()?;
        let id = u64_arg(args, "thread").ok_or_else(|| anyhow!("missing thread"))?;
        let resolved = args.get("resolved").and_then(Value::as_bool) != Some(false);
        let t = CommentStore::for_core(&cx.core).set_resolved(repo.workdir(), id, resolved)?;
        let verb = if resolved { "Resolved" } else { "Reopened" };
        Ok(Outcome::new(format!("{verb} {}", t.location())).with_data(thread_json(&t)))
    })
    .with_available(needs_repo)
}

fn delete_cmd() -> FnCommand {
    let spec = CommandSpec::read(
        "comments.delete",
        "Delete Review Comment",
        "Review",
        object(
            &[
                ("thread", prop("integer", "Thread id")),
                (
                    "comment",
                    prop("integer", "Only this comment of the thread"),
                ),
            ],
            &["thread"],
        ),
    );
    FnCommand::new(spec, |cx, args| {
        let repo = cx.require_repo()?;
        let id = u64_arg(args, "thread").ok_or_else(|| anyhow!("missing thread"))?;
        let store = CommentStore::for_core(&cx.core);
        match u64_arg(args, "comment") {
            Some(c) => {
                let left = store.delete_comment(repo.workdir(), id, c)?;
                Ok(Outcome::new("Comment deleted").with_data(json!({
                    "thread": left.as_ref().map(thread_json),
                })))
            }
            None => {
                store.delete(repo.workdir(), id)?;
                Ok(Outcome::new("Thread deleted").with_data(json!({ "thread": null })))
            }
        }
    })
    .with_available(needs_repo)
}

fn export_cmd() -> FnCommand {
    let spec = CommandSpec::read(
        "comments.export",
        "Export Review Comments",
        "Review",
        object(
            &[
                ("changeset", changeset_schema()),
                (
                    "format",
                    json!({"type": "string", "enum": ["markdown", "json"], "description": "Default markdown"}),
                ),
                (
                    "include_resolved",
                    prop("boolean", "Include resolved threads (default false)"),
                ),
            ],
            &[],
        ),
    );
    FnCommand::new(spec, |cx, args| {
        let repo = cx.require_repo()?;
        let (key, _) = review_key(cx, args);
        let mut threads = CommentStore::for_core(&cx.core)
            .threads(repo.workdir(), &ThreadFilter::changeset(key.clone()))?;
        if args.get("include_resolved").and_then(Value::as_bool) != Some(true) {
            threads.retain(|t| t.state != ThreadState::Resolved);
        }
        match str_arg(args, "format") {
            Some("json") => Ok(
                Outcome::new(format!("{} thread(s)", threads.len())).with_data(
                    json!({ "threads": threads.iter().map(thread_json).collect::<Vec<_>>() }),
                ),
            ),
            Some("markdown") | None => {
                let md = export_markdown(&key, &threads);
                Ok(
                    Outcome::new(format!("{} thread(s) exported", threads.len()))
                        .with_data(json!({ "markdown": md })),
                )
            }
            Some(other) => bail!("unknown format {other}"),
        }
    })
    .with_available(needs_repo)
}

#[cfg(test)]
mod tests;
