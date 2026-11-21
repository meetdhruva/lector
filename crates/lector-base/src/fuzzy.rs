//! The one fuzzy scorer (ADR 0005): case-insensitive subsequence matching, scored fzy-style.
//!
//! # Ranking rules
//!
//! A candidate matches when every non-whitespace character of the query occurs in it, in order
//! (after folding, see below). The score of the best alignment is the sum of
//!
//! * a **boundary bonus** for each matched character: start of the text or after `/` or `\`
//!   (900), after any other non-alphanumeric separator such as `_`, `-`, space or `:` (800),
//!   a lowercase-to-uppercase step as in `fooBar` (700), after `.` or at a letter/digit change
//!   (600);
//! * **1000** for each matched character that directly follows the previous match
//!   (consecutive runs beat scattered matches);
//! * **50** for each matched character whose case equals the query's (`Vec` beats `vec` for
//!   the query `Vec`);
//! * a **gap penalty**: 5 per candidate character before the first match, 10 per skipped
//!   character between matches, nothing after the last match;
//! * **1500** when the candidate starts with the query (prefix), plus **500** when it equals it.
//!
//! Hence prefix > word-boundary / camelCase > scattered. Text after the last match costs nothing,
//! so `fo` scores `foo` and `foobarbaz` alike: callers break ties (completion by `sortText`,
//! [`Matcher::rank`] by shorter text, then input order). Directory-aware bonuses (file name
//! over directory) belong to the file picker, not here.
//!
//! # Folding
//!
//! Matching compares [`fold`]ed characters: ASCII is lowercased; any other character maps to the
//! first character of its Unicode lowercase mapping (`Ä` -> `ä`, `É` -> `é`, `İ` -> `i`), and
//! final sigma `ς` is folded to `σ`. Nothing else is folded: `ß` does not match `ss`, there is no
//! Unicode normalisation (precomposed `é` and `e` + combining accent differ) and accents are not
//! stripped (`e` does not match `é`). Query characters that are whitespace are ignored.
//!
//! Only the first [`Options::max_chars`] characters of a candidate are matched (default 256),
//! which bounds the work per candidate.
//!
//! # Performance
//!
//! A [`Matcher`] owns all scratch buffers: after the first calls nothing allocates in
//! [`Matcher::score`] or [`Matcher::score_indexed`]. Candidates are rejected by a subsequence
//! test on bytes (ASCII) or characters before any scoring; the scoring itself runs on bytes when
//! query and candidate are ASCII. For many scorings of the same candidates, [`Index`] keeps their
//! folded characters and boundary bonuses so repeated queries skip that work.

/// Tuning that does not depend on the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Candidates are matched up to this many characters (the rest is ignored). Default 256.
    pub max_chars: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options { max_chars: 256 }
    }
}

impl Options {
    fn limit(&self) -> usize {
        self.max_chars.clamp(1, u16::MAX as usize)
    }
}

/// A successful match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// Higher is better. Comparable between candidates for the same query only.
    pub score: i32,
    /// Byte offsets (char boundaries, strictly increasing) of the matched characters, one per
    /// non-whitespace query character.
    pub positions: Vec<usize>,
}

/// One ranked candidate, see [`Matcher::rank`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranked {
    /// Index of the candidate in the iterator given to `rank`.
    pub index: usize,
    /// The candidate's score.
    pub score: i32,
    /// Byte offsets of the matched characters (as [`Match::positions`]).
    pub positions: Vec<usize>,
}

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

