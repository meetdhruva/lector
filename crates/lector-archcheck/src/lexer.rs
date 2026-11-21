//! A small Rust lexer pass (deliberately not a parser).
//!
//! [`scan`] turns source text into tokens with comments removed (line comments, nested block
//! comments) and string, byte-string, raw-string and char literals kept as single opaque tokens,
//! so a rule that looks for `thread::spawn` can never be fooled by a comment or a string that
//! mentions it. It also marks which tokens sit inside test-only regions:
//!
//! * the construct after `#[cfg(test)]` (also `cfg(all(test, ...))`; `cfg_attr(test, ...)` and
//!   `cfg(any(test, ...))` are not test-only), wherever an attribute may stand: item, field,
//!   enum variant, statement, block, match arm, parameter, generic parameter, expression
//!   element, impl/trait/extern member, macro invocation. The region covers the attribute and
//!   the construct it decorates and ends at the FIRST of, at the attribute's own nesting depth: the
//!   close of its body block (inclusive), a `;` (inclusive), a `,` (inclusive) or the closing
//!   delimiter of the enclosing group (exclusive). Commas inside `(..)`, `[..]` and the generic
//!   arguments of a type never end it,
//! * a function carrying `#[test]`, `#[gpui::test]`, `#[tokio::test]`, `#[bench]` and the like
//!   (same end rule),
//! * a whole file that has `#![cfg(test)]` before its first item; inside an inline module the
//!   inner attribute marks only that module's braces,
//! * `#[cfg(test)] mod name;` declarations, reported in [`Scanned::test_mod_decls`] so the
//!   caller can mark the file they point at as test-only (not when `#[path]` is present).
//!
//! # Fail-closed invariant
//!
//! A wrongly "test-only" token silently disables every guardrail for it, while a wrongly
//! "production" token only produces a violation a reviewer can read. So whenever this module is
//! unsure where a test-only region ends (unbalanced or mis-nested brackets, end of input before
//! a terminator, an inner attribute in an unexpected place, a `#[path]` redirect), the code in
//! question is classified as PRODUCTION: the region is dropped, never extended. Where the end
//! rule is ambiguous (a comma that may or may not sit in generic arguments, a `>` that may be a
//! comparison) it errs towards ending early. The same holds for [`crate::source`], which only
//! trusts Cargo's `tests/`, `benches/` and `examples/` target directories by path.
//!
//! Limits: macro bodies are tokens like any other (no expansion); `#[path = "..."]` is not
//! followed; `{` inside const-generic arguments of a signature ends an item early; `#[derive]`
//! style attributes that precede the `#[cfg(test)]` one stay production.

/// What a [`Token`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokKind {
    /// Identifier or keyword (raw identifiers keep their `r#` prefix).
    Ident,
    /// A lifetime such as `'a` or `'static`.
    Lifetime,
    /// A string, byte string, C string or raw string literal, quotes and prefix included.
    Str,
    /// A char or byte literal, quotes included.
    Char,
    /// A numeric literal.
    Num,
    /// Punctuation: one character, except the merged `::`, `->` and `=>`.
    Punct,
}

/// One token with its byte span and 1-based line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    /// Token class.
    pub kind: TokKind,
    /// Start byte offset in the source.
    pub start: usize,
    /// End byte offset (exclusive).
    pub end: usize,
    /// 1-based line of the first byte.
    pub line: u32,
}

/// A comment (line or block) with the lines it covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Comment {
    /// First line (1-based).
    pub start_line: u32,
    /// Last line (1-based).
    pub end_line: u32,
    /// Start byte offset.
    pub start: usize,
    /// End byte offset (exclusive).
    pub end: usize,
}

/// The result of [`scan`].
#[derive(Debug, Clone, Default)]
pub struct Scanned {
    /// Tokens in source order, comments excluded.
    pub tokens: Vec<Token>,
    /// `in_test[i]` is true when `tokens[i]` is inside a test-only item.
    pub in_test: Vec<bool>,
    /// Comments in source order.
    pub comments: Vec<Comment>,
    /// The file starts with `#![cfg(test)]`.
    pub whole_file_test: bool,
    /// Names of `#[cfg(test)] mod name;` declarations (out-of-line test modules).
    pub test_mod_decls: Vec<String>,
}

