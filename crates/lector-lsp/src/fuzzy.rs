//! A small fuzzy matcher for completion filtering (fzy-style scoring).
//!
//! `lector-search` has a matcher (nucleo) but it is a heavier crate that
//! lives above this one in the layering (walker, grep, ...), so completion
//! carries its own: case-insensitive subsequence matching scored by word
//! boundaries (start, after a separator, camelCase), consecutive runs and a
//! small bonus for matching the case exactly. A text that starts with the
//! query gets a prefix bonus; there is no penalty for text after the last
//! match, so equally good matches tie and `sortText` decides.

/// Texts are indexed (and matched) up to this many chars.
pub(crate) const MAX_CHARS: usize = 128;

const NEG_INF: i32 = i32::MIN / 4;
const GAP_LEADING: i32 = -5;
const GAP_INNER: i32 = -10;
const MATCH_CONSECUTIVE: i32 = 1000;
const BONUS_START: i32 = 900;
const BONUS_WORD: i32 = 800;
const BONUS_CAPITAL: i32 = 700;
const BONUS_DOT: i32 = 600;
const BONUS_CASE: i32 = 50;
const BONUS_PREFIX: i32 = 1500;
const BONUS_EXACT: i32 = 500;

/// Case folding used for matching.
pub(crate) fn fold(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

fn boundary_bonus(prev: Option<char>, cur: char) -> i32 {
    let Some(prev) = prev else {
        return BONUS_START;
    };
    if !cur.is_alphanumeric() {
        return 0;
    }
    if !prev.is_alphanumeric() {
        return match prev {
            '/' | '\\' => BONUS_START,
            '.' => BONUS_DOT,
            _ => BONUS_WORD,
        };
    }
    if prev.is_lowercase() && cur.is_uppercase() {
        return BONUS_CAPITAL;
    }
    if prev.is_alphabetic() != cur.is_alphabetic() {
        return BONUS_DOT;
    }
    0
}

/// A query prepared for matching.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Query {
    folded: Vec<char>,
    orig: Vec<char>,
}

impl Query {
    pub(crate) fn new(text: &str) -> Query {
        let orig: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
        let folded = orig.iter().map(|&c| fold(c)).collect();
        Query { folded, orig }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.folded.is_empty()
    }

    /// Whether `other` extends `self` (every text matching `other` matches
    /// `self`), so results for `other` can be searched among those of `self`.
    pub(crate) fn is_prefix_of(&self, other: &Query) -> bool {
        other.folded.starts_with(&self.folded)
    }
}

/// Chars, folded chars and boundary bonuses of many texts in three arenas.
#[derive(Debug, Default)]
pub(crate) struct FuzzyIndex {
    orig: Vec<char>,
    folded: Vec<char>,
    bonus: Vec<i16>,
    spans: Vec<(u32, u16)>,
}

impl FuzzyIndex {
    pub(crate) fn with_capacity(items: usize) -> FuzzyIndex {
        FuzzyIndex {
            spans: Vec::with_capacity(items),
            ..FuzzyIndex::default()
        }
    }

    /// Adds a text; returns its slot.
    pub(crate) fn push(&mut self, text: &str) -> usize {
        let start = self.orig.len();
        let mut prev = None;
        for c in text.chars().take(MAX_CHARS) {
            self.orig.push(c);
            self.folded.push(fold(c));
            self.bonus.push(boundary_bonus(prev, c) as i16);
            prev = Some(c);
        }
        let len = self.orig.len() - start;
        self.spans.push((start as u32, len as u16));
        self.spans.len() - 1
    }

    fn slices(&self, slot: usize) -> (&[char], &[char], &[i16]) {
        let (start, len) = self.spans[slot];
        let (s, e) = (start as usize, start as usize + len as usize);
        (&self.orig[s..e], &self.folded[s..e], &self.bonus[s..e])
    }
}

/// Reusable buffers for [`score`].
#[derive(Debug, Default)]
pub(crate) struct Scratch {
    d_prev: Vec<i32>,
    m_prev: Vec<i32>,
    d_cur: Vec<i32>,
    m_cur: Vec<i32>,
}

fn is_subsequence(query: &[char], text: &[char]) -> bool {
    let mut qi = 0;
    for &c in text {
        if qi < query.len() && c == query[qi] {
            qi += 1;
        }
    }
    qi == query.len()
}

