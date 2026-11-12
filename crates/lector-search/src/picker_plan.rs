//! Query analysis for the picker's scan loop: which cheap tests are valid for
//! a parsed nucleo [`Pattern`] and how much a haystack can score at most.
//!
//! Everything here is *conservative*: a test may let a non-matching haystack
//! through (the exact nucleo scorer decides), but it never rejects a haystack
//! nucleo would match, and the score bound never undershoots the real score.
//! Property tests compare both against nucleo on random ASCII haystacks.

use memchr::{memchr, memchr2};
use nucleo::pattern::{AtomKind, Pattern};
use nucleo::{Config, Utf32Str};

use crate::picker_store::{char_bit, Mask};

/// nucleo's `SCORE_MATCH` (per matched character).
const SCORE_MATCH: u32 = 16;
/// Largest bonus of a position that is not a word boundary
/// (camel-case / digit transitions, `BONUS_BOUNDARY - PENALTY_GAP_START`).
const WEAK_BONUS: u32 = 5;

/// How much of the cheap machinery a query can use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// No atoms: everything matches, ordered by boost.
    Empty,
    /// Only positive ASCII fuzzy atoms: mask test, own subsequence test,
    /// score bound, exact scoring only for candidates that can enter the top-k.
    Fast,
    /// ASCII needles with other atom kinds or negations: mask test, then
    /// exact scoring of every survivor.
    Masked,
    /// Non-ASCII needle: exact scoring of everything.
    Slow,
}

struct FuzzyAtom {
    needle: Vec<u8>,
    ignore_case: bool,
    bits: Vec<u64>,
}

/// Precomputed, immutable description of a query for the scan loop.
pub(crate) struct Plan {
    pub mode: Mode,
    /// Mask bits every candidate must contain (positive atoms, ASCII needles).
    pub qmask: u64,
    atoms: Vec<FuzzyAtom>,
    /// Largest boundary bonus of the configuration; `None` disables bounds.
    bmax: Option<u32>,
    exact_mask: bool,
}

/// Largest boundary bonus nucleo awards with `config`, if it is one of the
/// configurations the bound was verified for.
fn boundary_bonus(config: &Config) -> Option<u32> {
    if *config == Config::DEFAULT.match_paths() {
        Some(9)
    } else if *config == Config::DEFAULT {
        Some(10)
    } else {
        None
    }
}

impl Plan {
    pub fn new(pattern: &Pattern, config: &Config) -> Plan {
        let bmax = boundary_bonus(config);
        if pattern.atoms.is_empty() {
            return Plan {
                mode: Mode::Empty,
                qmask: 0,
                atoms: Vec::new(),
                bmax,
                exact_mask: false,
            };
        }
        let mut needles = Vec::new();
        for atom in &pattern.atoms {
            match atom.needle_text() {
                Utf32Str::Ascii(b) => needles.push(b),
                Utf32Str::Unicode(_) => {
                    return Plan {
                        mode: Mode::Slow,
                        qmask: 0,
                        atoms: Vec::new(),
                        bmax,
                        exact_mask: false,
                    };
                }
            }
        }
        let mut qmask = 0u64;
        for (atom, needle) in pattern.atoms.iter().zip(&needles) {
            if !atom.negative {
                for &b in *needle {
                    qmask |= char_bit(b);
                }
            }
        }
        let fast = pattern
            .atoms
            .iter()
            .all(|a| !a.negative && a.kind == AtomKind::Fuzzy);
        if !fast {
            return Plan {
                mode: Mode::Masked,
                qmask,
                atoms: Vec::new(),
                bmax,
                exact_mask: false,
            };
        }
        let atoms: Vec<FuzzyAtom> = needles
            .iter()
            .map(|needle| FuzzyAtom {
                ignore_case: !needle.iter().any(u8::is_ascii_uppercase),
                bits: needle.iter().map(|&b| char_bit(b)).collect(),
                needle: needle.to_vec(),
            })
            .collect();
        let exact_mask = atoms.len() == 1
            && atoms[0].needle.len() == 1
            && match atoms[0].needle[0] {
                b'a'..=b'z' => atoms[0].ignore_case,
                b'0'..=b'9' | b'.' | b'_' | b'-' | b'/' | b' ' => true,
                _ => false,
            };
        Plan {
            mode: Mode::Fast,
            qmask,
            atoms,
            bmax,
            exact_mask,
        }
    }

