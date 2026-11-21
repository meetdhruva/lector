//! The fallback for query atoms longer than [`super::MAX_FUZZY_ATOM_CHARS`]: a case-insensitive
//! contiguous substring search, linear in the candidate (Knuth-Morris-Pratt), instead of the
//! quadratic alignment.

use super::{BONUS_CASE, BONUS_EXACT, BONUS_PREFIX, GAP_LEADING, MATCH_CONSECUTIVE};

/// The Knuth-Morris-Pratt failure table of `pattern`.
pub(super) fn failure_table<T: PartialEq>(pattern: &[T]) -> Vec<u32> {
    let mut fail = vec![0u32; pattern.len()];
    let mut k = 0usize;
    for i in 1..pattern.len() {
        while k > 0 && pattern[i] != pattern[k] {
            k = fail[k - 1] as usize;
        }
        if pattern[i] == pattern[k] {
            k += 1;
        }
        fail[i] = k as u32;
    }
    fail
}

/// Start (in characters) of the best occurrence of `pattern` in `text`, `None` if there is none.
/// The best occurrence maximises `bonus[start] + GAP_LEADING * start`, the same trade-off the
/// alignment makes for its first character: a word start beats a later one; ties go to the
/// earliest. Linear in `text.len()`.
fn best_occurrence<T: Copy + PartialEq>(
    pattern: &[T],
    fail: &[u32],
    text: &[T],
    bonus: &[i16],
) -> Option<usize> {
    let (m, n) = (pattern.len(), text.len());
    if m == 0 || m > n {
        return None;
    }
    let mut best: Option<(i32, usize)> = None;
    let mut k = 0usize;
    for (j, &c) in text.iter().enumerate() {
        while k > 0 && (k == m || pattern[k] != c) {
            k = fail[k - 1] as usize;
        }
        if pattern[k] == c {
            k += 1;
        }
        if k == m {
            let start = j + 1 - m;
            let score = start as i32 * GAP_LEADING + bonus[start] as i32;
            if best.is_none_or(|(b, _)| score > b) {
                best = Some((score, start));
            }
        }
    }
    best.map(|(_, start)| start)
}

/// Score and start of the best occurrence of the atom (`pf` folded, `po` original characters,
/// `fail` its failure table) in a candidate. The score is what the alignment gives a perfectly
/// contiguous match at that place: the start's boundary bonus, the leading gap, 1000 for each
/// following character, 50 for each character of equal case, and the prefix / exact bonuses.
pub(super) fn score<T: Copy + PartialEq>(
    (pf, po, fail): (&[T], &[T], &[u32]),
    orig: &[T],
    folded: &[T],
    bonus: &[i16],
) -> Option<(i32, usize)> {
    let start = best_occurrence(pf, fail, folded, bonus)?;
    let m = pf.len();
    let cased = (0..m).filter(|&i| orig[start + i] == po[i]).count() as i32;
    let mut total = start as i32 * GAP_LEADING
        + bonus[start] as i32
        + (m as i32 - 1) * MATCH_CONSECUTIVE
        + cased * BONUS_CASE;
    if start == 0 {
        total += BONUS_PREFIX;
        if m == folded.len() {
            total += BONUS_EXACT;
        }
    }
    Some((total, start))
}
