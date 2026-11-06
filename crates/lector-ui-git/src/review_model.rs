//! Shared review state of a window (v0.4 review UX): which files of the working tree need
//! attention (risk order for the Changes panel), and whether the changes are new since the user
//! last looked at them (status-bar chip, welcome screen).
//!
//! Everything here is read-only: it diffs and analyzes in the background and keeps the user's
//! "last looked" mark in Lector's own database, never in the repository.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::{Context, EventEmitter, Task};
use lector_app::review_queue::{
    analyze, AnalysisOptions, ChangesetAnalysis, FileMoves, HunkFingerprint, QueueEntry, QueueGroup,
};
use lector_diff::DiffOptions;
use lector_git::{Changeset, FileChange, Repo, StatusEntry};
use lector_syntax::LanguageRegistry;
use lector_workspace::review::{self, FileDiff};
use lector_workspace::risk::RiskLevel;

/// Changesets with more files than this are not analyzed (the order stays alphabetical).
pub const MAX_ANALYZED_FILES: usize = 400;

/// How long the working tree must be quiet before it is analyzed again.
const ANALYSIS_DELAY: Duration = Duration::from_millis(600);

/// The risk of one file, as shown next to it.
#[derive(Clone, Debug, PartialEq)]
pub struct FileRisk {
    /// Bucket.
    pub level: RiskLevel,
    /// The sentence under the file name ("Changes a public function signature").
    pub reason: Option<String>,
    /// Score in `0..=1`.
    pub score: f32,
    /// Needs attention, or low-risk / generated.
    pub group: QueueGroup,
}

impl FileRisk {
    fn of(entry: &QueueEntry) -> FileRisk {
        FileRisk {
            level: entry.level(),
            reason: entry.top_reason().map(str::to_string),
            score: entry.report.score,
            group: entry.group,
        }
    }
}

/// What the analysis knows about one file.
#[derive(Clone, Debug)]
pub struct FileAnalysis {
    /// Risk.
    pub risk: FileRisk,
    /// Moved code.
    pub moves: Arc<FileMoves>,
    /// Hunk identities.
    pub hunks: Arc<Vec<HunkFingerprint>>,
}

/// The analysis of a changeset, keyed by repository path.
#[derive(Clone, Debug, Default)]
pub struct ChangesAnalysis {
    /// Paths in review order (attention group by descending risk, then the low-risk group).
    pub order: Vec<String>,
    files: HashMap<String, FileAnalysis>,
}

impl ChangesAnalysis {
    /// The analysis of `path`.
    pub fn get(&self, path: &str) -> Option<&FileAnalysis> {
        self.files.get(path)
    }

    /// Position of `path` in the review order.
    pub fn rank(&self, path: &str) -> Option<usize> {
        self.order.iter().position(|p| p == path)
    }

