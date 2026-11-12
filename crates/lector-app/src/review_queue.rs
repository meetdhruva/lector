//! The review queue (v0.4 review UX): what to look at first, what only moved, and what the
//! reviewer already saw.
//!
//! UI-free building blocks over [`FileDiff`]s, used by the review tab, the Changes panel and
//! tests:
//! * [`analyze`] orders the files of a changeset by [`lector_workspace::risk`] ("needs
//!   attention" first, generated / lock / negligible files in a trailing low-risk group),
//!   finds moved blocks within and across files ([`lector_diff::moves`]) and fingerprints
//!   every hunk.
//! * [`HunkFingerprint`] / [`match_marks`] / [`HunkReviewStore`]: hunk-level "reviewed"
//!   state that survives new revisions. A mark stores the hunk's content hash and a
//!   [`lector_diff::remap::Anchor`]; after the diff changed (agent turn, rebase, pull) a hunk
//!   is still reviewed when a hunk with the same content exists, identical hunks are told
//!   apart by re-anchoring the stored anchor in the new text. Hunks whose content changed
//!   start unreviewed again, all others keep their mark.
//! * [`next_unreviewed`] / [`totals`]: "next unviewed hunk across files" and the
//!   "7 of 12 files · 18 of 40 hunks reviewed" numbers.

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use lector_diff::moves::{detect_moved, detect_moved_across};
use lector_diff::remap::{reanchor_with, Anchor, ReanchorOptions};
use lector_workspace::review::{DiffKind, FileDiff};
use lector_workspace::risk::{self, RiskLevel, RiskReport};
use serde::{Deserialize, Serialize};

use crate::db::Db;

/// Minimum lines of a moved block.
pub const MOVE_MIN_LINES: u32 = 3;
/// Changesets with more text files than this skip cross-file move detection.
pub const MAX_MOVE_FILES: usize = 400;
/// Lines of context stored in an [`Anchor`].
const ANCHOR_CONTEXT: usize = 3;

/// Where a file sits in the review queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueGroup {
    /// Worth a careful look, highest risk first.
    Attention,
    /// Generated, lock and negligible files: collapsed by default.
    LowRisk,
}

/// One file of the ordered queue.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueEntry {
    /// Index into the analyzed file list.
    pub index: usize,
    /// The risk assessment.
    pub report: RiskReport,
    /// Group.
    pub group: QueueGroup,
}

impl QueueEntry {
    /// Risk bucket.
    pub fn level(&self) -> RiskLevel {
        self.report.level()
    }

    /// The sentence shown under the file name ("Changes a public function signature").
    pub fn top_reason(&self) -> Option<&str> {
        self.report.top_reason().map(|r| r.message.as_str())
    }
}

/// A run of lines that moved, seen from one side of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveSpan {
    /// Lines on this file's side (old lines for `removed`, new lines for `added`), 0-based.
    pub rows: Range<u32>,
    /// The other end: repository path (the same file for moves within a file).
    pub other_path: String,
    /// Lines at the other end, 0-based.
    pub other_lines: Range<u32>,
    /// The move stays inside this file.
    pub same_file: bool,
    /// Only the indentation differs.
    pub reindented: bool,
}

impl MoveSpan {
    /// "parser.rs:120" (1-based line) for the label.
    pub fn target_label(&self) -> String {
        let name = self
            .other_path
            .rsplit('/')
            .next()
            .unwrap_or(&self.other_path);
        format!("{name}:{}", self.other_lines.start + 1)
    }
}

/// Moved blocks of one file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileMoves {
    /// Old-side lines that now live elsewhere ("Moved to ...").
    pub removed: Vec<MoveSpan>,
    /// New-side lines that came from elsewhere ("Moved from ...").
    pub added: Vec<MoveSpan>,
}

impl FileMoves {
    /// No moved code.
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.added.is_empty()
    }

    /// The span covering new-side `line`.
    pub fn added_at(&self, line: u32) -> Option<&MoveSpan> {
        self.added.iter().find(|m| m.rows.contains(&line))
    }

    /// The span covering old-side `line`.
    pub fn removed_at(&self, line: u32) -> Option<&MoveSpan> {
        self.removed.iter().find(|m| m.rows.contains(&line))
    }
}

