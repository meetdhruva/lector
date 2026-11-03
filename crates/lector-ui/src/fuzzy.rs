//! A small fuzzy matcher for pickers (case-insensitive subsequence with
//! word-boundary and contiguity bonuses). Integration swaps in
//! `lector_search::Picker` (nucleo) for large candidate sets.

/// A successful match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuzzyMatch {
    /// Higher is better.
    pub score: i64,
    /// Byte offsets of matched characters in the candidate.
    pub positions: Vec<usize>,
}

fn is_boundary(prev: Option<char>, c: char) -> bool {
    match prev {
        None => true,
        Some(p) => {
            matches!(p, '/' | '\\' | '_' | '-' | ' ' | '.' | ':')
                || (p.is_lowercase() && c.is_uppercase())
                || (!p.is_alphanumeric() && c.is_alphanumeric())
        }
    }
}

/// Matches `query` against `candidate`. Whitespace in the query is ignored.
/// Returns `None` if not every query character appears in order.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<FuzzyMatch> {
    let query: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if query.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            positions: Vec::new(),
        });
    }
    let chars: Vec<(usize, char)> = candidate.char_indices().collect();
    let lower: Vec<char> = chars
        .iter()
        .map(|(_, c)| c.to_lowercase().next().unwrap_or(*c))
        .collect();

    // Try each possible start of the first query char and keep the best
    // greedy alignment; bounded to keep this linear-ish for long candidates.
    let mut best: Option<FuzzyMatch> = None;
    let starts: Vec<usize> = (0..lower.len())
        .filter(|&i| lower[i] == query[0])
        .take(16)
        .collect();
    for start in starts {
        let mut positions = Vec::with_capacity(query.len());
        let mut score: i64 = 0;
        let mut qi = 0;
        let mut ci = start;
        let mut last: Option<usize> = None;
        while qi < query.len() && ci < lower.len() {
            if lower[ci] == query[qi] {
                // Prefer a word-boundary occurrence within a short window.
                let mut pick = ci;
                if last.is_some_and(|l| l + 1 != ci) || last.is_none() {
                    let window_end = (ci + 12).min(lower.len());
                    if !is_boundary(ci.checked_sub(1).map(|p| chars[p].1), chars[ci].1) {
                        if let Some(b) = (ci..window_end).find(|&j| {
                            lower[j] == query[qi]
                                && is_boundary(j.checked_sub(1).map(|p| chars[p].1), chars[j].1)
                        }) {
                            pick = b;
                        }
                    }
                }
                let prev = pick.checked_sub(1).map(|p| chars[p].1);
                score += 16;
                if is_boundary(prev, chars[pick].1) {
                    score += 24;
                }
                if let Some(l) = last {
                    if l + 1 == pick {
                        score += 20;
                    } else {
                        score -= (pick - l - 1).min(24) as i64;
                    }
                } else {
                    score -= pick.min(20) as i64;
                }
                if chars[pick].1 == query[qi] {
                    score += 1; // exact case
                }
                positions.push(chars[pick].0);
                last = Some(pick);
                qi += 1;
                ci = pick + 1;
            } else {
                ci += 1;
            }
        }
        if qi == query.len() {
            // Shorter candidates and matches in the file name part rank higher.
            score -= (lower.len() / 8) as i64;
            if let Some(slash) = candidate.rfind('/') {
                if positions.first().is_some_and(|&p| p > slash) {
                    score += 12;
                }
            }
            if best.as_ref().is_none_or(|b| score > b.score) {
                best = Some(FuzzyMatch { score, positions });
            }
        }
    }
    best
}

