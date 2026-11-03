//! The data side of the code view: [`CodeSource`] and an in-memory
//! implementation with a tiny lexical highlighter for demos and tests.

use std::ops::Range;

use gpui::SharedString;
use lector_theme::{Color, TextStyle};

/// Style of a highlighted span.
#[derive(Clone, Debug, PartialEq)]
pub enum SpanStyle {
    /// A canonical capture name resolved through the active syntax theme
    /// (e.g. `"keyword"`, `"function.method"`).
    Capture(&'static str),
    /// An explicit style.
    Custom(TextStyle),
}

/// A highlighted byte range within one row.
#[derive(Clone, Debug, PartialEq)]
pub struct HighlightSpan {
    /// Row (0-based).
    pub row: u32,
    /// Byte range within the row's text (without the newline).
    pub range: Range<usize>,
    /// Style.
    pub style: SpanStyle,
}

/// Kind of a diff gutter mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GutterMarkKind {
    /// Lines added.
    Added,
    /// Lines modified.
    Modified,
    /// Lines deleted above/at this row (drawn as a wedge).
    Deleted,
}

/// A diff gutter mark over rows (in this source's coordinates).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GutterMark {
    /// Rows covered; for `Deleted`, an empty range at the row below the deletion.
    pub rows: Range<u32>,
    /// Kind.
    pub kind: GutterMarkKind,
}

/// Theme-aware colors for row decorations supplied by a source.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DecorationColor {
    /// Diff: added line background.
    AddedLine,
    /// Diff: removed line background.
    RemovedLine,
    /// Diff: changed words in an added line.
    AddedWord,
    /// Diff: changed words in a removed line.
    RemovedWord,
    /// Hatched filler row (empty side of a split diff).
    Filler,
    /// Hunk header background.
    HunkHeader,
    /// Search match.
    SearchMatch,
    /// Word / reference highlight.
    WordHighlight,
    /// Matching bracket.
    BracketMatch,
    /// Explicit color.
    Custom(Color),
}

/// Per-row decorations supplied by a source (e.g. a diff).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct RowDecoration {
    /// Row.
    pub row: u32,
    /// Full-width background.
    pub background: Option<DecorationColor>,
    /// Byte-range backgrounds within the row.
    pub ranges: Vec<(Range<usize>, DecorationColor)>,
}

/// Read-only text plus presentation data for a [`super::CodeView`].
///
/// All rows are 0-based. Methods taking a row range are only called for
/// (roughly) visible rows, so implementations can compute lazily.
pub trait CodeSource: 'static {
    /// Number of rows (a trailing newline yields a final empty row).
    fn line_count(&self) -> u32;

    /// Text of `row` without its line terminator.
    fn line(&self, row: u32) -> SharedString;

    /// Changes whenever the content changes (cache key).
    fn version(&self) -> u64 {
        0
    }

    /// Longest row length in bytes, if known (horizontal scroll extent).
    fn max_line_len(&self) -> Option<usize> {
        None
    }

    /// Syntax highlight spans for `rows`, sorted by row then start.
    fn highlights(&self, _rows: Range<u32>) -> Vec<HighlightSpan> {
        Vec::new()
    }

    /// Diff gutter marks intersecting `rows`.
    fn gutter_marks(&self, _rows: Range<u32>) -> Vec<GutterMark> {
        Vec::new()
    }

    /// Foldable regions; folding `r` hides rows `r.start + 1 .. r.end`.
    fn fold_ranges(&self) -> Vec<Range<u32>> {
        Vec::new()
    }

    /// Header rows of the scopes enclosing `top_row`, outermost first
    /// (pinned at the top while scrolling).
    fn sticky_rows(&self, _top_row: u32) -> Vec<u32> {
        Vec::new()
    }

    /// Line number displayed for `row` (0-based; the UI adds 1). `None`
    /// leaves the gutter blank (e.g. filler rows in a diff).
    fn line_number(&self, row: u32) -> Option<u32> {
        Some(row)
    }

    /// Custom gutter label for `row` replacing the line number (e.g. the
    /// "old new" number pair of a unified diff). Only used when
    /// [`CodeSource::line_number_chars`] is `Some`.
    fn line_number_label(&self, _row: u32) -> Option<SharedString> {
        None
    }

    /// Width of the line-number column in characters when labels come from
    /// [`CodeSource::line_number_label`]; `None` = numbers from `line_number`.
    fn line_number_chars(&self) -> Option<usize> {
        None
    }

    /// Row backgrounds and range highlights for `rows` (diffs).
    fn row_decorations(&self, _rows: Range<u32>) -> Vec<RowDecoration> {
        Vec::new()
    }

    /// Language display name for the status bar.
    fn language_name(&self) -> Option<SharedString> {
        None
    }

    /// The full text as an immutable snapshot, when the source has one
    /// (enables background search over the document).
    fn text_snapshot(&self) -> Option<lector_text::Text> {
        None
    }
}