/// Identity of one hunk, stable across revisions that do not touch the hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkFingerprint {
    /// sha256 of the removed and added lines.
    pub hash: String,
    /// The first new line (or the line after a pure deletion) with its context.
    pub anchor: Option<Anchor>,
    /// 0-based first new line of the hunk.
    pub new_start: u32,
}

/// The result of [`analyze`], index-aligned with the analyzed files.
#[derive(Debug, Clone)]
pub struct ChangesetAnalysis {
    /// Files in review order: attention group first, each by descending risk.
    pub order: Vec<QueueEntry>,
    /// Moved code per file.
    pub moves: Vec<FileMoves>,
    /// Hunk fingerprints per file (empty for binary / oversized files).
    pub hunks: Vec<Vec<HunkFingerprint>>,
}

impl ChangesetAnalysis {
    /// Position of file `index` in [`order`](Self::order).
    pub fn position_of(&self, index: usize) -> Option<usize> {
        self.order.iter().position(|e| e.index == index)
    }

    /// The queue entry of file `index`.
    pub fn entry(&self, index: usize) -> Option<&QueueEntry> {
        self.order.iter().find(|e| e.index == index)
    }
}

/// Knobs of [`analyze`].
#[derive(Debug, Clone, Copy)]
pub struct AnalysisOptions {
    /// Look for moved blocks.
    pub detect_moved: bool,
}

impl Default for AnalysisOptions {
    fn default() -> Self {
        Self { detect_moved: true }
    }
}

/// Orders, fingerprints and finds moves for `files` (blocking; run on a background thread).
pub fn analyze(files: &[Arc<FileDiff>], opts: &AnalysisOptions) -> ChangesetAnalysis {
    let ranked = risk::rank_files(files);
    let mut attention = Vec::new();
    let mut low = Vec::new();
    for (index, report) in ranked {
        let group = if report.is_low_value() {
            QueueGroup::LowRisk
        } else {
            QueueGroup::Attention
        };
        let entry = QueueEntry {
            index,
            report,
            group,
        };
        match group {
            QueueGroup::Attention => attention.push(entry),
            QueueGroup::LowRisk => low.push(entry),
        }
    }
    attention.extend(low);
    ChangesetAnalysis {
        order: attention,
        moves: if opts.detect_moved {
            analyze_moves(files)
        } else {
            files.iter().map(|_| FileMoves::default()).collect()
        },
        hunks: files.iter().map(|f| hunk_fingerprints(f)).collect(),
    }
}

fn is_text(diff: &FileDiff) -> bool {
    diff.kind == DiffKind::Text && !diff.diff.hunks.is_empty()
}

fn side_string(side: &lector_workspace::review::DiffSide) -> String {
    side.text
        .as_ref()
        .map(|t| t.to_string())
        .unwrap_or_default()
}

fn overlaps(spans: &[MoveSpan], rows: &Range<u32>) -> bool {
    spans
        .iter()
        .any(|s| s.rows.start < rows.end && rows.start < s.rows.end)
}

/// Moved blocks inside one file only (what a single-file diff can know).
pub fn intra_file_moves(diff: &FileDiff) -> FileMoves {
    let mut out = FileMoves::default();
    if !is_text(diff) {
        return out;
    }
    let (old, new) = (side_string(&diff.old), side_string(&diff.new));
    let path = diff.change.path().to_string();
    for m in detect_moved(&old, &new, &diff.diff, MOVE_MIN_LINES) {
        out.removed.push(MoveSpan {
            rows: m.old.clone(),
            other_path: path.clone(),
            other_lines: m.new.clone(),
            same_file: true,
            reindented: m.reindented,
        });
        out.added.push(MoveSpan {
            rows: m.new,
            other_path: path.clone(),
            other_lines: m.old,
            same_file: true,
            reindented: m.reindented,
        });
    }
    out
}

