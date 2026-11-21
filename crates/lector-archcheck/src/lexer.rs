//! A small Rust lexer pass (deliberately not a parser).
//!
//! [`scan`] turns source text into tokens with comments removed (line comments, nested block
//! comments) and string, byte-string, raw-string and char literals kept as single opaque tokens,
//! so a rule that looks for `thread::spawn` can never be fooled by a comment or a string that
//! mentions it. It also marks which tokens sit inside test-only items:
//!
//! * an item following `#[cfg(test)]` (also `cfg(all(test, ...))`), by brace matching,
//! * a function carrying `#[test]`, `#[gpui::test]`, `#[tokio::test]`, `#[bench]` and the like,
//! * a whole file that starts with `#![cfg(test)]`,
//! * `#[cfg(test)] mod name;` declarations, reported in [`Scanned::test_mod_decls`] so the
//!   caller can mark the file they point at as test-only.
//!
//! Limits: macro bodies are tokens like any other (no expansion); `#[path = "..."]` is not
//! followed; `{` inside const-generic arguments of a signature ends an item early.

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
    let mut c = Classifier {
        src,
        toks: &tokens,
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

struct Classifier<'a> {
    src: &'a str,
    toks: &'a [Token],
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

impl Classifier<'_> {
    fn t(&self, i: usize) -> &str {
        match self.toks.get(i) {
            Some(t) => self.src.get(t.start..t.end).unwrap_or(""),
            None => "",
        }
    }

    /// Index of the bracket closing the one at `open` (the last token when unbalanced).
    fn close_of(&self, open: usize) -> usize {
        let (o, c) = match self.t(open) {
            "(" => ("(", ")"),
            "[" => ("[", "]"),
            _ => ("{", "}"),
        };
        let mut depth = 0usize;
        let mut k = open;
        while k < self.toks.len() {
            let s = self.t(k);
            if s == o {
                depth += 1;
            } else if s == c {
                depth -= 1;
                if depth == 0 {
                    return k;
                }
            }
            k += 1;
        }
        self.toks.len().saturating_sub(1)
    }

    fn run(&mut self) {
        let n = self.toks.len();
        let mut i = 0;
        while i < n {
            if self.t(i) != "#" {
                i += 1;
                continue;
            }
            let inner = self.t(i + 1) == "!";
            let open = if inner { i + 2 } else { i + 1 };
            if self.t(open) != "[" {
                i += 1;
                continue;
            }
            let close = self.close_of(open);
            let verdict = self.attr(open + 1, close);
            if verdict == Attr::Other || inner {
                if inner && verdict == Attr::CfgTest {
                    self.whole = true;
                }
                i = close + 1;
                continue;
            }
            let end = self.item_end(close + 1, verdict == Attr::CfgTest);
            for flag in &mut self.in_test[i..=end.min(n - 1)] {
                *flag = true;
            }
            i = end + 1;
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

    /// Index of the last token of the item that starts at `start` (attributes may precede it).
    fn item_end(&mut self, start: usize, cfg_test: bool) -> usize {
        let n = self.toks.len();
        let mut k = start;
        while self.t(k) == "#" {
            let open = if self.t(k + 1) == "!" { k + 2 } else { k + 1 };
            if self.t(open) != "[" {
                break;
            }
            k = self.close_of(open) + 1;
        }
        let mut m = k;
        loop {
            match self.t(m) {
                "pub" => {
                    m += 1;
                    if self.t(m) == "(" {
                        m = self.close_of(m) + 1;
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
            if cfg_test {
                let name = self.t(m + 1).to_string();
                self.test_mods.push(name);
            }
            return m + 2;
        }
        let statement_like = matches!(kw.as_str(), "use" | "type" | "static" | "let" | "const");
        let mut p = k;
        while p < n {
            match self.t(p) {
                "(" | "[" => p = self.close_of(p) + 1,
                "{" => {
                    let close = self.close_of(p);
                    if !statement_like {
                        return close;
                    }
                    p = close + 1;
                }
                ";" => return p,
                _ => p += 1,
            }
        }
        n.saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<(TokKind, String)> {
        let s = scan(src);
        s.tokens
            .iter()
            .map(|t| (t.kind, src[t.start..t.end].to_string()))
            .collect()
    }

    fn texts(src: &str) -> Vec<String> {
        kinds(src).into_iter().map(|(_, t)| t).collect()
    }

    /// Texts of the idents that are outside test regions.
    fn prod_idents(src: &str) -> Vec<String> {
        let s = scan(src);
        (0..s.tokens.len())
            .filter(|i| !s.in_test[*i] && s.tokens[*i].kind == TokKind::Ident)
            .map(|i| s.text(src, i).to_string())
            .collect()
    }

    #[test]
    fn line_comments_are_dropped() {
        let t = texts("a // thread::spawn(\nb /// doc thread::spawn\nc //! x\n");
        assert_eq!(t, ["a", "b", "c"]);
    }

    #[test]
    fn nested_block_comments_are_dropped() {
        let t = texts("x /* a /* b */ still comment thread::spawn */ y /**/ z");
        assert_eq!(t, ["x", "y", "z"]);
        let s = scan("/* /* */ */ end");
        assert_eq!(s.comments.len(), 1);
        assert_eq!(s.tokens.len(), 1);
    }

    #[test]
    fn block_comment_counts_lines() {
        let s = scan("a\n/* one\ntwo\nthree */\nb");
        assert_eq!(s.tokens[1].line, 5);
        assert_eq!((s.comments[0].start_line, s.comments[0].end_line), (2, 4));
    }

    #[test]
    fn slashes_inside_strings_are_not_comments() {
        let k = kinds(r#"let u = "http://x /* y */"; z"#);
        assert!(k
            .iter()
            .any(|(kind, t)| *kind == TokKind::Str && t.contains("http://x")));
        assert_eq!(k.last().map(|(_, t)| t.as_str()), Some("z"));
    }

    #[test]
    fn escaped_quote_does_not_end_string() {
        let k = kinds(r#"f("a\"b // c") g"#);
        assert_eq!(k.len(), 5, "{k:?}");
        assert_eq!(k[2].0, TokKind::Str);
        assert_eq!(k[4].1, "g");
    }

    #[test]
    fn raw_strings_with_comment_markers_and_quotes() {
        let src = "let a = r#\"x // y \" z\"#; let b = r##\"p \"# q\"##; end";
        let k = kinds(src);
        let strs: Vec<&String> = k
            .iter()
            .filter(|(kind, _)| *kind == TokKind::Str)
            .map(|(_, t)| t)
            .collect();
        assert_eq!(strs.len(), 2, "{k:?}");
        assert_eq!(strs[0], "r#\"x // y \" z\"#");
        assert_eq!(strs[1], "r##\"p \"# q\"##");
        assert_eq!(k.last().map(|(_, t)| t.as_str()), Some("end"));
    }

    #[test]
    fn byte_and_c_strings() {
        let k = kinds(r##"b"a\"b" br#"x"y"# c"z" cr"w" b'a' b'\'' tail"##);
        let got: Vec<TokKind> = k.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(
            got,
            [
                TokKind::Str,
                TokKind::Str,
                TokKind::Str,
                TokKind::Str,
                TokKind::Char,
                TokKind::Char,
                TokKind::Ident
            ],
            "{k:?}"
        );
    }

    #[test]
    fn multi_line_raw_string_keeps_line_numbers() {
        let s = scan("a r\"l1\nl2\nl3\" b");
        assert_eq!(s.tokens[2].line, 3);
    }

    #[test]
    fn lifetimes_versus_char_literals() {
        let k = kinds("fn f<'a, 'static_>(x: &'a str) -> char { 'x' }");
        let lifetimes: Vec<&str> = k
            .iter()
            .filter(|(kind, _)| *kind == TokKind::Lifetime)
            .map(|(_, t)| t.as_str())
            .collect();
        assert_eq!(lifetimes, ["'a", "'static_", "'a"]);
        assert!(k.contains(&(TokKind::Char, "'x'".to_string())));
    }

    #[test]
    fn tricky_char_literals() {
        let k = kinds(r#"'"' '\'' '\\' '\u{1F600}' '_' 'é' x"#);
        let chars: Vec<&str> = k
            .iter()
            .filter(|(kind, _)| *kind == TokKind::Char)
            .map(|(_, t)| t.as_str())
            .collect();
        assert_eq!(
            chars,
            [r#"'"'"#, r"'\''", r"'\\'", r"'\u{1F600}'", "'_'", "'é'"]
        );
        assert_eq!(k.last().map(|(_, t)| t.as_str()), Some("x"));
    }

    #[test]
    fn quote_char_literal_does_not_open_a_string() {
        let t = prod_idents("let q = '\"'; thread::spawn(f);");
        assert!(t.contains(&"spawn".to_string()));
    }

    #[test]
    fn raw_identifiers_and_numbers() {
        assert_eq!(texts("r#type"), ["r#type"]);
        assert_eq!(texts("1..2"), ["1", ".", ".", "2"]);
        assert_eq!(texts("1.5e-3 + x"), ["1.5e-3", "+", "x"]);
        assert_eq!(texts("0xE+1"), ["0xE", "+", "1"]);
        assert_eq!(texts("v.0.len()"), ["v", ".", "0", ".", "len", "(", ")"]);
    }

    #[test]
    fn merged_punctuation() {
        assert_eq!(
            texts("a::b -> c => d"),
            ["a", "::", "b", "->", "c", "=>", "d"]
        );
    }

    #[test]
    fn cfg_test_mod_is_a_region_and_strings_do_not_confuse_braces() {
        let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() { let s = \"}\"; let c = '}'; /* } */ }\n    fn thread_spawn() {}\n}\nfn after() {}\n";
        let idents = prod_idents(src);
        assert!(idents.contains(&"a".to_string()));
        assert!(idents.contains(&"after".to_string()));
        assert!(!idents.contains(&"thread_spawn".to_string()));
        assert!(!idents.contains(&"tests".to_string()));
    }

    #[test]
    fn test_fn_attributes() {
        let src = "#[test]\nfn one() {}\n#[gpui::test]\nasync fn two() {}\n#[tokio::test(flavor = \"x\")]\nfn three() {}\n#[should_panic]\n#[test]\nfn four() {}\nfn real() {}";
        let idents = prod_idents(src);
        for hidden in ["one", "two", "three", "four"] {
            assert!(!idents.contains(&hidden.to_string()), "{hidden}");
        }
        assert!(idents.contains(&"real".to_string()));
    }

    #[test]
    fn cfg_predicates() {
        for (attr, is_test) in [
            ("#[cfg(test)]", true),
            ("#[cfg(all(test, feature = \"x\"))]", true),
            ("#[cfg(any(test, unix))]", false),
            ("#[cfg(any(test, all(test, unix)))]", true),
            ("#[cfg(not(test))]", false),
            ("#[cfg(feature = \"test\")]", false),
            ("#[cfg_attr(test, derive(Debug))]", false),
        ] {
            let src = format!("{attr}\nfn marker() {{}}\n");
            let in_test = !prod_idents(&src).contains(&"marker".to_string());
            assert_eq!(in_test, is_test, "{attr}");
        }
    }

    #[test]
    fn stacked_attributes_before_the_item() {
        let src = "#[cfg(test)]\n#[allow(dead_code)]\n#[derive(Debug)]\nstruct Hidden { a: u8 }\nstruct Shown;";
        assert_eq!(prod_idents(src), ["struct", "Shown"]);
    }

    #[test]
    fn out_of_line_test_module_is_reported() {
        let s =
            scan("#[cfg(test)]\nmod tests;\nmod real;\n#[cfg(test)]\npub(crate) mod support;\n");
        assert_eq!(s.test_mod_decls, ["tests", "support"]);
        let idents = prod_idents("#[cfg(test)]\nmod tests;\nmod real;\n");
        assert_eq!(idents, ["mod", "real"]);
    }

    #[test]
    fn whole_file_test_marker() {
        assert!(scan("#![cfg(test)]\nfn x() {}").whole_file_test);
        assert!(!scan("#![forbid(unsafe_code)]\nfn x() {}").whole_file_test);
    }

    #[test]
    fn statement_like_items_end_at_the_semicolon() {
        let src = "#[cfg(test)]\nuse foo::{a, b};\n#[cfg(test)]\nconst T: [u8; 3] = [1, 2, 3];\n#[cfg(test)]\nstatic S: Foo = Foo { x: 1 };\nfn live() {}";
        assert_eq!(prod_idents(src), ["fn", "live"]);
    }

    #[test]
    fn test_items_inside_impl_blocks() {
        let src = "impl X {\n fn keep(&self) {}\n #[cfg(test)]\n fn hide(&self) { let _ = 1; }\n fn keep2(&self) {}\n}";
        let idents = prod_idents(src);
        assert!(idents.contains(&"keep".to_string()));
        assert!(idents.contains(&"keep2".to_string()));
        assert!(!idents.contains(&"hide".to_string()));
    }

    #[test]
    fn where_clauses_and_generics_in_test_items() {
        let src =
            "#[cfg(test)]\nfn f<T>(x: [T; 2]) -> Result<(), E> where T: Fn() { body() }\nfn g() {}";
        assert_eq!(prod_idents(src), ["fn", "g"]);
    }

    #[test]
    fn unterminated_input_never_panics_and_spans_stay_in_bounds() {
        let sample = "#[cfg(test)] mod t { fn f() { let s = r#\"a\"#; 'a' 'b b\"c /* /* \\ } ";
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut inputs: Vec<String> = (0..=sample.len())
            .filter(|i| sample.is_char_boundary(*i))
            .map(|i| sample[..i].to_string())
            .collect();
        for _ in 0..300 {
            let mut bytes = sample.as_bytes().to_vec();
            for _ in 0..4 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let pos = (seed as usize) % bytes.len();
                let alphabet = b"\"'#r/*{}()[]\\ \n;#!bc";
                bytes[pos] = alphabet[(seed >> 20) as usize % alphabet.len()];
            }
            inputs.push(String::from_utf8_lossy(&bytes).into_owned());
        }
        for src in inputs {
            let s = scan(&src);
            let mut prev = 0;
            for t in &s.tokens {
                assert!(
                    t.start >= prev && t.start < t.end && t.end <= src.len(),
                    "{src:?}"
                );
                prev = t.end;
            }
            assert_eq!(s.in_test.len(), s.tokens.len());
        }
    }
}
