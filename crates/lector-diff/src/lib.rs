//! Line diff, word-level refinement and row layout for Lector's diff views.
//!
//! # Line model
//!
//! Text is split on `'\n'` only, so `line_count = newlines + 1` exactly like
//! `lector_text::Rope::line_count`: `"a\n"` has two lines (`"a"` and `""`) and
//! `""` has one empty line. A trailing `'\r'` is part of the line content: in
//! [`Whitespace::Exact`] mode a CRLF to LF conversion shows every line as changed
//! (as git does); use [`Whitespace::IgnoreEol`] (or any other ignore mode) to hide it.
//! A missing final newline therefore shows up as the removal/addition of the empty last
//! line, and UI line numbers (0-based here, +1 for display) always match the editor.
//!
//! Only this crate's [`diff_lines`] calls `imara-diff`; hunks always refer to the
//! original (un-normalized) lines.

#![forbid(unsafe_code)]

mod inline;
pub mod moves;
pub mod patch;
pub mod remap;
mod rows;

use std::hash::Hash;
use std::ops::Range;

use imara_diff::{Diff, InternedInput};

pub use inline::{refine_inline, InlineChanges};
pub use rows::{
    fold_context, gutter_marks, split_rows, unified_rows, GutterKind, GutterMark, HasKind, RowKind,
    Segment, SplitRow, UnifiedRow,
};

/// Line diff algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Algorithm {
    /// Histogram (patience-like), the default: more readable output.
    #[default]
    Histogram,
    /// Myers with git/GNU diff heuristics.
    Myers,
}

/// How whitespace differences are treated when comparing lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Whitespace {
    /// Lines must be byte-identical.
    #[default]
    Exact,
    /// All whitespace is ignored (`git diff -w`).
    IgnoreAll,
    /// Runs of whitespace are equivalent and trailing whitespace is ignored (`-b`).
    IgnoreChange,
    /// Trailing whitespace (including `\r`) is ignored (`--ignore-space-at-eol`).
    IgnoreEol,
}

/// Options for [`diff_lines`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct DiffOptions {
    /// Diff algorithm.
    pub algorithm: Algorithm,
    /// Whitespace handling.
    pub whitespace: Whitespace,
}

/// A changed region: `old` lines were replaced by `new` lines (0-based, half-open).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Hunk {
    /// Replaced lines in the old text (may be empty for pure insertions).
    pub old: Range<u32>,
    /// Replacing lines in the new text (may be empty for pure deletions).
    pub new: Range<u32>,
}

/// Counts of added and removed lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffStats {
    /// Lines added.
    pub added: u32,
    /// Lines removed.
    pub removed: u32,
}

/// Result of a line diff.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LineDiff {
    /// Changed regions in increasing order; never adjacent or overlapping.
    pub hunks: Vec<Hunk>,
    /// Number of lines in the old text (`newlines + 1`).
    pub old_lines: u32,
    /// Number of lines in the new text (`newlines + 1`).
    pub new_lines: u32,
}

impl LineDiff {
    /// Total added/removed line counts.
    pub fn stats(&self) -> DiffStats {
        let mut s = DiffStats::default();
        for h in &self.hunks {
            s.added += h.new.end - h.new.start;
            s.removed += h.old.end - h.old.start;
        }
        s
    }

    /// True when there are no changed lines.
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }
}

/// Diffs two texts line by line. See the crate docs for the line model.
pub fn diff_lines(old: &str, new: &str, opts: &DiffOptions) -> LineDiff {
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    let hunks = match opts.whitespace {
        Whitespace::Exact => run(
            old_lines.iter().copied(),
            new_lines.iter().copied(),
            opts.algorithm,
        ),
        ws => run(
            old_lines.iter().map(|l| normalize(l, ws)),
            new_lines.iter().map(|l| normalize(l, ws)),
            opts.algorithm,
        ),
    };
    LineDiff {
        hunks,
        old_lines: old_lines.len() as u32,
        new_lines: new_lines.len() as u32,
    }
}

fn normalize(line: &str, ws: Whitespace) -> String {
    match ws {
        Whitespace::Exact => line.to_string(),
        Whitespace::IgnoreAll => line.chars().filter(|c| !c.is_whitespace()).collect(),
        Whitespace::IgnoreEol => line.trim_end().to_string(),
        Whitespace::IgnoreChange => {
            let mut out = String::with_capacity(line.len());
            let mut in_ws = false;
            for c in line.trim_end().chars() {
                if c.is_whitespace() {
                    if !in_ws {
                        out.push(' ');
                    }
                    in_ws = true;
                } else {
                    out.push(c);
                    in_ws = false;
                }
            }
            out
        }
    }
}

fn run<T: Hash + Eq + AsRef<[u8]> + Default>(
    before: impl Iterator<Item = T>,
    after: impl Iterator<Item = T>,
    algorithm: Algorithm,
) -> Vec<Hunk> {
    let mut input: InternedInput<T> = InternedInput::default();
    input.update_before(before);
    input.update_after(after);
    let mut diff = Diff::compute(to_imara(algorithm), &input);
    // Git-style slider placement using the indent heuristic.
    diff.postprocess_lines(&input);
    diff.hunks()
        .map(|h| Hunk {
            old: h.before,
            new: h.after,
        })
        .collect()
}

fn to_imara(a: Algorithm) -> imara_diff::Algorithm {
    match a {
        Algorithm::Histogram => imara_diff::Algorithm::Histogram,
        Algorithm::Myers => imara_diff::Algorithm::Myers,
    }
}

/// Diffs arbitrary token slices (used for inline refinement); returns hunks as ranges.
pub(crate) fn diff_tokens<T: Hash + Eq + Copy + Default>(a: &[T], b: &[T]) -> Vec<Hunk> {
    let mut input: InternedInput<T> = InternedInput::default();
    input.update_before(a.iter().copied());
    input.update_after(b.iter().copied());
    let diff = Diff::compute(imara_diff::Algorithm::Myers, &input);
    diff.hunks()
        .map(|h| Hunk {
            old: h.before,
            new: h.after,
        })
        .collect()
}

/// Returns true when `bytes` look binary: a NUL byte in the first 8000 bytes (git's heuristic).
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8000)].contains(&0)
}

#[cfg(test)]
mod tests;