    /// Queries that rank with the score bound first and count in a second
    /// pass (see `Pass` in the picker).
    #[inline]
    pub fn two_phase(&self) -> bool {
        self.mode == Mode::Fast && self.bmax.is_some()
    }

    /// The mask test alone decides membership: a single lowercase-or-digit
    /// (or `. _ - / space`) character, which the mask represents exactly.
    #[inline]
    pub fn mask_is_exact(&self) -> bool {
        self.exact_mask
    }

    /// True when [`Plan::upper_bound`] is available.
    #[cfg(test)]
    pub fn has_bound(&self) -> bool {
        self.bmax.is_some()
    }

    /// Exact membership test for [`Mode::Fast`] queries on an ASCII
    /// haystack: every atom must be a (case-insensitive unless the atom has an
    /// uppercase letter) subsequence.
    #[inline]
    pub fn subsequence(&self, hay: &[u8]) -> bool {
        for atom in &self.atoms {
            let mut rest = hay;
            for &c in &atom.needle {
                let pos = if atom.ignore_case && c.is_ascii_lowercase() {
                    memchr2(c, c - 32, rest)
                } else {
                    memchr(c, rest)
                };
                match pos {
                    Some(i) => rest = &rest[i + 1..],
                    None => return false,
                }
            }
        }
        true
    }

    /// Upper bound of the nucleo score (without boost) of an ASCII haystack
    /// whose boundary mask is `boundary` and that matches a [`Mode::Fast`]
    /// query. Per atom: `16 * n` plus, for each needle char, the largest
    /// bonus its best position could carry (the boundary bonus when the char
    /// occurs at a boundary or follows a char that could carry it, else the
    /// weak bonus), the first char counted twice. Gap penalties only lower
    /// the real score.
    #[inline]
    pub fn upper_bound(&self, boundary: u64) -> u32 {
        let Some(bmax) = self.bmax else {
            return u32::MAX;
        };
        let mut total = 0u32;
        for atom in &self.atoms {
            total += SCORE_MATCH * atom.bits.len() as u32;
            let mut chain = false;
            for (i, &bit) in atom.bits.iter().enumerate() {
                let strong = chain || boundary & bit != 0;
                let b = if strong { bmax } else { WEAK_BONUS };
                total += if i == 0 { 2 * b } else { b };
                chain = strong;
            }
        }
        total
    }

    /// The needle and case mode of a query that is a single plain fuzzy atom
    /// of at least two characters (the queries whose match end positions are
    /// kept to extend them cheaply).
    pub fn single_atom(&self) -> Option<(&[u8], bool)> {
        match (self.mode, self.atoms.as_slice()) {
            (Mode::Fast, [a]) if a.needle.len() >= 2 => Some((&a.needle, a.ignore_case)),
            _ => None,
        }
    }

    /// Where the leftmost subsequence match of the single atom ends
    /// (exclusive), see [`Plan::single_atom`].
    pub fn match_end(&self, hay: &[u8]) -> Option<usize> {
        self.extend_end(hay, 0, 0)
    }

    /// Continues the leftmost match of the single atom at byte `from`,
    /// skipping the first `skip` needle chars (already matched before
    /// `from`). Returns the end (exclusive) of the whole match.
    pub fn extend_end(&self, hay: &[u8], from: usize, skip: usize) -> Option<usize> {
        let atom = self.atoms.first()?;
        let mut pos = from;
        for &c in atom.needle.get(skip..)? {
            let rest = hay.get(pos..)?;
            let i = if atom.ignore_case && c.is_ascii_lowercase() {
                memchr2(c, c - 32, rest)?
            } else {
                memchr(c, rest)?
            };
            pos += i + 1;
        }
        Some(pos)
    }

