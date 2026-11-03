//! Immutable snapshot of one loaded file with lazily computed syntax-derived data.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use lector_diff::{diff_lines, gutter_marks, DiffOptions, GutterMark, Whitespace};
use lector_fs::{FileKind, LoadedFile};
use lector_syntax::{HighlightId, Language, LanguageRegistry, OutlineItem, Syntax};
use lector_text::{Bias, Text};
use parking_lot::Mutex;

use crate::project::Project;

const CHUNK_ROWS: u32 = 128;
const MAX_CACHED_CHUNKS: usize = 96;
const SAMPLE_BYTES: usize = 64 * 1024;
const INDENT_SAMPLE_LINES: u32 = 1000;

/// Line terminator style of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEnding {
    /// `\n` (also files without any newline).
    Lf,
    /// `\r\n`.
    CrLf,
    /// Both.
    Mixed,
}

/// Indentation style of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Indentation {
    /// Tab characters.
    Tabs,
    /// This many spaces per level.
    Spaces(u32),
}

/// Highlight spans of one line; ranges are byte offsets within the line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineHighlights {
    /// 0-based row.
    pub row: u32,
    /// Sorted, non-overlapping spans.
    pub spans: Vec<(Range<usize>, HighlightId)>,
}

struct Inner {
    path: PathBuf,
    rel: Option<String>,
    kind: FileKind,
    size: u64,
    encoding: &'static str,
    lossy: bool,
    text: Option<Text>,
    language: Option<Arc<Language>>,
    syntax: Option<Syntax>,
    line_ending: LineEnding,
    indentation: Indentation,
    highlight_cache: Mutex<HashMap<u32, Arc<Vec<LineHighlights>>>>,
    outline: OnceLock<Arc<Vec<OutlineItem>>>,
    folds: OnceLock<Arc<Vec<Range<u32>>>>,
}

/// An immutable snapshot of a file. Cheap to clone.
#[derive(Clone)]
pub struct Document {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Document {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Document")
            .field("path", &self.inner.path)
            .field("kind", &self.inner.kind)
            .finish()
    }
}

impl Document {
    pub(crate) fn build(
        path: PathBuf,
        rel: Option<String>,
        loaded: LoadedFile,
        languages: &LanguageRegistry,
        default_indent: u32,
    ) -> Document {
        let LoadedFile {
            kind,
            text,
            size,
            encoding,
            lossy,
        } = loaded;
        let language;
        let mut syntax = None;
        let mut line_ending = LineEnding::Lf;
        let mut indentation = Indentation::Spaces(default_indent);
        if let Some(t) = &text {
            let first_line = if t.is_empty() {
                None
            } else {
                // Only look at a bounded prefix: the first line of a minified or
                // mapped file can be gigabytes long.
                let head_end = t
                    .line_range(0)
                    .end
                    .min(t.clip_offset(t.len().min(4096), Bias::Left));
                let l = t.text_for_range(0..head_end);
                let end = l.char_indices().nth(256).map_or(l.len(), |(i, _)| i);
                Some(l[..end].to_string())
            };
            language = languages.language_for_path(&path, first_line.as_deref());
            if !t.is_mapped() {
                if let Some(lang) = &language {
                    let parsed = std::panic::catch_unwind(|| Syntax::parse(lang, t));
                    syntax = parsed.ok().and_then(|r| r.ok());
                }
                indentation = detect_indentation(t, default_indent);
            }
            line_ending = detect_line_ending(t);
        } else {
            language = languages.language_for_path(&path, None);
        }
        Document {
            inner: Arc::new(Inner {
                path,
                rel,
                kind,
                size,
                encoding,
                lossy,
                text,
                language,
                syntax,
                line_ending,
                indentation,
                highlight_cache: Mutex::new(HashMap::new()),
                outline: OnceLock::new(),
                folds: OnceLock::new(),
            }),
        }
    }

    /// Absolute path.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Path relative to the project root, if inside it.
    pub fn rel_path(&self) -> Option<&str> {
        self.inner.rel.as_deref()
    }

    /// Detected file kind.
    pub fn kind(&self) -> &FileKind {
        &self.inner.kind
    }

    /// Size on disk in bytes.
    pub fn size(&self) -> u64 {
        self.inner.size
    }

    /// Detected encoding name.
    pub fn encoding(&self) -> &str {
        self.inner.encoding
    }

    /// Whether decoding replaced invalid bytes.
    pub fn lossy(&self) -> bool {
        self.inner.lossy
    }

    /// The text (`None` for binary files and non-SVG images).
    pub fn text(&self) -> Option<&Text> {
        self.inner.text.as_ref()
    }

    /// Detected language.
    pub fn language(&self) -> Option<&Arc<Language>> {
        self.inner.language.as_ref()
    }

    /// Parsed syntax tree (`None` when large, unknown language or parse failure).
    pub fn syntax(&self) -> Option<&Syntax> {
        self.inner.syntax.as_ref()
    }

