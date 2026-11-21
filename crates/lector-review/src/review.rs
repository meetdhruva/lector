//! Review model: changesets with per-file stats and "viewed" state, and file
//! diffs (both sides loaded, parsed and diffed) ready for split or unified
//! presentation.
//!
//! Everything here is blocking; call it from a background thread. Functions take
//! the handles they read through (a [`Repo`], a [`LanguageRegistry`]), the viewed marks as a
//! read-only input ([`ViewedMarks`]) and, where they parse or diff, a [`DerivedCache`].
//! Nothing here writes: viewed marks are stored, and hunks applied, by `lector-app`.

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use crate::content::content_hash;
use lector_base::CancelToken;
use lector_diff::{
    diff_lines, fold_context, refine_inline, split_rows, unified_rows, Algorithm, DiffOptions,
    DiffStats, InlineChanges, LineDiff, RowKind, Segment, SplitRow, UnifiedRow, Whitespace,
};
use lector_fs::{FileKind, ImageFormat};
use lector_git::{Changeset, FileChange, Oid, Repo, Side};
use lector_syntax::{HighlightId, Language, LanguageRegistry, Syntax};
use lector_text::Text;
use parking_lot::Mutex;

/// Files larger than this (either side) are summarized instead of diffed.
pub const MAX_DIFF_BYTES: u64 = 5 * 1024 * 1024;

/// Changesets with more files than this get no eager stats (computed on demand).
pub const MAX_EAGER_STATS: usize = 3000;

use crate::check_cancel;
use crate::content::{ContentId, DerivedCache};
pub use crate::{Error, Result};

/// A changeset ready for review: files with stats and viewed state.
#[derive(Clone, Debug)]
pub struct ChangesetView {
    /// Stable key for the viewed marks ("worktree", "commit:<sha>", "branch:main...feature").
    pub key: String,
    /// Human title ("Working tree", "abc1234 Fix parser", "main…feature").
    pub title: String,
    /// The changeset.
    pub changeset: Changeset,
    /// Changed files, sorted by path.
    pub files: Vec<ReviewFile>,
}

impl ChangesetView {
    /// Number of files marked viewed.
    pub fn viewed_count(&self) -> usize {
        self.files.iter().filter(|f| f.viewed).count()
    }

    /// Sum of all known file stats.
    pub fn total_stats(&self) -> DiffStats {
        let mut total = DiffStats::default();
        for s in self.files.iter().filter_map(|f| f.stats) {
            total.added += s.added;
            total.removed += s.removed;
        }
        total
    }

    /// Index of the file whose path (new or old) is `path`.
    pub fn index_of(&self, path: &str) -> Option<usize> {
        self.files.iter().position(|f| {
            f.change.new_path.as_deref() == Some(path) || f.change.old_path.as_deref() == Some(path)
        })
    }
}

/// One file of a [`ChangesetView`].
#[derive(Clone, Debug)]
pub struct ReviewFile {
    /// The change.
    pub change: FileChange,
    /// Path for display: `new`, or `old → new` for renames/copies.
    pub display_path: String,
    /// Added/removed line counts (`None`: binary, too large, or not computed).
    pub stats: Option<DiffStats>,
    /// Marked viewed at the current content.
    pub viewed: bool,
    /// Hash identifying the diff content (both sides) for the viewed marks.
    pub content_hash: String,
    /// Either side is binary (known after loading).
    pub binary: bool,
}

impl ReviewFile {
    /// The path to show / open (new path, or old for deletions).
    pub fn path(&self) -> &str {
        self.change.path()
    }
}

/// Stable viewed-marks key for a changeset.
pub fn changeset_key(cs: &Changeset) -> String {
    match cs {
        Changeset::Unstaged => "unstaged".into(),
        Changeset::Staged => "staged".into(),
        Changeset::Untracked => "untracked".into(),
        Changeset::WorkingTree => "worktree".into(),
        Changeset::Commit(oid) => format!("commit:{oid}"),
        Changeset::Range { from, to } => format!("range:{from}..{to}"),
        Changeset::BranchVsBase { branch, base } => format!("branch:{base}...{branch}"),
        Changeset::Snapshots { from, to } => format!("snapshots:{from}..{to}"),
    }
}

/// Inverse of [`changeset_key`] (plus `workdir:<rev>` from
/// [`load_vs_workdir_with`]), for restoring tabs from a session.
pub fn parse_changeset_key(key: &str) -> Option<Changeset> {
    Some(match key {
        "unstaged" => Changeset::Unstaged,
        "staged" => Changeset::Staged,
        "untracked" => Changeset::Untracked,
        "worktree" => Changeset::WorkingTree,
        _ => {
            let (kind, rest) = key.split_once(':')?;
            match kind {
                "commit" => Changeset::Commit(rest.parse().ok()?),
                "range" => {
                    let (from, to) = rest.split_once("..")?;
                    Changeset::Range {
                        from: from.into(),
                        to: to.into(),
                    }
                }
                "branch" => {
                    let (base, branch) = rest.split_once("...")?;
                    Changeset::BranchVsBase {
                        branch: branch.into(),
                        base: base.into(),
                    }
                }
                "snapshots" => {
                    let (from, to) = rest.split_once("..")?;
                    Changeset::Snapshots {
                        from: from.parse().ok()?,
                        to: to.parse().ok()?,
                    }
                }
                "workdir" => Changeset::Range {
                    from: rest.into(),
                    to: WORKDIR_REV.into(),
                },
                _ => return None,
            }
        }
    })
}

