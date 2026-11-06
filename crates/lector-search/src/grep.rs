use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use grep_matcher::{Captures, Matcher};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkFinish, SinkMatch};
use ignore::WalkState;

use crate::globs::build_set;
use crate::walk::{build_walker, WalkOptions};
use crate::{CancelToken, Result};

/// Files larger than this are skipped by project search.
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;
/// Longest line text (bytes) reported in a [`LineMatch`].
const MAX_LINE_TEXT: usize = 500;
/// Context kept before the first match when a long line is windowed.
const WINDOW_LEAD: usize = 100;

/// Case sensitivity of a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaseMode {
    /// Insensitive unless the pattern contains an uppercase letter.
    #[default]
    Smart,
    /// Case-sensitive.
    Sensitive,
    /// Case-insensitive.
    Insensitive,
}

/// A text search request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchQuery {
    /// The pattern (regex syntax when `regex`, otherwise a literal).
    pub pattern: String,
    /// Interpret `pattern` as a regular expression.
    pub regex: bool,
    /// Case sensitivity.
    pub case: CaseMode,
    /// Match whole words only.
    pub whole_word: bool,
    /// Only search files matching one of these globs (empty = all files).
    pub include: Vec<String>,
    /// Skip files/directories matching these globs (in addition to `WalkOptions::exclude`).
    pub exclude: Vec<String>,
    /// Stop after this many matching lines (0 = unlimited).
    pub max_results: usize,
}

/// A matching line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineMatch {
    /// 0-based line number.
    pub line: u32,
    /// Line text without the trailing newline. Lines over ~500 bytes are cut to a window
    /// around the first match (on char boundaries); `ranges` always index into this text.
    pub text: String,
    /// Byte ranges of each match within `text`.
    pub ranges: Vec<Range<usize>>,
}

/// All matching lines of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMatches {
    /// Path relative to the search root.
    pub path: PathBuf,
    /// Matching lines in file order.
    pub matches: Vec<LineMatch>,
}

/// Progress events emitted by [`search_project`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchEvent {
    /// Matches in one file (files arrive in unspecified order).
    File(FileMatches),
    /// Always the last event (exactly one).
    Done {
        /// Files actually searched (excludes skipped binary/oversized files).
        files_searched: usize,
        /// Matching lines reported.
        matches: usize,
        /// True if `max_results` cut the search short.
        truncated: bool,
    },
    /// Non-fatal or fatal problem description (e.g. invalid regex), followed by `Done`.
    Error(String),
}

/// Search failures.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// The pattern is not a valid regular expression.
    #[error("invalid pattern: {0}")]
    Pattern(String),
}

fn build_matcher(query: &SearchQuery) -> Result<RegexMatcher> {
    let pattern = if query.regex {
        query.pattern.clone()
    } else {
        regex::escape(&query.pattern)
    };
    let mut b = RegexMatcherBuilder::new();
    b.multi_line(true)
        .line_terminator(Some(b'\n'))
        .word(query.whole_word);
    match query.case {
        CaseMode::Smart => b.case_smart(true),
        CaseMode::Insensitive => b.case_insensitive(true),
        CaseMode::Sensitive => &mut b,
    };
    b.build(&pattern)
        .map_err(|e| SearchError::Pattern(e.to_string()))
}

fn match_ranges(matcher: &RegexMatcher, text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let _ = matcher.find_iter(text.as_bytes(), |m| {
        if m.start() < m.end() {
            out.push(m.start()..m.end());
        }
        true
    });
    out
}

/// Finds all non-empty matches of `query` in `text`; returns byte ranges.
/// Same semantics as [`search_project`] (`include`/`exclude`/`max_results` are ignored);
/// `^`/`$` anchor at line boundaries. An empty pattern yields no matches.
pub fn find_in_text(text: &str, query: &SearchQuery) -> Result<Vec<Range<usize>>> {
    if query.pattern.is_empty() {
        return Ok(Vec::new());
    }
    let matcher = build_matcher(query)?;
    let mut ranges = match_ranges(&matcher, text);
    if query.max_results > 0 {
        ranges.truncate(query.max_results);
    }
    Ok(ranges)
}