    /// Dominant line ending.
    pub fn line_ending(&self) -> LineEnding {
        self.inner.line_ending
    }

    /// Detected indentation.
    pub fn indentation(&self) -> Indentation {
        self.inner.indentation
    }

    /// Memory-mapped (very large) file: syntax features are disabled.
    pub fn is_large(&self) -> bool {
        self.inner.text.as_ref().is_some_and(|t| t.is_mapped())
    }

    /// Number of text rows (0 without text).
    pub fn line_count(&self) -> u32 {
        self.text().map_or(0, |t| t.line_count())
    }

    /// Per-line highlight spans for `rows` (clamped to the text); one entry per row, with
    /// empty spans when there is no syntax. Results are cached in 128-row chunks.
    pub fn line_highlights(&self, rows: Range<u32>) -> Vec<LineHighlights> {
        let Some(text) = self.text() else {
            return Vec::new();
        };
        let end = rows.end.min(text.line_count());
        if rows.start >= end {
            return Vec::new();
        }
        if self.inner.syntax.is_none() {
            return (rows.start..end)
                .map(|row| LineHighlights {
                    row,
                    spans: Vec::new(),
                })
                .collect();
        }
        let mut out = Vec::with_capacity((end - rows.start) as usize);
        for chunk in rows.start / CHUNK_ROWS..=(end - 1) / CHUNK_ROWS {
            let lines = self.chunk(chunk);
            let first = chunk * CHUNK_ROWS;
            let lo = rows.start.max(first) - first;
            let hi = (end.min(first + CHUNK_ROWS) - first).min(lines.len() as u32);
            if lo < hi {
                out.extend_from_slice(&lines[lo as usize..hi as usize]);
            }
        }
        out
    }

    fn chunk(&self, chunk: u32) -> Arc<Vec<LineHighlights>> {
        if let Some(c) = self.inner.highlight_cache.lock().get(&chunk) {
            return c.clone();
        }
        let computed = Arc::new(self.compute_chunk(chunk));
        let mut cache = self.inner.highlight_cache.lock();
        if cache.len() >= MAX_CACHED_CHUNKS {
            cache.clear();
        }
        cache.insert(chunk, computed.clone());
        computed
    }

    fn compute_chunk(&self, chunk: u32) -> Vec<LineHighlights> {
        let (Some(text), Some(syntax)) = (self.text(), self.syntax()) else {
            return Vec::new();
        };
        let start = chunk * CHUNK_ROWS;
        let total = text.line_count();
        if start >= total {
            return Vec::new();
        }
        let end = (start + CHUNK_ROWS).min(total);
        let b0 = text.line_range(start).start;
        let b1 = text.line_range(end - 1).end;
        let spans = syntax.highlights(text, b0..b1);
        let mut rows = Vec::with_capacity((end - start) as usize);
        let mut idx = 0;
        for row in start..end {
            let r = text.line_range(row);
            while idx < spans.len() && spans[idx].range.end <= r.start {
                idx += 1;
            }
            let mut line = Vec::new();
            let mut j = idx;
            while j < spans.len() && spans[j].range.start < r.end {
                let s = spans[j].range.start.max(r.start) - r.start;
                let e = spans[j].range.end.min(r.end) - r.start;
                if e > s {
                    line.push((s..e, spans[j].id));
                }
                j += 1;
            }
            rows.push(LineHighlights { row, spans: line });
        }
        rows
    }

    /// Symbol outline (document order); empty without syntax.
    pub fn outline(&self) -> Arc<Vec<OutlineItem>> {
        self.inner
            .outline
            .get_or_init(|| match (self.text(), self.syntax()) {
                (Some(t), Some(s)) => Arc::new(s.outline(t)),
                _ => Arc::new(Vec::new()),
            })
            .clone()
    }

    /// Foldable row ranges (see ARCHITECTURE.md "Fold ranges"); empty without syntax.
    pub fn fold_ranges(&self) -> Arc<Vec<Range<u32>>> {
        self.inner
            .folds
            .get_or_init(|| match (self.text(), self.syntax()) {
                (Some(t), Some(s)) => Arc::new(s.fold_ranges(t)),
                _ => Arc::new(Vec::new()),
            })
            .clone()
    }

    /// Scopes (outermost first) enclosing the first non-blank character of `row` that start on
    /// an earlier row: the sticky-scroll / breadcrumb chain.
    pub fn scopes_at_row(&self, row: u32) -> Vec<OutlineItem> {
        let (Some(text), Some(syntax)) = (self.text(), self.syntax()) else {
            return Vec::new();
        };
        if row >= text.line_count() {
            return Vec::new();
        }
        let range = text.line_range(row);
        let line = text.text_for_range(range.clone());
        let lead = line.len() - line.trim_start().len();
        let offset = if lead >= line.len() {
            range.start
        } else {
            range.start + lead
        };
        syntax
            .enclosing_scopes(text, offset)
            .into_iter()
            .filter(|s| text.offset_to_point(s.range.start).row < row)
            .collect()
    }

