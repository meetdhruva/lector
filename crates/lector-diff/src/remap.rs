//! Line mapping and re-anchoring: keep review comments and "viewed" marks
//! attached to the right line through new revisions and agent turns.
//!
//! * [`LineMap`] maps old line numbers to new ones through a [`LineDiff`] (and,
//!   optionally, through detected moves). Unchanged lines always map exactly.
//! * [`reanchor`] finds the line of a saved [`Anchor`] (its text plus a few
//!   lines of context) in a new text without any diff: exact text with
//!   matching context first, then a fuzzy match inside a window, with a
//!   confidence score. It answers `None` when it is not sure.

use std::cmp::Reverse;
use std::collections::HashMap;

use crate::moves::{detect_moved, MovedBlock};
use crate::LineDiff;

/// Where an old line ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mapped {
    /// The line is unchanged; this is its new line number.
    Exact(u32),
    /// The line was moved (see [`crate::moves`]); this is its new line number.
    Moved(u32),
    /// The line was removed or rewritten. `near` is the new line at which it
    /// used to be (for rewritten lines: the corresponding line of the
    /// replacement, clamped), a good starting point for [`reanchor`].
    Deleted {
        /// New line number closest to where the old line was.
        near: u32,
    },
}

impl Mapped {
    /// The new line for `Exact` and `Moved`.
    pub fn line(self) -> Option<u32> {
        match self {
            Mapped::Exact(l) | Mapped::Moved(l) => Some(l),
            Mapped::Deleted { .. } => None,
        }
    }
}

/// Old to new line mapping derived from a line diff.
#[derive(Debug, Clone)]
pub struct LineMap {
    /// (old range, new range) per hunk.
    hunks: Vec<(std::ops::Range<u32>, std::ops::Range<u32>)>,
    moved: HashMap<u32, u32>,
    new_lines: u32,
}

impl LineMap {
    /// Builds the map, detecting moved blocks of at least 3 lines.
    pub fn new(old: &str, new: &str, diff: &LineDiff) -> LineMap {
        let moves = detect_moved(old, new, diff, 3);
        Self::with_moves(diff, &moves)
    }

    /// Builds the map from the diff alone (no move detection).
    pub fn from_diff(diff: &LineDiff) -> LineMap {
        Self::with_moves(diff, &[])
    }

    /// Builds the map from a diff and precomputed moves.
    pub fn with_moves(diff: &LineDiff, moves: &[MovedBlock]) -> LineMap {
        let mut moved = HashMap::new();
        for m in moves {
            for k in 0..(m.old.end - m.old.start).min(m.new.end - m.new.start) {
                moved.insert(m.old.start + k, m.new.start + k);
            }
        }
        LineMap {
            hunks: diff
                .hunks
                .iter()
                .map(|h| (h.old.clone(), h.new.clone()))
                .collect(),
            moved,
            new_lines: diff.new_lines,
        }
    }

    /// Maps an old line (0-based) to the new text.
    pub fn map(&self, old_line: u32) -> Mapped {
        // Hunks entirely before the line.
        let before = self.hunks.partition_point(|(o, _)| o.end <= old_line);
        if let Some((o, n)) = self.hunks.get(before) {
            if o.start <= old_line {
                // Inside a hunk (removed or rewritten line).
                if let Some(&to) = self.moved.get(&old_line) {
                    return Mapped::Moved(to);
                }
                let near = if n.is_empty() {
                    n.start
                } else {
                    (n.start + (old_line - o.start)).min(n.end - 1)
                };
                return Mapped::Deleted { near };
            }
        }
        // Unchanged: offset from the end of the previous hunk.
        let (old_end, new_end) = match before.checked_sub(1).and_then(|i| self.hunks.get(i)) {
            Some((o, n)) => (o.end, n.end),
            None => (0, 0),
        };
        Mapped::Exact(new_end + (old_line - old_end))
    }

    /// Maps an unchanged new line back to the old text (`None` for added lines).
    pub fn map_back(&self, new_line: u32) -> Option<u32> {
        let before = self.hunks.partition_point(|(_, n)| n.end <= new_line);
        if let Some((_, n)) = self.hunks.get(before) {
            if n.start <= new_line {
                return None;
            }
        }
        let (old_end, new_end) = match before.checked_sub(1).and_then(|i| self.hunks.get(i)) {
            Some((o, n)) => (o.end, n.end),
            None => (0, 0),
        };
        Some(old_end + (new_line - new_end))
    }