/// Human title for a changeset. Commits need the summary, so pass the repo.
pub fn changeset_title(repo: Option<&Repo>, cs: &Changeset) -> String {
    match cs {
        Changeset::Unstaged => "Unstaged changes".into(),
        Changeset::Staged => "Staged changes".into(),
        Changeset::Untracked => "Untracked files".into(),
        Changeset::WorkingTree => "Working tree".into(),
        Changeset::Commit(oid) => {
            let summary = repo
                .and_then(|r| r.commit_infos(&[*oid]).ok())
                .and_then(|mut v| v.pop())
                .map(|c| c.summary)
                .unwrap_or_default();
            if summary.is_empty() {
                oid.short()
            } else {
                format!("{} {}", oid.short(), summary)
            }
        }
        Changeset::Range { from, to } => format!("{from}..{to}"),
        Changeset::BranchVsBase { branch, base } => format!("{base}…{branch}"),
        Changeset::Snapshots { from, to } => repo
            .and_then(|r| lector_git::ShadowRepo::known_note(r, *to))
            .unwrap_or_else(|| format!("Checkpoint {} → {}", from.short(), to.short())),
    }
}

/// [`DiffOptions`] for the `[diff] ignore_whitespace` setting (histogram algorithm).
pub fn diff_options(ignore_whitespace: bool) -> DiffOptions {
    DiffOptions {
        algorithm: Algorithm::Histogram,
        whitespace: if ignore_whitespace {
            Whitespace::IgnoreAll
        } else {
            Whitespace::Exact
        },
    }
}

fn display_path(change: &FileChange) -> String {
    match (&change.old_path, &change.new_path) {
        (Some(old), Some(new)) if old != new => format!("{old} → {new}"),
        _ => change.path().to_string(),
    }
}

/// Loaded bytes of one side, or a reason they were not loaded.
pub(crate) enum Loaded {
    Missing,
    TooLarge(u64),
    Bytes(Vec<u8>),
}

fn side_size_hint(repo: &Repo, side: &Side) -> Option<u64> {
    match side {
        Side::Workdir(p) => std::fs::metadata(repo.workdir().join(p))
            .ok()
            .map(|m| m.len()),
        _ => None,
    }
}

fn load_side(repo: &Repo, side: &Side) -> Result<Loaded> {
    if matches!(side, Side::None) {
        return Ok(Loaded::Missing);
    }
    if let Some(size) = side_size_hint(repo, side) {
        if size > MAX_DIFF_BYTES {
            return Ok(Loaded::TooLarge(size));
        }
    }
    Ok(match repo.load(side)? {
        None => Loaded::Missing,
        Some(b) if b.len() as u64 > MAX_DIFF_BYTES => Loaded::TooLarge(b.len() as u64),
        Some(b) => Loaded::Bytes(b),
    })
}

/// Mirrors git's checkout line-ending conversion (`core.autocrlf`, `eol=crlf`) for a
/// working-tree file compared with a stored version: when the stored side uses LF only
/// and the working file uses CRLF, the working file is compared with LF endings, as git
/// does after its clean filter. Without this, every line of every file would show as
/// changed for Windows users with `core.autocrlf=true`.
pub(crate) fn normalize_checkout_eol(stored: Option<&[u8]>, working: Vec<u8>) -> Vec<u8> {
    let Some(stored) = stored else {
        return working;
    };
    if memchr::memchr(b'\r', stored).is_some() || memchr::memmem::find(&working, b"\r\n").is_none()
    {
        return working;
    }
    let mut out = Vec::with_capacity(working.len());
    let mut rest = working.as_slice();
    while let Some(at) = memchr::memmem::find(rest, b"\r\n") {
        out.extend_from_slice(&rest[..at]);
        out.push(b'\n');
        rest = &rest[at + 2..];
    }
    out.extend_from_slice(rest);
    out
}

/// Applies [`normalize_checkout_eol`] when `new_side` is the working tree.
pub(crate) fn normalize_loaded_pair(old: &Loaded, new_side: &Side, new: Loaded) -> Loaded {
    match (old, new_side, new) {
        (Loaded::Bytes(stored), Side::Workdir(_), Loaded::Bytes(working)) => {
            Loaded::Bytes(normalize_checkout_eol(Some(stored), working))
        }
        (_, _, new) => new,
    }
}