    /// Mask test.
    #[inline]
    pub fn mask_ok(&self, m: &Mask) -> bool {
        m.any & self.qmask == self.qmask
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker_store::mask_of;
    use nucleo::pattern::{CaseMatching, Normalization};
    use nucleo::{Matcher, Utf32String};
    use proptest::prelude::*;

    fn parse(q: &str) -> Pattern {
        Pattern::parse(q, CaseMatching::Smart, Normalization::Smart)
    }

    #[test]
    fn match_end_extends_incrementally() {
        let cfg = Config::DEFAULT.match_paths();
        let hay = b"src/Module_main.rs";
        let full = Plan::new(&parse("mdle"), &cfg);
        let short = Plan::new(&parse("md"), &cfg);
        let end_short = short.match_end(hay).expect("md matches");
        assert_eq!(full.extend_end(hay, end_short, 2), full.match_end(hay));
        assert_eq!(end_short, 7);
        assert_eq!(full.match_end(hay), Some(10));
        assert!(Plan::new(&parse("zz"), &cfg).match_end(hay).is_none());
        assert!(Plan::new(&parse("a"), &cfg).single_atom().is_none());
        assert!(Plan::new(&parse("ab cd"), &cfg).single_atom().is_none());
    }

    #[test]
    fn modes_follow_query_syntax() {
        let cfg = Config::DEFAULT.match_paths();
        assert_eq!(Plan::new(&parse(""), &cfg).mode, Mode::Empty);
        assert_eq!(Plan::new(&parse("abc de"), &cfg).mode, Mode::Fast);
        assert_eq!(Plan::new(&parse("'abc"), &cfg).mode, Mode::Masked);
        assert_eq!(Plan::new(&parse("abc !de"), &cfg).mode, Mode::Masked);
        assert_eq!(Plan::new(&parse("^abc"), &cfg).mode, Mode::Masked);
        assert_eq!(Plan::new(&parse("é"), &cfg).mode, Mode::Slow);
        let mut odd = cfg.clone();
        odd.prefer_prefix = true;
        assert!(!Plan::new(&parse("ab"), &odd).has_bound());
        assert!(Plan::new(&parse("ab"), &cfg).has_bound());
    }

    fn check(hay: &str, q: &str, config: &Config) -> Result<(), TestCaseError> {
        let pattern = parse(q);
        let plan = Plan::new(&pattern, config);
        let mut matcher = Matcher::new(config.clone());
        let u = Utf32String::from(hay);
        let score = pattern.score(u.slice(..), &mut matcher);
        let mask = mask_of(hay.as_bytes());
        if plan.mode == Mode::Fast && hay.is_ascii() {
            prop_assert_eq!(
                plan.subsequence(hay.as_bytes()),
                score.is_some(),
                "membership of {:?} in {:?}",
                q,
                hay
            );
        }
        if let Some(s) = score {
            prop_assert!(plan.mask_ok(&mask), "mask rejected {q:?} in {hay:?}");
            if plan.mode == Mode::Fast && hay.is_ascii() {
                let ub = plan.upper_bound(mask.boundary);
                prop_assert!(ub >= s, "bound {ub} < score {s} for {q:?} in {hay:?}");
            }
        }
        Ok(())
    }

    fn haystack() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                8 => proptest::sample::select(vec!['a', 'b', 'c', 'm', 'o', 'd', 'e', 'r', 's', 'x']),
                3 => proptest::sample::select(vec!['A', 'B', 'M', 'D', 'R']),
                2 => proptest::sample::select(vec!['1', '2', '9']),
                3 => proptest::sample::select(vec!['/', '_', '-', '.', ' ']),
                1 => proptest::sample::select(vec!['é', '$', '(', ':']),
            ],
            1..40,
        )
        .prop_map(|v| v.into_iter().collect())
    }

    fn query() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            proptest::sample::select(vec![
                'a', 'b', 'c', 'm', 'o', 'd', 'e', 'r', 's', 'x', 'A', 'M', '1', '9', '/', '_',
                '.', ' ',
            ]),
            1..7,
        )
        .prop_map(|v| v.into_iter().collect())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4000))]

        #[test]
        fn bound_and_membership_agree_with_nucleo_paths(h in haystack(), q in query()) {
            check(&h, &q, &Config::DEFAULT.match_paths())?;
        }

        #[test]
        fn bound_and_membership_agree_with_nucleo_default(h in haystack(), q in query()) {
            check(&h, &q, &Config::DEFAULT)?;
        }
    }
}