/// An in-memory [`CodeSource`] over a string, with optional lexical
/// highlighting and indentation-based folds / sticky headers.
pub struct InMemorySource {
    lines: Vec<SharedString>,
    version: u64,
    max_len: usize,
    highlighter: Option<LexicalHighlighter>,
    language: Option<SharedString>,
    gutter_marks: Vec<GutterMark>,
}

impl InMemorySource {
    /// Plain text without highlighting.
    pub fn new(text: &str) -> Self {
        static NEXT_VERSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mut lines: Vec<SharedString> = text
            .split('\n')
            .map(|l| SharedString::from(l.strip_suffix('\r').unwrap_or(l).to_string()))
            .collect();
        if lines.is_empty() {
            lines.push(SharedString::default());
        }
        let max_len = lines.iter().map(|l| l.len()).max().unwrap_or(0);
        InMemorySource {
            lines,
            version: NEXT_VERSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            max_len,
            highlighter: None,
            language: None,
            gutter_marks: Vec::new(),
        }
    }

    /// Text with a lexical highlighter chosen from the file name.
    pub fn for_file(text: &str, file_name: &str) -> Self {
        let mut source = Self::new(text);
        if let Some((language, highlighter)) = LexicalHighlighter::for_file_name(file_name) {
            source.language = Some(language.into());
            source.highlighter = Some(highlighter);
        } else {
            source.language = Some("Plain Text".into());
        }
        source
    }

    /// Sets demo gutter marks.
    pub fn with_gutter_marks(mut self, marks: Vec<GutterMark>) -> Self {
        self.gutter_marks = marks;
        self
    }

    fn indent_of(&self, row: u32) -> Option<usize> {
        let line = self.lines.get(row as usize)?;
        if line.trim().is_empty() {
            return None;
        }
        Some(indent_width(line, 4))
    }
}

/// Display width of leading whitespace (tabs to `tab_size` stops).
pub fn indent_width(line: &str, tab_size: usize) -> usize {
    let mut width = 0;
    for c in line.chars() {
        match c {
            ' ' => width += 1,
            '\t' => width += tab_size - width % tab_size,
            _ => break,
        }
    }
    width
}

impl CodeSource for InMemorySource {
    fn line_count(&self) -> u32 {
        self.lines.len() as u32
    }

    fn line(&self, row: u32) -> SharedString {
        self.lines.get(row as usize).cloned().unwrap_or_default()
    }

    fn version(&self) -> u64 {
        self.version
    }

    fn max_line_len(&self) -> Option<usize> {
        Some(self.max_len)
    }

    fn highlights(&self, rows: Range<u32>) -> Vec<HighlightSpan> {
        let Some(h) = &self.highlighter else {
            return Vec::new();
        };
        // Block comments may start above the range; scan a bounded prefix.
        let scan_start = rows.start.saturating_sub(200);
        let mut in_block = false;
        let mut out = Vec::new();
        for row in scan_start..rows.end.min(self.line_count()) {
            let line = &self.lines[row as usize];
            let spans = h.highlight_line(line, &mut in_block);
            if row >= rows.start {
                out.extend(spans.into_iter().map(|(range, capture)| HighlightSpan {
                    row,
                    range,
                    style: SpanStyle::Capture(capture),
                }));
            }
        }
        out
    }