impl Scanned {
    /// Text of token `i`, or `""` when out of range.
    pub fn text<'a>(&self, src: &'a str, i: usize) -> &'a str {
        match self.tokens.get(i) {
            Some(t) => src.get(t.start..t.end).unwrap_or(""),
            None => "",
        }
    }
}

/// Lex `src` and classify test-only regions.
pub fn scan(src: &str) -> Scanned {
    let (tokens, comments) = lex(src);
    let matching = match_brackets(src, &tokens);
    let mut c = Classifier {
        src,
        toks: &tokens,
        matching,
        in_test: vec![false; tokens.len()],
        test_mods: Vec::new(),
        whole: false,
    };
    c.run();
    let Classifier {
        in_test,
        test_mods,
        whole,
        ..
    } = c;
    Scanned {
        tokens,
        in_test,
        comments,
        whole_file_test: whole,
        test_mod_decls: test_mods,
    }
}

fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

fn is_ident_start_byte(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

/// Index just past the closing quote of the string whose opening quote is at `i`.
fn scan_string(b: &[u8], i: usize, line: &mut u32) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => {
                if b.get(j + 1) == Some(&b'\n') {
                    *line += 1;
                }
                j += 2;
            }
            b'"' => return j + 1,
            b'\n' => {
                *line += 1;
                j += 1;
            }
            _ => j += 1,
        }
    }
    b.len()
}

/// Raw string starting at the first `#` or `"` after the `r` prefix; `None` when it is not one.
fn scan_raw(b: &[u8], i: usize, line: &mut u32) -> Option<usize> {
    let mut hashes = 0;
    while b.get(i + hashes) == Some(&b'#') {
        hashes += 1;
    }
    if b.get(i + hashes) != Some(&b'"') {
        return None;
    }
    let mut j = i + hashes + 1;
    while j < b.len() {
        match b[j] {
            b'\n' => {
                *line += 1;
                j += 1;
            }
            b'"' => {
                let closing = (0..hashes).all(|k| b.get(j + 1 + k) == Some(&b'#'));
                if closing {
                    return Some(j + 1 + hashes);
                }
                j += 1;
            }
            _ => j += 1,
        }
    }
    Some(b.len())
}

/// Char literal whose opening quote is at `i` and which starts with a backslash escape.
fn scan_escaped_char(src: &str, i: usize) -> usize {
    let b = src.as_bytes();
    // Skip the quote, the backslash and the escaped character, then find the closing quote.
    let mut j = i + 2;
    if let Some(ch) = src.get(j..).and_then(|s| s.chars().next()) {
        j += ch.len_utf8();
    }
    while j < b.len() && b[j] != b'\'' && b[j] != b'\n' {
        j += 1;
    }
    if b.get(j) == Some(&b'\'') {
        j + 1
    } else {
        j
    }
}