fn side_identity(side: &Side, loaded: &Loaded) -> String {
    match (side, loaded) {
        (Side::Blob(oid), _) => format!("blob:{oid}"),
        (_, Loaded::Missing) => "none".into(),
        (_, Loaded::TooLarge(n)) => format!("large:{n}"),
        (_, Loaded::Bytes(b)) => content_hash(b),
    }
}

struct FileSummary {
    hash: String,
    stats: Option<DiffStats>,
    binary: bool,
}

fn summarize(repo: &Repo, change: &FileChange, opts: &DiffOptions) -> Result<FileSummary> {
    let old = load_side(repo, &change.old)?;
    let new = normalize_loaded_pair(&old, &change.new, load_side(repo, &change.new)?);
    let hash = content_hash(
        format!(
            "{}|{}",
            side_identity(&change.old, &old),
            side_identity(&change.new, &new)
        )
        .as_bytes(),
    );
    fn bytes(l: &Loaded) -> Option<&[u8]> {
        match l {
            Loaded::Bytes(b) => Some(b.as_slice()),
            Loaded::Missing => Some(&[][..]),
            Loaded::TooLarge(_) => None,
        }
    }
    let (stats, binary) = match (bytes(&old), bytes(&new)) {
        (Some(o), Some(n)) => {
            if lector_diff::is_binary(o) || lector_diff::is_binary(n) {
                (None, true)
            } else {
                let (o, _, _) = lector_fs::decode(o);
                let (n, _, _) = lector_fs::decode(n);
                let diff = diff_text(&o, &n, opts);
                (Some(diff.stats()), false)
            }
        }
        _ => (None, false),
    };
    Ok(FileSummary {
        hash,
        stats,
        binary,
    })
}

/// Diff that treats an empty side as zero lines (an added file is all `Added`).
fn diff_text(old: &str, new: &str, opts: &DiffOptions) -> LineDiff {
    // `diff_lines("")` sees one empty line; present additions/deletions of
    // whole files as pure inserts/removals of their real lines (a trailing
    // newline does not count as an extra empty line), like `git diff`.
    let real_lines = |s: &str| {
        let n = s.bytes().filter(|&b| b == b'\n').count() as u32;
        if s.ends_with('\n') {
            n
        } else {
            n + 1
        }
    };
    if old.is_empty() && !new.is_empty() {
        let n = real_lines(new);
        return LineDiff {
            hunks: vec![lector_diff::Hunk {
                old: 0..0,
                new: 0..n,
            }],
            old_lines: 0,
            new_lines: n,
        };
    }
    if new.is_empty() && !old.is_empty() {
        let n = real_lines(old);
        return LineDiff {
            hunks: vec![lector_diff::Hunk {
                old: 0..n,
                new: 0..0,
            }],
            old_lines: n,
            new_lines: 0,
        };
    }
    diff_lines(old, new, opts)
}

/// Stats, binary flag and content hash for one change (loads both sides).
pub fn file_summary(
    repo: &Repo,
    change: &FileChange,
    opts: &DiffOptions,
) -> Result<(Option<DiffStats>, bool, String)> {
    let s = summarize(repo, change, opts)?;
    Ok((s.stats, s.binary, s.hash))
}

/// Read-only view of the "viewed" marks, supplied by the caller (the app owns the store).
///
/// A mark counts only while the content hash recorded with it equals the file's current one.
/// Any `Fn(changeset_key, path, content_hash) -> bool` is a `ViewedMarks`; [`NoMarks`] has none.
pub trait ViewedMarks {
    /// Whether `path` of the changeset `changeset_key` is marked viewed at `content_hash`.
    fn is_viewed(&self, changeset_key: &str, path: &str, content_hash: &str) -> bool;
}

impl<F: Fn(&str, &str, &str) -> bool> ViewedMarks for F {
    fn is_viewed(&self, changeset_key: &str, path: &str, content_hash: &str) -> bool {
        self(changeset_key, path, content_hash)
    }
}

/// [`ViewedMarks`] with nothing viewed.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoMarks;

impl ViewedMarks for NoMarks {
    fn is_viewed(&self, _: &str, _: &str, _: &str) -> bool {
        false
    }
}

/// Loads a changeset with stats and viewed state, using `repo` directly.
pub fn load_changeset_with(
    repo: &Repo,
    cs: &Changeset,
    viewed: &dyn ViewedMarks,
    opts: &DiffOptions,
) -> Result<ChangesetView> {
    load_changeset_cancellable(repo, cs, viewed, opts, &CancelToken::new())
}