/// Score of `query` against indexed text `slot`, `None` if it does not match.
/// An empty query matches everything with score 0.
pub(crate) fn score(
    index: &FuzzyIndex,
    slot: usize,
    query: &Query,
    scratch: &mut Scratch,
) -> Option<i32> {
    let m = query.folded.len();
    if m == 0 {
        return Some(0);
    }
    let (orig, folded, bonus) = index.slices(slot);
    let n = folded.len();
    if m > n || !is_subsequence(&query.folded, folded) {
        return None;
    }
    let prefix = folded[..m] == query.folded[..];
    let mut total = if m == 1 {
        // Best single match: the best-bonus occurrence, with a leading-gap penalty.
        let mut best = NEG_INF;
        for j in 0..n {
            if folded[j] == query.folded[0] {
                let mut s = j as i32 * GAP_LEADING + bonus[j] as i32;
                if orig[j] == query.orig[0] {
                    s += BONUS_CASE;
                }
                best = best.max(s);
            }
        }
        best
    } else {
        dp_score(query, orig, folded, bonus, scratch)
    };
    if prefix {
        total += BONUS_PREFIX;
        if m == n {
            total += BONUS_EXACT;
        }
    }
    Some(total)
}

fn dp_score(query: &Query, orig: &[char], folded: &[char], bonus: &[i16], s: &mut Scratch) -> i32 {
    let (m, n) = (query.folded.len(), folded.len());
    for v in [&mut s.d_prev, &mut s.m_prev, &mut s.d_cur, &mut s.m_cur] {
        v.clear();
        v.resize(n, NEG_INF);
    }
    for i in 0..m {
        let gap = if i == m - 1 { 0 } else { GAP_INNER };
        let qc = query.folded[i];
        let mut prev_score = NEG_INF;
        for j in 0..n {
            if qc == folded[j] {
                let mut sc = NEG_INF;
                if i == 0 {
                    sc = j as i32 * GAP_LEADING + bonus[j] as i32;
                } else if j > 0 {
                    let via_gap = s.m_prev[j - 1].saturating_add(bonus[j] as i32);
                    let via_run = s.d_prev[j - 1].saturating_add(MATCH_CONSECUTIVE);
                    sc = via_gap.max(via_run);
                }
                if sc > NEG_INF && orig[j] == query.orig[i] {
                    sc += BONUS_CASE;
                }
                s.d_cur[j] = sc;
                prev_score = sc.max(prev_score.saturating_add(gap));
            } else {
                s.d_cur[j] = NEG_INF;
                prev_score = prev_score.saturating_add(gap);
            }
            s.m_cur[j] = prev_score;
        }
        std::mem::swap(&mut s.d_prev, &mut s.d_cur);
        std::mem::swap(&mut s.m_prev, &mut s.m_cur);
    }
    s.m_prev[n - 1]
}

