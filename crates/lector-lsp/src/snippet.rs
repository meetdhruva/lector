//! LSP snippet syntax parsed into plain text plus tab stops (data only).
//!
//! Supported: `$1`, `${1}`, `${1:placeholder}` (nested), `${1|a,b,c|}`,
//! `$0`, variables (`$NAME`, `${NAME}`, `${NAME:default}`) and the escapes
//! `\$`, `\}` and `\\`. Variables are never resolved: they insert their default
//! text, or nothing. `${NAME/regex/format/options}` transforms insert nothing.
//! Malformed syntax is kept as literal text; parsing never fails.

use std::ops::Range;

/// One tab stop of a snippet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabStop {
    /// Tab-stop number; `0` is the final cursor position.
    pub index: u32,
    /// Byte ranges in [`Snippet::text`]: the placeholder text, or an empty
    /// range. Several ranges mean mirrored occurrences of the same stop.
    pub ranges: Vec<Range<usize>>,
    /// Choices of a `${1|a,b|}` stop (the text holds the first one).
    pub choices: Vec<String>,
}

/// A parsed snippet.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snippet {
    /// The text to insert with placeholders replaced by their default text.
    pub text: String,
    /// Tab stops in visiting order: ascending by index, `0` last (present
    /// only when the snippet has a `$0`).
    pub tab_stops: Vec<TabStop>,
}

impl Snippet {
    /// Parses `source`.
    pub fn parse(source: &str) -> Snippet {
        let mut p = Parser {
            chars: source.chars().collect(),
            pos: 0,
            out: String::new(),
            stops: Vec::new(),
        };
        p.parse_until(&[]);
        let mut stops = p.stops;
        stops.sort_by_key(|s| if s.index == 0 { u32::MAX } else { s.index });
        Snippet {
            text: p.out,
            tab_stops: stops,
        }
    }
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    out: String,
    stops: Vec<TabStop>,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    /// A bare `$N` / `${N}`: mirrors the text of an earlier definition of `N`.
    fn add_bare_stop(&mut self, index: u32) {
        let copy = self
            .stops
            .iter()
            .find(|s| s.index == index)
            .and_then(|s| s.ranges.first())
            .and_then(|r| self.out.get(r.clone()))
            .map(str::to_string)
            .unwrap_or_default();
        let start = self.out.len();
        self.out.push_str(&copy);
        let end = self.out.len();
        self.add_stop(index, start..end, vec![]);
    }

    fn add_stop(&mut self, index: u32, range: Range<usize>, choices: Vec<String>) {
        match self.stops.iter_mut().find(|s| s.index == index) {
            Some(s) => {
                s.ranges.push(range);
                if s.choices.is_empty() {
                    s.choices = choices;
                }
            }
            None => self.stops.push(TabStop {
                index,
                ranges: vec![range],
                choices,
            }),
        }
    }