fn lex(src: &str) -> (Vec<Token>, Vec<Comment>) {
    let b = src.as_bytes();
    let n = b.len();
    let mut i = 0;
    let mut line = 1u32;
    let mut toks: Vec<Token> = Vec::new();
    let mut comments: Vec<Comment> = Vec::new();
    while i < n {
        let c = b[i];
        let start = i;
        let start_line = line;
        match c {
            b'\n' => {
                line += 1;
                i += 1;
            }
            b' ' | b'\t' | b'\r' | 0x0b | 0x0c => i += 1,
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < n && b[i] != b'\n' {
                    i += 1;
                }
                comments.push(Comment {
                    start_line,
                    end_line: start_line,
                    start,
                    end: i,
                });
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 0usize;
                while i < n {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        if b[i] == b'\n' {
                            line += 1;
                        }
                        i += 1;
                    }
                }
                comments.push(Comment {
                    start_line,
                    end_line: line,
                    start,
                    end: i.min(n),
                });
            }
            b'"' => {
                i = scan_string(b, i, &mut line);
                push(&mut toks, TokKind::Str, start, i, start_line);
            }
            b'\'' => {
                let next = src.get(i + 1..).and_then(|s| s.chars().next());
                match next {
                    Some('\\') => {
                        i = scan_escaped_char(src, i);
                        push(&mut toks, TokKind::Char, start, i, start_line);
                    }
                    Some(ch) => {
                        let after = i + 1 + ch.len_utf8();
                        if b.get(after) == Some(&b'\'') {
                            i = after + 1;
                            push(&mut toks, TokKind::Char, start, i, start_line);
                        } else if ch == '_' || ch.is_alphabetic() {
                            i += 1 + ch.len_utf8();
                            while i < n && is_ident_byte(b[i]) {
                                i += 1;
                            }
                            push(&mut toks, TokKind::Lifetime, start, i, start_line);
                        } else {
                            i += 1;
                            push(&mut toks, TokKind::Punct, start, i, start_line);
                        }
                    }
                    None => {
                        i += 1;
                        push(&mut toks, TokKind::Punct, start, i, start_line);
                    }
                }
            }
            b'0'..=b'9' => {
                let hex = c == b'0' && matches!(b.get(i + 1), Some(b'x') | Some(b'X'));
                i += 1;
                while i < n {
                    let ch = b[i];
                    let next_is_digit = b.get(i + 1).is_some_and(|d| d.is_ascii_digit());
                    let part_of_number = ch.is_ascii_alphanumeric()
                        || ch == b'_'
                        || (ch == b'.' && next_is_digit)
                        || ((ch == b'+' || ch == b'-')
                            && !hex
                            && matches!(b[i - 1], b'e' | b'E')
                            && next_is_digit);
                    if !part_of_number {
                        break;
                    }
                    i += 1;
                }
                push(&mut toks, TokKind::Num, start, i, start_line);
            }
            _ if is_ident_start_byte(c) => {
                if let Some((kind, end)) = prefixed_literal(src, i, &mut line) {
                    i = end;
                    push(&mut toks, kind, start, i, start_line);
                    continue;
                }
                // Raw identifier `r#name`.
                if c == b'r'
                    && b.get(i + 1) == Some(&b'#')
                    && b.get(i + 2).is_some_and(|d| is_ident_start_byte(*d))
                {
                    i += 2;
                }
                while i < n && is_ident_byte(b[i]) {
                    i += 1;
                }
                push(&mut toks, TokKind::Ident, start, i, start_line);
            }
            _ => {
                let two = match (c, b.get(i + 1)) {
                    (b':', Some(b':')) | (b'-', Some(b'>')) | (b'=', Some(b'>')) => 2,
                    _ => 1,
                };
                i += two;
                push(&mut toks, TokKind::Punct, start, i, start_line);
            }
        }
    }
    (toks, comments)
}

fn push(toks: &mut Vec<Token>, kind: TokKind, start: usize, end: usize, line: u32) {
    toks.push(Token {
        kind,
        start,
        end,
        line,
    });
}

/// `b"…"`, `b'x'`, `r"…"`, `r#"…"#`, `br#"…"#`, `c"…"`, `cr"…"` starting at `i`.
fn prefixed_literal(src: &str, i: usize, line: &mut u32) -> Option<(TokKind, usize)> {
    let b = src.as_bytes();
    let (plen, raw) = match (b[i], b.get(i + 1)) {
        (b'b', Some(b'r')) | (b'c', Some(b'r')) => (2, true),
        (b'r', _) => (1, true),
        (b'b', _) | (b'c', _) => (1, false),
        _ => return None,
    };
    let j = i + plen;
    if raw {
        return scan_raw(b, j, line).map(|end| (TokKind::Str, end));
    }
    match b.get(j) {
        Some(b'"') => Some((TokKind::Str, scan_string(b, j, line))),
        Some(b'\'') if b[i] == b'b' => {
            // Byte literal: always a char literal, never a lifetime.
            let end = if b.get(j + 1) == Some(&b'\\') {
                scan_escaped_char(src, j)
            } else {
                let w = src
                    .get(j + 1..)
                    .and_then(|s| s.chars().next())
                    .map_or(1, char::len_utf8);
                if b.get(j + 1 + w) == Some(&b'\'') {
                    j + 2 + w
                } else {
                    return None;
                }
            };
            Some((TokKind::Char, end))
        }
        _ => None,
    }
}

