//! Moved-code detection (in the spirit of `git diff --color-moved=dimmed-zebra`).
//!
//! Given a line diff, blocks of removed lines that reappear as added lines
//! elsewhere (in the same file or in another file of the same change) are
//! reported as moves, so a reviewer can skip them. Matching is greedy,
//! longest-first and one-to-one:
//!
//! 1. Every removed and added line gets a whitespace-insensitive hash.
//! 2. Equal non-trivial lines seed candidate blocks (maximal diagonal runs of
//!    equal lines, never crossing a hunk boundary).
//! 3. Blocks are trimmed so that they start and end on a non-trivial line
//!    (blank lines and lone braces/brackets are not anchors), must have at
//!    least `min_lines` lines and enough alphanumeric characters, and are
//!    accepted longest first; a block that overlaps an already accepted one is
//!    split into its free pieces, which compete again.
//!
//! A moved block is `reindented` when its text only matches modulo whitespace.

use std::collections::{BinaryHeap, HashMap, HashSet};
use std::ops::Range;

use crate::LineDiff;

/// Parameters of the move detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveOptions {
    /// Minimum number of lines of a moved block (after trimming trivial edges).
    pub min_lines: u32,
    /// Minimum number of alphanumeric characters in a moved block.
    pub min_chars: usize,
    /// Lines that occur more often than this among the removed (or added)
    /// lines are too common to seed a block.
    pub max_seed_occurrences: usize,
}

impl Default for MoveOptions {
    fn default() -> Self {
        Self {
            min_lines: 3,
            min_chars: 12,
            max_seed_occurrences: 64,
        }
    }
}

/// A block of lines that was moved within a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovedBlock {
    /// Lines in the old text (all of them removed by the diff).
    pub old: Range<u32>,
    /// Lines in the new text (all of them added by the diff).
    pub new: Range<u32>,
    /// True when the lines differ in whitespace/indentation.
    pub reindented: bool,
}

/// A block that moved from one file of a change to another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossFileMove {
    /// Index (into the input slice) of the file the block was removed from.
    pub old_file: usize,
    /// Index of the file the block was added to.
    pub new_file: usize,
    /// Removed lines in the old text of `old_file`.
    pub old: Range<u32>,
    /// Added lines in the new text of `new_file`.
    pub new: Range<u32>,
    /// True when the lines differ in whitespace/indentation.
    pub reindented: bool,
}

/// Detects blocks of at least `min_lines` lines that were moved within one file.
pub fn detect_moved(old: &str, new: &str, diff: &LineDiff, min_lines: u32) -> Vec<MovedBlock> {
    detect_moved_with(
        old,
        new,
        diff,
        &MoveOptions {
            min_lines,
            ..MoveOptions::default()
        },
    )
}

/// [`detect_moved`] with explicit [`MoveOptions`]. Sorted by new start line.
pub fn detect_moved_with(
    old: &str,
    new: &str,
    diff: &LineDiff,
    opts: &MoveOptions,
) -> Vec<MovedBlock> {
    let mut raw = find_moves(&[(old, new, diff)], opts);
    raw.retain(|m| m.old_file == 0 && m.new_file == 0);
    let mut out: Vec<MovedBlock> = raw
        .into_iter()
        .map(|m| MovedBlock {
            old: m.old,
            new: m.new,
            reindented: m.reindented,
        })
        .collect();
    out.sort_by_key(|m| (m.new.start, m.old.start));
    out
}

/// Detects blocks moved *between* the files of one change (removed in one file,
/// added in another). Moves inside a single file are not returned; use
/// [`detect_moved`] for those. Sorted by (new file, new start line).
pub fn detect_moved_across(files: &[(&str, &str, &LineDiff)]) -> Vec<CrossFileMove> {
    detect_moved_across_with(files, &MoveOptions::default())
}

/// [`detect_moved_across`] with explicit [`MoveOptions`].
pub fn detect_moved_across_with(
    files: &[(&str, &str, &LineDiff)],
    opts: &MoveOptions,
) -> Vec<CrossFileMove> {
    let mut out = find_moves(files, opts);
    out.retain(|m| m.old_file != m.new_file);
    out.sort_by_key(|m| (m.new_file, m.new.start, m.old_file, m.old.start));
    out
}

// ---------------------------------------------------------------------------

/// One removed or added line.
struct Line<'a> {
    /// Whitespace-insensitive hash.
    norm: u64,
    /// Text with trailing whitespace removed (exact modulo EOL).
    exact: &'a str,
    trivial: bool,
    alnum: u16,
    file: u32,
    /// Line number in its file.
    no: u32,
    /// Hunk side id: lines are contiguous only within one segment.
    segment: u32,
}

