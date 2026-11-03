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
