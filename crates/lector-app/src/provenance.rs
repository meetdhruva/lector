//! Provenance: "why is this line like this?"
//!
//! A [`ProvenanceProvider`] answers for one line of one file; providers are kept in a
//! [`ProvenanceRegistry`] reachable from [`AppCore::provenance`]. The UI asks the
//! registry when the pointer rests on a changed line (diff views and code views) and shows
//! the answers best-first: the agent prompt and turn when the agent module registered a
//! provider (`AgentTurn`), then the commit message (`Commit`), or "not committed yet"
//! (`Uncommitted`).
//!
//! Built-in providers (registered when the registry is created):
//! * [`UncommittedProvider`] (priority [`PRIORITY_UNCOMMITTED`]): lines of the working tree
//!   that are not in any commit (blame says so, or the file is untracked).
//! * [`CommitProvider`] (priority [`PRIORITY_COMMIT`]): the commit that last changed the line
//!   (`git blame`): summary, author, date and the message body.
//!
//! Providers are blocking and may be slow (they run git); call them from a scheduler job,
//! never from the UI thread. Reading provenance never writes anything.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lector_git::{BlameHunk, Oid, Repo};
use parking_lot::{Mutex, RwLock};

use crate::AppCore;

/// Priority of the agent provider (highest: the prompt explains more than a commit).
pub const PRIORITY_AGENT: i32 = 100;
/// Priority of [`UncommittedProvider`].
pub const PRIORITY_UNCOMMITTED: i32 = 50;
/// Priority of [`CommitProvider`].
pub const PRIORITY_COMMIT: i32 = 10;

/// What explains a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProvenanceKind {
    /// A commit (`id` = full sha).
    Commit,
    /// A turn of a coding agent (`id` = the turn id).
    AgentTurn,
    /// A change in the working tree that is not committed yet (`id` empty).
    Uncommitted,
}

/// The answer for one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineProvenance {
    /// What kind of origin this is.
    pub kind: ProvenanceKind,
    /// One line: the commit summary, "Agent turn: <prompt>", "Not committed yet".
    pub title: String,
    /// Longer text: author and date plus the commit body, or the full agent prompt.
    pub detail: String,
    /// Commit sha or turn id (empty for [`ProvenanceKind::Uncommitted`]).
    pub id: String,
}

/// A line to explain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineQuery {
    /// Repository working directory.
    pub repo: PathBuf,
    /// Repository-relative path (`/`-separated).
    pub path: String,
    /// 0-based line of the file as of `rev` (the working tree when `rev` is `None`).
    pub line: u32,
    /// Revision the line belongs to (`None` = working tree, a sha for a commit's new side).
    pub rev: Option<String>,
}

/// Answers "why is this line like this?". Blocking; `explain` may run git.
pub trait ProvenanceProvider: Send + Sync {
    /// Short stable name ("commit", "uncommitted", "agent").
    fn name(&self) -> &'static str;
    /// The explanation for `query`, if this provider knows one.
    fn explain(&self, query: &LineQuery) -> Option<LineProvenance>;
}

/// Identifies a registered provider (for [`ProvenanceRegistry::unregister`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProviderId(u64);

struct Entry {
    id: ProviderId,
    priority: i32,
    provider: Arc<dyn ProvenanceProvider>,
}

/// The providers of an [`AppCore`], consulted best-priority first.
#[derive(Default)]
pub struct ProvenanceRegistry {
    entries: RwLock<Vec<Entry>>,
    next: Mutex<u64>,
}

impl std::fmt::Debug for ProvenanceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProvenanceRegistry")
            .field("providers", &self.names())
            .finish()
    }
}