    /// Appends text to `out` until one of `stops` (unescaped) or the end.
    fn parse_until(&mut self, stops: &[char]) {
        while let Some(c) = self.peek() {
            if stops.contains(&c) {
                return;
            }
            match c {
                '\\' => {
                    match self.chars.get(self.pos + 1) {
                        Some(&n) if matches!(n, '$' | '}' | '\\') => {
                            self.out.push(n);
                            self.pos += 2;
                        }
                        _ => {
                            self.out.push('\\');
                            self.pos += 1;
                        }
                    };
                }
                '$' => {
                    let saved = (self.pos, self.out.len(), self.stops.clone());
                    self.pos += 1;
                    if !self.parse_dollar() {
                        self.pos = saved.0;
                        self.out.truncate(saved.1);
                        self.stops = saved.2;
                        self.out.push('$');
                        self.pos += 1;
                    }
                }
                _ => {
                    self.out.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    fn read_digits(&mut self) -> Option<u32> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        if start == self.pos {
            return None;
        }
        self.chars[start..self.pos]
            .iter()
            .collect::<String>()
            .parse()
            .ok()
    }

    fn read_name(&mut self) -> Option<String> {
        let start = self.pos;
        if !self
            .peek()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            return None;
        }
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            self.pos += 1;
        }
        Some(self.chars[start..self.pos].iter().collect())
    }

    /// After a `$`. Returns false (caller rewinds) on malformed input.
    fn parse_dollar(&mut self) -> bool {
        match self.peek() {
            Some(c) if c.is_ascii_digit() => {
                let Some(index) = self.read_digits() else {
                    return false;
                };
                self.add_bare_stop(index);
                true
            }
            Some('{') => {
                self.pos += 1;
                self.parse_braced()
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                self.read_name();
                true
            }
            _ => false,
        }
    }

    /// After `${`.
    fn parse_braced(&mut self) -> bool {
        if self.peek().is_some_and(|c| c.is_ascii_digit()) {
            let Some(index) = self.read_digits() else {
                return false;
            };
            match self.peek() {
                Some('}') => {
                    self.pos += 1;
                    self.add_bare_stop(index);
                    true
                }
                Some(':') => {
                    self.pos += 1;
                    let start = self.out.len();
                    self.parse_until(&['}']);
                    if self.peek() != Some('}') {
                        return false;
                    }
                    self.pos += 1;
                    let end = self.out.len();
                    self.add_stop(index, start..end, vec![]);
                    true
                }
                Some('|') => {
                    self.pos += 1;
                    let Some(choices) = self.parse_choices() else {
                        return false;
                    };
                    let start = self.out.len();
                    if let Some(first) = choices.first() {
                        self.out.push_str(first);
                    }
                    let end = self.out.len();
                    self.add_stop(index, start..end, choices);
                    true
                }
                _ => false,
            }
        } else if self.read_name().is_some() {
            match self.peek() {
                Some('}') => {
                    self.pos += 1;
                    true
                }
                Some(':') => {
                    self.pos += 1;
                    self.parse_until(&['}']);
                    if self.peek() != Some('}') {
                        return false;
                    }
                    self.pos += 1;
                    true
                }
                Some('/') => {
                    // Transform: skip to the closing brace.
                    while let Some(c) = self.peek() {
                        match c {
                            '\\' => self.pos += 2,
                            '}' => {
                                self.pos += 1;
                                return true;
                            }
                            _ => self.pos += 1,
                        }
                    }
                    false
                }
                _ => false,
            }
        } else {
            false
        }
    }

    /// After `${1|`: reads `a,b,c|}`.
    fn parse_choices(&mut self) -> Option<Vec<String>> {
        let mut choices = Vec::new();
        let mut cur = String::new();
        loop {
            match self.peek()? {
                '\\' => match self.chars.get(self.pos + 1) {
                    Some(&n) if matches!(n, '$' | '}' | '\\' | ',' | '|') => {
                        cur.push(n);
                        self.pos += 2;
                    }
                    _ => {
                        cur.push('\\');
                        self.pos += 1;
                    }
                },
                ',' => {
                    choices.push(std::mem::take(&mut cur));
                    self.pos += 1;
                }
                '|' => {
                    self.pos += 1;
                    if self.peek() == Some('}') {
                        self.pos += 1;
                        choices.push(cur);
                        return Some(choices);
                    }
                    cur.push('|');
                }
                c => {
                    cur.push(c);
                    self.pos += 1;
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)] // one-range tab stops are the common case
mod tests {
    use super::*;

    fn stops(s: &Snippet) -> Vec<(u32, Vec<Range<usize>>)> {
        s.tab_stops
            .iter()
            .map(|t| (t.index, t.ranges.clone()))
            .collect()
    }

    #[test]
    fn plain_text_is_unchanged() {
        let s = Snippet::parse("hello world");
        assert_eq!(s.text, "hello world");
        assert!(s.tab_stops.is_empty());
    }

    #[test]
    fn tab_stops_and_placeholders() {
        let s = Snippet::parse("fn ${1:name}($2) { $0 }");
        assert_eq!(s.text, "fn name() {  }");
        assert_eq!(
            stops(&s),
            vec![(1, vec![3..7]), (2, vec![8..8]), (0, vec![12..12])]
        );
    }

    #[test]
    fn mirrors_nesting_and_order() {
        let s = Snippet::parse("${1:a ${2:b}} $1 $2 $0");
        assert_eq!(s.text, "a b a b b ");
        assert_eq!(
            stops(&s),
            vec![
                (1, vec![0..3, 4..7]),
                (2, vec![2..3, 8..9]),
                (0, vec![10..10])
            ]
        );
        let s = Snippet::parse("$3$1$2");
        assert_eq!(
            s.tab_stops.iter().map(|t| t.index).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn choices_and_escapes() {
        let s = Snippet::parse("${1|one,two\\,x,three|} \\$1 \\} \\\\ \\n");
        assert_eq!(s.text, "one $1 } \\ \\n");
        assert_eq!(s.tab_stops[0].choices, vec!["one", "two,x", "three"]);
        assert_eq!(s.tab_stops[0].ranges, vec![0..3]);
    }

    #[test]
    fn variables_use_defaults() {
        let s = Snippet::parse("$TM_FILENAME ${TM_SELECTED_TEXT:sel} ${name} ${v/a/b/g}x");
        assert_eq!(s.text, " sel  x");
        assert!(s.tab_stops.is_empty());
    }

    #[test]
    fn malformed_stays_literal() {
        for src in ["cost: $", "$ 5", "${1:unclosed", "${", "${1|a,b}", "a$-b"] {
            let s = Snippet::parse(src);
            assert!(s.text.contains('$'), "{src} -> {:?}", s.text);
        }
        let s = Snippet::parse("${1:unclosed");
        assert_eq!(s.text, "${1:unclosed");
        assert!(s.tab_stops.is_empty());
    }

    #[test]
    fn unicode_offsets_are_bytes() {
        let s = Snippet::parse("é${1:ü}$0");
        assert_eq!(s.text, "éü");
        assert_eq!(stops(&s), vec![(1, vec![2..4]), (0, vec![4..4])]);
    }
}
