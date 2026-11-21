//! The one fuzzy scorer (ADR 0005): case-insensitive subsequence matching, scored fzy-style.
//!
//! # Ranking rules
//!
//! The query is split on whitespace into **atoms** (leading, trailing and repeated whitespace
//! is ignored). A candidate matches when every atom matches it; the atoms are independent and
//! order-free (`push git` finds `Git: Push`), may overlap on the same characters, and the
//! candidate's score is the sum of the atom scores, so more specific matches still win. The
//! match positions are the sorted union of the atoms' positions. A query of one atom (every
//! completion query) is scored exactly as the atom alone. An empty or all-whitespace query
//! matches everything with score 0.
//!
//! An atom matches when every character of it occurs in the candidate, in order (after folding,
//! see below). The score of the best alignment of one atom is the sum of
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
//! stripped (`e` does not match `é`). Whitespace is never matched: it separates atoms.
//!
//! Only the first [`Options::max_chars`] characters of a candidate are matched (default 256),
//! which bounds the work per candidate.
//!
//! # Long atoms
//!
//! The alignment costs `atom length x candidate length`, so an atom longer than
//! [`MAX_FUZZY_ATOM_CHARS`] (64) characters, typically a pasted identifier or path, is not
//! matched as a subsequence. It is matched as a case-insensitive **contiguous substring** of the
//! candidate's first characters (linear time). A candidate without that substring does not
//! match. The substring scores what the alignment would give a contiguous match (boundary bonus
//! of its first character, 1000 per following character, 50 per character of equal case, prefix
//! and exact bonuses); when it occurs several times, the occurrence with the best boundary
//! bonus minus leading gap is taken, the earliest on a tie. The positions are the run.
//!
//! # Work
//!
//! A [`Matcher`] counts the work it does in units of about one alignment cell (see
//! [`Matcher::work`]). [`Matcher::rank_by_budgeted`] gives up when a ranking would exceed a
//! budget ([`INTERACTIVE_WORK_BUDGET`] suits a UI thread), and the cancellable rankings look at
//! their token every [`CANCEL_CHECK_WORK`] units, whatever the length of query and candidates.
//!
//! # Performance
//!
//! A [`Matcher`] owns all scratch buffers: after the first calls nothing allocates in
//! [`Matcher::score`] or [`Matcher::score_indexed`]. Candidates are rejected by a subsequence
//! test on bytes (ASCII) or characters before any scoring; the scoring itself runs on bytes when
//! query and candidate are ASCII. For many scorings of the same candidates, [`Index`] keeps their
//! folded characters and boundary bonuses so repeated queries skip that work.

use crate::{CancelToken, Cancelled};

mod substring;

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

/// Atoms longer than this many characters are matched as a contiguous substring instead of a
/// subsequence (see the module documentation).
pub const MAX_FUZZY_ATOM_CHARS: usize = 64;

/// A query keeps at most this many atoms; later words are ignored, so the work per candidate
/// has a bound whatever is pasted into the query.
pub const MAX_QUERY_ATOMS: usize = 16;

/// [`Matcher::rank_cancellable`] looks at its token whenever this much [`Matcher::work`] has been
/// done since the last look: about 0.1 ms in the workspace dev profile, so cancellation is
/// prompt for 1,000 short candidates and for 100 candidates of 256 characters alike.
pub const CANCEL_CHECK_WORK: u64 = 32_768;

/// The work a ranking may spend on a UI thread ([`Matcher::rank_by_budgeted`]): 500,000 units.
/// Measured in the workspace dev profile (`opt-level = 1`; `calibrate_work_units` in
/// `fuzzy/work_tests.rs` prints it): one unit takes 1.9 to 3.1 ns on typical names, 256-character
/// names, all-`a` worst cases for the alignment and substring searches, and 4.8 ns on the worst
/// shape found (a two-letter alphabet, where half the cells branch each way). The budget is
/// therefore 1.0 to 1.6 ms, 2.4 ms at worst, and less in a release build. The candidate in
/// progress when the budget runs out adds at most one atom's alignment (16,500 units, 0.05 ms).
pub const INTERACTIVE_WORK_BUDGET: u64 = 500_000;

/// The least work charged for a candidate (the call and its buffers): a list of `n` candidates
/// costs at least `n * WORK_PER_CANDIDATE`.
pub const WORK_PER_CANDIDATE: u64 = 8;

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

/// One whitespace-free term of a query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Atom {
    folded: Vec<char>,
    orig: Vec<char>,
    folded_ascii: Vec<u8>,
    orig_ascii: Vec<u8>,
    ascii: bool,
    /// Longer than [`MAX_FUZZY_ATOM_CHARS`]: matched as a substring, with this failure table.
    fail: Option<Vec<u32>>,
}