/// The case folding used for matching, see the module documentation.
pub fn fold(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else if c == 'ς' {
        'σ'
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

/// A query prepared for matching: whitespace removed, characters folded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    folded: Vec<char>,
    orig: Vec<char>,
    folded_ascii: Vec<u8>,
    orig_ascii: Vec<u8>,
    ascii: bool,
}

impl Query {
    /// Prepares `text`; whitespace characters are dropped.
    pub fn new(text: &str) -> Query {
        let orig: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
        let folded: Vec<char> = orig.iter().map(|&c| fold(c)).collect();
        let ascii = orig.iter().all(char::is_ascii);
        let (orig_ascii, folded_ascii) = if ascii {
            (
                orig.iter().map(|&c| c as u8).collect(),
                folded.iter().map(|&c| c as u8).collect(),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        Query {
            folded,
            orig,
            folded_ascii,
            orig_ascii,
            ascii,
        }
    }

    /// True when nothing is left to match (an empty query matches everything with score 0).
    pub fn is_empty(&self) -> bool {
        self.folded.is_empty()
    }

    /// Number of characters to match.
    pub fn len(&self) -> usize {
        self.folded.len()
    }

    /// Whether `other` extends `self` (every text matching `other` matches `self`), so the
    /// results for `other` can be searched among those for `self`.
    pub fn is_prefix_of(&self, other: &Query) -> bool {
        other.folded.starts_with(&self.folded)
    }
}

/// Folded characters and boundary bonuses of many candidates in three arenas, for repeated
/// queries against the same set (completion keystrokes). Score with [`Matcher::score_indexed`].
#[derive(Debug, Clone, Default)]
pub struct Index {
    limit: usize,
    orig: Vec<char>,
    folded: Vec<char>,
    bonus: Vec<i16>,
    spans: Vec<(u32, u16)>,
}

impl Index {
    /// An empty index that keeps the first [`Options::max_chars`] characters of each text.
    pub fn new(options: &Options) -> Index {
        Index::with_capacity(options, 0)
    }

    /// Like [`Index::new`] with room for `items` texts.
    pub fn with_capacity(options: &Options, items: usize) -> Index {
        Index {
            limit: options.limit(),
            spans: Vec::with_capacity(items),
            ..Index::default()
        }
    }

    /// Adds a text; returns its slot (0, 1, 2, ... in push order).
    pub fn push(&mut self, text: &str) -> usize {
        let limit = if self.limit == 0 {
            Options::default().limit()
        } else {
            self.limit
        };
        let start = self.orig.len();
        let mut prev = None;
        for c in text.chars().take(limit) {
            self.orig.push(c);
            self.folded.push(fold(c));
            self.bonus.push(boundary_bonus(prev, c) as i16);
            prev = Some(c);
        }
        let len = self.orig.len() - start;
        self.spans.push((start as u32, len as u16));
        self.spans.len() - 1
    }

    /// Number of texts.
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// True when no text was pushed.
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    fn slices(&self, slot: usize) -> (&[char], &[char], &[i16]) {
        let (start, len) = self.spans[slot];
        let (s, e) = (start as usize, start as usize + len as usize);
        (&self.orig[s..e], &self.folded[s..e], &self.bonus[s..e])
    }
}

#[derive(Debug, Default)]
struct Rows {
    d_prev: Vec<i32>,
    m_prev: Vec<i32>,
    d_cur: Vec<i32>,
    m_cur: Vec<i32>,
}

/// Scores candidates. Holds the scratch buffers, so keep one per thread and reuse it.
#[derive(Debug, Default)]
pub struct Matcher {
    limit: usize,
    rows: Rows,
    orig_c: Vec<char>,
    fold_c: Vec<char>,
    orig_b: Vec<u8>,
    fold_b: Vec<u8>,
    bonus: Vec<i16>,
    offsets: Vec<usize>,
    d_mat: Vec<i32>,
    m_mat: Vec<i32>,
}

fn subsequence<T: Copy + PartialEq>(query: &[T], text: &[T]) -> bool {
    let mut qi = 0;
    for &c in text {
        if qi < query.len() && c == query[qi] {
            qi += 1;
        }
    }
    qi == query.len()
}

impl Matcher {
    /// A matcher with default [`Options`].
    pub fn new() -> Matcher {
        Matcher::with_options(&Options::default())
    }

    /// A matcher that looks at the first `options.max_chars` characters of each candidate.
    pub fn with_options(options: &Options) -> Matcher {
        Matcher {
            limit: options.limit(),
            ..Matcher::default()
        }
    }

    fn limit(&self) -> usize {
        if self.limit == 0 {
            Options::default().limit()
        } else {
            self.limit
        }
    }

    /// Cheap rejection: is the query a subsequence of the candidate's first characters?
    fn prefilter(&self, q: &Query, text: &str) -> bool {
        let limit = self.limit();
        if q.ascii && text.is_ascii() {
            let bytes = &text.as_bytes()[..text.len().min(limit)];
            let mut qi = 0;
            for &b in bytes {
                if qi < q.folded_ascii.len() && b.to_ascii_lowercase() == q.folded_ascii[qi] {
                    qi += 1;
                }
            }
            qi == q.folded_ascii.len()
        } else {
            let mut qi = 0;
            for c in text.chars().take(limit) {
                if qi < q.folded.len() && fold(c) == q.folded[qi] {
                    qi += 1;
                }
            }
            qi == q.folded.len()
        }
    }

    /// Score of `query` against `text`, `None` if it does not match. An empty query matches
    /// everything with score 0. Does not allocate once the buffers have grown.
    pub fn score(&mut self, query: &Query, text: &str) -> Option<i32> {
        if query.is_empty() {
            return Some(0);
        }
        if !self.prefilter(query, text) {
            return None;
        }
        let limit = self.limit();
        if query.ascii && text.is_ascii() {
            let bytes = &text.as_bytes()[..text.len().min(limit)];
            self.orig_b.clear();
            self.orig_b.extend_from_slice(bytes);
            self.fold_b.clear();
            self.fold_b.extend(bytes.iter().map(u8::to_ascii_lowercase));
            self.bonus.clear();
            let mut prev = None;
            for &b in bytes {
                let c = b as char;
                self.bonus.push(boundary_bonus(prev, c) as i16);
                prev = Some(c);
            }
            Some(score_core(
                &mut self.rows,
                &query.folded_ascii,
                &query.orig_ascii,
                &self.orig_b,
                &self.fold_b,
                &self.bonus,
            ))
        } else {
            self.fill_chars(text);
            Some(score_core(
                &mut self.rows,
                &query.folded,
                &query.orig,
                &self.orig_c,
                &self.fold_c,
                &self.bonus,
            ))
        }
    }

    /// [`Matcher::score`] for text `slot` of an [`Index`] (the same score as scoring the text).
    pub fn score_indexed(&mut self, index: &Index, slot: usize, query: &Query) -> Option<i32> {
        if query.is_empty() {
            return Some(0);
        }
        let (orig, folded, bonus) = index.slices(slot);
        if query.len() > folded.len() || !subsequence(&query.folded, folded) {
            return None;
        }
        Some(score_core(
            &mut self.rows,
            &query.folded,
            &query.orig,
            orig,
            folded,
            bonus,
        ))
    }

    fn fill_chars(&mut self, text: &str) {
        let limit = self.limit();
        self.orig_c.clear();
        self.fold_c.clear();
        self.bonus.clear();
        self.offsets.clear();
        let mut prev = None;
        for (at, c) in text.char_indices().take(limit) {
            self.orig_c.push(c);
            self.fold_c.push(fold(c));
            self.bonus.push(boundary_bonus(prev, c) as i16);
            self.offsets.push(at);
            prev = Some(c);
        }
    }

    /// The score and the matched positions (byte offsets) of the best alignment, `None` if
    /// `text` does not match. An empty query matches with score 0 and no positions.
    pub fn matches(&mut self, query: &Query, text: &str) -> Option<Match> {
        if query.is_empty() {
            return Some(Match {
                score: 0,
                positions: Vec::new(),
            });
        }
        if !self.prefilter(query, text) {
            return None;
        }
        self.fill_chars(text);
        let score = score_core(
            &mut self.rows,
            &query.folded,
            &query.orig,
            &self.orig_c,
            &self.fold_c,
            &self.bonus,
        );
        let chars = self.trace(query)?;
        let positions = chars.iter().map(|&j| self.offsets[j]).collect();
        Some(Match { score, positions })
    }

    /// Character indices of the best alignment of `query` in the characters in the buffers.
    fn trace(&mut self, query: &Query) -> Option<Vec<usize>> {
        let (m, n) = (query.folded.len(), self.fold_c.len());
        let (orig, folded, bonus) = (&self.orig_c, &self.fold_c, &self.bonus);
        self.d_mat.clear();
        self.d_mat.resize(m * n, NEG_INF);
        self.m_mat.clear();
        self.m_mat.resize(m * n, NEG_INF);
        let (d, mm) = (&mut self.d_mat, &mut self.m_mat);
        for i in 0..m {
            let gap = if i == m - 1 { 0 } else { GAP_INNER };
            let mut prev_score = NEG_INF;
            for j in 0..n {
                let at = i * n + j;
                if query.folded[i] == folded[j] {
                    let mut sc = NEG_INF;
                    if i == 0 {
                        sc = j as i32 * GAP_LEADING + bonus[j] as i32;
                    } else if j > 0 {
                        let via_gap = mm[at - n - 1].saturating_add(bonus[j] as i32);
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
                    let case = if orig[j] == query.orig[i] {
                        BONUS_CASE
                    } else {
                        0
                    };
                    match_required = i > 0
                        && j > 0
                        && mm[at] == d[at - n - 1].saturating_add(MATCH_CONSECUTIVE) + case;
                    out[i] = j;
                    break;
                }
            }
        }
        Some(out)
    }

    /// The best `limit` of `candidates` for `query`, best first: higher score, then shorter text
    /// (fewer characters), then lower index. Non-matching candidates are left out. An empty query
    /// keeps the first `limit` candidates in input order. Positions are computed for the
    /// returned candidates only.
    pub fn rank<S: AsRef<str>>(
        &mut self,
        query: &Query,
        candidates: &[S],
        limit: usize,
    ) -> Vec<Ranked> {
        self.rank_by(query, candidates, S::as_ref, limit)
    }

    /// [`Matcher::rank`] over any items, matching against the text `text(item)` returns.
    pub fn rank_by<T>(
        &mut self,
        query: &Query,
        items: &[T],
        text: impl Fn(&T) -> &str,
        limit: usize,
    ) -> Vec<Ranked> {
        if limit == 0 {
            return Vec::new();
        }
        // (score, chars, index)
        let mut scored: Vec<(i32, usize, usize)> = Vec::new();
        for (index, item) in items.iter().enumerate() {
            if query.is_empty() {
                if index >= limit {
                    break;
                }
                scored.push((0, 0, index));
            } else {
                let t = text(item);
                if let Some(score) = self.score(query, t) {
                    scored.push((score, t.chars().count(), index));
                }
            }
        }
        let order = |a: &(i32, usize, usize), b: &(i32, usize, usize)| {
            b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2))
        };
        if scored.len() > limit {
            scored.select_nth_unstable_by(limit - 1, order);
            scored.truncate(limit);
        }
        scored.sort_unstable_by(order);
        scored
            .into_iter()
            .map(|(score, _, index)| Ranked {
                index,
                score,
                positions: self
                    .matches(query, text(&items[index]))
                    .map(|m| m.positions)
                    .unwrap_or_default(),
            })
            .collect()
    }
}

/// `query` against `candidate` in one call (allocates; use a [`Matcher`] in loops).
pub fn score(query: &str, candidate: &str, options: &Options) -> Option<Match> {
    Matcher::with_options(options).matches(&Query::new(query), candidate)
}

/// The score of the best alignment. `qf`/`qo` are the folded/original query, `orig`/`folded`/
/// `bonus` the candidate; the caller has checked that the query is a subsequence.
fn score_core<T: Copy + PartialEq>(
    rows: &mut Rows,
    qf: &[T],
    qo: &[T],
    orig: &[T],
    folded: &[T],
    bonus: &[i16],
) -> i32 {
    let (m, n) = (qf.len(), folded.len());
    let prefix = folded[..m] == *qf;
    let mut total = if m == 1 {
        // Best single match: the best-bonus occurrence, with a leading-gap penalty.
        let mut best = NEG_INF;
        for j in 0..n {
            if folded[j] == qf[0] {
                let mut s = j as i32 * GAP_LEADING + bonus[j] as i32;
                if orig[j] == qo[0] {
                    s += BONUS_CASE;
                }
                best = best.max(s);
            }
        }
        best
    } else {
        dp_score(rows, qf, qo, orig, folded, bonus)
    };
    if prefix {
        total += BONUS_PREFIX;
        if m == n {
            total += BONUS_EXACT;
        }
    }
    total
}

fn dp_score<T: Copy + PartialEq>(
    s: &mut Rows,
    qf: &[T],
    qo: &[T],
    orig: &[T],
    folded: &[T],
    bonus: &[i16],
) -> i32 {
    let (m, n) = (qf.len(), folded.len());
    for v in [&mut s.d_prev, &mut s.m_prev, &mut s.d_cur, &mut s.m_cur] {
        v.clear();
        v.resize(n, NEG_INF);
    }
    for i in 0..m {
        let gap = if i == m - 1 { 0 } else { GAP_INNER };
        let qc = qf[i];
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
                if sc > NEG_INF && orig[j] == qo[i] {
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn s(query: &str, text: &str) -> Option<i32> {
        Matcher::new().score(&Query::new(query), text)
    }

    fn m(query: &str, text: &str) -> Option<Match> {
        score(query, text, &Options::default())
    }

    fn order(query: &str, candidates: &[&str]) -> Vec<String> {
        let reversed: Vec<&str> = candidates.iter().rev().copied().collect();
        Matcher::new()
            .rank(&Query::new(query), &reversed, usize::MAX)
            .into_iter()
            .map(|r| reversed[r.index].to_string())
            .collect()
    }

    /// The shared ranking table (ADR 0005): query -> candidates, best first. It is the union of
    /// the ranking tests of the old completion matcher (lector-lsp) and the old picker matcher
    /// (lector-ui). Candidates are fed in reverse order, so a tie would fail the test too.
    #[test]
    fn shared_ranking_table() {
        let table: &[(&str, &[&str])] = &[
            // Prefix beats later occurrences.
            ("fo", &["foo", "xfoo"]),
            ("fo", &["foobar", "buffoon"]),
            ("pr", &["print", "xxprint"]),
            ("name", &["name", "rename"]),
            // Word boundaries (separator, camelCase) beat scattered matches.
            ("fb", &["foo_bar", "fxxbxx"]),
            ("fb", &["fooBar", "fobar_x"]),
            ("gc", &["get_count", "gic"]),
            ("sl", &["slice", "selfLen"]),
            ("ab", &["ab", "a_b", "axb"]),
            ("tsb", &["Toggle Side Bar", "tasbar", "testsubject"]),
            // Contiguity and word starts in paths.
            ("main", &["src/main.rs", "src/domain.rs"]),
            ("main", &["main.rs", "main_window.rs"]),
            ("lib", &["src/lib.rs", "src/public_library.rs"]),
            // Case: an exact-case match beats a different-case one.
            ("Vec", &["Vec", "vec"]),
            ("vec", &["vec", "Vec"]),
            // Equal scores: shorter text first, then input order (picker tie-break).
            ("fo", &["foo", "foobar", "foobarbaz"]),
            // Exact beats prefix.
            ("name", &["name", "names"]),
        ];
        for (query, expected) in table {
            assert_eq!(
                order(query, expected),
                expected.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
                "query {query:?}"
            );
        }
    }

    #[test]
    fn scores_order_without_the_tie_break_where_the_old_lsp_table_did() {
        // The completion matcher compared raw scores: the better one must be strictly higher.
        for (q, better, worse) in [
            ("fo", "foo", "xfoo"),
            ("fb", "fooBar", "fobar_x"),
            ("Vec", "Vec", "vec"),
            ("vec", "vec", "Vec"),
            ("ab", "a_b", "axb"),
        ] {
            let (a, b) = (s(q, better).unwrap(), s(q, worse).unwrap());
            assert!(a > b, "{q}: {better} ({a}) should beat {worse} ({b})");
        }
    }

    #[test]
    fn trailing_text_does_not_matter() {
        assert_eq!(s("fo", "foo"), s("fo", "foobarbaz"));
    }

    #[test]
    fn subsequence_required_and_empty_query_matches_everything() {
        assert!(s("fb", "foo_bar").is_some());
        assert!(s("bf", "foo_bar").is_none());
        assert!(s("foox", "foo").is_none());
        assert!(s("abd", "abc").is_none());
        assert!(s("abc", "a_b_c").is_some());
        assert_eq!(s("", "anything"), Some(0));
        assert_eq!(s("  \t", "anything"), Some(0));
        assert_eq!(m("", "x").unwrap().positions, Vec::<usize>::new());
        assert_eq!(s("a b", "ab"), s("ab", "ab"));
    }

    #[test]
    fn folding_is_documented_and_exact() {
        assert!(s("ärg", "Ärger").is_some());
        assert!(s("ÄRG", "ärger").is_some());
        assert!(s("é", "café").is_some());
        assert!(s("é", "CAFÉ").is_some());
        assert!(s("σ", "ς").is_some(), "final sigma folds to sigma");
        assert!(s("ς", "Σ").is_some());
        assert!(
            s("i", "İstanbul").is_some(),
            "İ folds to i (first char of its lowercase)"
        );
        // Not folded: multi-char expansion, normalisation, accents.
        assert!(s("straße", "STRASSE").is_none());
        assert!(s("ss", "straße").is_none());
        assert!(s("e", "café").is_none());
        assert!(s("e\u{301}", "caf\u{e9}").is_none());
        assert_eq!(fold('A'), 'a');
        assert_eq!(fold('Ω'), 'ω');
        assert_eq!(fold('1'), '1');
    }

    #[test]
    fn ascii_and_unicode_paths_agree() {
        // The same ASCII text scored on the byte path and (through a non-ASCII query char that
        // folds to ASCII: KELVIN SIGN -> k) on the char path.
        let ascii = s("kb", "kilo_byte").unwrap();
        let chars = s("\u{212a}b", "kilo_byte").unwrap();
        // The case bonus differs (the query's K is not the candidate's k), nothing else.
        assert_eq!(ascii - chars, BONUS_CASE);
    }

    #[test]
    fn long_text_is_truncated_not_rejected_with_a_panic() {
        let text = "a".repeat(300);
        assert!(s("aa", &text).is_some());
        assert!(s("b", &text).is_none());
        let tail = format!("{}b", "a".repeat(300));
        assert!(
            s("b", &tail).is_none(),
            "characters past max_chars are ignored"
        );
        let wide = Options { max_chars: 400 };
        assert!(score("b", &tail, &wide).is_some());
        assert!(Matcher::with_options(&Options { max_chars: 0 })
            .score(&Query::new("a"), "a")
            .is_some());
    }

    #[test]
    fn positions_are_byte_offsets_of_the_best_alignment() {
        let p = |q: &str, t: &str| m(q, t).map(|m| m.positions);
        assert_eq!(p("fb", "foo_bar"), Some(vec![0, 4]));
        assert_eq!(p("fb", "fooBar"), Some(vec![0, 3]));
        assert_eq!(p("pr", "print"), Some(vec![0, 1]));
        assert_eq!(p("fo", "xxfooxfo"), Some(vec![2, 3]));
        assert_eq!(p("ts", "toggle sidebar"), Some(vec![0, 7]));
        assert_eq!(p("", "abc"), Some(vec![]));
        assert_eq!(p("z", "abc"), None);
        assert_eq!(p("main", "src/main.rs"), Some(vec![4, 5, 6, 7]));
    }

    #[test]
    fn positions_are_correct_for_multi_byte_text() {
        let p = |q: &str, t: &str| m(q, t).map(|m| m.positions);
        assert_eq!(p("é", "café"), Some(vec![3]));
        assert_eq!(p("hl", "héllo"), Some(vec![0, 3]));
        assert_eq!(p("日本", "今日の日本語"), Some(vec![9, 12]));
        assert_eq!(p("ab", "αβaβb"), Some(vec![4, 7]));
        let text = "ünï_cödé";
        let pos = p("ücd", text).unwrap();
        assert!(pos.iter().all(|&i| text.is_char_boundary(i)));
        assert_eq!(
            pos.iter()
                .map(|&i| text[i..].chars().next().unwrap())
                .collect::<String>(),
            "ücd"
        );
    }

    #[test]
    fn match_score_equals_scorer_score() {
        for (q, t) in [
            ("get_user", "get_user_by_id"),
            ("hm", "HashMap"),
            ("tstr", "to_string"),
            ("ln", "Lïne_nö"),
        ] {
            assert_eq!(m(q, t).unwrap().score, s(q, t).unwrap(), "{q} {t}");
        }
    }

    #[test]
    fn indexed_scores_equal_direct_scores() {
        let texts = ["get_count", "getCount", "GC", "né_ü", "x", ""];
        let mut index = Index::new(&Options::default());
        for t in texts {
            index.push(t);
        }
        assert_eq!(index.len(), texts.len());
        assert!(!index.is_empty());
        let mut matcher = Matcher::new();
        for q in ["gc", "g", "né", "", "zz", "ü"] {
            let query = Query::new(q);
            for (slot, t) in texts.iter().enumerate() {
                assert_eq!(
                    matcher.score_indexed(&index, slot, &query),
                    matcher.score(&query, t),
                    "{q:?} vs {t:?}"
                );
            }
        }
    }

    #[test]
    fn query_prefix_relation() {
        let (a, ab) = (Query::new("a"), Query::new("A b"));
        assert!(a.is_prefix_of(&ab));
        assert!(!ab.is_prefix_of(&a));
        assert!(Query::default().is_prefix_of(&a));
        assert_eq!(ab.len(), 2);
    }

    #[test]
    fn rank_limits_orders_and_gives_positions() {
        let items = ["xxfoo", "foo", "bar", "foo_bar", "fOo"];
        let mut mt = Matcher::new();
        let all = mt.rank(&Query::new("foo"), &items, usize::MAX);
        let names: Vec<&str> = all.iter().map(|r| items[r.index]).collect();
        assert_eq!(names, ["foo", "fOo", "foo_bar", "xxfoo"]);
        assert_eq!(all[0].positions, vec![0, 1, 2]);
        let top2 = mt.rank(&Query::new("foo"), &items, 2);
        assert_eq!(top2.iter().map(|r| r.index).collect::<Vec<_>>(), [1, 4]);
        assert!(mt.rank(&Query::new("foo"), &items, 0).is_empty());
        let none = mt.rank(&Query::new("qq"), &items, 5);
        assert!(none.is_empty());
        let empty = mt.rank(&Query::new(""), &items, 3);
        assert_eq!(empty.iter().map(|r| r.index).collect::<Vec<_>>(), [0, 1, 2]);
        let by = mt.rank_by(&Query::new("b"), &[(1, "abc"), (2, "bcd")], |t| t.1, 9);
        assert_eq!(by[0].index, 1);
    }

    #[test]
    fn long_queries_do_not_panic() {
        let long_q = "a".repeat(500);
        assert_eq!(s(&long_q, "aaa"), None);
        assert_eq!(s(&long_q, &"a".repeat(600)), None);
        assert!(m(&"ab".repeat(20), &"ab".repeat(100)).is_some());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn never_panics_on_arbitrary_unicode(q in "\\PC{0,12}", t in "\\PC{0,60}") {
            let mut mt = Matcher::new();
            let query = Query::new(&q);
            let _ = mt.score(&query, &t);
            let _ = mt.matches(&query, &t);
            let _ = mt.rank(&query, &[t.as_str(), "x"], 3);
        }

        #[test]
        fn deterministic_and_reusing_a_matcher_changes_nothing(q in "[a-zA-Z_é]{0,6}", t in "[a-zA-Z_é ./]{0,40}", u in "[a-zA-Z_é]{0,40}") {
            let query = Query::new(&q);
            let mut one = Matcher::new();
            let first = one.score(&query, &t);
            let _ = one.score(&query, &u); // dirty the scratch buffers
            let _ = one.matches(&query, &u);
            prop_assert_eq!(one.score(&query, &t), first);
            prop_assert_eq!(Matcher::new().score(&query, &t), first);
            prop_assert_eq!(m(&q, &t).map(|x| x.score), first);
        }

        #[test]
        fn positions_are_increasing_char_boundaries_and_spell_the_query(q in "[a-zA-Zé日_]{1,6}", t in "[a-zA-Zé日_ /.]{0,50}") {
            if let Some(found) = m(&q, &t) {
                prop_assert_eq!(found.positions.len(), q.chars().count());
                prop_assert!(found.positions.windows(2).all(|w| w[0] < w[1]));
                let mut folded_query = q.chars().map(fold);
                for &p in &found.positions {
                    prop_assert!(t.is_char_boundary(p));
                    let c = t[p..].chars().next().unwrap();
                    prop_assert_eq!(Some(fold(c)), folded_query.next());
                }
            }
        }

        #[test]
        fn no_match_without_the_characters_in_order(q in "[a-e]{1,5}", t in "[a-e]{0,20}") {
            let in_order = {
                let mut it = t.chars();
                q.chars().all(|c| it.any(|x| x == c))
            };
            prop_assert_eq!(s(&q, &t).is_some(), in_order);
        }

        #[test]
        fn adding_a_matching_prefix_never_lowers_rank_below_a_scattered_match(q in "[a-m]{2,4}", pad in "[n-z]{1,4}") {
            // A: the query typed in full at the start; B: the same letters scattered with
            // padding. Appending text to A (not changing the prefix) keeps A above B.
            let scattered: String = q.chars().flat_map(|c| [c].into_iter().chain(pad.chars())).collect();
            let rank_of = |first: &str, second: &str| {
                let items = [first, second];
                Matcher::new().rank(&Query::new(&q), &items, 2)[0].index
            };
            for extra in ["", "_more", "Tail", "zzzzzz"] {
                let prefixed = format!("{q}{extra}");
                prop_assert_eq!(rank_of(&prefixed, &scattered), 0);
                prop_assert_eq!(rank_of(&scattered, &prefixed), 1);
            }
        }

        #[test]
        fn rank_agrees_with_score_and_is_sorted(q in "[a-c]{1,3}", ts in proptest::collection::vec("[a-c_]{0,8}", 0..12)) {
            let mut mt = Matcher::new();
            let query = Query::new(&q);
            let ranked = mt.rank(&query, &ts, usize::MAX);
            let expected = ts.iter().filter(|t| mt.score(&query, t).is_some()).count();
            prop_assert_eq!(ranked.len(), expected);
            for w in ranked.windows(2) {
                prop_assert!(w[0].score >= w[1].score);
            }
            for r in &ranked {
                prop_assert_eq!(Some(r.score), mt.score(&query, &ts[r.index]));
            }
        }
    }
}