impl ProvenanceRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry with the commit and uncommitted providers.
    pub fn with_builtin() -> Self {
        let registry = Self::new();
        let blame = Arc::new(BlameCache::default());
        registry.register(
            PRIORITY_UNCOMMITTED,
            Arc::new(UncommittedProvider::new(blame.clone())),
        );
        registry.register(PRIORITY_COMMIT, Arc::new(CommitProvider::new(blame)));
        registry
    }

    /// Adds a provider; higher `priority` answers first.
    pub fn register(&self, priority: i32, provider: Arc<dyn ProvenanceProvider>) -> ProviderId {
        let id = {
            let mut next = self.next.lock();
            *next += 1;
            ProviderId(*next)
        };
        let mut entries = self.entries.write();
        entries.push(Entry {
            id,
            priority,
            provider,
        });
        // Stable: equal priorities keep their registration order.
        entries.sort_by_key(|e| std::cmp::Reverse(e.priority));
        id
    }

    /// Removes a provider; returns whether it was registered.
    pub fn unregister(&self, id: ProviderId) -> bool {
        let mut entries = self.entries.write();
        let before = entries.len();
        entries.retain(|e| e.id != id);
        entries.len() != before
    }

    /// Provider names, best priority first.
    pub fn names(&self) -> Vec<&'static str> {
        self.entries
            .read()
            .iter()
            .map(|e| e.provider.name())
            .collect()
    }

    /// Every provider's answer for `query`, best priority first.
    pub fn explain_all(&self, query: &LineQuery) -> Vec<LineProvenance> {
        // Clone the providers out so a slow one never blocks registration.
        let providers: Vec<Arc<dyn ProvenanceProvider>> = self
            .entries
            .read()
            .iter()
            .map(|e| e.provider.clone())
            .collect();
        providers.iter().filter_map(|p| p.explain(query)).collect()
    }

    /// The best answer for `query`.
    pub fn explain(&self, query: &LineQuery) -> Option<LineProvenance> {
        self.explain_all(query).into_iter().next()
    }
}

impl ProvenanceKind {
    /// "Commit", "Agent turn", "Uncommitted".
    pub fn label(self) -> &'static str {
        match self {
            ProvenanceKind::Commit => "Commit",
            ProvenanceKind::AgentTurn => "Agent turn",
            ProvenanceKind::Uncommitted => "Uncommitted",
        }
    }
}