    fn gutter_marks(&self, rows: Range<u32>) -> Vec<GutterMark> {
        self.gutter_marks
            .iter()
            .filter(|m| m.rows.start <= rows.end && m.rows.end >= rows.start)
            .cloned()
            .collect()
    }

    fn fold_ranges(&self) -> Vec<Range<u32>> {
        // Indentation-based: a non-blank row followed by more-indented rows.
        let n = self.line_count();
        let mut out = Vec::new();
        let mut stack: Vec<(u32, usize)> = Vec::new();
        let mut last_nonblank = 0;
        for row in 0..n {
            let Some(indent) = self.indent_of(row) else {
                continue;
            };
            while let Some(&(start, start_indent)) = stack.last() {
                if indent <= start_indent {
                    stack.pop();
                    // Keep a closing line (same indent) visible.
                    let end = if indent == start_indent && is_closer(&self.lines[row as usize]) {
                        row
                    } else {
                        last_nonblank + 1
                    };
                    if end > start + 1 {
                        out.push(start..end);
                    }
                } else {
                    break;
                }
            }
            stack.push((row, indent));
            last_nonblank = row;
        }
        while let Some((start, _)) = stack.pop() {
            if last_nonblank + 1 > start + 1 {
                out.push(start..last_nonblank + 1);
            }
        }
        out.retain(|r| r.end - r.start > 1);
        out.sort_by_key(|r| r.start);
        out
    }

    fn sticky_rows(&self, top_row: u32) -> Vec<u32> {
        // Enclosing headers: walk upwards collecting rows with strictly
        // smaller indentation that open a block.
        let mut out = Vec::new();
        let Some(mut current) =
            (top_row..self.line_count().min(top_row + 50)).find_map(|r| self.indent_of(r))
        else {
            return out;
        };
        let limit = top_row.saturating_sub(3000);
        let mut row = top_row;
        while row > limit && current > 0 {
            row -= 1;
            if let Some(indent) = self.indent_of(row) {
                if indent < current {
                    let line = self.lines[row as usize].trim_end();
                    if line.ends_with('{')
                        || line.ends_with(':')
                        || line.ends_with('(')
                        || line.ends_with('[')
                    {
                        out.push(row);
                    }
                    current = indent;
                }
            }
        }
        out.reverse();
        out
    }

    fn language_name(&self) -> Option<SharedString> {
        self.language.clone()
    }
}

fn is_closer(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('}')
        || t.starts_with(']')
        || t.starts_with(')')
        || t == "end"
        || t.starts_with("</")
}

/// A small keyword/string/comment highlighter used until tree-sitter
/// highlighting is integrated. Good enough for demos and screenshots.
pub struct LexicalHighlighter {
    keywords: &'static [&'static str],
    types: &'static [&'static str],
    constants: &'static [&'static str],
    line_comment: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    capitalized_types: bool,
}

const RUST_KW: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "static", "struct", "super", "trait", "type", "unsafe", "use", "where", "while",
];
const RUST_TYPES: &[&str] = &[
    "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize", "f32",
    "f64", "bool", "char", "str", "Self",
];
const C_LIKE_KW: &[&str] = &[
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "from",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "of",
    "return",
    "static",
    "switch",
    "throw",
    "try",
    "type",
    "typeof",
    "var",
    "void",
    "while",
    "async",
    "await",
    "yield",
    "public",
    "private",
    "protected",
    "implements",
    "package",
    "func",
    "go",
    "defer",
    "struct",
    "map",
    "chan",
    "range",
    "select",
    "namespace",
    "using",
    "template",
    "typename",
    "virtual",
    "override",
    "sizeof",
    "include",
    "define",
];
const PY_KW: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda",
    "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with", "yield",
];
const SH_KW: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "do", "done", "while", "case", "esac", "function",
    "in", "return", "export", "local", "set", "unset",
];
const CONSTANTS: &[&str] = &[
    "true",
    "false",
    "null",
    "nil",
    "None",
    "True",
    "False",
    "undefined",
    "self",
    "this",
];
const NO_TYPES: &[&str] = &[];