/// [`load_changeset_with`] that stops with [`Error::Cancelled`] soon after `cancel` is set
/// (checked between files; a file being summarized finishes first).
pub fn load_changeset_cancellable(
    repo: &Repo,
    cs: &Changeset,
    viewed: &dyn ViewedMarks,
    opts: &DiffOptions,
    cancel: &CancelToken,
) -> Result<ChangesetView> {
    check_cancel(cancel)?;
    let changes = repo.changes(cs)?;
    let key = changeset_key(cs);
    let title = changeset_title(Some(repo), cs);
    let eager = changes.len() <= MAX_EAGER_STATS;
    let summaries = summarize_all(repo, &changes, opts, eager, cancel);
    check_cancel(cancel)?;
    let files = changes
        .into_iter()
        .zip(summaries)
        .map(|(change, summary)| {
            let (stats, binary, hash) = match summary {
                Some(s) => (s.stats, s.binary, s.hash),
                None => (None, false, String::new()),
            };
            let is_viewed = !hash.is_empty() && viewed.is_viewed(&key, change.path(), &hash);
            ReviewFile {
                display_path: display_path(&change),
                change,
                stats,
                viewed: is_viewed,
                content_hash: hash,
                binary,
            }
        })
        .collect();
    Ok(ChangesetView {
        key,
        title,
        changeset: cs.clone(),
        files,
    })
}

/// Summaries for all changes, using a couple of worker threads.
fn summarize_all(
    repo: &Repo,
    changes: &[FileChange],
    opts: &DiffOptions,
    eager: bool,
    cancel: &CancelToken,
) -> Vec<Option<FileSummary>> {
    if !eager || changes.is_empty() {
        return changes.iter().map(|_| None).collect();
    }
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(1, 4)
        .min(changes.len());
    let chunk = changes.len().div_ceil(threads);
    let mut out: Vec<Option<FileSummary>> = Vec::with_capacity(changes.len());
    std::thread::scope(|s| {
        let handles: Vec<_> = changes
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    part.iter()
                        .map(|c| {
                            if cancel.is_cancelled() {
                                None
                            } else {
                                summarize(repo, c, opts).ok()
                            }
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for h in handles {
            match h.join() {
                Ok(v) => out.extend(v),
                Err(_) => out.extend((0..chunk).map(|_| None)),
            }
        }
    });
    out.truncate(changes.len());
    while out.len() < changes.len() {
        out.push(None);
    }
    out
}

// ---------------------------------------------------------------------------
// File diffs

/// What kind of diff a [`FileDiff`] holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffKind {
    /// Text on both sides; rows are available.
    Text,
    /// At least one side is binary (non-image).
    Binary,
    /// An image on at least one side (bytes in [`DiffSide::bytes`]).
    Image(ImageFormat),
    /// A side is larger than [`MAX_DIFF_BYTES`]; only sizes are known.
    TooLarge,
}

/// One side (old or new) of a [`FileDiff`].
#[derive(Clone, Debug, Default)]
pub struct DiffSide {
    /// Repository-relative path on this side.
    pub path: Option<String>,
    /// Whether the side exists.
    pub exists: bool,
    /// Size in bytes.
    pub size: u64,
    /// Text (for text diffs).
    pub text: Option<Text>,
    /// Raw bytes (images only).
    pub bytes: Option<Arc<Vec<u8>>>,
    /// Detected language.
    pub language: Option<Arc<Language>>,
    /// Parsed tree for highlighting.
    pub syntax: Option<Syntax>,
    /// Text encoding name.
    pub encoding: &'static str,
}

/// A highlight span within one line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineSpan {
    /// Row (0-based) in this side's text.
    pub row: u32,
    /// Byte range within the line (without newline).
    pub range: Range<usize>,
    /// Highlight.
    pub id: HighlightId,
}

impl DiffSide {
    /// Line `row` of the text (empty when out of range / no text).
    pub fn line(&self, row: u32) -> String {
        match &self.text {
            Some(t) if row < t.line_count() => t.line(row).into_owned(),
            _ => String::new(),
        }
    }

    /// Number of lines (0 when the side does not exist).
    pub fn line_count(&self) -> u32 {
        match &self.text {
            Some(t) if self.exists && !t.is_empty() => t.line_count(),
            _ => 0,
        }
    }

    /// Syntax highlight spans for `rows`, split per line.
    pub fn line_highlights(&self, rows: Range<u32>) -> Vec<LineSpan> {
        let (Some(text), Some(syntax)) = (&self.text, &self.syntax) else {
            return Vec::new();
        };
        let count = text.line_count();
        let rows = rows.start.min(count)..rows.end.min(count);
        if rows.is_empty() {
            return Vec::new();
        }
        let start = text.line_range(rows.start).start;
        let end = text.line_range(rows.end - 1).end;
        let mut out = Vec::new();
        let mut row = rows.start;
        let mut line = text.line_range(row);
        for span in syntax.highlights(text, start..end) {
            let mut s = span.range.start;
            while s < span.range.end {
                while s > line.end && row + 1 < rows.end {
                    row += 1;
                    line = text.line_range(row);
                }
                if s < line.start {
                    s = line.start;
                    if s >= span.range.end {
                        break;
                    }
                }
                let e = span.range.end.min(line.end);
                if e > s {
                    out.push(LineSpan {
                        row,
                        range: s - line.start..e - line.start,
                        id: span.id,
                    });
                }
                if span.range.end <= line.end || row + 1 >= rows.end {
                    break;
                }
                row += 1;
                line = text.line_range(row);
                s = line.start;
            }
        }
        out
    }
}