    /// No file was analyzed.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

/// Diffs every change and runs the review-queue analysis (blocking; call from a background
/// thread). Files that cannot be read are left out.
pub fn analyze_changes(
    repo: &Repo,
    languages: &LanguageRegistry,
    changes: &[FileChange],
    diff_options: &DiffOptions,
    detect_moved: bool,
) -> ChangesAnalysis {
    if changes.len() > MAX_ANALYZED_FILES {
        return ChangesAnalysis::default();
    }
    let diffs: Vec<Arc<FileDiff>> = changes
        .iter()
        .filter_map(|c| review::load_file_diff_with(repo, languages, c, diff_options).ok())
        .map(Arc::new)
        .collect();
    analysis_of(&diffs, detect_moved)
}

/// The queue analysis of already loaded diffs, keyed by path.
pub fn analysis_of(diffs: &[Arc<FileDiff>], detect_moved: bool) -> ChangesAnalysis {
    let queue: ChangesetAnalysis = analyze(diffs, &AnalysisOptions { detect_moved });
    let mut out = ChangesAnalysis::default();
    for entry in &queue.order {
        let Some(diff) = diffs.get(entry.index) else {
            continue;
        };
        let path = diff.change.path().to_string();
        out.order.push(path.clone());
        out.files.insert(
            path,
            FileAnalysis {
                risk: FileRisk::of(entry),
                moves: Arc::new(queue.moves.get(entry.index).cloned().unwrap_or_default()),
                hunks: Arc::new(queue.hunks.get(entry.index).cloned().unwrap_or_default()),
            },
        );
    }
    out
}

/// A fingerprint of the uncommitted changes: changes whenever a changed file is added, removed,
/// staged, or its size or modification time changes.
pub fn changes_fingerprint(workdir: &Path, status: &[StatusEntry]) -> String {
    let mut text = String::new();
    for e in status {
        text.push_str(&e.path);
        text.push('|');
        text.push_str(&format!(
            "{:?}|{:?}|{}|{}|",
            e.staged, e.unstaged, e.untracked, e.conflicted
        ));
        if let Ok(meta) = std::fs::metadata(workdir.join(&e.path)) {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            text.push_str(&format!("{}|{mtime}", meta.len()));
        }
        text.push('\n');
    }
    lector_config::content_hash(text.as_bytes())
}

/// What a refresh of the model needs from the store.
pub struct ModelInputs {
    /// The repository.
    pub repo: Repo,
    /// Languages (syntax for the risk heuristics).
    pub languages: Arc<LanguageRegistry>,
    /// `git status` entries.
    pub status: Arc<Vec<StatusEntry>>,
    /// Diff options from the settings.
    pub diff_options: DiffOptions,
    /// Whether to look for moved code.
    pub detect_moved: bool,
}

/// Events of the [`ReviewModel`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewModelEvent {
    /// The risk analysis of the working tree changed.
    Analysis,
    /// The number of changes or whether they are new changed.
    Hint,
}

/// Risk analysis of the working tree and the "new since you last looked" state of a window.
#[derive(Default)]
pub struct ReviewModel {
    working: Option<Arc<ChangesAnalysis>>,
    fingerprint: Option<String>,
    uncommitted: usize,
    seen: Option<String>,
    seen_loaded: bool,
    repo_root: Option<PathBuf>,
    analysis_task: Option<Task<()>>,
    fingerprint_task: Option<Task<()>>,
}

impl EventEmitter<ReviewModelEvent> for ReviewModel {}

fn seen_key(root: &Path) -> String {
    format!("review.seen/{}", root.display())
}

impl ReviewModel {
    /// An empty model.
    pub fn new() -> Self {
        Self::default()
    }

    /// The analysis of the whole working tree (`HEAD` to working files), once computed.
    pub fn working(&self) -> Option<&Arc<ChangesAnalysis>> {
        self.working.as_ref()
    }

    /// The risk of `path` in the working tree.
    pub fn risk_of(&self, path: &str) -> Option<&FileRisk> {
        self.working.as_ref()?.get(path).map(|f| &f.risk)
    }

    /// Number of changed files and whether they differ from what the user last looked at.
    pub fn hint(&self) -> (usize, bool) {
        let new =
            self.uncommitted > 0 && self.fingerprint.is_some() && self.fingerprint != self.seen;
        (self.uncommitted, new)
    }

    /// Records that the user looked at the current changes (review tab opened or refreshed).
    pub fn mark_seen(&mut self, cx: &mut Context<Self>) {
        let (Some(root), Some(fingerprint)) = (self.repo_root.clone(), self.fingerprint.clone())
        else {
            return;
        };
        if self.seen.as_deref() == Some(fingerprint.as_str()) {
            return;
        }
        let db = lector_ui::commands_bridge::core(cx).db().clone();
        if let Err(e) = db.kv_put_json(&seen_key(&root), &fingerprint) {
            tracing::warn!("could not remember the reviewed changes: {e}");
        }
        self.seen = Some(fingerprint);
        cx.emit(ReviewModelEvent::Hint);
        cx.notify();
    }