    /// The identifier under `offset` (also when the cursor sits right after it).
    pub fn word_at(&self, offset: usize) -> Option<(Range<usize>, String)> {
        let text = self.text()?;
        let offset = text.clip_offset(offset.min(text.len()), Bias::Left);
        if let Some(node) = self.syntax().and_then(|s| s.node_at(offset)) {
            if node.is_identifier() && node.range.start <= offset && offset < node.range.end {
                let s = text.text_for_range(node.range.clone()).into_owned();
                if !s.is_empty() {
                    return Some((node.range, s));
                }
            }
        }
        scan_word(text, offset)
    }

    /// Change marks against `HEAD` for the diff gutter. Untracked/added files: every row Added.
    /// Empty outside a repository, for binary or very large files.
    pub fn diff_gutter(&self, project: &Project) -> Vec<GutterMark> {
        let (Some(text), Some(rel), Some(repo)) = (self.text(), self.rel_path(), project.repo())
        else {
            return Vec::new();
        };
        if self.is_large() {
            return Vec::new();
        }
        let old = match repo.head_text(&project.repo_path(rel)) {
            Ok(Some(bytes)) => {
                if lector_diff::is_binary(&bytes) {
                    return Vec::new();
                }
                String::from_utf8_lossy(&bytes).into_owned()
            }
            Ok(None) => {
                if project.git_status().get(rel).is_none() {
                    return Vec::new();
                }
                String::new()
            }
            Err(_) => return Vec::new(),
        };
        let new = text.to_string();
        let opts = DiffOptions {
            whitespace: Whitespace::IgnoreEol,
            ..DiffOptions::default()
        };
        gutter_marks(&diff_lines(&old, &new, &opts))
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn scan_word(text: &Text, offset: usize) -> Option<(Range<usize>, String)> {
    let row = text.offset_to_point(offset).row;
    let lr = text.line_range(row);
    let line = text.text_for_range(lr.clone());
    let rel = offset.saturating_sub(lr.start).min(line.len());
    if !line.is_char_boundary(rel) {
        return None;
    }
    let on_char = line[rel..].chars().next().is_some_and(is_word_char);
    let before = line[..rel].chars().next_back().is_some_and(is_word_char);
    if !on_char && !before {
        return None;
    }
    let mut start = rel;
    for (i, c) in line[..rel].char_indices().rev() {
        if is_word_char(c) {
            start = i;
        } else {
            break;
        }
    }
    let mut end = rel;
    for (i, c) in line[rel..].char_indices() {
        if is_word_char(c) {
            end = rel + i + c.len_utf8();
        } else {
            break;
        }
    }
    let word = &line[start..end];
    if word.is_empty() || word.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((lr.start + start..lr.start + end, word.to_string()))
}

fn detect_line_ending(text: &Text) -> LineEnding {
    let end = text.clip_offset(text.len().min(SAMPLE_BYTES), Bias::Left);
    let sample = text.text_for_range(0..end);
    let bytes = sample.as_bytes();
    let (mut crlf, mut lf) = (0usize, 0usize);
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            if i > 0 && bytes[i - 1] == b'\r' {
                crlf += 1;
            } else {
                lf += 1;
            }
        }
    }
    match (crlf, lf) {
        (0, _) => LineEnding::Lf,
        (_, 0) => LineEnding::CrLf,
        _ => LineEnding::Mixed,
    }
}

fn detect_indentation(text: &Text, default: u32) -> Indentation {
    let rows = text.line_count().min(INDENT_SAMPLE_LINES);
    let mut tabs = 0usize;
    let mut spaced = 0usize;
    let mut deltas = [0usize; 9];
    let mut prev = 0usize;
    for line in text.lines(0..rows) {
        let b = line.as_bytes();
        match b.first() {
            Some(b'\t') => {
                tabs += 1;
                continue;
            }
            Some(b' ') => {}
            _ => {
                prev = 0;
                continue;
            }
        }
        let n = b.iter().take_while(|&&c| c == b' ').count();
        if n == b.len() {
            continue; // whitespace-only line
        }
        spaced += 1;
        if n > prev {
            let d = n - prev;
            if matches!(d, 2 | 3 | 4 | 8) {
                deltas[d] += 1;
            }
        }
        prev = n;
    }
    if tabs > spaced {
        return Indentation::Tabs;
    }
    let best = [2usize, 4, 8, 3]
        .into_iter()
        .max_by_key(|&d| (deltas[d], std::cmp::Reverse(d)));
    match best {
        Some(d) if deltas[d] > 0 => Indentation::Spaces(d as u32),
        _ => Indentation::Spaces(default),
    }
}