/// Moved blocks within and across the files of a changeset (index-aligned with `files`).
pub fn analyze_moves(files: &[Arc<FileDiff>]) -> Vec<FileMoves> {
    let mut out: Vec<FileMoves> = files.iter().map(|f| intra_file_moves(f)).collect();
    let text: Vec<usize> = files
        .iter()
        .enumerate()
        .filter(|(_, f)| is_text(f))
        .map(|(i, _)| i)
        .collect();
    if text.len() < 2 || text.len() > MAX_MOVE_FILES {
        return out;
    }
    let sides: Vec<(String, String)> = text
        .iter()
        .map(|&i| (side_string(&files[i].old), side_string(&files[i].new)))
        .collect();
    let inputs: Vec<(&str, &str, &lector_diff::LineDiff)> = text
        .iter()
        .zip(&sides)
        .map(|(&i, (o, n))| (o.as_str(), n.as_str(), &files[i].diff))
        .collect();
    for m in detect_moved_across(&inputs) {
        let (from, to) = (text[m.old_file], text[m.new_file]);
        if overlaps(&out[from].removed, &m.old) || overlaps(&out[to].added, &m.new) {
            continue;
        }
        let from_path = files[from]
            .change
            .old_path
            .clone()
            .unwrap_or_else(|| files[from].change.path().to_string());
        let to_path = files[to].change.path().to_string();
        out[from].removed.push(MoveSpan {
            rows: m.old.clone(),
            other_path: to_path,
            other_lines: m.new.clone(),
            same_file: false,
            reindented: m.reindented,
        });
        out[to].added.push(MoveSpan {
            rows: m.new,
            other_path: from_path,
            other_lines: m.old,
            same_file: false,
            reindented: m.reindented,
        });
    }
    for f in &mut out {
        f.removed.sort_by_key(|s| s.rows.start);
        f.added.sort_by_key(|s| s.rows.start);
    }
    out
}

// ---------------------------------------------------------------------------
// Hunk fingerprints and marks

fn strip_cr(s: &str) -> &str {
    s.strip_suffix('\r').unwrap_or(s)
}

/// Fingerprints of the hunks of `diff`, in hunk order.
pub fn hunk_fingerprints(diff: &FileDiff) -> Vec<HunkFingerprint> {
    if diff.kind != DiffKind::Text {
        return Vec::new();
    }
    let new_count = diff.new.line_count();
    let new_line = |r: u32| strip_cr(&diff.new.line(r)).to_string();
    diff.diff
        .hunks
        .iter()
        .map(|h| {
            let mut text = String::new();
            for r in h.old.clone() {
                text.push('-');
                text.push_str(strip_cr(&diff.old.line(r)));
                text.push('\n');
            }
            for r in h.new.clone() {
                text.push('+');
                text.push_str(strip_cr(&diff.new.line(r)));
                text.push('\n');
            }
            let line = h.new.start;
            let anchor = (line < new_count).then(|| Anchor {
                line,
                text: new_line(line),
                context_before: (line.saturating_sub(ANCHOR_CONTEXT as u32)..line)
                    .map(new_line)
                    .collect(),
                context_after: (line + 1..(line + 1 + ANCHOR_CONTEXT as u32).min(new_count))
                    .map(new_line)
                    .collect(),
            });
            HunkFingerprint {
                hash: lector_config::content_hash(text.as_bytes()),
                anchor,
                new_start: line,
            }
        })
        .collect()
}

/// A persisted "reviewed" mark.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMark {
    /// [`HunkFingerprint::hash`].
    pub hash: String,
    /// Stored anchor.
    #[serde(default)]
    pub anchor: Option<StoredAnchor>,
    /// First new line when the mark was made.
    pub new_start: u32,
}

/// Serializable [`Anchor`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAnchor {
    /// Line.
    pub line: u32,
    /// Text of the line.
    pub text: String,
    /// Lines before.
    pub before: Vec<String>,
    /// Lines after.
    pub after: Vec<String>,
}

impl From<&Anchor> for StoredAnchor {
    fn from(a: &Anchor) -> Self {
        StoredAnchor {
            line: a.line,
            text: a.text.clone(),
            before: a.context_before.clone(),
            after: a.context_after.clone(),
        }
    }
}