struct Side<'a> {
    lines: Vec<Line<'a>>,
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes().filter(|b| !b.is_ascii_whitespace()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn is_trivial(line: &str) -> bool {
    let t: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    if t.is_empty() {
        return true;
    }
    if t.chars().all(|c| "{}()[];,".contains(c)) {
        return true;
    }
    matches!(
        t.as_str(),
        "end" | "else" | "fi" | "done" | "endif" | "do" | "then" | "}else{" | "});" | "esac"
    )
}

fn collect<'a>(
    files: &[(&'a str, &'a str, &LineDiff)],
    removed: bool,
    segments: &mut u32,
) -> Side<'a> {
    let mut lines = Vec::new();
    for (fi, (old, new, diff)) in files.iter().enumerate() {
        let text: Vec<&str> = if removed {
            old.split('\n').collect()
        } else {
            new.split('\n').collect()
        };
        for h in &diff.hunks {
            let range = if removed { &h.old } else { &h.new };
            *segments += 1;
            for no in range.clone() {
                let Some(line) = text.get(no as usize) else {
                    break;
                };
                let exact = line.trim_end();
                lines.push(Line {
                    norm: fnv(line),
                    exact,
                    trivial: is_trivial(line),
                    alnum: line
                        .chars()
                        .filter(|c| c.is_alphanumeric())
                        .count()
                        .min(65535) as u16,
                    file: fi as u32,
                    no,
                    segment: *segments,
                });
            }
        }
    }
    Side { lines }
}

/// Candidate block: `len` lines starting at removed index `r` and added index `a`.
#[derive(PartialEq, Eq)]
struct Cand {
    len: u32,
    same_file: bool,
    r: u32,
    a: u32,
}

impl Ord for Cand {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.len
            .cmp(&other.len)
            .then(self.same_file.cmp(&other.same_file))
            .then(other.a.cmp(&self.a))
            .then(other.r.cmp(&self.r))
    }
}
impl PartialOrd for Cand {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn find_moves(files: &[(&str, &str, &LineDiff)], opts: &MoveOptions) -> Vec<CrossFileMove> {
    let mut seg = 0;
    let rem = collect(files, true, &mut seg);
    let add = collect(files, false, &mut seg);
    if rem.lines.is_empty() || add.lines.is_empty() {
        return Vec::new();
    }
    let min_lines = opts.min_lines.max(1);

    // Seeds: equal non-trivial lines, skipping very common ones.
    let mut by_hash: HashMap<u64, Vec<u32>> = HashMap::new();
    for (i, l) in add.lines.iter().enumerate() {
        if !l.trivial {
            by_hash.entry(l.norm).or_default().push(i as u32);
        }
    }
    let mut rem_count: HashMap<u64, usize> = HashMap::new();
    for l in rem.lines.iter().filter(|l| !l.trivial) {
        *rem_count.entry(l.norm).or_default() += 1;
    }

    let extend_ok = |r: usize, a: usize| -> bool {
        let (lr, la) = (&rem.lines[r], &add.lines[a]);
        lr.norm == la.norm
    };
    let mut seen: HashSet<(u32, u32)> = HashSet::new();
    let mut heap: BinaryHeap<Cand> = BinaryHeap::new();
    let push_candidate = |heap: &mut BinaryHeap<Cand>, r0: usize, a0: usize, len: usize| {
        if len as u32 >= min_lines {
            heap.push(Cand {
                len: len as u32,
                same_file: rem.lines[r0].file == add.lines[a0].file,
                r: r0 as u32,
                a: a0 as u32,
            });
        }
    };
    for (ri, rl) in rem.lines.iter().enumerate() {
        if rl.trivial {
            continue;
        }
        let Some(adds) = by_hash.get(&rl.norm) else {
            continue;
        };
        if adds.len() > opts.max_seed_occurrences
            || rem_count.get(&rl.norm).copied().unwrap_or(0) > opts.max_seed_occurrences
        {
            continue;
        }
        for &ai in adds {
            let ai = ai as usize;
            // Walk back to the start of the diagonal run.
            let (mut r0, mut a0) = (ri, ai);
            while r0 > 0
                && a0 > 0
                && rem.lines[r0 - 1].segment == rem.lines[ri].segment
                && add.lines[a0 - 1].segment == add.lines[ai].segment
                && extend_ok(r0 - 1, a0 - 1)
            {
                r0 -= 1;
                a0 -= 1;
            }
            if !seen.insert((r0 as u32, a0 as u32)) {
                continue;
            }
            let mut len = 1;
            while r0 + len < rem.lines.len()
                && a0 + len < add.lines.len()
                && rem.lines[r0 + len].segment == rem.lines[r0].segment
                && add.lines[a0 + len].segment == add.lines[a0].segment
                && extend_ok(r0 + len, a0 + len)
            {
                len += 1;
            }
            if len as u32 >= min_lines {
                push_candidate(&mut heap, r0, a0, len);
            }
        }
    }