/// Matches and sorts candidates, best first. Returns `(index, match)`.
pub fn match_and_sort<'a>(
    query: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Vec<(usize, FuzzyMatch)> {
    let mut out: Vec<(usize, FuzzyMatch)> = candidates
        .into_iter()
        .enumerate()
        .filter_map(|(ix, c)| fuzzy_match(query, c).map(|m| (ix, m)))
        .collect();
    out.sort_by(|a, b| b.1.score.cmp(&a.1.score).then(a.0.cmp(&b.0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequence_required() {
        assert!(fuzzy_match("abc", "a_b_c").is_some());
        assert!(fuzzy_match("abd", "abc").is_none());
        assert_eq!(fuzzy_match("", "x").unwrap().positions, Vec::<usize>::new());
    }

    #[test]
    fn positions_are_byte_offsets() {
        let m = fuzzy_match("ts", "toggle sidebar").unwrap();
        assert_eq!(m.positions, vec![0, 7]);
        let m = fuzzy_match("é", "café").unwrap();
        assert_eq!(m.positions, vec![3]);
    }

    #[test]
    fn ranking_prefers_word_starts_and_contiguity() {
        let results = match_and_sort("tsb", ["tasbar", "Toggle Side Bar", "testsubject"]);
        assert_eq!(results[0].0, 1);
        let results = match_and_sort("main", ["src/domain.rs", "src/main.rs"]);
        assert_eq!(results[0].0, 1);
    }
}

/// One ranked match of a [`NucleoList`].
#[derive(Clone, Debug)]
pub struct NucleoMatch<T> {
    /// The item.
    pub item: T,
    /// Matched byte positions in the item's haystack.
    pub positions: Vec<usize>,
}

/// A fuzzy-ranked list backed by `lector_search::Picker` (nucleo): items can
/// be pushed from any thread; matching runs on nucleo's worker threads.
pub struct NucleoList<T: Clone + Send + Sync + 'static> {
    picker: lector_search::Picker<(T, std::sync::Arc<str>)>,
    query: String,
}

impl<T: Clone + Send + Sync + 'static> NucleoList<T> {
    /// Empty list. `paths` tunes scoring for file paths.
    pub fn new(paths: bool) -> Self {
        let notify: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(|| {});
        let picker = if paths {
            lector_search::Picker::new_for_paths(notify)
        } else {
            lector_search::Picker::new(notify)
        };
        NucleoList {
            picker,
            query: String::new(),
        }
    }

    /// A sendable handle that adds items (`push(item, haystack)`).
    pub fn injector(&self) -> NucleoInjector<T> {
        NucleoInjector(self.picker.injector())
    }

    /// Adds `item` matched against `haystack`.
    pub fn push(&self, item: T, haystack: &str) {
        self.picker
            .injector()
            .push((item, std::sync::Arc::from(haystack)), haystack);
    }

    /// Sets the query (nucleo syntax).
    pub fn set_query(&mut self, query: &str) {
        if query != self.query {
            self.picker.set_query(query);
            self.query = query.to_string();
        }
    }

    /// Current query.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Lets matching progress for up to `ms`; true while still running.
    pub fn tick(&mut self, ms: u64) -> bool {
        self.picker.tick(ms)
    }

    /// Ticks until matching settles (at most ~`budget_ms`). For small lists.
    pub fn settle(&mut self, budget_ms: u64) {
        let start = std::time::Instant::now();
        while self.picker.tick(5) && start.elapsed().as_millis() < budget_ms as u128 {}
    }

    /// Number of matches / items.
    pub fn counts(&self) -> (u32, u32) {
        (self.picker.matched_count(), self.picker.total_count())
    }

    /// The best `limit` matches with byte positions.
    pub fn top(&self, limit: u32) -> Vec<NucleoMatch<T>> {
        self.picker
            .matches(0..limit)
            .into_iter()
            .map(|m| {
                let (item, hay) = m.item;
                NucleoMatch {
                    item: item.clone(),
                    positions: char_to_byte_positions(hay, &m.indices),
                }
            })
            .collect()
    }
}

/// Sendable injector for a [`NucleoList`].
pub struct NucleoInjector<T: Clone + Send + Sync + 'static>(
    lector_search::PickerInjector<(T, std::sync::Arc<str>)>,
);

impl<T: Clone + Send + Sync + 'static> Clone for NucleoInjector<T> {
    fn clone(&self) -> Self {
        NucleoInjector(self.0.clone())
    }
}

impl<T: Clone + Send + Sync + 'static> NucleoInjector<T> {
    /// Adds `item` matched against `haystack`.
    pub fn push(&self, item: T, haystack: &str) {
        self.0
            .push((item, std::sync::Arc::from(haystack)), haystack);
    }
}

/// Converts sorted char indices in `text` to byte offsets.
pub fn char_to_byte_positions(text: &str, chars: &[u32]) -> Vec<usize> {
    let mut out = Vec::with_capacity(chars.len());
    let mut wanted = chars.iter().peekable();
    for (ci, (bi, _)) in text.char_indices().enumerate() {
        match wanted.peek() {
            Some(&&w) if w as usize == ci => {
                out.push(bi);
                wanted.next();
            }
            Some(_) => {}
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod nucleo_tests {
    use super::*;

    #[test]
    fn ranks_and_positions() {
        let mut list = NucleoList::<usize>::new(true);
        for (i, p) in ["src/main.rs", "crates/x/src/lib.rs", "README.md"]
            .iter()
            .enumerate()
        {
            list.push(i, p);
        }
        list.set_query("main");
        list.settle(500);
        let top = list.top(10);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].item, 0);
        assert_eq!(top[0].positions, vec![4, 5, 6, 7]);
        assert_eq!(char_to_byte_positions("aé b", &[0, 1, 3]), vec![0, 1, 4]);
    }
}