/// A file diff: both sides, the line diff, rows for both layouts and lazily
/// refined word-level changes. `Send + Sync`; share it behind an `Arc`.
#[derive(Debug)]
pub struct FileDiff {
    /// The change.
    pub change: FileChange,
    /// Kind of content.
    pub kind: DiffKind,
    /// Old side.
    pub old: DiffSide,
    /// New side.
    pub new: DiffSide,
    /// Line diff (empty unless `kind == Text`).
    pub diff: LineDiff,
    /// Side-by-side rows.
    pub split: Vec<SplitRow>,
    /// Unified rows.
    pub unified: Vec<UnifiedRow>,
    /// Options used.
    pub options: DiffOptions,
    /// Identity of the exact contents this diff was computed from (see
    /// [`FileDiff::content_id`]).
    content_id: String,
    pairs: Vec<(u32, u32)>,
    pair_by_old: HashMap<u32, usize>,
    pair_by_new: HashMap<u32, usize>,
    inline: Mutex<HashMap<usize, Arc<InlineChanges>>>,
}

impl FileDiff {
    /// Identity of the contents (both sides) this diff was computed from. Pass it to
    /// [`apply_hunk_action_if_unchanged`] so a hunk is never applied to content that
    /// changed since the diff was shown.
    pub fn content_id(&self) -> &str {
        &self.content_id
    }

    /// Line stats.
    pub fn stats(&self) -> DiffStats {
        self.diff.stats()
    }

    /// Display path (`old → new` for renames).
    pub fn display_path(&self) -> String {
        display_path(&self.change)
    }

    /// Folded segments of the split rows with `context` lines around changes.
    pub fn split_segments(&self, context: u32) -> Vec<Segment> {
        fold_context(&self.split, context)
    }

    /// Folded segments of the unified rows.
    pub fn unified_segments(&self, context: u32) -> Vec<Segment> {
        fold_context(&self.unified, context)
    }

    /// Word-level changes for the (old, new) line pair with this old line.
    pub fn inline_for_old(&self, old_line: u32) -> Option<Arc<InlineChanges>> {
        let ix = *self.pair_by_old.get(&old_line)?;
        Some(self.inline_pair(ix))
    }

    /// Word-level changes for the (old, new) line pair with this new line.
    pub fn inline_for_new(&self, new_line: u32) -> Option<Arc<InlineChanges>> {
        let ix = *self.pair_by_new.get(&new_line)?;
        Some(self.inline_pair(ix))
    }

    /// The new line paired with `old_line` (changed lines), if any.
    pub fn paired_new(&self, old_line: u32) -> Option<u32> {
        self.pair_by_old.get(&old_line).map(|&ix| self.pairs[ix].1)
    }

    fn inline_pair(&self, ix: usize) -> Arc<InlineChanges> {
        if let Some(hit) = self.inline.lock().get(&ix) {
            return hit.clone();
        }
        let (o, n) = self.pairs[ix];
        let changes = Arc::new(refine_inline(&self.old.line(o), &self.new.line(n)));
        self.inline.lock().insert(ix, changes.clone());
        changes
    }

    /// Indices into `split` of the first row of every hunk.
    pub fn split_hunk_starts(&self) -> Vec<usize> {
        hunk_starts(self.split.iter().map(|r| r.hunk))
    }

    /// Indices into `unified` of the first row of every hunk.
    pub fn unified_hunk_starts(&self) -> Vec<usize> {
        hunk_starts(self.unified.iter().map(|r| r.hunk))
    }

    /// The new-side line closest to split row `ix` (for "open in editor").
    pub fn new_line_near_split_row(&self, ix: usize) -> u32 {
        nearest(self.split.iter().map(|r| r.new), ix)
    }

    /// The new-side line closest to unified row `ix`.
    pub fn new_line_near_unified_row(&self, ix: usize) -> u32 {
        nearest(self.unified.iter().map(|r| r.new), ix)
    }

    /// An unchanged diff (no hunks).
    pub fn is_identical(&self) -> bool {
        self.kind == DiffKind::Text && self.diff.hunks.is_empty()
    }
}

fn hunk_starts(hunks: impl Iterator<Item = Option<usize>>) -> Vec<usize> {
    let mut out = Vec::new();
    let mut last = None;
    for (i, h) in hunks.enumerate() {
        if h.is_some() && h != last {
            out.push(i);
        }
        last = h;
    }
    out
}

fn nearest(lines: impl Iterator<Item = Option<u32>>, ix: usize) -> u32 {
    let all: Vec<Option<u32>> = lines.collect();
    if let Some(Some(n)) = all.get(ix) {
        return *n;
    }
    // Search forward, then backward.
    if let Some(n) = all.iter().skip(ix).find_map(|n| *n) {
        return n;
    }
    all.iter()
        .take(ix.min(all.len()))
        .rev()
        .find_map(|n| *n)
        .unwrap_or(0)
}