/// Escapes Markdown punctuation so commit messages and prompts render as the plain text they are.
fn md_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '|' | '~'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Markdown for a hover card: one section per answer (`**Kind** · title`, then the detail),
/// separated by rules. Empty when there is nothing to say.
pub fn explanation_markdown(items: &[LineProvenance]) -> String {
    items
        .iter()
        .map(|p| {
            let mut s = format!("**{}** · {}", p.kind.label(), md_escape(&p.title));
            if !p.detail.trim().is_empty() {
                s.push_str("\n\n");
                s.push_str(&md_escape(p.detail.trim()));
            }
            s
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

impl AppCore {
    /// The provenance registry (created with the built-in providers on first use).
    pub fn provenance(&self) -> Arc<ProvenanceRegistry> {
        self.extension_or_init(|| Arc::new(ProvenanceRegistry::with_builtin()))
    }
}

// ---------------------------------------------------------------------------
// Built-in providers

/// Blame results shared by the built-in providers: keyed by (repo, path, rev, HEAD, mtime).
#[derive(Default)]
pub struct BlameCache {
    repos: Mutex<HashMap<PathBuf, Repo>>,
    blames: Mutex<Vec<(BlameKey, Blamed)>>,
    bodies: Mutex<HashMap<Oid, String>>,
}

/// Blame of one file (`None` when git could not blame it).
type Blamed = Arc<Option<Vec<BlameHunk>>>;

#[derive(Clone, PartialEq, Eq)]
struct BlameKey {
    repo: PathBuf,
    path: String,
    rev: Option<String>,
    head: Option<Oid>,
    stamp: Option<(u64, u64)>,
}

const BLAME_CACHE_ENTRIES: usize = 12;

impl BlameCache {
    fn repo(&self, workdir: &Path) -> Option<Repo> {
        let mut repos = self.repos.lock();
        if let Some(r) = repos.get(workdir) {
            return Some(r.clone());
        }
        let repo = Repo::discover(workdir).ok()?;
        repos.insert(workdir.to_path_buf(), repo.clone());
        Some(repo)
    }

    /// Blame of the query's file; `None` inside means "blame failed" (untracked, binary...).
    fn blame(&self, q: &LineQuery) -> Option<Arc<Option<Vec<BlameHunk>>>> {
        let repo = self.repo(&q.repo)?;
        let stamp = std::fs::metadata(repo.workdir().join(&q.path))
            .ok()
            .map(|m| {
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                (mtime, m.len())
            });
        let head = repo.head().ok().and_then(|h| h.oid);
        let key = BlameKey {
            repo: q.repo.clone(),
            path: q.path.clone(),
            rev: q.rev.clone(),
            head,
            // Committed revisions never change; only the working tree needs the stamp.
            stamp: if q.rev.is_none() { stamp } else { None },
        };
        {
            let cache = self.blames.lock();
            if let Some((_, hit)) = cache.iter().find(|(k, _)| *k == key) {
                return Some(hit.clone());
            }
        }
        let result = Arc::new(repo.blame(&q.path, q.rev.as_deref()).ok());
        let mut cache = self.blames.lock();
        if cache.len() >= BLAME_CACHE_ENTRIES {
            cache.remove(0);
        }
        cache.push((key, result.clone()));
        Some(result)
    }

    fn hunk_at(&self, q: &LineQuery) -> Option<Option<BlameHunk>> {
        let blame = self.blame(q)?;
        Some(match blame.as_ref() {
            Some(hunks) => hunks.iter().find(|h| h.lines.contains(&q.line)).cloned(),
            None => None,
        })
    }

    fn message_body(&self, workdir: &Path, oid: Oid) -> String {
        if let Some(b) = self.bodies.lock().get(&oid) {
            return b.clone();
        }
        let body = self
            .repo(workdir)
            .and_then(|r| r.commit(oid).ok())
            .map(|c| message_body(&c.message, &c.info.summary))
            .unwrap_or_default();
        let mut bodies = self.bodies.lock();
        if bodies.len() > 256 {
            bodies.clear();
        }
        bodies.insert(oid, body.clone());
        body
    }
}

/// The message without its first (summary) line, trimmed.
fn message_body(message: &str, summary: &str) -> String {
    let m = message.trim();
    m.strip_prefix(summary.trim())
        .unwrap_or(m)
        .trim()
        .to_string()
}

/// "2 hours ago" style age of a unix time, relative to `now`.
pub fn age(time: i64, now: i64) -> String {
    let secs = (now - time).max(0);
    let (n, unit) = match secs {
        0..=59 => return "just now".into(),
        60..=3599 => (secs / 60, "minute"),
        3600..=86_399 => (secs / 3600, "hour"),
        86_400..=2_591_999 => (secs / 86_400, "day"),
        2_592_000..=31_535_999 => (secs / 2_592_000, "month"),
        _ => (secs / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Explains lines by the commit that last changed them (`git blame`).
pub struct CommitProvider {
    cache: Arc<BlameCache>,
}

impl CommitProvider {
    /// A provider using `cache` (share it with [`UncommittedProvider`]).
    pub fn new(cache: Arc<BlameCache>) -> Self {
        Self { cache }
    }
}

impl ProvenanceProvider for CommitProvider {
    fn name(&self) -> &'static str {
        "commit"
    }

    fn explain(&self, q: &LineQuery) -> Option<LineProvenance> {
        let hunk = self.cache.hunk_at(q)??;
        if hunk.uncommitted || hunk.oid.is_zero() {
            return None;
        }
        let body = self.cache.message_body(&q.repo, hunk.oid);
        let who = format!(
            "{} committed {} · {}",
            hunk.author,
            age(hunk.time, now_secs()),
            hunk.oid.short()
        );
        Some(LineProvenance {
            kind: ProvenanceKind::Commit,
            title: hunk.summary.clone(),
            detail: if body.is_empty() {
                who
            } else {
                format!("{who}\n\n{body}")
            },
            id: hunk.oid.to_string(),
        })
    }
}

/// Explains working-tree lines that are not committed yet.
pub struct UncommittedProvider {
    cache: Arc<BlameCache>,
}

impl UncommittedProvider {
    /// A provider using `cache` (share it with [`CommitProvider`]).
    pub fn new(cache: Arc<BlameCache>) -> Self {
        Self { cache }
    }
}

impl ProvenanceProvider for UncommittedProvider {
    fn name(&self) -> &'static str {
        "uncommitted"
    }

    fn explain(&self, q: &LineQuery) -> Option<LineProvenance> {
        if q.rev.is_some() {
            return None;
        }
        let uncommitted = match self.cache.hunk_at(q)? {
            Some(hunk) => hunk.uncommitted || hunk.oid.is_zero(),
            // Blame failed or no hunk covers the line: an untracked (new) file.
            None => !self.cache.blame(q).is_some_and(|b| b.is_some()),
        };
        uncommitted.then(|| LineProvenance {
            kind: ProvenanceKind::Uncommitted,
            title: "Not committed yet".into(),
            detail: "This line changed in your working tree and is in no commit. \
                     Review it, then stage and commit it."
                .into(),
            id: String::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Ada")
            .env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        std::fs::write(p.join("a.rs"), "fn one() {}\nfn two() {}\n").expect("write");
        git(p, &["add", "."]);
        git(
            p,
            &[
                "commit",
                "-q",
                "-m",
                "Add the first two functions",
                "-m",
                "They are needed by the parser.",
            ],
        );
        std::fs::write(
            p.join("a.rs"),
            "fn one() {}\nfn two() { 2 }\nfn three() {}\n",
        )
        .expect("write");
        dir
    }

    fn q(dir: &Path, path: &str, line: u32, rev: Option<&str>) -> LineQuery {
        LineQuery {
            repo: dir.to_path_buf(),
            path: path.into(),
            line,
            rev: rev.map(str::to_string),
        }
    }

    #[test]
    fn commit_and_uncommitted_providers() {
        let dir = fixture();
        let registry = ProvenanceRegistry::with_builtin();
        assert_eq!(registry.names(), ["uncommitted", "commit"]);

        // Line 0 is unchanged: the commit explains it, nothing uncommitted.
        let all = registry.explain_all(&q(dir.path(), "a.rs", 0, None));
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].kind, ProvenanceKind::Commit);
        assert_eq!(all[0].title, "Add the first two functions");
        assert!(all[0].detail.contains("Ada committed"));
        assert!(all[0].detail.contains("They are needed by the parser."));
        assert_eq!(all[0].id.len(), 40);

        // Lines 1 and 2 are edited / new in the working tree.
        for line in [1, 2] {
            let best = registry
                .explain(&q(dir.path(), "a.rs", line, None))
                .expect("explained");
            assert_eq!(best.kind, ProvenanceKind::Uncommitted, "line {line}");
            assert!(best.id.is_empty());
        }

        // At HEAD the same lines belong to the commit.
        let at_head = registry
            .explain(&q(dir.path(), "a.rs", 1, Some("HEAD")))
            .expect("explained");
        assert_eq!(at_head.kind, ProvenanceKind::Commit);
    }

    #[test]
    fn untracked_files_are_uncommitted() {
        let dir = fixture();
        std::fs::write(dir.path().join("new.rs"), "fn new() {}\n").expect("write");
        let registry = ProvenanceRegistry::with_builtin();
        let got = registry
            .explain(&q(dir.path(), "new.rs", 0, None))
            .expect("explained");
        assert_eq!(got.kind, ProvenanceKind::Uncommitted);
    }

    struct Fake;
    impl ProvenanceProvider for Fake {
        fn name(&self) -> &'static str {
            "agent"
        }
        fn explain(&self, _: &LineQuery) -> Option<LineProvenance> {
            Some(LineProvenance {
                kind: ProvenanceKind::AgentTurn,
                title: "Agent turn: make two return 2".into(),
                detail: "make two return 2".into(),
                id: "turn-7".into(),
            })
        }
    }

    #[test]
    fn registry_orders_by_priority_and_unregisters() {
        let dir = fixture();
        let registry = ProvenanceRegistry::with_builtin();
        let id = registry.register(PRIORITY_AGENT, Arc::new(Fake));
        assert_eq!(registry.names(), ["agent", "uncommitted", "commit"]);
        let query = q(dir.path(), "a.rs", 1, None);
        assert_eq!(
            registry.explain(&query).map(|p| p.kind),
            Some(ProvenanceKind::AgentTurn)
        );
        assert_eq!(registry.explain_all(&query).len(), 2, "agent + uncommitted");
        assert!(registry.unregister(id));
        assert!(!registry.unregister(id));
        assert_eq!(
            registry.explain(&query).map(|p| p.kind),
            Some(ProvenanceKind::Uncommitted)
        );
    }

    #[test]
    fn core_exposes_the_registry() {
        let core = AppCore::open(crate::CoreOptions::for_tests());
        assert_eq!(core.provenance().names(), ["uncommitted", "commit"]);
        // The same registry on every call.
        let id = core.provenance().register(1, Arc::new(Fake));
        assert!(core.provenance().names().contains(&"agent"));
        assert!(core.provenance().unregister(id));
    }

    #[test]
    fn markdown_sections_and_escaping() {
        let items = [
            LineProvenance {
                kind: ProvenanceKind::AgentTurn,
                title: "make `two` return 2".into(),
                detail: "make two return 2\n\nplease".into(),
                id: "t1".into(),
            },
            LineProvenance {
                kind: ProvenanceKind::Commit,
                title: "Fix *it*".into(),
                detail: String::new(),
                id: "abc".into(),
            },
        ];
        let md = explanation_markdown(&items);
        assert_eq!(
            md,
            "**Agent turn** · make \\`two\\` return 2\n\nmake two return 2\n\nplease\n\n---\n\n**Commit** · Fix \\*it\\*"
        );
        assert_eq!(explanation_markdown(&[]), "");
    }

    #[test]
    fn age_is_human() {
        assert_eq!(age(100, 130), "just now");
        assert_eq!(age(0, 3 * 3600), "3 hours ago");
        assert_eq!(age(0, 86_400), "1 day ago");
    }
}