/// Char positions (indices into `text.chars()`) of the best match of
/// `query` in `text`, ascending. `None` if there is no match.
pub(crate) fn positions(text: &str, query: &Query) -> Option<Vec<usize>> {
    let m = query.folded.len();
    if m == 0 {
        return Some(vec![]);
    }
    let orig: Vec<char> = text.chars().take(MAX_CHARS).collect();
    let folded: Vec<char> = orig.iter().map(|&c| fold(c)).collect();
    let n = orig.len();
    if m > n || !is_subsequence(&query.folded, &folded) {
        return None;
    }
    let mut bonus = Vec::with_capacity(n);
    let mut prev = None;
    for &c in &orig {
        bonus.push(boundary_bonus(prev, c));
        prev = Some(c);
    }
    let mut d = vec![NEG_INF; m * n];
    let mut mm = vec![NEG_INF; m * n];
    for i in 0..m {
        let gap = if i == m - 1 { 0 } else { GAP_INNER };
        let mut prev_score = NEG_INF;
        for j in 0..n {
            let at = i * n + j;
            if query.folded[i] == folded[j] {
                let mut sc = NEG_INF;
                if i == 0 {
                    sc = j as i32 * GAP_LEADING + bonus[j];
                } else if j > 0 {
                    let via_gap = mm[at - n - 1].saturating_add(bonus[j]);
                    let via_run = d[at - n - 1].saturating_add(MATCH_CONSECUTIVE);
                    sc = via_gap.max(via_run);
                }
                if sc > NEG_INF && orig[j] == query.orig[i] {
                    sc += BONUS_CASE;
                }
                d[at] = sc;
                prev_score = sc.max(prev_score.saturating_add(gap));
            } else {
                prev_score = prev_score.saturating_add(gap);
            }
            mm[at] = prev_score;
        }
    }
    let mut out = vec![0usize; m];
    let mut j = n;
    let mut match_required = false;
    for i in (0..m).rev() {
        loop {
            if j == 0 {
                return None;
            }
            j -= 1;
            let at = i * n + j;
            if d[at] > NEG_INF && (match_required || d[at] == mm[at]) {
                match_required = i > 0
                    && j > 0
                    && mm[at]
                        == d[at - n - 1].saturating_add(MATCH_CONSECUTIVE)
                            + if orig[j] == query.orig[i] {
                                BONUS_CASE
                            } else {
                                0
                            };
                out[i] = j;
                break;
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(query: &str, text: &str) -> Option<i32> {
        let mut index = FuzzyIndex::default();
        let slot = index.push(text);
        score(&index, slot, &Query::new(query), &mut Scratch::default())
    }

    #[test]
    fn subsequence_required() {
        assert!(s("fb", "foo_bar").is_some());
        assert!(s("bf", "foo_bar").is_none());
        assert!(s("foox", "foo").is_none());
        assert_eq!(s("", "anything"), Some(0));
    }

    #[test]
    fn ranking_table() {
        // (query, better, worse)
        let table = [
            ("fo", "foo", "xfoo"),
            ("fo", "foobar", "buffoon"),
            ("fb", "foo_bar", "fxxbxx"),
            ("fb", "fooBar", "fobar_x"),
            ("gc", "get_count", "gic"),
            ("name", "name", "rename"),
            ("Vec", "Vec", "vec"),
            ("vec", "vec", "Vec"),
            ("sl", "slice", "selfLen"),
            ("pr", "print", "xxprint"),
            ("ab", "ab", "a_b"),
            ("ab", "a_b", "axb"),
        ];
        for (q, better, worse) in table {
            let (a, b) = (s(q, better).unwrap(), s(q, worse).unwrap());
            assert!(a > b, "{q}: {better} ({a}) should beat {worse} ({b})");
        }
    }

    #[test]
    fn trailing_text_does_not_matter() {
        assert_eq!(s("fo", "foo"), s("fo", "foobarbaz"));
    }

    #[test]
    fn unicode_case_folding() {
        assert!(s("ärg", "Ärger").is_some());
        assert!(s("straße", "STRASSE").is_none());
        assert!(s("é", "café").is_some());
    }

    #[test]
    fn long_text_is_truncated_not_rejected_with_a_panic() {
        let text = "a".repeat(300);
        assert!(s("aa", &text).is_some());
        assert!(s("b", &text).is_none());
    }

    #[test]
    fn positions_follow_best_match() {
        let q = |t: &str| Query::new(t);
        assert_eq!(positions("foo_bar", &q("fb")), Some(vec![0, 4]));
        assert_eq!(positions("fooBar", &q("fb")), Some(vec![0, 3]));
        assert_eq!(positions("print", &q("pr")), Some(vec![0, 1]));
        assert_eq!(positions("xxfooxfo", &q("fo")), Some(vec![2, 3]));
        assert_eq!(positions("abc", &q("")), Some(vec![]));
        assert_eq!(positions("abc", &q("z")), None);
        assert_eq!(positions("héllo", &q("hl")), Some(vec![0, 2]));
    }

    #[test]
    fn positions_are_consistent_with_matching() {
        for (text, query) in [
            ("get_user_by_id", "gubi"),
            ("HashMap", "hm"),
            ("to_string", "tstr"),
            ("aaaa", "aa"),
            ("some_long_identifier_name", "sln"),
        ] {
            let p = positions(text, &Query::new(query)).expect(text);
            let chars: Vec<char> = text.chars().collect();
            let q: Vec<char> = query.chars().collect();
            assert_eq!(p.len(), q.len());
            assert!(p.windows(2).all(|w| w[0] < w[1]));
            for (pos, qc) in p.iter().zip(q) {
                assert_eq!(fold(chars[*pos]), fold(qc), "{text} / {query}");
            }
        }
    }
}