impl From<&StoredAnchor> for Anchor {
    fn from(a: &StoredAnchor) -> Self {
        Anchor {
            line: a.line,
            text: a.text.clone(),
            context_before: a.before.clone(),
            context_after: a.after.clone(),
        }
    }
}

impl StoredMark {
    /// The mark of `fingerprint`.
    pub fn of(fingerprint: &HunkFingerprint) -> Self {
        StoredMark {
            hash: fingerprint.hash.clone(),
            anchor: fingerprint.anchor.as_ref().map(StoredAnchor::from),
            new_start: fingerprint.new_start,
        }
    }
}

/// Which of `current` hunks are reviewed according to `stored`. Hunks match by content hash; when
/// several hunks share a hash, each stored mark goes to the unclaimed hunk nearest to where its
/// anchor re-anchors in `new_text` ([`lector_diff::remap::reanchor_with`]). The result has one
/// entry per current hunk.
pub fn match_marks(
    current: &[HunkFingerprint],
    stored: &[StoredMark],
    new_text: Option<&str>,
) -> Vec<bool> {
    let mut reviewed = vec![false; current.len()];
    let mut by_hash: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, h) in current.iter().enumerate() {
        by_hash.entry(h.hash.as_str()).or_default().push(i);
    }
    let mut marks: Vec<&StoredMark> = stored.iter().collect();
    marks.sort_by_key(|m| m.new_start);
    for mark in marks {
        let Some(candidates) = by_hash.get(mark.hash.as_str()) else {
            continue;
        };
        let target = match (&mark.anchor, new_text) {
            (Some(anchor), Some(text)) if candidates.len() > 1 => reanchor_with(
                &Anchor::from(anchor),
                text,
                &ReanchorOptions {
                    hint: Some(mark.new_start),
                    ..ReanchorOptions::default()
                },
            )
            .map(|r| r.line),
            _ => None,
        }
        .unwrap_or(mark.new_start);
        let best = candidates
            .iter()
            .copied()
            .filter(|&i| !reviewed[i])
            .min_by_key(|&i| current[i].new_start.abs_diff(target));
        if let Some(i) = best {
            reviewed[i] = true;
        }
    }
    reviewed
}

/// Reviewed hunks persisted in the application database (Lector's own state).
#[derive(Clone)]
pub struct HunkReviewStore {
    db: Db,
}

impl HunkReviewStore {
    /// A store over `db`.
    pub fn new(db: &Db) -> Self {
        Self { db: db.clone() }
    }

    fn key(repo: &Path, changeset: &str, path: &str) -> String {
        format!("review.hunks/{}/{changeset}/{path}", repo.display())
    }

    /// The marks stored for a file (empty when none or unreadable).
    pub fn load(&self, repo: &Path, changeset: &str, path: &str) -> Vec<StoredMark> {
        self.db
            .kv_get_json::<Vec<StoredMark>>(&Self::key(repo, changeset, path))
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    /// Replaces the marks of a file (an empty list removes the entry).
    pub fn save(&self, repo: &Path, changeset: &str, path: &str, marks: &[StoredMark]) {
        let key = Self::key(repo, changeset, path);
        let result = if marks.is_empty() {
            self.db.kv_put(&key, None)
        } else {
            self.db.kv_put_json(&key, &marks)
        };
        if let Err(e) = result {
            tracing::warn!("could not save reviewed hunks: {e}");
        }
    }

    /// Which hunks of `diff` are reviewed.
    pub fn reviewed(
        &self,
        repo: &Path,
        changeset: &str,
        path: &str,
        hunks: &[HunkFingerprint],
        new_text: Option<&str>,
    ) -> Vec<bool> {
        if hunks.is_empty() {
            return Vec::new();
        }
        match_marks(hunks, &self.load(repo, changeset, path), new_text)
    }

    /// Persists the reviewed state of every hunk of a file.
    pub fn store_reviewed(
        &self,
        repo: &Path,
        changeset: &str,
        path: &str,
        hunks: &[HunkFingerprint],
        reviewed: &[bool],
    ) {
        let marks: Vec<StoredMark> = hunks
            .iter()
            .zip(reviewed)
            .filter(|(_, r)| **r)
            .map(|(h, _)| StoredMark::of(h))
            .collect();
        self.save(repo, changeset, path, &marks);
    }
}

// ---------------------------------------------------------------------------
// Progress and "next unreviewed"

/// Review state of one file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileProgress {
    /// Reviewed flag per hunk (empty for files without hunks).
    pub hunks: Vec<bool>,
    /// The file is marked viewed.
    pub viewed: bool,
}