impl LexicalHighlighter {
    /// Picks a highlighter by file extension; returns the language name.
    pub fn for_file_name(name: &str) -> Option<(&'static str, LexicalHighlighter)> {
        let lower = name.to_ascii_lowercase();
        let ext = lower.rsplit('.').next().unwrap_or("");
        let c_like = |kw| LexicalHighlighter {
            keywords: kw,
            types: NO_TYPES,
            constants: CONSTANTS,
            line_comment: &["//"],
            block_comment: Some(("/*", "*/")),
            capitalized_types: true,
        };
        let hl = match ext {
            "rs" => (
                "Rust",
                LexicalHighlighter {
                    keywords: RUST_KW,
                    types: RUST_TYPES,
                    constants: CONSTANTS,
                    line_comment: &["//"],
                    block_comment: Some(("/*", "*/")),
                    capitalized_types: true,
                },
            ),
            "ts" | "tsx" | "mts" | "cts" => ("TypeScript", c_like(C_LIKE_KW)),
            "js" | "jsx" | "mjs" | "cjs" => ("JavaScript", c_like(C_LIKE_KW)),
            "go" => ("Go", c_like(C_LIKE_KW)),
            "c" | "h" => ("C", c_like(C_LIKE_KW)),
            "cc" | "cpp" | "cxx" | "hpp" | "hh" => ("C++", c_like(C_LIKE_KW)),
            "java" | "kt" | "swift" | "cs" | "scala" | "dart" => ("Source", c_like(C_LIKE_KW)),
            "py" | "pyi" => (
                "Python",
                LexicalHighlighter {
                    keywords: PY_KW,
                    types: NO_TYPES,
                    constants: CONSTANTS,
                    line_comment: &["#"],
                    block_comment: None,
                    capitalized_types: true,
                },
            ),
            "sh" | "bash" | "zsh" | "fish" => (
                "Shell",
                LexicalHighlighter {
                    keywords: SH_KW,
                    types: NO_TYPES,
                    constants: CONSTANTS,
                    line_comment: &["#"],
                    block_comment: None,
                    capitalized_types: false,
                },
            ),
            "toml" | "yaml" | "yml" | "ini" | "cfg" => (
                if ext == "toml" { "TOML" } else { "YAML" },
                LexicalHighlighter {
                    keywords: &[],
                    types: NO_TYPES,
                    constants: CONSTANTS,
                    line_comment: &["#"],
                    block_comment: None,
                    capitalized_types: false,
                },
            ),
            "json" | "jsonc" => (
                "JSON",
                LexicalHighlighter {
                    keywords: &[],
                    types: NO_TYPES,
                    constants: CONSTANTS,
                    line_comment: &["//"],
                    block_comment: None,
                    capitalized_types: false,
                },
            ),
            "md" | "markdown" => return Some(("Markdown", LexicalHighlighter::markdown())),
            _ => return None,
        };
        Some(hl)
    }

    fn markdown() -> LexicalHighlighter {
        LexicalHighlighter {
            keywords: &[],
            types: NO_TYPES,
            constants: &[],
            line_comment: &[],
            block_comment: Some(("<!--", "-->")),
            capitalized_types: false,
        }
    }