    let mut used_r = vec![false; rem.lines.len()];
    let mut used_a = vec![false; add.lines.len()];
    let mut out = Vec::new();
    while let Some(c) = heap.pop() {
        let (r0, a0, len) = (c.r as usize, c.a as usize, c.len as usize);
        // Split into free pieces (both sides must be free at each offset).
        let mut k = 0;
        while k < len {
            if used_r[r0 + k] || used_a[a0 + k] {
                k += 1;
                continue;
            }
            let start = k;
            while k < len && !used_r[r0 + k] && !used_a[a0 + k] {
                k += 1;
            }
            let (mut s, mut e) = (start, k); // piece [s, e)
                                             // Trim trivial edges.
            while s < e && rem.lines[r0 + s].trivial {
                s += 1;
            }
            while e > s && rem.lines[r0 + e - 1].trivial {
                e -= 1;
            }
            let piece_len = e - s;
            if piece_len < min_lines as usize {
                continue;
            }
            let alnum: usize = (s..e).map(|i| rem.lines[r0 + i].alnum as usize).sum();
            if alnum < opts.min_chars {
                continue;
            }
            if (s, e) != (0, len) {
                // A trimmed or shortened piece competes again with the others.
                heap.push(Cand {
                    len: piece_len as u32,
                    same_file: c.same_file,
                    r: (r0 + s) as u32,
                    a: (a0 + s) as u32,
                });
                continue;
            }
            // Accept.
            let reindented = (s..e).any(|i| rem.lines[r0 + i].exact != add.lines[a0 + i].exact);
            for i in s..e {
                used_r[r0 + i] = true;
                used_a[a0 + i] = true;
            }
            let (rf, af) = (&rem.lines[r0 + s], &add.lines[a0 + s]);
            out.push(CrossFileMove {
                old_file: rf.file as usize,
                new_file: af.file as usize,
                old: rf.no..rem.lines[r0 + e - 1].no + 1,
                new: af.no..add.lines[a0 + e - 1].no + 1,
                reindented,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{diff_lines, DiffOptions};
    use proptest::prelude::*;

    fn moves(old: &str, new: &str) -> Vec<MovedBlock> {
        let d = diff_lines(old, new, &DiffOptions::default());
        detect_moved(old, new, &d, 3)
    }

    const FUNC: &str = "fn compute(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n    let product = a * b;\n    sum * product\n}\n";

    const KEEP: &str = "let keep_alpha = initialize_alpha();\nlet keep_beta = initialize_beta();\nlet keep_gamma = initialize_gamma();\nlet keep_delta = initialize_delta();\nlet keep_epsilon = initialize_epsilon();\nlet keep_zeta = initialize_zeta();\nlet keep_eta = initialize_eta();\nlet keep_theta = initialize_theta();\n";

    #[test]
    fn function_moved_down() {
        let old = format!("{FUNC}{KEEP}");
        let new = format!("{KEEP}{FUNC}");
        let m = moves(&old, &new);
        assert_eq!(m.len(), 1, "{m:?}");
        // The closing brace is a trivial edge and is trimmed off the block.
        assert_eq!(m[0].old, 0..4);
        assert_eq!(m[0].new, 8..12);
        assert!(!m[0].reindented);
        assert!(old
            .lines()
            .nth(m[0].old.start as usize)
            .unwrap()
            .contains("compute"));
        assert!(new
            .lines()
            .nth(m[0].new.start as usize)
            .unwrap()
            .contains("compute"));
    }

    #[test]
    fn reindented_move_is_flagged() {
        let indented: String = FUNC.lines().map(|l| format!("    {l}\n")).collect();
        let old = format!("mod a {{\n{indented}}}\n{KEEP}");
        let new = format!("mod a {{\n}}\n{KEEP}{FUNC}");
        let m = moves(&old, &new);
        assert_eq!(m.len(), 1, "{m:?}");
        assert!(m[0].reindented);
    }

    #[test]
    fn short_blocks_below_min_lines_are_ignored() {
        let old = "alpha_one();\nalpha_two();\nkeep1\nkeep2\nkeep3\nzz\n";
        let new = "keep1\nkeep2\nkeep3\nzz\nalpha_one();\nalpha_two();\n";
        assert!(moves(old, new).is_empty());
    }

    #[test]
    fn trivial_lines_are_not_anchors() {
        let old = "keep a\n}\n}\n}\n\nkeep b\nkeep c\n";
        let new = "keep a\nkeep b\nkeep c\n}\n}\n}\n\n";
        assert!(moves(old, new).is_empty());
    }

    #[test]
    fn block_with_trivial_edges_is_trimmed() {
        let body = "    let first_value = compute_first();\n    let second_value = compute_second();\n    first_value + second_value\n";
        let old = format!("{{\n{body}}}\n{KEEP}");
        let new = format!("{KEEP}{{\n{body}}}\n");
        let m = moves(&old, &new);
        assert_eq!(m.len(), 1, "{m:?}");
        // The braces are not part of the block.
        assert_eq!(m[0].old, 1..4);
        assert_eq!(m[0].new, 9..12);
    }

    #[test]
    fn identical_files_have_no_moves() {
        assert!(moves(FUNC, FUNC).is_empty());
    }

    #[test]
    fn one_to_one_matching() {
        // The removed block is duplicated in the new text: only one copy is a move.
        let old = format!("{FUNC}{KEEP}");
        let new = format!("{KEEP}{FUNC}\n{FUNC}");
        let m = moves(&old, &new);
        assert_eq!(m.len(), 1, "{m:?}");
    }

    #[test]
    fn across_files() {
        let moved = "pub fn parse_header(input: &str) -> Header {\n    let mut parts = input.split(':');\n    Header { name: parts.next().unwrap_or(\"\").into() }\n}\n";
        let a_old = format!("use std::fmt;\n\n{moved}\nfn other() {{}}\n");
        let a_new = "use std::fmt;\n\nfn other() {}\n";
        let b_old = "mod util;\n";
        let b_new = format!("mod util;\n\n{moved}");
        let da = diff_lines(&a_old, a_new, &DiffOptions::default());
        let db = diff_lines(b_old, &b_new, &DiffOptions::default());
        let m = detect_moved_across(&[(&a_old, a_new, &da), (b_old, &b_new, &db)]);
        assert_eq!(m.len(), 1, "{m:?}");
        assert_eq!((m[0].old_file, m[0].new_file), (0, 1));
        assert_eq!(m[0].old.end - m[0].old.start, 3);
        // Within-file detection does not report it.
        assert!(detect_moved(&a_old, a_new, &da, 3).is_empty());
    }

    #[test]
    fn across_files_ignores_same_file_moves() {
        let old = format!("{FUNC}{KEEP}");
        let new = format!("{KEEP}{FUNC}");
        let d = diff_lines(&old, &new, &DiffOptions::default());
        assert!(detect_moved_across(&[(&old, &new, &d)]).is_empty());
        assert_eq!(detect_moved(&old, &new, &d, 3).len(), 1);
    }

    fn arb_text() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                Just("}".to_string()),
                Just("".to_string()),
                "[a-d]{3,8} [a-d]{3,8};".prop_map(|s| s),
                "  [a-d]{3,8}\\(\\);".prop_map(|s| s),
            ],
            0..40,
        )
        .prop_map(|v| v.join("\n"))
    }

    proptest! {
        #[test]
        fn moves_are_valid(old in arb_text(), new in arb_text(), min in 2u32..5) {
            let d = diff_lines(&old, &new, &DiffOptions::default());
            let ms = detect_moved(&old, &new, &d, min);
            let ol: Vec<&str> = old.split('\n').collect();
            let nl: Vec<&str> = new.split('\n').collect();
            let in_hunk = |side_old: bool, line: u32| d.hunks.iter().any(|h| {
                let r = if side_old { &h.old } else { &h.new };
                r.contains(&line)
            });
            let mut used_old = HashSet::new();
            let mut used_new = HashSet::new();
            for m in &ms {
                prop_assert_eq!(m.old.end - m.old.start, m.new.end - m.new.start);
                prop_assert!(m.new.end - m.new.start >= min);
                for k in 0..(m.old.end - m.old.start) {
                    let (o, n) = (m.old.start + k, m.new.start + k);
                    prop_assert!(in_hunk(true, o) && in_hunk(false, n));
                    let strip = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
                    prop_assert_eq!(strip(ol[o as usize]), strip(nl[n as usize]));
                    prop_assert!(used_old.insert(o));
                    prop_assert!(used_new.insert(n));
                }
                // Anchored on non-trivial lines.
                prop_assert!(!is_trivial(ol[m.old.start as usize]));
                prop_assert!(!is_trivial(ol[(m.old.end - 1) as usize]));
            }
        }
    }
}