    /// Called after every change of the repository state: recomputes the fingerprint right away
    /// and the risk analysis once the working tree has been quiet for a moment.
    pub fn refresh(&mut self, inputs: ModelInputs, cx: &mut Context<Self>) {
        let root = inputs.repo.workdir().to_path_buf();
        if !self.seen_loaded || self.repo_root.as_deref() != Some(root.as_path()) {
            let db = lector_ui::commands_bridge::core(cx).db().clone();
            self.seen = db.kv_get_json::<String>(&seen_key(&root)).ok().flatten();
            self.seen_loaded = true;
            self.repo_root = Some(root.clone());
        }
        self.uncommitted = inputs.status.len();
        if inputs.status.is_empty() {
            self.working = None;
            self.fingerprint = Some(changes_fingerprint(&root, &[]));
            self.analysis_task = None;
            self.fingerprint_task = None;
            cx.emit(ReviewModelEvent::Hint);
            cx.emit(ReviewModelEvent::Analysis);
            cx.notify();
            return;
        }
        let status = inputs.status.clone();
        let work_root = root.clone();
        self.fingerprint_task = Some(cx.spawn(async move |this, cx| {
            let fingerprint = cx
                .background_executor()
                .spawn(async move { changes_fingerprint(&work_root, &status) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.fingerprint = Some(fingerprint);
                cx.emit(ReviewModelEvent::Hint);
                cx.notify();
            });
        }));
        self.analysis_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(ANALYSIS_DELAY).await;
            let result = cx
                .background_executor()
                .spawn(async move {
                    let changes = inputs.repo.changes(&Changeset::WorkingTree).ok()?;
                    Some(analyze_changes(
                        &inputs.repo,
                        &inputs.languages,
                        &changes,
                        &inputs.diff_options,
                        inputs.detect_moved,
                    ))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.working = result.map(Arc::new);
                cx.emit(ReviewModelEvent::Analysis);
                cx.notify();
            });
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lector_diff::DiffOptions;
    use lector_git::ChangeKind;

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Ada Lovelace")
            .env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada Lovelace")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .status();
        assert!(status.is_ok_and(|s| s.success()), "git {args:?}");
    }

    #[test]
    fn fingerprint_changes_with_content_and_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").expect("write");
        let entry = |path: &str| StatusEntry {
            path: path.into(),
            unstaged: Some(ChangeKind::Modified),
            ..StatusEntry::default()
        };
        let one = changes_fingerprint(dir.path(), &[entry("a.rs")]);
        assert_eq!(one, changes_fingerprint(dir.path(), &[entry("a.rs")]));
        assert_ne!(one, changes_fingerprint(dir.path(), &[]));
        assert_ne!(
            one,
            changes_fingerprint(dir.path(), &[entry("a.rs"), entry("b.rs")])
        );
        std::fs::write(dir.path().join("a.rs"), "fn a() { 1; }\n").expect("write");
        assert_ne!(one, changes_fingerprint(dir.path(), &[entry("a.rs")]));
    }

    #[test]
    fn analysis_puts_risky_files_first_and_lock_files_last() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("Cargo.lock"), "# lock\nname = \"a\"\n").expect("write");
        std::fs::write(root.join("notes.txt"), "one\n").expect("write");
        std::fs::write(
            root.join("auth.rs"),
            "pub fn check(token: &str) -> bool {\n    token == \"x\"\n}\n",
        )
        .expect("write");
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "init"]);
        std::fs::write(
            root.join("Cargo.lock"),
            "# lock\nname = \"b\"\nversion = \"2\"\n",
        )
        .expect("write");
        std::fs::write(root.join("notes.txt"), "one\ntwo\n").expect("write");
        std::fs::write(
            root.join("auth.rs"),
            "pub fn check(token: &str, admin: bool) -> bool {\n    unsafe { std::mem::transmute::<u8, bool>(1) };\n    token == \"x\" || admin\n}\n",
        )
        .expect("write");
        let repo = Repo::discover(root).expect("repo");
        let changes = repo.changes(&Changeset::WorkingTree).expect("changes");
        let analysis = analyze_changes(
            &repo,
            &LanguageRegistry::new(),
            &changes,
            &DiffOptions::default(),
            true,
        );
        assert_eq!(analysis.order.len(), 3);
        assert_eq!(analysis.order.first().map(String::as_str), Some("auth.rs"));
        assert!(
            analysis.rank("auth.rs") < analysis.rank("Cargo.lock"),
            "attention files precede the low-risk group: {:?}",
            analysis.order
        );
        let lock = analysis.get("Cargo.lock").expect("lock");
        assert_eq!(lock.risk.group, QueueGroup::LowRisk);
        assert!(analysis.rank("auth.rs") < analysis.rank("notes.txt"));
        let auth = analysis.get("auth.rs").expect("auth");
        assert_eq!(auth.hunks.len(), 1);
    }
}