/// Finds all non-empty matches of `query` in `text` together with their
/// replacement. With `query.regex`, `replacement` may refer to capture groups
/// (`$1`, `${name}`; `$$` is a literal dollar); otherwise it is used as is.
/// Ranges are byte ranges in `text`, in order and non-overlapping.
pub fn replacements_in_text(
    text: &str,
    query: &SearchQuery,
    replacement: &str,
) -> Result<Vec<(Range<usize>, String)>> {
    if query.pattern.is_empty() {
        return Ok(Vec::new());
    }
    let matcher = build_matcher(query)?;
    let mut captures = matcher
        .new_captures()
        .map_err(|e| SearchError::Pattern(e.to_string()))?;
    let mut out = Vec::new();
    let haystack = text.as_bytes();
    let found = matcher.captures_iter(haystack, &mut captures, |caps| {
        let Some(whole) = caps.get(0) else {
            return true;
        };
        if whole.start() >= whole.end() {
            return true;
        }
        let replaced = if query.regex {
            let mut dst = Vec::new();
            caps.interpolate(
                |name| matcher.capture_index(name),
                haystack,
                replacement.as_bytes(),
                &mut dst,
            );
            String::from_utf8_lossy(&dst).into_owned()
        } else {
            replacement.to_string()
        };
        out.push((whole.start()..whole.end(), replaced));
        true
    });
    found.map_err(|e| SearchError::Pattern(e.to_string()))?;
    if query.max_results > 0 {
        out.truncate(query.max_results);
    }
    Ok(out)
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

/// Builds a `LineMatch`, windowing very long lines. `None` if the line has no non-empty match.
fn make_line_match(matcher: &RegexMatcher, line: u32, raw: &str) -> Option<LineMatch> {
    let text = raw.strip_suffix('\n').unwrap_or(raw);
    let text = text.strip_suffix('\r').unwrap_or(text);
    let ranges = match_ranges(matcher, text);
    let first = ranges.first()?.clone();
    if text.len() <= MAX_LINE_TEXT {
        return Some(LineMatch {
            line,
            text: text.to_string(),
            ranges,
        });
    }
    let start = floor_boundary(text, first.start.saturating_sub(WINDOW_LEAD));
    let end = floor_boundary(text, (start + MAX_LINE_TEXT).min(text.len()));
    let start = if end == text.len() {
        ceil_boundary(text, text.len().saturating_sub(MAX_LINE_TEXT))
    } else {
        start
    };
    let ranges = ranges
        .into_iter()
        .filter(|r| r.start < end && r.end > start)
        .map(|r| r.start.max(start) - start..r.end.min(end) - start)
        .collect();
    Some(LineMatch {
        line,
        text: text[start..end].to_string(),
        ranges,
    })
}

struct FileSink<'a> {
    matcher: &'a RegexMatcher,
    out: Vec<LineMatch>,
    binary: bool,
    total: &'a AtomicUsize,
    max: usize,
    truncated: &'a AtomicBool,
    cancel: &'a CancelToken,
}

impl Sink for FileSink<'_> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _: &Searcher,
        mat: &SinkMatch<'_>,
    ) -> std::result::Result<bool, Self::Error> {
        if self.cancel.is_cancelled() {
            return Ok(false);
        }
        let Some(n) = mat.line_number() else {
            return Ok(true);
        };
        let raw = String::from_utf8_lossy(mat.bytes());
        let Some(lm) = make_line_match(
            self.matcher,
            (n.saturating_sub(1)).min(u32::MAX as u64) as u32,
            &raw,
        ) else {
            return Ok(true);
        };
        if self.max > 0 {
            let prev = self.total.fetch_add(1, Ordering::Relaxed);
            if prev >= self.max {
                self.total.fetch_sub(1, Ordering::Relaxed);
                self.truncated.store(true, Ordering::Relaxed);
                return Ok(false);
            }
        } else {
            self.total.fetch_add(1, Ordering::Relaxed);
        }
        self.out.push(lm);
        Ok(true)
    }

    fn finish(&mut self, _: &Searcher, f: &SinkFinish) -> std::result::Result<(), Self::Error> {
        self.binary = f.binary_byte_offset().is_some();
        Ok(())
    }
}

fn looks_binary(path: &Path) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else {
        return true;
    };
    let mut buf = [0u8; 8000];
    let n = f.read(&mut buf).unwrap_or(0);
    buf[..n].contains(&0)
}