fn pair_rows(split: &[SplitRow]) -> Vec<(u32, u32)> {
    split
        .iter()
        .filter(|r| r.kind == RowKind::Changed)
        .filter_map(|r| Some((r.old?, r.new?)))
        .collect()
}

fn side_path(change: &FileChange, old: bool) -> Option<String> {
    if old {
        change.old_path.clone().or_else(|| change.new_path.clone())
    } else {
        change.new_path.clone().or_else(|| change.old_path.clone())
    }
}

/// Cache identity of a loaded side: the blob id when known, else a hash of the bytes.
fn loaded_id(side: Option<&Side>, loaded: &Loaded) -> Option<ContentId> {
    match (side, loaded) {
        (Some(Side::Blob(oid)), _) => Some(ContentId::Blob(*oid)),
        (_, Loaded::Missing) => Some(ContentId::of_bytes(&[])),
        (_, Loaded::Bytes(b)) => Some(ContentId::of_bytes(b)),
        (_, Loaded::TooLarge(_)) => None,
    }
}

fn prepare_side(
    path: Option<String>,
    loaded: &Loaded,
    languages: &LanguageRegistry,
    parse: bool,
    id: Option<ContentId>,
    cache: &dyn DerivedCache,
) -> (DiffSide, Option<FileKind>) {
    let mut side = DiffSide {
        path: path.clone(),
        ..Default::default()
    };
    let rel = PathBuf::from(path.unwrap_or_default());
    match loaded {
        Loaded::Missing => (side, None),
        Loaded::TooLarge(n) => {
            side.exists = true;
            side.size = *n;
            (side, None)
        }
        Loaded::Bytes(b) => {
            side.exists = true;
            side.size = b.len() as u64;
            let head = &b[..b.len().min(lector_fs::SNIFF_LEN)];
            let kind = lector_fs::detect_kind(&rel, head);
            match kind {
                FileKind::Text | FileKind::Image(ImageFormat::Svg) => {
                    let (s, encoding, _) = lector_fs::decode(b);
                    side.encoding = encoding;
                    if matches!(kind, FileKind::Image(_)) {
                        side.bytes = Some(Arc::new(b.clone()));
                    }
                    let first_line = s.lines().next().map(str::to_string);
                    let text = Text::from(s);
                    side.language = languages.language_for_path(&rel, first_line.as_deref());
                    if parse {
                        if let Some(lang) = &side.language {
                            side.syntax = match id {
                                Some(id) => cache.syntax(id, lang, &text).ok(),
                                None => Syntax::parse(lang, &text).ok(),
                            };
                        }
                    }
                    side.text = Some(text);
                }
                FileKind::Image(_) => side.bytes = Some(Arc::new(b.clone())),
                FileKind::Binary | FileKind::Directory => {}
            }
            (side, Some(kind))
        }
    }
}

/// Loads and diffs one change using the core services directly.
pub fn load_file_diff_with(
    repo: &Repo,
    languages: &LanguageRegistry,
    change: &FileChange,
    opts: &DiffOptions,
    cache: &dyn DerivedCache,
) -> Result<FileDiff> {
    let old_loaded = load_side(repo, &change.old)?;
    let new_loaded = normalize_loaded_pair(&old_loaded, &change.new, load_side(repo, &change.new)?);
    let mut fd = build_file_diff(
        change.clone(),
        &old_loaded,
        &new_loaded,
        languages,
        opts,
        cache,
    );
    fd.content_id = pair_content_id(change, &old_loaded, &new_loaded);
    Ok(fd)
}

/// Identity of both loaded sides of `change` (blob ids, else content hashes).
pub(crate) fn pair_content_id(change: &FileChange, old: &Loaded, new: &Loaded) -> String {
    format!(
        "{}|{}",
        side_identity(&change.old, old),
        side_identity(&change.new, new)
    )
}

/// Builds a diff of two in-memory texts (e.g. a file vs a revision).
pub fn diff_bytes(
    change: FileChange,
    old: Option<Vec<u8>>,
    new: Option<Vec<u8>>,
    languages: &LanguageRegistry,
    opts: &DiffOptions,
    cache: &dyn DerivedCache,
) -> FileDiff {
    let wrap = |b: Option<Vec<u8>>| match b {
        None => Loaded::Missing,
        Some(b) if b.len() as u64 > MAX_DIFF_BYTES => Loaded::TooLarge(b.len() as u64),
        Some(b) => Loaded::Bytes(b),
    };
    build_file_diff(change, &wrap(old), &wrap(new), languages, opts, cache)
}