impl FileProgress {
    /// The file counts as reviewed.
    pub fn is_done(&self) -> bool {
        self.viewed || (!self.hunks.is_empty() && self.hunks.iter().all(|r| *r))
    }

    /// Hunks that count as reviewed (all of them for a viewed file).
    pub fn reviewed_hunks(&self) -> usize {
        if self.viewed {
            self.hunks.len()
        } else {
            self.hunks.iter().filter(|r| **r).count()
        }
    }

    /// Whether hunk `k` counts as reviewed.
    pub fn hunk_reviewed(&self, k: usize) -> bool {
        self.viewed || self.hunks.get(k).copied().unwrap_or(false)
    }
}

/// Counts for the progress bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Totals {
    /// Files reviewed.
    pub files_done: usize,
    /// All files.
    pub files: usize,
    /// Hunks reviewed.
    pub hunks_done: usize,
    /// All hunks.
    pub hunks: usize,
}

/// "7 of 12 files · 18 of 40 hunks reviewed".
pub fn totals(progress: &[FileProgress]) -> Totals {
    Totals {
        files_done: progress.iter().filter(|p| p.is_done()).count(),
        files: progress.len(),
        hunks_done: progress.iter().map(|p| p.reviewed_hunks()).sum(),
        hunks: progress.iter().map(|p| p.hunks.len()).sum(),
    }
}

impl Totals {
    /// The progress sentence.
    pub fn label(&self) -> String {
        if self.hunks > 0 {
            format!(
                "{} of {} files · {} of {} hunks reviewed",
                self.files_done, self.files, self.hunks_done, self.hunks
            )
        } else {
            format!("{} of {} files reviewed", self.files_done, self.files)
        }
    }

    /// Everything reviewed.
    pub fn complete(&self) -> bool {
        self.files > 0 && self.files_done == self.files
    }
}

/// A place in the queue: the file's position in the review order and a hunk (`None` for files
/// without hunks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Position in the order slice.
    pub file: usize,
    /// Hunk index within the file.
    pub hunk: Option<usize>,
}