/// Searches exactly `files` (paths relative to `root`, e.g. the candidates of a
/// [`crate::TrigramIndex`]) with the same semantics and events as
/// [`search_project`] (no walking: `WalkOptions` and `query.exclude` do not
/// apply; `query.include` still filters). Files are searched on all cores.
pub fn search_files(
    root: &Path,
    files: &[PathBuf],
    query: &SearchQuery,
    cancel: &CancelToken,
    sink: impl Fn(SearchEvent) + Send + Sync,
) -> Result<()> {
    let done = |files_searched, matches, truncated| SearchEvent::Done {
        files_searched,
        matches,
        truncated,
    };
    if query.pattern.is_empty() {
        sink(done(0, 0, false));
        return Ok(());
    }
    let matcher = match build_matcher(query) {
        Ok(m) => m,
        Err(e) => {
            sink(SearchEvent::Error(e.to_string()));
            sink(done(0, 0, false));
            return Err(e);
        }
    };
    let (include, _) = build_set(&query.include);
    let has_include = !query.include.is_empty();
    let files_searched = AtomicUsize::new(0);
    let total = AtomicUsize::new(0);
    let truncated = AtomicBool::new(false);
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .min(8);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut searcher = SearcherBuilder::new()
                    .line_number(true)
                    .binary_detection(BinaryDetection::quit(b'\0'))
                    .build();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(rel) = files.get(i) else { break };
                    if cancel.is_cancelled() || truncated.load(Ordering::Relaxed) {
                        break;
                    }
                    if has_include && !include.is_match(rel) {
                        continue;
                    }
                    let full = root.join(rel);
                    if std::fs::metadata(&full)
                        .map(|m| !m.is_file() || m.len() > MAX_FILE_SIZE)
                        .unwrap_or(true)
                        || looks_binary(&full)
                    {
                        continue;
                    }
                    let mut fs = FileSink {
                        matcher: &matcher,
                        out: Vec::new(),
                        binary: false,
                        total: &total,
                        max: query.max_results,
                        truncated: &truncated,
                        cancel,
                    };
                    if searcher.search_path(&matcher, &full, &mut fs).is_ok() {
                        files_searched.fetch_add(1, Ordering::Relaxed);
                    }
                    if fs.binary {
                        total.fetch_sub(fs.out.len(), Ordering::Relaxed);
                        continue;
                    }
                    if !fs.out.is_empty() {
                        sink(SearchEvent::File(FileMatches {
                            path: rel.clone(),
                            matches: fs.out,
                        }));
                    }
                }
            });
        }
    });
    sink(done(
        files_searched.load(Ordering::Relaxed),
        total.load(Ordering::Relaxed),
        truncated.load(Ordering::Relaxed),
    ));
    Ok(())
}

/// Searches the files under `root` (walked with `walk` plus `query.exclude`/`include`) in
/// parallel, emitting one [`SearchEvent::File`] per file with matches and finally exactly
/// one [`SearchEvent::Done`]. Binary files (NUL byte) and files over 10 MB are skipped.
///
/// Returns `Err` only for an invalid pattern (after emitting `Error` and `Done`).
/// An empty pattern completes immediately with zero results.
pub fn search_project(
    root: &Path,
    query: &SearchQuery,
    walk: &WalkOptions,
    cancel: &CancelToken,
    sink: impl Fn(SearchEvent) + Send + Sync,
) -> Result<()> {
    if query.pattern.is_empty() {
        sink(SearchEvent::Done {
            files_searched: 0,
            matches: 0,
            truncated: false,
        });
        return Ok(());
    }
    let matcher = match build_matcher(query) {
        Ok(m) => m,
        Err(e) => {
            sink(SearchEvent::Error(e.to_string()));
            sink(SearchEvent::Done {
                files_searched: 0,
                matches: 0,
                truncated: false,
            });
            return Err(e);
        }
    };
    let (include, _) = build_set(&query.include);
    let has_include = !query.include.is_empty();
    let walker = build_walker(root, walk, &query.exclude).build_parallel();

    let files_searched = AtomicUsize::new(0);
    let total = AtomicUsize::new(0);
    let truncated = AtomicBool::new(false);
    let (sink_ref, matcher_ref, include_ref) = (&sink, &matcher, &include);
    let (files_ref, total_ref, trunc_ref) = (&files_searched, &total, &truncated);

    walker.run(|| {
        let mut searcher = SearcherBuilder::new()
            .line_number(true)
            .binary_detection(BinaryDetection::quit(b'\0'))
            .build();
        Box::new(move |entry| {
            if cancel.is_cancelled() || trunc_ref.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            let Ok(rel) = entry.path().strip_prefix(root) else {
                return WalkState::Continue;
            };
            if has_include && !include_ref.is_match(rel) {
                return WalkState::Continue;
            }
            if entry
                .metadata()
                .map(|m| m.len() > MAX_FILE_SIZE)
                .unwrap_or(true)
                || looks_binary(entry.path())
            {
                return WalkState::Continue;
            }
            let mut fs = FileSink {
                matcher: matcher_ref,
                out: Vec::new(),
                binary: false,
                total: total_ref,
                max: query.max_results,
                truncated: trunc_ref,
                cancel,
            };
            let res = searcher.search_path(matcher_ref, entry.path(), &mut fs);
            if res.is_ok() {
                files_ref.fetch_add(1, Ordering::Relaxed);
            }
            if fs.binary {
                // Binary data past the first 8000 bytes: drop partial results.
                total_ref.fetch_sub(fs.out.len(), Ordering::Relaxed);
                return WalkState::Continue;
            }
            if !fs.out.is_empty() {
                sink_ref(SearchEvent::File(FileMatches {
                    path: rel.to_path_buf(),
                    matches: fs.out,
                }));
            }
            WalkState::Continue
        })
    });

    sink(SearchEvent::Done {
        files_searched: files_searched.load(Ordering::Relaxed),
        matches: total.load(Ordering::Relaxed),
        truncated: truncated.load(Ordering::Relaxed),
    });
    Ok(())
}