    /// Highlights one line; `in_block` carries block-comment state.
    pub fn highlight_line(
        &self,
        line: &str,
        in_block: &mut bool,
    ) -> Vec<(Range<usize>, &'static str)> {
        let mut out = Vec::new();
        let bytes = line.as_bytes();
        let n = bytes.len();
        let mut i = 0;
        let is_markdown = self.keywords.is_empty()
            && self.line_comment.is_empty()
            && self.block_comment.is_some();
        if is_markdown && !*in_block {
            let t = line.trim_start();
            if t.starts_with('#') {
                out.push((0..n, "markup.heading"));
                return out;
            }
            if t.starts_with("- ") || t.starts_with("* ") {
                let start = n - t.len();
                out.push((start..start + 1, "markup.list"));
            }
            if t.starts_with('>') {
                out.push((0..n, "markup.quote"));
                return out;
            }
        }
        while i < n {
            if *in_block {
                let (_, close) = self.block_comment.unwrap_or(("", ""));
                let end = line[i..]
                    .find(close)
                    .map(|p| i + p + close.len())
                    .unwrap_or(n);
                out.push((i..end, "comment"));
                if end < n || line[i..].contains(close) {
                    *in_block = false;
                }
                i = end;
                continue;
            }
            let rest = &line[i..];
            if let Some(lc) = self.line_comment.iter().find(|lc| rest.starts_with(**lc)) {
                let doc = rest.starts_with("///") || rest.starts_with("//!");
                let _ = lc;
                out.push((i..n, if doc { "comment.doc" } else { "comment" }));
                break;
            }
            if let Some((open, _)) = self.block_comment {
                if rest.starts_with(open) {
                    *in_block = true;
                    out.push((i..i + open.len(), "comment"));
                    i += open.len();
                    continue;
                }
            }
            let c = bytes[i];
            if is_markdown {
                if c == b'`' {
                    let end = line[i + 1..].find('`').map(|p| i + 1 + p + 1).unwrap_or(n);
                    out.push((i..end, "markup.raw"));
                    i = end;
                    continue;
                }
                i += rest.chars().next().map_or(1, char::len_utf8);
                continue;
            }
            if c == b'"' || c == b'\'' || c == b'`' {
                // Rust lifetimes / chars: treat 'a (no closing quote nearby) as label.
                if c == b'\'' && !self.keywords.contains(&"def") {
                    let close = line[i + 1..]
                        .char_indices()
                        .take(3)
                        .find(|(_, ch)| *ch == '\'');
                    if close.is_none() {
                        let end = i
                            + 1
                            + line[i + 1..]
                                .find(|ch: char| !ch.is_alphanumeric() && ch != '_')
                                .unwrap_or(n - i - 1);
                        out.push((i..end, "label"));
                        i = end;
                        continue;
                    }
                }
                let mut j = i + 1;
                while j < n {
                    if bytes[j] == b'\\' {
                        out.push((j..(j + 2).min(n), "string.escape"));
                        j += 2;
                        continue;
                    }
                    if bytes[j] == c {
                        j += 1;
                        break;
                    }
                    j += 1;
                }
                let j = j.min(n);
                out.push((i..j, "string"));
                i = j;
                continue;
            }
            if c.is_ascii_digit() {
                let end = i + rest
                    .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '_'))
                    .unwrap_or(rest.len());
                out.push((i..end, "number"));
                i = end;
                continue;
            }
            if c.is_ascii_alphabetic()
                || c == b'_'
                || c == b'@'
                || c == b'#' && self.keywords.is_empty()
            {
                let end = i
                    + 1
                    + rest[1..]
                        .find(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                        .unwrap_or(rest.len() - 1);
                let word = &line[i..end];
                let next = line[end..].chars().next();
                let capture = if c == b'@' || c == b'#' {
                    Some("attribute")
                } else if self.keywords.contains(&word) {
                    Some(if word == "return" {
                        "keyword.return"
                    } else if matches!(word, "use" | "import" | "from" | "mod" | "include") {
                        "keyword.import"
                    } else {
                        "keyword"
                    })
                } else if self.constants.contains(&word) {
                    Some(if matches!(word, "self" | "this") {
                        "variable.builtin"
                    } else {
                        "constant.builtin"
                    })
                } else if self.types.contains(&word) {
                    Some("type.builtin")
                } else if next == Some('!') && line[end..].starts_with("!(")
                    || line[end..].starts_with("![")
                    || line[end..].starts_with("! {")
                {
                    Some("function.macro")
                } else if next == Some('(') {
                    let prev = line[..i].trim_end();
                    Some(if prev.ends_with('.') {
                        "function.method"
                    } else {
                        "function"
                    })
                } else if self.capitalized_types
                    && word.chars().next().is_some_and(char::is_uppercase)
                {
                    Some(
                        if word
                            .chars()
                            .all(|ch| ch.is_uppercase() || ch == '_' || ch.is_ascii_digit())
                            && word.len() > 1
                        {
                            "constant"
                        } else {
                            "type"
                        },
                    )
                } else if line[..i].ends_with('.')
                    || (self.keywords.is_empty()
                        && next.is_some()
                        && line[end..].trim_start().starts_with(['=', ':']))
                {
                    Some("property")
                } else {
                    None
                };
                if let Some(capture) = capture {
                    let end = if capture == "function.macro" {
                        end + 1
                    } else {
                        end
                    };
                    out.push((i..end, capture));
                }
                i = end;
                continue;
            }
            if matches!(c, b'{' | b'}' | b'(' | b')' | b'[' | b']') {
                out.push((i..i + 1, "punctuation.bracket"));
            } else if matches!(
                c,
                b'=' | b'+'
                    | b'-'
                    | b'*'
                    | b'/'
                    | b'<'
                    | b'>'
                    | b'!'
                    | b'&'
                    | b'|'
                    | b'%'
                    | b'^'
                    | b'?'
            ) {
                out.push((i..i + 1, "operator"));
            } else if matches!(c, b',' | b';' | b'.' | b':') {
                out.push((i..i + 1, "punctuation.delimiter"));
            }
            i += rest.chars().next().map_or(1, char::len_utf8);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_and_crlf() {
        let s = InMemorySource::new("a\r\nbb\n");
        assert_eq!(s.line_count(), 3);
        assert_eq!(s.line(0).as_ref(), "a");
        assert_eq!(s.line(1).as_ref(), "bb");
        assert_eq!(s.line(2).as_ref(), "");
        assert_eq!(s.line(99).as_ref(), "");
        assert_eq!(s.max_line_len(), Some(2));
    }

    #[test]
    fn rust_highlighting() {
        let s = InMemorySource::for_file(
            "fn main() {\n    // hi\n    let x = \"s\\n\";\n}",
            "main.rs",
        );
        let spans = s.highlights(0..4);
        let at = |row: u32, text: &str| {
            let line = s.line(row);
            let start = line.find(text).unwrap();
            spans
                .iter()
                .find(|sp| sp.row == row && sp.range.start == start)
                .map(|sp| sp.style.clone())
        };
        assert_eq!(at(0, "fn"), Some(SpanStyle::Capture("keyword")));
        assert_eq!(at(0, "main"), Some(SpanStyle::Capture("function")));
        assert_eq!(at(1, "//"), Some(SpanStyle::Capture("comment")));
        assert_eq!(at(2, "\"s"), Some(SpanStyle::Capture("string")));
        assert_eq!(
            s.language_name().map(|l| l.to_string()).as_deref(),
            Some("Rust")
        );
    }

    #[test]
    fn folds_and_sticky() {
        let text = "mod a {\n    fn b() {\n        x();\n        y();\n    }\n}\n";
        let s = InMemorySource::new(text);
        let folds = s.fold_ranges();
        assert!(folds.contains(&(0..5)), "{folds:?}");
        assert!(folds.contains(&(1..4)), "{folds:?}");
        assert_eq!(s.sticky_rows(3), vec![0, 1]);
        assert_eq!(s.sticky_rows(0), Vec::<u32>::new());
    }
}
