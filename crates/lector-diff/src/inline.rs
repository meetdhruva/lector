//! Token- and character-level refinement of a changed line pair.

use std::ops::Range;

use crate::diff_tokens;

/// Byte ranges (within each line) that differ.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InlineChanges {
    /// Changed ranges in the old line.
    pub old: Vec<Range<usize>>,
    /// Changed ranges in the new line.
    pub new: Vec<Range<usize>>,
}

type Ranges = Vec<Range<usize>>;

/// Lines longer than this (in tokens) are not refined.
const MAX_TOKENS: usize = 4000;
/// Below this share of common non-whitespace bytes the lines are "too dissimilar".
const MIN_LINE_SIMILARITY: f32 = 0.35;
/// Below this share of common chars a replaced word is highlighted whole.
const MIN_WORD_SIMILARITY: f32 = 0.5;
/// Longest word (bytes) that receives character-level refinement.
const MAX_WORD_BYTES: usize = 64;

#[derive(PartialEq, Eq, Clone, Copy)]
enum Class {
    Word,
    Space,
    Punct,
}

fn class(c: char) -> Class {
    if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Punct
    }
}

/// Tokens: identifier/word runs, whitespace runs, single punctuation chars.
fn tokenize(s: &str) -> Vec<(Range<usize>, Class)> {
    let mut out: Vec<(Range<usize>, Class)> = Vec::new();
    for (i, c) in s.char_indices() {
        let cl = class(c);
        let end = i + c.len_utf8();
        match out.last_mut() {
            Some((r, last)) if cl != Class::Punct && *last == cl => r.end = end,
            _ => out.push((i..end, cl)),
        }
    }
    out
}

fn whole(old: &str, new: &str) -> InlineChanges {
    InlineChanges {
        old: (!old.is_empty())
            .then_some(0..old.len())
            .into_iter()
            .collect(),
        new: (!new.is_empty())
            .then_some(0..new.len())
            .into_iter()
            .collect(),
    }
}

fn push_merged(v: &mut Vec<Range<usize>>, r: Range<usize>) {
    if r.is_empty() {
        return;
    }
    if let Some(last) = v.last_mut() {
        if r.start <= last.end {
            last.end = last.end.max(r.end);
            return;
        }
    }
    v.push(r);
}

/// Word-level diff of two lines, with character-level refinement inside replaced words.
///
/// Returns whole-line ranges when the lines are too dissimilar (or too long) for a
/// fine-grained result to be readable.
pub fn refine_inline(old: &str, new: &str) -> InlineChanges {
    if old == new {
        return InlineChanges::default();
    }
    if old.is_empty() || new.is_empty() {
        return whole(old, new);
    }
    let (ta, tb) = (tokenize(old), tokenize(new));
    if ta.len() > MAX_TOKENS || tb.len() > MAX_TOKENS {
        return whole(old, new);
    }
    let sa: Vec<&str> = ta.iter().map(|(r, _)| &old[r.clone()]).collect();
    let sb: Vec<&str> = tb.iter().map(|(r, _)| &new[r.clone()]).collect();
    let hunks = diff_tokens(&sa, &sb);

    // Similarity over non-whitespace bytes.
    let nonws = |t: &[(Range<usize>, Class)]| -> usize {
        t.iter()
            .filter(|(_, c)| *c != Class::Space)
            .map(|(r, _)| r.len())
            .sum()
    };
    let (na, nb) = (nonws(&ta), nonws(&tb));
    let changed = |t: &[(Range<usize>, Class)], rs: Vec<Range<u32>>| -> usize {
        rs.into_iter()
            .flat_map(|r| r.start as usize..r.end as usize)
            .filter(|&i| t[i].1 != Class::Space)
            .map(|i| t[i].0.len())
            .sum()
    };
    let ca = changed(&ta, hunks.iter().map(|h| h.old.clone()).collect());
    let cb = changed(&tb, hunks.iter().map(|h| h.new.clone()).collect());
    let common = (na - ca).min(nb - cb);
    if na + nb > 0 && (2 * common) as f32 / ((na + nb) as f32) < MIN_LINE_SIMILARITY {
        return whole(old, new);
    }

    let mut out = InlineChanges::default();
    for h in &hunks {
        let (a0, a1) = (h.old.start as usize, h.old.end as usize);
        let (b0, b1) = (h.new.start as usize, h.new.end as usize);
        let a_range = (a0 < a1).then(|| ta[a0].0.start..ta[a1 - 1].0.end);
        let b_range = (b0 < b1).then(|| tb[b0].0.start..tb[b1 - 1].0.end);
        if let (Some(ar), Some(br)) = (&a_range, &b_range) {
            let single_words = a1 - a0 == 1
                && b1 - b0 == 1
                && ta[a0].1 == Class::Word
                && tb[b0].1 == Class::Word
                && ar.len() <= MAX_WORD_BYTES
                && br.len() <= MAX_WORD_BYTES;
            if single_words {
                if let Some((ro, rn)) = refine_chars(&old[ar.clone()], &new[br.clone()]) {
                    for r in ro {
                        push_merged(&mut out.old, ar.start + r.start..ar.start + r.end);
                    }
                    for r in rn {
                        push_merged(&mut out.new, br.start + r.start..br.start + r.end);
                    }
                    continue;
                }
            }
        }
        if let Some(r) = a_range {
            push_merged(&mut out.old, r);
        }
        if let Some(r) = b_range {
            push_merged(&mut out.new, r);
        }
    }
    out
}

/// Character diff of two short words; `None` when they share too little.
fn refine_chars(a: &str, b: &str) -> Option<(Ranges, Ranges)> {
    let ca: Vec<(usize, char)> = a.char_indices().collect();
    let cb: Vec<(usize, char)> = b.char_indices().collect();
    let chars_a: Vec<char> = ca.iter().map(|&(_, c)| c).collect();
    let chars_b: Vec<char> = cb.iter().map(|&(_, c)| c).collect();
    let hunks = diff_tokens(&chars_a, &chars_b);
    let changed_a: usize = hunks
        .iter()
        .map(|h| (h.old.end - h.old.start) as usize)
        .sum();
    let changed_b: usize = hunks
        .iter()
        .map(|h| (h.new.end - h.new.start) as usize)
        .sum();
    let common = (chars_a.len() - changed_a).min(chars_b.len() - changed_b);
    if (2 * common) as f32 / ((chars_a.len() + chars_b.len()) as f32) < MIN_WORD_SIMILARITY {
        return None;
    }
    let byte_range = |s: &str, cs: &[(usize, char)], r: Range<u32>| -> Range<usize> {
        let start = cs.get(r.start as usize).map_or(s.len(), |&(i, _)| i);
        let end = cs.get(r.end as usize).map_or(s.len(), |&(i, _)| i);
        start..end
    };
    let mut ro = Vec::new();
    let mut rn = Vec::new();
    for h in hunks {
        push_merged(&mut ro, byte_range(a, &ca, h.old));
        push_merged(&mut rn, byte_range(b, &cb, h.new));
    }
    Some((ro, rn))
}