/// Keywords that start an item whose header may contain depth-0 commas (generics, `where`
/// clauses), so a comma never ends such an item.
const COMMA_TOLERANT_ITEMS: [&str; 8] = [
    "fn",
    "struct",
    "enum",
    "union",
    "trait",
    "impl",
    "mod",
    "macro_rules",
];

/// Keywords whose items are one statement ending at `;` and may contain braces and commas
/// (`use a::{b, c};`, `let x: Map<A, B> = Foo { .. };`).
const STATEMENT_ITEMS: [&str; 5] = ["use", "type", "static", "let", "const"];

/// Tokens that may appear inside the generic arguments of a type.
fn is_type_token(kind: TokKind, text: &str) -> bool {
    match kind {
        TokKind::Ident | TokKind::Lifetime | TokKind::Num | TokKind::Str | TokKind::Char => true,
        TokKind::Punct => matches!(
            text,
            "::" | "," | "<" | ">" | "&" | "*" | "+" | "?" | "!" | "->" | "=" | "-" | ":"
        ),
    }
}

struct Classifier<'a> {
    src: &'a str,
    toks: &'a [Token],
    /// `matching[i]` is the index of the closer of the `(`, `[` or `{` at `i`, only when the
    /// whole bracket structure around it is balanced and properly nested.
    matching: Vec<Option<usize>>,
    in_test: Vec<bool>,
    test_mods: Vec<String>,
    whole: bool,
}

#[derive(PartialEq, Eq)]
enum Attr {
    CfgTest,
    TestFn,
    Other,
}

/// How the extent of an attributed construct is decided.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Shape {
    /// `fn`, `struct`, `impl`, `mod`...: ends at its body block or `;`; commas are ignored.
    Item,
    /// `use`, `let`, `const`, `type`, `static`: ends at `;`; braces and commas are ignored.
    Statement,
    /// Field, variant, parameter, argument, match arm, expression statement, macro call:
    /// ends at a body block, `;` or `,`.
    Element,
}

/// Pair every `(`, `[` and `{` with its closer. An opener whose structure is unbalanced or
/// mis-nested gets `None`, so nothing derived from it can reach past an unknown boundary.
fn match_brackets(src: &str, toks: &[Token]) -> Vec<Option<usize>> {
    let mut out = vec![None; toks.len()];
    let mut stack: Vec<(usize, u8)> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        if t.kind != TokKind::Punct {
            continue;
        }
        match src.as_bytes().get(t.start).copied() {
            Some(b'(') if t.end - t.start == 1 => stack.push((i, b')')),
            Some(b'[') if t.end - t.start == 1 => stack.push((i, b']')),
            Some(b'{') if t.end - t.start == 1 => stack.push((i, b'}')),
            Some(c @ (b')' | b']' | b'}')) if t.end - t.start == 1 => match stack.pop() {
                Some((open, want)) if want == c => out[open] = Some(i),
                // Mis-nested: forget every open bracket, their extents are unknown.
                _ => stack.clear(),
            },
            _ => {}
        }
    }
    out
}