fn build_file_diff(
    change: FileChange,
    old_loaded: &Loaded,
    new_loaded: &Loaded,
    languages: &LanguageRegistry,
    opts: &DiffOptions,
    cache: &dyn DerivedCache,
) -> FileDiff {
    let too_large =
        matches!(old_loaded, Loaded::TooLarge(_)) || matches!(new_loaded, Loaded::TooLarge(_));
    let old_id = loaded_id(Some(&change.old), old_loaded);
    let new_id = loaded_id(Some(&change.new), new_loaded);
    let (old, old_kind) = prepare_side(
        side_path(&change, true),
        old_loaded,
        languages,
        !too_large,
        old_id,
        cache,
    );
    let (new, new_kind) = prepare_side(
        side_path(&change, false),
        new_loaded,
        languages,
        !too_large,
        new_id,
        cache,
    );
    let image = [&old_kind, &new_kind].into_iter().find_map(|k| match k {
        Some(FileKind::Image(f)) if *f != ImageFormat::Svg => Some(*f),
        _ => None,
    });
    let binary = [&old_kind, &new_kind]
        .into_iter()
        .any(|k| matches!(k, Some(FileKind::Binary)));
    let kind = if too_large {
        DiffKind::TooLarge
    } else if let Some(f) = image {
        DiffKind::Image(f)
    } else if binary {
        DiffKind::Binary
    } else {
        DiffKind::Text
    };
    let mut fd = FileDiff {
        change,
        kind,
        old,
        new,
        diff: LineDiff::default(),
        split: Vec::new(),
        unified: Vec::new(),
        options: *opts,
        content_id: String::new(),
        pairs: Vec::new(),
        pair_by_old: HashMap::new(),
        pair_by_new: HashMap::new(),
        inline: Mutex::new(HashMap::new()),
    };
    if fd.kind == DiffKind::Text {
        let o = fd
            .old
            .text
            .as_ref()
            .map(|t| t.to_string())
            .unwrap_or_default();
        let n = fd
            .new
            .text
            .as_ref()
            .map(|t| t.to_string())
            .unwrap_or_default();
        fd.diff = match (old_id, new_id) {
            (Some(a), Some(b)) => cache.line_diff(a, b, opts, &|| diff_text(&o, &n, opts)),
            _ => diff_text(&o, &n, opts),
        };
        fd.split = split_rows(&fd.diff);
        fd.unified = unified_rows(&fd.diff);
        fd.pairs = pair_rows(&fd.split);
        for (i, (o, n)) in fd.pairs.iter().enumerate() {
            fd.pair_by_old.insert(*o, i);
            fd.pair_by_new.insert(*n, i);
        }
    }
    fd
}

/// Diff of the working copy of `rel_path` against `rev` (for "Compare with…").
pub fn file_vs_revision_with(
    repo: &Repo,
    languages: &LanguageRegistry,
    rel_path: &str,
    rev: &str,
    opts: &DiffOptions,
    cache: &dyn DerivedCache,
) -> Result<FileDiff> {
    let old = repo.file_at(rev, rel_path)?;
    let new = std::fs::read(repo.workdir().join(rel_path))
        .ok()
        .map(|working| normalize_checkout_eol(old.as_deref(), working));
    let kind = match (&old, &new) {
        (None, Some(_)) => lector_git::ChangeKind::Added,
        (Some(_), None) => lector_git::ChangeKind::Deleted,
        _ => lector_git::ChangeKind::Modified,
    };
    let oid = repo.resolve(rev).ok();
    let change = FileChange {
        old_path: old.as_ref().map(|_| rel_path.to_string()),
        new_path: new.as_ref().map(|_| rel_path.to_string()),
        kind,
        old: match oid {
            Some(_) if old.is_some() => Side::Index(rel_path.to_string()),
            _ => Side::None,
        },
        new: Side::Workdir(rel_path.to_string()),
        binary: None,
    };
    Ok(diff_bytes(change, old, new, languages, opts, cache))
}

/// A parsed [`DiffSide`] for arbitrary bytes of `path` (e.g. a file at a
/// revision, for a read-only view with highlighting).
pub fn side_from_bytes(
    path: &str,
    bytes: Vec<u8>,
    languages: &LanguageRegistry,
    cache: &dyn DerivedCache,
) -> DiffSide {
    let loaded = if bytes.len() as u64 > MAX_DIFF_BYTES * 4 {
        Loaded::TooLarge(bytes.len() as u64)
    } else {
        Loaded::Bytes(bytes)
    };
    let id = loaded_id(None, &loaded);
    prepare_side(Some(path.to_string()), &loaded, languages, true, id, cache).0
}

/// The text of `path` at `rev` (for "view file at revision").
pub fn file_at_revision(repo: &Repo, rev: &str, path: &str) -> Result<Option<String>> {
    Ok(repo.file_at(rev, path)?.map(|b| lector_fs::decode(&b).0))
}

/// Pseudo revision used as `to` of a [`Changeset::Range`] built by
/// [`load_vs_workdir_with`]: the working tree (not loadable via `Repo::changes`).
pub const WORKDIR_REV: &str = ":workdir";