impl Atom {
    fn new(text: &str) -> Atom {
        let orig: Vec<char> = text.chars().collect();
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
        let fail = (orig.len() > MAX_FUZZY_ATOM_CHARS).then(|| substring::failure_table(&folded));
        Atom {
            folded,
            orig,
            folded_ascii,
            orig_ascii,
            ascii,
            fail,
        }
    }
}

/// A query prepared for matching: split on whitespace into atoms, characters folded. See the
/// module documentation for how several atoms combine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    atoms: Vec<Atom>,
    ascii: bool,
}

impl Query {
    /// Prepares `text`: it is split on whitespace (Unicode `White_Space`) into atoms; leading,
    /// trailing and repeated whitespace is ignored, and so are the words after the first
    /// [`MAX_QUERY_ATOMS`].
    pub fn new(text: &str) -> Query {
        let atoms: Vec<Atom> = text
            .split_whitespace()
            .take(MAX_QUERY_ATOMS)
            .map(Atom::new)
            .collect();
        let ascii = atoms.iter().all(|a| a.ascii);
        Query { atoms, ascii }
    }

    /// True when there is no atom (an empty or all-whitespace query matches everything with
    /// score 0).
    pub fn is_empty(&self) -> bool {
        self.atoms.is_empty()
    }

    /// Number of characters to match, over all atoms.
    pub fn len(&self) -> usize {
        self.atoms.iter().map(|a| a.folded.len()).sum()
    }