impl Classifier<'_> {
    fn t(&self, i: usize) -> &str {
        match self.toks.get(i) {
            Some(t) => self.src.get(t.start..t.end).unwrap_or(""),
            None => "",
        }
    }

    /// Index of the bracket closing the one at `open`; `None` when the structure is unbalanced.
    fn matching(&self, open: usize) -> Option<usize> {
        self.matching.get(open).copied().flatten()
    }

    fn mark(&mut self, from: usize, to: usize) {
        let to = to.min(self.in_test.len());
        if from < to {
            for flag in &mut self.in_test[from..to] {
                *flag = true;
            }
        }
    }

    fn run(&mut self) {
        let n = self.toks.len();
        let mut i = 0;
        // The `{` tokens that are still open, each with "only inner attributes seen so far":
        // an inner attribute counts only before the first item of its scope, and applies to
        // the innermost open brace (or to the whole file when there is none).
        let mut open_braces: Vec<(usize, bool)> = Vec::new();
        let mut file_leading = true;
        while i < n {
            if self.t(i) != "#" {
                let is_open = self.t(i) == "{";
                if self.t(i) == "}" {
                    open_braces.pop();
                }
                // Any other token is the start of an item: inner attributes are over.
                match open_braces.last_mut() {
                    Some(scope) => scope.1 = false,
                    None => file_leading = false,
                }
                if is_open {
                    open_braces.push((i, true));
                }
                i += 1;
                continue;
            }
            let inner = self.t(i + 1) == "!";
            let open = if inner { i + 2 } else { i + 1 };
            let close = match (self.t(open) == "[", self.matching(open)) {
                (true, Some(close)) => close,
                _ => {
                    i += 1;
                    continue;
                }
            };
            let verdict = self.attr(open + 1, close);
            if inner {
                if verdict == Attr::CfgTest {
                    match open_braces.last() {
                        None if file_leading => self.whole = true,
                        Some(&(brace, true)) => {
                            if let Some(end) = self.matching(brace) {
                                self.mark(brace, end + 1);
                            }
                        }
                        // Not before the first item of its scope: unexpected, so production.
                        _ => {}
                    }
                }
                i = close + 1;
                continue;
            }
            // An outer attribute is the start of an item (or field, statement...) in its scope.
            match open_braces.last_mut() {
                Some(scope) => scope.1 = false,
                None => file_leading = false,
            }
            if verdict == Attr::Other {
                i = close + 1;
                continue;
            }
            let at_root = open_braces.is_empty();
            match self.region_end(close + 1, verdict == Attr::CfgTest && at_root) {
                Some(end) => {
                    self.mark(i, end);
                    i = end.max(close + 1);
                }
                // Unknown extent: fail closed, everything after the attribute is production.
                None => i = close + 1,
            }
        }
    }

    /// Classify the attribute whose inside is `toks[from..to]`.
    fn attr(&self, from: usize, to: usize) -> Attr {
        let mut k = from;
        let mut last = "";
        let first = self.t(k);
        while k < to {
            if self.toks[k].kind != TokKind::Ident {
                break;
            }
            last = self.t(k);
            k += 1;
            if self.t(k) == "::" {
                k += 1;
            } else {
                break;
            }
        }
        if first == "cfg" && self.t(k) == "(" {
            let mut p = k + 1;
            return if self.eval_cfg(&mut p, to) {
                Attr::CfgTest
            } else {
                Attr::Other
            };
        }
        if last == "test" || last == "bench" {
            Attr::TestFn
        } else {
            Attr::Other
        }
    }

    /// True when the cfg predicate at `*p` is satisfied only in test builds.
    fn eval_cfg(&self, p: &mut usize, end: usize) -> bool {
        let name = self.t(*p).to_string();
        *p += 1;
        match name.as_str() {
            "test" => true,
            "all" | "any" | "not" if self.t(*p) == "(" => {
                *p += 1;
                let mut vals = Vec::new();
                while *p < end && self.t(*p) != ")" {
                    vals.push(self.eval_cfg(p, end));
                    if self.t(*p) == "," {
                        *p += 1;
                    }
                }
                *p += 1;
                match name.as_str() {
                    "all" => vals.iter().any(|v| *v),
                    "any" => !vals.is_empty() && vals.iter().all(|v| *v),
                    _ => false,
                }
            }
            _ => {
                if self.t(*p) == "=" {
                    *p += 2;
                }
                false
            }
        }
    }

    /// The one rule for where an attributed region ends. `start` is the first token after the
    /// attribute; the result is the exclusive end of the region, or `None` when the extent is
    /// unknown (the caller then marks nothing: fail closed).
    ///
    /// The region is the construct the attribute decorates, and it ends at the FIRST of, at the
    /// attribute's own nesting depth: the close of its body brace block (inclusive), a `;`
    /// (inclusive), a `,` (inclusive) or the closing delimiter of the enclosing group
    /// (exclusive). Commas inside `(..)`, `[..]` and generic arguments `<..>` never count, and
    /// stacked attributes are skipped. `record_mod` is true for a `#[cfg(test)]` item at file
    /// level, whose `mod x;` declaration then names a test-only file.
    fn region_end(&mut self, start: usize, record_mod: bool) -> Option<usize> {
        let n = self.toks.len();
        let mut k = start;
        let mut has_path_attr = false;
        // Stacked attributes belong to the same construct.
        while self.t(k) == "#" {
            let open = if self.t(k + 1) == "!" { k + 2 } else { k + 1 };
            if self.t(open) != "[" {
                break;
            }
            let close = self.matching(open)?;
            has_path_attr |= self.t(open + 1) == "path";
            k = close + 1;
        }
        let mut m = k;
        loop {
            match self.t(m) {
                "pub" => {
                    m += 1;
                    if self.t(m) == "(" {
                        m = self.matching(m)? + 1;
                    }
                }
                "async" | "unsafe" | "default" => m += 1,
                "extern" => {
                    m += 1;
                    if self.toks.get(m).is_some_and(|t| t.kind == TokKind::Str) {
                        m += 1;
                    }
                }
                "const" if matches!(self.t(m + 1), "fn" | "unsafe" | "async" | "extern") => m += 1,
                _ => break,
            }
        }
        let kw = self.t(m).to_string();
        if kw == "mod" && self.t(m + 2) == ";" {
            // `#[path = "..."]` redirects the file: we cannot name it, so do not record it.
            if record_mod && !has_path_attr {
                let name = self.t(m + 1).to_string();
                self.test_mods.push(name);
            }
            return Some(m + 3);
        }
        let shape = if STATEMENT_ITEMS.contains(&kw.as_str()) {
            Shape::Statement
        } else if COMMA_TOLERANT_ITEMS.contains(&kw.as_str()) {
            Shape::Item
        } else {
            Shape::Element
        };
        let mut p = k;
        while p < n {
            match self.t(p) {
                "(" | "[" => p = self.matching(p)? + 1,
                "{" => {
                    let close = self.matching(p)?;
                    if shape == Shape::Statement {
                        p = close + 1;
                        continue;
                    }
                    let next = self.t(close + 1);
                    // A pattern or `if` expression continues after its block.
                    if shape == Shape::Element && matches!(next, "=>" | "else") {
                        p = close + 1;
                        continue;
                    }
                    let mut end = close + 1;
                    if shape == Shape::Element && next == "," {
                        end += 1;
                    }
                    return Some(end);
                }
                ";" => return Some(p + 1),
                "," if shape == Shape::Element => return Some(p + 1),
                // A closer of the enclosing group; a lone `>` closes generic parameters
                // (`<A, #[cfg(test)] B>`) or is a comparison, where ending is still safe.
                ")" | "]" | "}" => return Some(p),
                ">" if shape == Shape::Element => return Some(p),
                "<" if shape == Shape::Element => p = self.generic_args_end(p).unwrap_or(p + 1),
                _ => p += 1,
            }
        }
        // End of input without a terminator: unknown extent.
        None
    }

    /// If the `<` at `open` starts generic arguments of a type (`Map<K, V>`), the index just
    /// past the matching `>`. Anything doubtful (a comparison, a stray `<`) gives `None`, which
    /// makes the caller treat the `<` as an ordinary token: commas after it then end the region
    /// early, which only ever classifies more code as production.
    fn generic_args_end(&self, open: usize) -> Option<usize> {
        let prev = self.toks.get(open.checked_sub(1)?)?;
        if prev.kind != TokKind::Ident && self.t(open - 1) != "::" {
            return None;
        }
        let mut depth = 0usize;
        let mut k = open;
        while k < self.toks.len() {
            match self.t(k) {
                "<" => depth += 1,
                ">" => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        break;
                    }
                }
                "(" | "[" => {
                    k = self.matching(k)?;
                }
                _ => {
                    let tok = self.toks.get(k)?;
                    if !is_type_token(tok.kind, self.t(k)) {
                        return None;
                    }
                }
            }
            k += 1;
        }
        if k >= self.toks.len() {
            return None;
        }
        // What follows a type: a separator, a closer, a bound, a path, or a `where` clause.
        let follows_a_type = matches!(
            self.t(k + 1),
            "," | ";" | "=" | ")" | "]" | "}" | ">" | "+" | "::" | "where"
        );
        follows_a_type.then_some(k + 1)
    }
}

#[cfg(test)]
mod tests;