/// The next unreviewed hunk after `from` (a file without hunks counts as one unit until it is
/// viewed), walking `order` and wrapping around; `from` itself is tried last. `None` when
/// everything is reviewed. `order` holds indices into `progress`.
pub fn next_unreviewed(
    progress: &[FileProgress],
    order: &[usize],
    from: Option<Position>,
) -> Option<Position> {
    let units = |file: usize| -> Vec<Option<usize>> {
        let Some(p) = order.get(file).and_then(|&i| progress.get(i)) else {
            return Vec::new();
        };
        if p.viewed {
            Vec::new()
        } else if p.hunks.is_empty() {
            vec![None]
        } else {
            p.hunks
                .iter()
                .enumerate()
                .filter(|(_, r)| !**r)
                .map(|(k, _)| Some(k))
                .collect()
        }
    };
    if order.is_empty() {
        return None;
    }
    let (start_file, after) = match from {
        Some(p) => (p.file.min(order.len() - 1), Some(p)),
        None => (0, None),
    };
    // Rest of the current file after the current hunk.
    if let Some(p) = after {
        let cur = p.hunk.map_or(-1, |k| k as isize);
        if let Some(hunk) = units(start_file)
            .into_iter()
            .find(|u| u.map_or(-1, |k| k as isize) > cur)
        {
            return Some(Position {
                file: start_file,
                hunk,
            });
        }
    } else if let Some(&hunk) = units(start_file).first() {
        return Some(Position {
            file: start_file,
            hunk,
        });
    }
    // Following files, wrapping; the start file's earlier hunks come last.
    for d in 1..=order.len() {
        let file = (start_file + d) % order.len();
        if let Some(&hunk) = units(file).first() {
            return Some(Position { file, hunk });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use lector_diff::DiffOptions;
    use lector_git::{ChangeKind, FileChange, Side};
    use lector_syntax::LanguageRegistry;
    use lector_workspace::review::diff_bytes;

    fn diff_for(path: &str, old: Option<&str>, new: Option<&str>) -> Arc<FileDiff> {
        let change = FileChange {
            old_path: old.map(|_| path.to_string()),
            new_path: new.map(|_| path.to_string()),
            kind: match (old, new) {
                (None, _) => ChangeKind::Added,
                (_, None) => ChangeKind::Deleted,
                _ => ChangeKind::Modified,
            },
            old: Side::None,
            new: Side::None,
            binary: None,
        };
        Arc::new(diff_bytes(
            change,
            old.map(|s| s.as_bytes().to_vec()),
            new.map(|s| s.as_bytes().to_vec()),
            &LanguageRegistry::new(),
            &DiffOptions::default(),
        ))
    }

    const PARSER: &str = "pub fn parse_header(input: &str) -> Header {\n    let mut parts = input.split(':');\n    let name = parts.next().unwrap_or(\"\");\n    let value = parts.next().unwrap_or(\"\");\n    Header { name: name.into(), value: value.into() }\n}\n";

    fn filler(n: usize) -> String {
        (0..n)
            .map(|i| format!("// filler line number {i}\n"))
            .collect()
    }

    #[test]
    fn queue_puts_risky_files_first_and_collapses_generated() {
        let calm = diff_for("src/util.rs", Some("fn a() {}\n"), Some("fn a() { 1 }\n"));
        let lock = diff_for(
            "Cargo.lock",
            Some("# lock\nversion = 1\n"),
            Some("# lock\nversion = 2\n"),
        );
        let risky = diff_for(
            "src/api.rs",
            Some("pub fn load(path: &str) -> String {\n    String::new()\n}\n"),
            Some("pub fn load(path: &str, strict: bool) -> String {\n    String::new()\n}\n"),
        );
        let files = vec![calm, lock, risky];
        let analysis = analyze(&files, &AnalysisOptions::default());
        let order: Vec<usize> = analysis.order.iter().map(|e| e.index).collect();
        assert_eq!(order[0], 2, "public signature change first: {order:?}");
        let groups: Vec<QueueGroup> = analysis.order.iter().map(|e| e.group).collect();
        let first_low = groups.iter().position(|g| *g == QueueGroup::LowRisk);
        assert!(
            first_low.is_some_and(|i| groups[i..].iter().all(|g| *g == QueueGroup::LowRisk)),
            "low-risk files trail the attention group: {order:?} {groups:?}"
        );
        let lock_entry = analysis.entry(1).unwrap();
        assert_eq!(lock_entry.group, QueueGroup::LowRisk);
        assert_eq!(analysis.entry(2).unwrap().group, QueueGroup::Attention);
        assert!(analysis.entry(2).unwrap().top_reason().is_some());
        assert_eq!(analysis.hunks[2].len(), 1);
    }

    #[test]
    fn moves_within_and_across_files() {
        // Within a file: PARSER moves from the top to the bottom.
        let old = format!("{PARSER}{}fn tail() {{}}\n", filler(8));
        let new = format!("{}fn tail() {{}}\n{PARSER}", filler(8));
        let same = diff_for("src/a.rs", Some(&old), Some(&new));
        let m = intra_file_moves(&same);
        assert_eq!(m.removed.len(), 1);
        assert_eq!(m.added.len(), 1);
        assert!(m.added[0].same_file);
        assert_eq!(m.added[0].other_lines.start, 0);
        assert_eq!(m.added_at(m.added[0].rows.start), Some(&m.added[0]));

        // Across files: PARSER leaves parser.rs and appears in lexer.rs.
        let from = diff_for(
            "src/parser.rs",
            Some(&format!("{}{PARSER}", filler(3))),
            Some(&filler(3)),
        );
        let to = diff_for(
            "src/lexer.rs",
            Some(&filler(5)),
            Some(&format!("{}{PARSER}", filler(5))),
        );
        let files = vec![from, to];
        let moves = analyze_moves(&files);
        assert_eq!(moves[0].removed.len(), 1);
        assert_eq!(moves[0].removed[0].other_path, "src/lexer.rs");
        assert!(!moves[0].removed[0].same_file);
        assert_eq!(moves[1].added.len(), 1);
        assert_eq!(moves[1].added[0].other_path, "src/parser.rs");
        assert_eq!(moves[1].added[0].target_label(), "parser.rs:4");
    }

    #[test]
    fn moves_can_be_switched_off() {
        let a = diff_for("a.rs", Some("fn a() {}\n"), Some("fn b() {}\n"));
        let analysis = analyze(
            &[a],
            &AnalysisOptions {
                detect_moved: false,
            },
        );
        assert!(analysis.moves[0].is_empty());
    }

    fn numbered(n: usize, edit: &[usize]) -> String {
        (0..n)
            .map(|i| {
                if edit.contains(&i) {
                    format!("let value_{i} = compute({i}); // changed\n")
                } else {
                    format!("let value_{i} = compute({i});\n")
                }
            })
            .collect()
    }

    #[test]
    fn marks_survive_new_revisions_and_reset_changed_hunks() {
        let base = numbered(60, &[]);
        // Revision 1: hunks at 5, 25 and 45.
        let rev1 = numbered(60, &[5, 25, 45]);
        let d1 = diff_for("a.rs", Some(&base), Some(&rev1));
        let h1 = hunk_fingerprints(&d1);
        assert_eq!(h1.len(), 3);
        // The reviewer approves the first two.
        let stored: Vec<StoredMark> = h1[..2].iter().map(StoredMark::of).collect();

        // Revision 2: the agent adds lines above (everything shifts), edits hunk 3's
        // neighbour and leaves hunks 1 and 2 alone, then changes hunk 2 itself in rev 3.
        let rev2 = format!("// header\n// header 2\n{}", numbered(60, &[5, 25, 45]));
        let d2 = diff_for(
            "a.rs",
            Some(&format!("// header\n// header 2\n{base}")),
            Some(&rev2),
        );
        let h2 = hunk_fingerprints(&d2);
        assert_eq!(match_marks(&h2, &stored, Some(&rev2)), [true, true, false]);

        // The agent replaces hunk 2's edit by a different one (line 26 instead of 25): only
        // that mark resets.
        let rev3 = numbered(60, &[5, 45])
            .replace("let value_26 = compute(26);", "let value_26 = other(26);");
        let d3 = diff_for("a.rs", Some(&base), Some(&rev3));
        let h3 = hunk_fingerprints(&d3);
        let reviewed = match_marks(&h3, &stored, Some(&rev3));
        assert_eq!(reviewed.len(), 3);
        assert!(reviewed[0], "untouched hunk stays reviewed");
        assert!(
            !reviewed[1],
            "the hunk the agent edited is unreviewed again"
        );
    }

    #[test]
    fn identical_hunks_are_told_apart_by_their_anchor() {
        // Three identical edits at different places.
        let base: String = (0..90)
            .map(|i| {
                if i % 30 == 10 {
                    "same();\n".to_string()
                } else {
                    format!("unique_{i}();\n")
                }
            })
            .collect();
        let new = base.replace("same();", "same(1);");
        let d = diff_for("a.rs", Some(&base), Some(&new));
        let h = hunk_fingerprints(&d);
        assert_eq!(h.len(), 3);
        assert_eq!(h[0].hash, h[1].hash);
        // Only the middle one was reviewed (a mark at line 40).
        let stored = vec![StoredMark::of(&h[1])];
        // New revision: ten lines inserted at the top, shifting everything.
        let shifted = format!("{}{new}", "added();\n".repeat(10));
        let shifted_base = format!("{}{base}", "added();\n".repeat(10));
        let d2 = diff_for("a.rs", Some(&shifted_base), Some(&shifted));
        let h2 = hunk_fingerprints(&d2);
        assert_eq!(
            match_marks(&h2, &stored, Some(&shifted)),
            [false, true, false]
        );
    }

    #[test]
    fn store_round_trip() {
        let db = Db::in_memory();
        let store = HunkReviewStore::new(&db);
        let base = numbered(30, &[]);
        let d = diff_for("a.rs", Some(&base), Some(&numbered(30, &[3, 20])));
        let h = hunk_fingerprints(&d);
        let repo = Path::new("/repo");
        assert_eq!(
            store.reviewed(repo, "worktree", "a.rs", &h, None),
            [false, false]
        );
        store.store_reviewed(repo, "worktree", "a.rs", &h, &[true, false]);
        assert_eq!(
            store.reviewed(repo, "worktree", "a.rs", &h, None),
            [true, false]
        );
        assert!(store
            .reviewed(repo, "staged", "a.rs", &h, None)
            .iter()
            .all(|r| !r));
        store.store_reviewed(repo, "worktree", "a.rs", &h, &[false, false]);
        assert!(store.load(repo, "worktree", "a.rs").is_empty());
    }

    fn p(hunks: &[bool], viewed: bool) -> FileProgress {
        FileProgress {
            hunks: hunks.to_vec(),
            viewed,
        }
    }

    #[test]
    fn totals_and_label() {
        let progress = [
            p(&[true, true], false),
            p(&[true, false, false], false),
            p(&[false], true),
            p(&[], false),
        ];
        let t = totals(&progress);
        assert_eq!(
            t,
            Totals {
                files_done: 2,
                files: 4,
                hunks_done: 4,
                hunks: 6
            }
        );
        assert_eq!(t.label(), "2 of 4 files · 4 of 6 hunks reviewed");
        assert!(!t.complete());
        assert_eq!(totals(&[p(&[], false)]).label(), "0 of 1 files reviewed");
    }

    #[test]
    fn next_unreviewed_walks_files_in_queue_order_and_wraps() {
        let progress = vec![
            p(&[true, true], false),   // file 0: done
            p(&[true, false], false),  // file 1: hunk 1 open
            p(&[false, false], false), // file 2
            p(&[], false),             // file 3: no hunks, not viewed
        ];
        // Queue order differs from file order.
        let order = [2, 1, 3, 0];
        let first = next_unreviewed(&progress, &order, None).unwrap();
        assert_eq!(
            first,
            Position {
                file: 0,
                hunk: Some(0)
            },
            "file 2 hunk 0"
        );
        let second = next_unreviewed(&progress, &order, Some(first)).unwrap();
        assert_eq!(
            second,
            Position {
                file: 0,
                hunk: Some(1)
            }
        );
        let third = next_unreviewed(&progress, &order, Some(second)).unwrap();
        assert_eq!(
            third,
            Position {
                file: 1,
                hunk: Some(1)
            },
            "then file 1"
        );
        let fourth = next_unreviewed(&progress, &order, Some(third)).unwrap();
        assert_eq!(
            fourth,
            Position {
                file: 2,
                hunk: None
            },
            "file 3 has no hunks"
        );
        let wrapped = next_unreviewed(&progress, &order, Some(fourth)).unwrap();
        assert_eq!(
            wrapped,
            Position {
                file: 0,
                hunk: Some(0)
            }
        );
        // All reviewed: nothing.
        let done = vec![p(&[true], false), p(&[], true)];
        assert_eq!(next_unreviewed(&done, &[0, 1], None), None);
        assert_eq!(next_unreviewed(&done, &[], None), None);
        // Only the current position is open: it comes back around.
        let one = vec![p(&[false], false), p(&[true], false)];
        assert_eq!(
            next_unreviewed(
                &one,
                &[0, 1],
                Some(Position {
                    file: 0,
                    hunk: Some(0)
                })
            ),
            Some(Position {
                file: 0,
                hunk: Some(0)
            })
        );
    }
}