    /// Whether `other` extends `self`: every text matching `other` matches `self`, so the
    /// results for `other` can be searched among those for `self`. True when all atoms of `self`
    /// but the last equal the first atoms of `other`, and the last one is a prefix of the
    /// corresponding atom of `other` (typing at the end of the query). Sound, not complete:
    /// reordered atoms are not recognised.
    pub fn is_prefix_of(&self, other: &Query) -> bool {
        let Some((last, head)) = self.atoms.split_last() else {
            return true;
        };
        other.atoms.len() >= self.atoms.len()
            && head
                .iter()
                .zip(&other.atoms)
                .all(|(a, b)| a.folded == b.folded)
            && other.atoms[head.len()].folded.starts_with(&last.folded)
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
    work: u64,
    /// While ranking under a budget: the `work` value past which scoring gives up in the middle
    /// of a candidate (the caller then discards the answer). `0` means no limit.
    stop_at: u64,
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

    /// Whether the candidate's first characters are all ASCII. Looks at those characters only,
    /// so it costs at most `max_chars` however long the text is.
    fn head_is_ascii(&self, text: &str) -> bool {
        text.as_bytes()[..text.len().min(self.limit())].is_ascii()
    }

    /// The work done so far, in units of about one alignment cell: the alignment costs one per
    /// cell (query characters x candidate characters), a test of a candidate, a substring search
    /// and the preparation of a candidate one per character, plus a few per candidate. It only
    /// grows; take differences. Bounded per candidate by
    /// `MAX_FUZZY_ATOM_CHARS x max_chars x (number of atoms)`, never by the length of a long atom
    /// or text.
    pub fn work(&self) -> u64 {
        self.work
    }

    fn add_work(&mut self, units: usize) {
        self.work = self.work.saturating_add(units as u64);
    }

    /// Whether a ranking under a budget has overrun it: scoring then stops early and its answer
    /// is meaningless (the ranking discards it).
    fn overrun(&self) -> bool {
        self.stop_at != 0 && self.work > self.stop_at
    }

    /// Cheap rejection: is every (short) atom a subsequence of the candidate's first characters?
    /// Long atoms are not tested here: their substring search is the test.
    fn prefilter(&mut self, q: &Query, text: &str, text_ascii: bool) -> bool {
        let limit = self.limit();
        self.add_work(WORK_PER_CANDIDATE as usize);
        let scanned = text.len().min(limit);
        let mut cost = 0;
        let passed = q.atoms.iter().filter(|a| a.fail.is_none()).all(|a| {
            cost += scanned;
            if a.ascii && text_ascii {
                let bytes = &text.as_bytes()[..text.len().min(limit)];
                let mut qi = 0;
                for &b in bytes {
                    if qi < a.folded_ascii.len() && b.to_ascii_lowercase() == a.folded_ascii[qi] {
                        qi += 1;
                    }
                }
                qi == a.folded_ascii.len()
            } else {
                let mut qi = 0;
                for c in text.chars().take(limit) {
                    if qi < a.folded.len() && fold(c) == a.folded[qi] {
                        qi += 1;
                    }
                }
                qi == a.folded.len()
            }
        });
        self.add_work(cost);
        passed
    }

    /// Score of `query` against `text`, `None` if it does not match. With several atoms every
    /// atom must match and the score is the sum of the atom scores. An empty query matches
    /// everything with score 0. Does not allocate once the buffers have grown.
    pub fn score(&mut self, query: &Query, text: &str) -> Option<i32> {
        if query.is_empty() {
            return Some(0);
        }
        let text_ascii = self.head_is_ascii(text);
        if !self.prefilter(query, text, text_ascii) {
            return None;
        }
        let limit = self.limit();
        let mut total = 0;
        if query.ascii && text_ascii {
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
            let n = bytes.len();
            self.add_work(n);
            for a in &query.atoms {
                if self.overrun() {
                    return None;
                }
                total += match &a.fail {
                    Some(fail) => {
                        self.add_work(3 * n);
                        let parts = (&a.folded_ascii[..], &a.orig_ascii[..], &fail[..]);
                        substring::score(parts, &self.orig_b, &self.fold_b, &self.bonus)?.0
                    }
                    None => {
                        self.add_work(a.folded_ascii.len() * n);
                        score_core(
                            &mut self.rows,
                            &a.folded_ascii,
                            &a.orig_ascii,
                            &self.orig_b,
                            &self.fold_b,
                            &self.bonus,
                        )
                    }
                };
            }
        } else {
            self.fill_chars(text);
            let n = self.fold_c.len();
            self.add_work(n);
            for a in &query.atoms {
                if self.overrun() {
                    return None;
                }
                total += match &a.fail {
                    Some(fail) => {
                        self.add_work(3 * n);
                        let parts = (&a.folded[..], &a.orig[..], &fail[..]);
                        substring::score(parts, &self.orig_c, &self.fold_c, &self.bonus)?.0
                    }
                    None => {
                        self.add_work(a.folded.len() * n);
                        score_core(
                            &mut self.rows,
                            &a.folded,
                            &a.orig,
                            &self.orig_c,
                            &self.fold_c,
                            &self.bonus,
                        )
                    }
                };
            }
        }
        Some(total)
    }

    /// [`Matcher::score`] for text `slot` of an [`Index`] (the same score as scoring the text).
    pub fn score_indexed(&mut self, index: &Index, slot: usize, query: &Query) -> Option<i32> {
        let (orig, folded, bonus) = index.slices(slot);
        let n = folded.len();
        self.add_work(WORK_PER_CANDIDATE as usize);
        for a in &query.atoms {
            self.add_work(n);
            if a.folded.len() > n || (a.fail.is_none() && !subsequence(&a.folded, folded)) {
                return None;
            }
        }
        let mut total = 0;
        for a in &query.atoms {
            total += match &a.fail {
                Some(fail) => {
                    self.add_work(3 * n);
                    let parts = (&a.folded[..], &a.orig[..], &fail[..]);
                    substring::score(parts, orig, folded, bonus)?.0
                }
                None => {
                    self.add_work(a.folded.len() * n);
                    score_core(&mut self.rows, &a.folded, &a.orig, orig, folded, bonus)
                }
            };
        }
        Some(total)
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
    /// `text` does not match. With several atoms the positions are the sorted union of the
    /// atoms' positions without duplicates. An empty query matches with score 0 and no positions.
    pub fn matches(&mut self, query: &Query, text: &str) -> Option<Match> {
        if query.is_empty() {
            return Some(Match {
                score: 0,
                positions: Vec::new(),
            });
        }
        let text_ascii = self.head_is_ascii(text);
        if !self.prefilter(query, text, text_ascii) {
            return None;
        }
        self.fill_chars(text);
        let n = self.fold_c.len();
        self.add_work(n);
        let mut score = 0;
        let mut chars: Vec<usize> = Vec::with_capacity(query.len().min(n));
        for a in &query.atoms {
            if self.overrun() {
                return None;
            }
            if let Some(fail) = &a.fail {
                self.add_work(3 * n);
                let parts = (&a.folded[..], &a.orig[..], &fail[..]);
                let (sc, start) = substring::score(parts, &self.orig_c, &self.fold_c, &self.bonus)?;
                score += sc;
                chars.extend(start..start + a.folded.len());
                continue;
            }
            // The alignment, again for its score, then its trace back.
            self.add_work(4 * a.folded.len() * n);
            score += score_core(
                &mut self.rows,
                &a.folded,
                &a.orig,
                &self.orig_c,
                &self.fold_c,
                &self.bonus,
            );
            chars.extend(self.trace(a)?);
        }
        if query.atoms.len() > 1 {
            chars.sort_unstable();
            chars.dedup();
        }
        let positions = chars.iter().map(|&j| self.offsets[j]).collect();
        Some(Match { score, positions })
    }

    /// Character indices of the best alignment of `atom` in the characters in the buffers.
    fn trace(&mut self, query: &Atom) -> Option<Vec<usize>> {
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
    /// (fewer characters, counting at most [`Options::max_chars`] of them), then lower index.
    /// Non-matching candidates are left out. An empty query keeps the first `limit` candidates
    /// in input order. Positions are computed for the returned candidates only.
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
        // Without a token or a budget the ranking cannot stop early.
        self.rank_impl(query, items, text, limit, Control::default())
            .unwrap_or_default()
    }

    /// [`Matcher::rank`] that stops with `Err(Cancelled)` soon after `cancel` is set, for ranking
    /// large lists off the UI thread. The token is looked at every [`CANCEL_CHECK_WORK`] units
    /// of [`Matcher::work`], so it is prompt whatever the length of the query and of the
    /// candidates (not once per number of candidates).
    pub fn rank_cancellable<S: AsRef<str>>(
        &mut self,
        query: &Query,
        candidates: &[S],
        limit: usize,
        cancel: &CancelToken,
    ) -> Result<Vec<Ranked>, Cancelled> {
        self.rank_by_cancellable(query, candidates, S::as_ref, limit, cancel)
    }

    /// [`Matcher::rank_by`] with cancellation, see [`Matcher::rank_cancellable`].
    pub fn rank_by_cancellable<T>(
        &mut self,
        query: &Query,
        items: &[T],
        text: impl Fn(&T) -> &str,
        limit: usize,
        cancel: &CancelToken,
    ) -> Result<Vec<Ranked>, Cancelled> {
        let control = Control {
            cancel: Some(cancel),
            max_work: u64::MAX,
        };
        self.rank_impl(query, items, text, limit, control)
            .map_err(|_| Cancelled)
    }

    /// [`Matcher::rank_by`] for a thread that must not stall: `None` as soon as the ranking has
    /// done more than `max_work` units of [`Matcher::work`] (see [`INTERACTIVE_WORK_BUDGET`]),
    /// with nothing kept. The work wasted is at most `max_work` plus one candidate. Not
    /// overrunning is decided by the work actually done (a candidate the cheap test rejects
    /// costs little), not by the number of candidates.
    pub fn rank_by_budgeted<T>(
        &mut self,
        query: &Query,
        items: &[T],
        text: impl Fn(&T) -> &str,
        limit: usize,
        max_work: u64,
    ) -> Option<Vec<Ranked>> {
        let control = Control {
            cancel: None,
            max_work,
        };
        self.stop_at = self.work.saturating_add(max_work).max(1);
        let ranked = self.rank_impl(query, items, text, limit, control).ok();
        self.stop_at = 0;
        ranked
    }

    fn rank_impl<T>(
        &mut self,
        query: &Query,
        items: &[T],
        text: impl Fn(&T) -> &str,
        limit: usize,
        control: Control<'_>,
    ) -> Result<Vec<Ranked>, Stop> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let start = self.work;
        let mut next_check = start;
        let mut guard = |m: &Matcher| -> Result<(), Stop> {
            let done = m.work - start;
            if done > control.max_work {
                return Err(Stop::OverBudget);
            }
            if m.work >= next_check {
                next_check = m.work.saturating_add(CANCEL_CHECK_WORK);
                if let Some(cancel) = control.cancel {
                    cancel.check().map_err(|_| Stop::Cancelled)?;
                }
            }
            Ok(())
        };
        let max_chars = self.limit();
        // (score, chars, index)
        let mut scored: Vec<(i32, usize, usize)> = Vec::new();
        for (index, item) in items.iter().enumerate() {
            guard(self)?;
            if query.is_empty() {
                if index >= limit {
                    break;
                }
                self.add_work(WORK_PER_CANDIDATE as usize);
                scored.push((0, 0, index));
            } else {
                let t = text(item);
                let score = self.score(query, t);
                // After the call: a candidate abandoned in the middle must not count.
                guard(self)?;
                if let Some(score) = score {
                    scored.push((score, t.chars().take(max_chars).count(), index));
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
        let mut ranked = Vec::with_capacity(scored.len());
        for (score, _, index) in scored {
            guard(self)?;
            let positions = self
                .matches(query, text(&items[index]))
                .map(|m| m.positions)
                .unwrap_or_default();
            guard(self)?;
            ranked.push(Ranked {
                index,
                score,
                positions,
            });
        }
        guard(self)?;
        Ok(ranked)
    }
}

/// What may end a ranking early.
#[derive(Clone, Copy)]
struct Control<'a> {
    cancel: Option<&'a CancelToken>,
    max_work: u64,
}

impl Default for Control<'_> {
    fn default() -> Self {
        Control {
            cancel: None,
            max_work: u64::MAX,
        }
    }
}

/// Why a ranking ended early.
enum Stop {
    Cancelled,
    OverBudget,
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
mod tests;
#[cfg(test)]
mod work_tests;