    /// Number of lines of the new text.
    pub fn new_line_count(&self) -> u32 {
        self.new_lines
    }
}

// ---------------------------------------------------------------------------
// Re-anchoring

/// A saved position: the line, its text and the lines around it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Anchor {
    /// 0-based line when the anchor was created.
    pub line: u32,
    /// Text of that line.
    pub text: String,
    /// Up to a few lines directly before it, nearest last.
    pub context_before: Vec<String>,
    /// Up to a few lines directly after it, nearest first.
    pub context_after: Vec<String>,
}

impl Anchor {
    /// Builds an anchor for `line` of `text` with `context` lines on each side.
    pub fn capture(text: &str, line: u32, context: usize) -> Option<Anchor> {
        let lines: Vec<&str> = text.split('\n').collect();
        let at = line as usize;
        let own = strip_cr(lines.get(at)?);
        Some(Anchor {
            line,
            text: own.to_string(),
            context_before: lines[at.saturating_sub(context)..at]
                .iter()
                .map(|l| strip_cr(l).to_string())
                .collect(),
            context_after: lines[(at + 1).min(lines.len())..(at + 1 + context).min(lines.len())]
                .iter()
                .map(|l| strip_cr(l).to_string())
                .collect(),
        })
    }
}

/// Tuning of [`reanchor_with`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReanchorOptions {
    /// Fuzzy search radius around the expected line, in lines.
    pub window: u32,
    /// Minimum confidence to answer.
    pub min_confidence: f32,
    /// Where to look first instead of `anchor.line` (e.g. `Mapped::Deleted::near`).
    pub hint: Option<u32>,
}

impl Default for ReanchorOptions {
    fn default() -> Self {
        Self {
            window: 120,
            min_confidence: 0.6,
            hint: None,
        }
    }
}

/// The result of re-anchoring.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reanchored {
    /// New 0-based line.
    pub line: u32,
    /// Confidence in 0..=1 (1 = identical line with identical context in place).
    pub confidence: f32,
    /// True when the line text itself is not identical (fuzzy match).
    pub fuzzy: bool,
}

/// Finds `anchor` in `new_text`; `None` when unsure. See [`reanchor_with`].
pub fn reanchor(anchor: &Anchor, new_text: &str) -> Option<u32> {
    reanchor_with(anchor, new_text, &ReanchorOptions::default()).map(|r| r.line)
}

fn strip_cr(s: &str) -> &str {
    s.strip_suffix('\r').unwrap_or(s)
}

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_trivial(norm: &str) -> bool {
    norm.is_empty() || norm.chars().all(|c| "{}()[];,".contains(c) || c == ' ')
}

/// Normalized Levenshtein similarity in 0..=1 (1 = equal). Long strings fall
/// back to token-set similarity.
fn similarity(a: &str, b: &str) -> f32 {
    if a == b {
        return 1.0;
    }
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    if ac.is_empty() || bc.is_empty() {
        return 0.0;
    }
    if ac.len() > 240 || bc.len() > 240 {
        return token_similarity(a, b);
    }
    // Cheap length bound.
    let max = ac.len().max(bc.len());
    if ac.len().abs_diff(bc.len()) * 2 > max {
        // At least half of the longer string is unmatched.
        return 0.0f32.max(1.0 - (ac.len().abs_diff(bc.len()) as f32 / max as f32));
    }
    let mut prev: Vec<usize> = (0..=bc.len()).collect();
    let mut cur = vec![0; bc.len() + 1];
    for (i, ca) in ac.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in bc.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    1.0 - prev[bc.len()] as f32 / max as f32
}