/// Changes from `rev` to the working tree (including untracked files):
/// `rev..HEAD` combined with `HEAD..workdir`. The returned view's changeset
/// is `Range { from: rev, to: WORKDIR_REV }`.
pub fn load_vs_workdir_with(
    repo: &Repo,
    rev: &str,
    viewed: &dyn ViewedMarks,
    opts: &DiffOptions,
) -> Result<ChangesetView> {
    load_vs_workdir_cancellable(repo, rev, viewed, opts, &CancelToken::new())
}

/// [`load_vs_workdir_with`] that stops with [`Error::Cancelled`] soon after `cancel` is set.
pub fn load_vs_workdir_cancellable(
    repo: &Repo,
    rev: &str,
    viewed: &dyn ViewedMarks,
    opts: &DiffOptions,
    cancel: &CancelToken,
) -> Result<ChangesetView> {
    use std::collections::BTreeMap;
    check_cancel(cancel)?;
    let committed = repo.changes(&Changeset::Range {
        from: rev.to_string(),
        to: "HEAD".into(),
    })?;
    let worktree = repo.changes(&Changeset::WorkingTree)?;
    let mut by_path: BTreeMap<String, FileChange> = BTreeMap::new();
    for c in committed {
        let mut c = c;
        // The new side is the working copy (equal to HEAD unless modified below).
        c.new = match &c.new_path {
            Some(p) if repo.workdir().join(p).exists() => Side::Workdir(p.clone()),
            Some(_) => Side::None,
            None => Side::None,
        };
        by_path.insert(c.path().to_string(), c);
    }
    for w in worktree {
        let key = w.path().to_string();
        match by_path.get_mut(&key) {
            Some(c) => c.new = w.new.clone(),
            None => {
                // Unchanged between rev and HEAD: HEAD's blob is rev's blob.
                by_path.insert(key, w);
            }
        }
    }
    let changes: Vec<FileChange> = by_path
        .into_values()
        .map(|mut c| {
            c.kind = match (&c.old, &c.new) {
                (Side::None, Side::None) => c.kind,
                (Side::None, _) => lector_git::ChangeKind::Added,
                (_, Side::None) => lector_git::ChangeKind::Deleted,
                _ if c.old_path != c.new_path && c.old_path.is_some() && c.new_path.is_some() => {
                    lector_git::ChangeKind::Renamed
                }
                _ => lector_git::ChangeKind::Modified,
            };
            if matches!(c.new, Side::None) {
                c.new_path = None;
            }
            c
        })
        .filter(|c| !(matches!(c.old, Side::None) && matches!(c.new, Side::None)))
        .collect();
    let cs = Changeset::Range {
        from: rev.to_string(),
        to: WORKDIR_REV.to_string(),
    };
    let key = format!("workdir:{rev}");
    let summaries = summarize_all(
        repo,
        &changes,
        opts,
        changes.len() <= MAX_EAGER_STATS,
        cancel,
    );
    check_cancel(cancel)?;
    let files = changes
        .into_iter()
        .zip(summaries)
        .filter_map(|(change, summary)| {
            let (stats, binary, hash) = match summary {
                Some(s) => (s.stats, s.binary, s.hash),
                None => (None, false, String::new()),
            };
            // Changed and changed back: nothing to show.
            if stats.is_some_and(|s| s.added == 0 && s.removed == 0)
                && change.kind == lector_git::ChangeKind::Modified
            {
                return None;
            }
            let is_viewed = !hash.is_empty() && viewed.is_viewed(&key, change.path(), &hash);
            Some(ReviewFile {
                display_path: display_path(&change),
                change,
                stats,
                viewed: is_viewed,
                content_hash: hash,
                binary,
            })
        })
        .collect();
    let short = repo
        .resolve(rev)
        .map(|o| o.short())
        .unwrap_or_else(|_| rev.to_string());
    let label = if rev.len() >= 40 {
        short
    } else {
        rev.to_string()
    };
    Ok(ChangesetView {
        key,
        title: format!("{label} → working tree"),
        changeset: cs,
        files,
    })
}

/// Resolves the default base branch for "branch vs base": the upstream of
/// `branch` if any, else `main`, `master`, `origin/main`, `origin/master`.
pub fn default_base(repo: &Repo, branch: &str) -> Option<String> {
    let branches = repo.branches().ok()?;
    if let Some(up) = branches
        .iter()
        .find(|b| !b.remote && b.name == branch)
        .and_then(|b| b.upstream.clone())
    {
        // An upstream of the same name is not a useful base; fall through.
        if up.rsplit('/').next() != Some(branch) {
            return Some(up);
        }
    }
    ["main", "master", "origin/main", "origin/master", "develop"]
        .into_iter()
        .find(|cand| *cand != branch && branches.iter().any(|b| b.name == *cand))
        .map(str::to_string)
}

/// Resolves `rev` to a commit oid (helper for UI code).
pub fn resolve(repo: &Repo, rev: &str) -> Option<Oid> {
    repo.resolve(rev).ok()
}

pub use crate::hunks::{hunk_patch, hunk_patch_if_unchanged, HunkAction, LineSelection};

#[cfg(test)]
mod tests;