fn token_similarity(a: &str, b: &str) -> f32 {
    let toks = |s: &str| -> Vec<String> {
        let mut v: Vec<String> = s
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let (ta, tb) = (toks(a), toks(b));
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let common = ta.iter().filter(|t| tb.binary_search(t).is_ok()).count();
    common as f32 / (ta.len() + tb.len() - common) as f32
}

/// Agreement of the lines around `at` with the anchor's context, in 0..=1,
/// or `None` when the anchor has no context.
fn context_score(anchor: &Anchor, lines: &[&str], at: usize) -> Option<f32> {
    let total = anchor.context_before.len() + anchor.context_after.len();
    if total == 0 {
        return None;
    }
    let mut score = 0.0;
    let mut weight = 0.0;
    for (k, want) in anchor.context_before.iter().rev().enumerate() {
        let w = 1.0 / (k as f32 + 1.0);
        weight += w;
        if let Some(got) = at.checked_sub(k + 1).and_then(|i| lines.get(i)) {
            score += w * similarity(&normalize(want), &normalize(got)).max(0.0);
        }
    }
    for (k, want) in anchor.context_after.iter().enumerate() {
        let w = 1.0 / (k as f32 + 1.0);
        weight += w;
        if let Some(got) = lines.get(at + k + 1) {
            score += w * similarity(&normalize(want), &normalize(got)).max(0.0);
        }
    }
    Some(score / weight)
}

/// Finds the line of `anchor` in `new_text`.
///
/// Phase 1 looks at every line whose text equals the anchor's (ignoring
/// whitespace differences) and scores it by how well its context matches and
/// how close it is to the expected line. Phase 2 (when phase 1 is not
/// confident) compares similar lines inside `window` lines around the expected
/// position with a normalized Levenshtein distance. The best candidate is
/// returned with its confidence; `None` when the best confidence is below
/// `min_confidence`, when two candidates are indistinguishable (duplicate
/// lines without telling context), or when the anchor is a blank/brace-only
/// line without matching context.
pub fn reanchor_with(
    anchor: &Anchor,
    new_text: &str,
    opts: &ReanchorOptions,
) -> Option<Reanchored> {
    let lines: Vec<&str> = new_text.split('\n').map(strip_cr).collect();
    let want = normalize(&anchor.text);
    // Whitespace-insensitive equality without allocating.
    let (want_first, want_last) = (want.as_bytes().first(), want.as_bytes().last());
    let eq_want = |l: &str| {
        let t = l.trim();
        if want.is_empty() {
            return t.is_empty();
        }
        // Cheap rejection before the token comparison.
        t.as_bytes().first() == want_first
            && t.as_bytes().last() == want_last
            && t.len() >= want.len()
            && t.split_whitespace().eq(want.split(' '))
    };
    let trivial = is_trivial(&want);
    let center = opts.hint.unwrap_or(anchor.line) as i64;
    let proximity = |at: usize| -> f32 { 1.0 / (1.0 + (at as i64 - center).abs() as f32 / 20.0) };

    let score = |at: usize, text_sim: f32| -> f32 {
        let p = proximity(at);
        match context_score(anchor, &lines, at) {
            Some(c) => 0.6 * text_sim + 0.3 * c + 0.1 * p,
            None => 0.85 * text_sim + 0.15 * p,
        }
    };

    // Lines with the anchor's text. With very many (duplicates all over the
    // file) only the most promising ones, judged by the two adjacent context
    // lines and the distance, get the full context scoring.
    let mut exact: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| eq_want(l))
        .map(|(i, _)| i)
        .collect();
    if exact.len() > 64 {
        let eq_ws = |a: &str, b: &str| a.split_whitespace().eq(b.split_whitespace());
        let adjacent = |i: usize| -> usize {
            let before = anchor
                .context_before
                .last()
                .zip(i.checked_sub(1).and_then(|j| lines.get(j)))
                .is_some_and(|(w, g)| eq_ws(w, g));
            let after = anchor
                .context_after
                .first()
                .zip(lines.get(i + 1))
                .is_some_and(|(w, g)| eq_ws(w, g));
            usize::from(before) + usize::from(after)
        };
        exact.sort_by_key(|&i| (Reverse(adjacent(i)), (i as i64 - center).unsigned_abs()));
        exact.truncate(64);
    }
    let mut candidates: Vec<(usize, f32, f32)> = Vec::new(); // (line, text_sim, confidence)
    if !trivial {
        for &i in &exact {
            candidates.push((i, 1.0, score(i, 1.0)));
        }
    } else {
        // Blank / brace lines: only context can place them.
        for &i in &exact {
            let c = context_score(anchor, &lines, i)?;
            if c >= 0.5 {
                candidates.push((i, 1.0, 0.6 + 0.3 * c + 0.1 * proximity(i)));
            }
        }
        if candidates.is_empty() {
            return None;
        }
    }

    let confident = candidates.iter().any(|c| c.2 >= 0.85);
    if !confident && !trivial {
        // Fuzzy window.
        let lo = (center - opts.window as i64).max(0) as usize;
        let hi = ((center + opts.window as i64) as usize + 1).min(lines.len());
        for (i, l) in lines.iter().enumerate().take(hi).skip(lo) {
            let ln = normalize(l);
            if ln == want || is_trivial(&ln) {
                continue;
            }
            let s = similarity(&want, &ln);
            if s >= 0.6 {
                candidates.push((i, s, score(i, s)));
            }
        }
    }

    candidates.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.0.cmp(&b.0)));
    let best = *candidates.first()?;
    if best.2 < opts.min_confidence {
        return None;
    }
    if let Some(second) = candidates.get(1) {
        // Indistinguishable duplicates: unsure.
        if second.0 != best.0 && best.2 - second.2 < 0.01 {
            return None;
        }
    }
    Some(Reanchored {
        line: best.0 as u32,
        confidence: best.2.clamp(0.0, 1.0),
        fuzzy: best.1 < 1.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{diff_lines, DiffOptions};
    use proptest::prelude::*;

    fn map_for(old: &str, new: &str) -> LineMap {
        LineMap::new(old, new, &diff_lines(old, new, &DiffOptions::default()))
    }

    #[test]
    fn map_basic() {
        let old = "a\nb\nc\nd\ne";
        let new = "a\nX\nY\nc\nd\nnew1\nnew2\ne";
        let m = map_for(old, new);
        assert_eq!(m.map(0), Mapped::Exact(0));
        assert_eq!(m.map(1), Mapped::Deleted { near: 1 });
        assert_eq!(m.map(2), Mapped::Exact(3));
        assert_eq!(m.map(3), Mapped::Exact(4));
        assert_eq!(m.map(4), Mapped::Exact(7));
        assert_eq!(m.map_back(3), Some(2));
        assert_eq!(m.map_back(1), None);
        assert_eq!(m.map_back(5), None);
    }

    #[test]
    fn map_pure_deletion_and_beyond_end() {
        let old = "a\nb\nc\nd";
        let new = "a\nd";
        let m = map_for(old, new);
        assert_eq!(m.map(1), Mapped::Deleted { near: 1 });
        assert_eq!(m.map(2), Mapped::Deleted { near: 1 });
        assert_eq!(m.map(3), Mapped::Exact(1));
    }

    #[test]
    fn map_moved_lines() {
        let blk = "let alpha_value = compute_alpha();\nlet beta_value = compute_beta();\nlet gamma_value = compute_gamma();\n";
        let old = format!("{blk}k1\nk2\nk3\nk4\n");
        let new = format!("k1\nk2\nk3\nk4\n{blk}");
        let m = map_for(&old, &new);
        assert_eq!(m.map(0), Mapped::Moved(4));
        assert_eq!(m.map(2), Mapped::Moved(6));
        assert_eq!(m.map(3), Mapped::Exact(0));
        let plain = LineMap::from_diff(&diff_lines(&old, &new, &DiffOptions::default()));
        assert!(matches!(plain.map(0), Mapped::Deleted { .. }));
    }

    fn text_strategy() -> impl Strategy<Value = String> {
        proptest::collection::vec("[a-c]{0,3}", 0..30).prop_map(|v| v.join("\n"))
    }

    proptest! {
        #[test]
        fn unchanged_lines_map_exactly(old in text_strategy(), new in text_strategy()) {
            let d = diff_lines(&old, &new, &DiffOptions::default());
            let m = LineMap::new(&old, &new, &d);
            let ol: Vec<&str> = old.split('\n').collect();
            let nl: Vec<&str> = new.split('\n').collect();
            for (i, line) in ol.iter().enumerate() {
                let in_hunk = d.hunks.iter().any(|h| h.old.contains(&(i as u32)));
                match m.map(i as u32) {
                    Mapped::Exact(n) => {
                        prop_assert!(!in_hunk);
                        prop_assert_eq!(*line, nl[n as usize]);
                        prop_assert_eq!(m.map_back(n), Some(i as u32));
                    }
                    Mapped::Moved(n) => {
                        prop_assert!(in_hunk);
                        prop_assert!(n < d.new_lines);
                    }
                    Mapped::Deleted { near } => {
                        prop_assert!(in_hunk);
                        prop_assert!(near <= d.new_lines);
                    }
                }
                if !in_hunk {
                    prop_assert!(matches!(m.map(i as u32), Mapped::Exact(_)));
                }
            }
        }

        #[test]
        fn reanchor_finds_unique_lines_after_insertions(
            lines in proptest::collection::hash_set("[a-z]{6,12} [a-z]{6,12}", 5..25),
            k in 0usize..6,
            pick in 0usize..1000,
        ) {
            let lines: Vec<String> = lines.into_iter().collect();
            let text = lines.join("\n");
            let i = pick % lines.len();
            let anchor = Anchor::capture(&text, i as u32, 2).unwrap();
            prop_assert_eq!(reanchor(&anchor, &text), Some(i as u32));
            // Insert k unrelated lines at the top.
            let mut shifted: Vec<String> = (0..k).map(|n| format!("INSERTED-{n}")).collect();
            shifted.extend(lines.iter().cloned());
            prop_assert_eq!(reanchor(&anchor, &shifted.join("\n")), Some((i + k) as u32));
        }
    }

    const SRC: &str = "fn main() {\n    let config = load_config();\n    let server = Server::new(config);\n    server.run();\n}\n";

    #[test]
    fn reanchor_exact_after_shift() {
        let a = Anchor::capture(SRC, 2, 2).unwrap();
        let new = format!("// header\n// more\n{SRC}");
        let r = reanchor_with(&a, &new, &ReanchorOptions::default()).unwrap();
        assert_eq!(r.line, 4);
        assert!(!r.fuzzy);
        assert!(r.confidence > 0.9);
    }

    #[test]
    fn reanchor_fuzzy_after_edit() {
        let a = Anchor::capture(SRC, 2, 2).unwrap();
        let new = SRC.replace("Server::new(config)", "Server::with_config(config.clone())");
        let r = reanchor_with(&a, &new, &ReanchorOptions::default()).unwrap();
        assert_eq!(r.line, 2);
        assert!(r.fuzzy);
        assert!(r.confidence < 1.0 && r.confidence >= 0.6);
    }

    #[test]
    fn reanchor_deleted_is_none() {
        let a = Anchor::capture(SRC, 2, 2).unwrap();
        let new = "fn other() {\n    println!(\"unrelated\");\n}\n";
        assert_eq!(reanchor(&a, new), None);
    }

    #[test]
    fn reanchor_duplicate_lines_use_context() {
        let text = "fn a() {\n    return None;\n}\nfn b() {\n    log(1);\n    return None;\n}\n";
        let a = Anchor::capture(text, 5, 2).unwrap();
        let new = format!("// x\n{text}");
        assert_eq!(reanchor(&a, &new), Some(6));
    }

    #[test]
    fn reanchor_duplicate_lines_without_context_is_unsure() {
        let a = Anchor {
            line: 5,
            text: "return None;".into(),
            ..Default::default()
        };
        let new = "x\nreturn None;\ny\ny\ny\ny\nz\nz\nz\nz\nreturn None;\nq\n";
        assert_eq!(reanchor(&a, new), None);
    }

    #[test]
    fn reanchor_blank_line_needs_context() {
        let a = Anchor {
            line: 1,
            text: "".into(),
            context_before: vec!["use a;".into()],
            context_after: vec!["fn f() {}".into()],
        };
        assert_eq!(reanchor(&a, "use a;\n\nfn f() {}\n"), Some(1));
        assert_eq!(reanchor(&a, "other\n\nstuff\n"), None);
    }

    #[test]
    fn reanchor_crlf_and_whitespace() {
        let a = Anchor::capture(SRC, 1, 1).unwrap();
        let new = SRC.replace('\n', "\r\n").replace("    let", "\tlet");
        assert_eq!(reanchor(&a, &new), Some(1));
    }
}
