//! Code navigation: definitions, references, hover (LSP first, tags/text fallback).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::future::{select, Either};
use lector_search::{search_project, CancelToken, CaseMode, SearchEvent, SearchQuery, WalkOptions};
use lector_syntax::SymbolLocation;
use lector_text::{Point, Text};
use parking_lot::{Condvar, Mutex};

use crate::document::Document;
use crate::project::Project;
use crate::Result;

/// How long LSP requests may take before falling back.
const LSP_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_DEFINITIONS: usize = 100;
const MAX_REFERENCES: usize = 500;

/// Where a [`Target`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetSource {
    /// Language server.
    Lsp,
    /// Tree-sitter tags index.
    Tags,
    /// Plain text search.
    Text,
}

/// A navigation result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// Absolute path.
    pub path: PathBuf,
    /// Range in 0-based row / byte-column coordinates.
    pub range: Range<Point>,
    /// The (trimmed) text of the first line, for lists.
    pub preview: String,
    /// Origin of the result.
    pub source: TargetSource,
}

/// (path, row, start column, end column, line text)
type TextHit = (PathBuf, u32, usize, usize, String);

/// Runs `fut` to completion, giving up after `dur`.
fn block_on_timeout<F: Future>(fut: F, dur: Duration) -> Option<F::Output> {
    let (tx, rx) = futures::channel::oneshot::channel::<()>();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let g2 = gate.clone();
    std::thread::spawn(move || {
        let (lock, cv) = &*g2;
        let mut done = lock.lock();
        let result = cv.wait_while_for(&mut done, |d| !*d, dur);
        if result.timed_out() && !*done {
            let _ = tx.send(());
        }
    });
    let out = futures::executor::block_on(async {
        futures::pin_mut!(fut);
        match select(fut, rx).await {
            Either::Left((v, _)) => Some(v),
            Either::Right(_) => None,
        }
    });
    let (lock, cv) = &*gate;
    *lock.lock() = true;
    cv.notify_all();
    out
}

/// Loads and caches target texts within one call.
struct TextCache<'a> {
    project: &'a Project,
    doc: &'a Document,
    loaded: HashMap<PathBuf, Option<Text>>,
}

impl<'a> TextCache<'a> {
    fn new(project: &'a Project, doc: &'a Document) -> Self {
        Self {
            project,
            doc,
            loaded: HashMap::new(),
        }
    }

    fn get(&mut self, path: &Path) -> Option<Text> {
        if path == self.doc.path() {
            return self.doc.text().cloned();
        }
        let project = self.project;
        self.loaded
            .entry(path.to_path_buf())
            .or_insert_with(|| {
                let threshold = project
                    .settings()
                    .files
                    .large_file_threshold_mb
                    .saturating_mul(1024 * 1024);
                lector_fs::load_file(path, threshold)
                    .ok()
                    .and_then(|f| f.text)
            })
            .clone()
    }

    fn preview(&mut self, path: &Path, row: u32) -> String {
        match self.get(path) {
            Some(t) if row < t.line_count() => preview_line(&t.line(row)),
            _ => String::new(),
        }
    }
}

fn preview_line(line: &str) -> String {
    let t = line.trim();
    match t.char_indices().nth(200) {
        Some((i, _)) => format!("{}…", &t[..i]),
        None => t.to_string(),
    }
}

fn lsp_locations(
    cache: &mut TextCache<'_>,
    locs: Vec<lector_lsp::Location>,
    limit: usize,
) -> Vec<Target> {
    let mut out = Vec::new();
    for loc in locs.into_iter().take(limit) {
        let (start, end) = match cache.get(&loc.path) {
            Some(t) => (
                t.point_utf16_to_point(loc.range.start),
                t.point_utf16_to_point(loc.range.end),
            ),
            None => (
                Point {
                    row: loc.range.start.row,
                    column: loc.range.start.column,
                },
                Point {
                    row: loc.range.end.row,
                    column: loc.range.end.column,
                },
            ),
        };
        let preview = cache.preview(&loc.path, start.row);
        out.push(Target {
            path: loc.path,
            range: start..end,
            preview,
            source: TargetSource::Lsp,
        });
    }
    out
}

fn tag_target(cache: &mut TextCache<'_>, loc: &SymbolLocation, source: TargetSource) -> Target {
    let end = Point {
        row: loc.point.row,
        column: loc.point.column + loc.range.len() as u32,
    };
    Target {
        path: loc.path.clone(),
        range: loc.point..end,
        preview: cache.preview(&loc.path, loc.point.row),
        source,
    }
}

/// Makes sure the document's own tags are in the index.
fn ensure_indexed(project: &Project, doc: &Document) {
    if project.symbols().definitions_in_file(doc.path()).is_empty() {
        if let (Some(text), Some(syntax)) = (doc.text(), doc.syntax()) {
            project
                .symbols()
                .insert_file(doc.path().to_path_buf(), text, syntax.tags(text));
        }
    }
}

fn lsp_position(doc: &Document, offset: usize) -> Option<lector_text::PointUtf16> {
    let text = doc.text()?;
    Some(text.offset_to_point_utf16(offset.min(text.len())))
}

fn rank_definitions(defs: &mut [SymbolLocation], doc: &Document) {
    let doc_path = doc.path().to_path_buf();
    let ext = doc_path.extension().map(|e| e.to_os_string());
    let dir: Vec<_> = doc_path
        .parent()
        .map(|p| p.components().collect())
        .unwrap_or_default();
    let common = |p: &Path| -> usize {
        p.parent()
            .map(|d| {
                d.components()
                    .zip(dir.iter())
                    .take_while(|(a, b)| a == *b)
                    .count()
            })
            .unwrap_or(0)
    };
    defs.sort_by(|a, b| {
        let key = |l: &SymbolLocation| {
            (
                l.path != doc_path,
                l.path.extension().map(|e| e.to_os_string()) != ext,
                std::cmp::Reverse(common(&l.path)),
            )
        };
        key(a)
            .cmp(&key(b))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.range.start.cmp(&b.range.start))
    });
}

/// Definitions of the symbol at `offset`: language server (3 s timeout), else tags index.
pub fn definitions(project: &Project, doc: &Document, offset: usize) -> Result<Vec<Target>> {
    let mut cache = TextCache::new(project, doc);
    if project.lsp_open(doc) {
        if let (Some(lsp), Some(pos)) = (project.lsp(), lsp_position(doc, offset)) {
            let res = block_on_timeout(lsp.definition(doc.path(), pos), LSP_TIMEOUT);
            if let Some(Ok(locs)) = res {
                if !locs.is_empty() {
                    return Ok(lsp_locations(&mut cache, locs, MAX_DEFINITIONS));
                }
            }
        }
    }
    let Some((_, word)) = doc.word_at(offset) else {
        return Ok(Vec::new());
    };
    ensure_indexed(project, doc);
    let mut defs = project.symbols().definitions(&word);
    rank_definitions(&mut defs, doc);
    defs.truncate(MAX_DEFINITIONS);
    Ok(defs
        .iter()
        .map(|d| tag_target(&mut cache, d, TargetSource::Tags))
        .collect())
}

/// References to the symbol at `offset`: language server, else tag references plus a
/// whole-word, case-sensitive project text search (deduplicated by file and row).
pub fn references(project: &Project, doc: &Document, offset: usize) -> Result<Vec<Target>> {
    let mut cache = TextCache::new(project, doc);
    if project.lsp_open(doc) {
        if let (Some(lsp), Some(pos)) = (project.lsp(), lsp_position(doc, offset)) {
            let res = block_on_timeout(lsp.references(doc.path(), pos), LSP_TIMEOUT);
            if let Some(Ok(locs)) = res {
                if !locs.is_empty() {
                    return Ok(lsp_locations(&mut cache, locs, MAX_REFERENCES));
                }
            }
        }
    }
    let Some((_, word)) = doc.word_at(offset) else {
        return Ok(Vec::new());
    };
    ensure_indexed(project, doc);
    let mut seen: HashSet<(PathBuf, u32)> = HashSet::new();
    let mut out: Vec<Target> = Vec::new();
    let mut tag_locs = project.symbols().references(&word);
    tag_locs.extend(project.symbols().definitions(&word));
    tag_locs.sort_by(|a, b| a.path.cmp(&b.path).then(a.range.start.cmp(&b.range.start)));
    for loc in &tag_locs {
        if out.len() >= MAX_REFERENCES {
            break;
        }
        if seen.insert((loc.path.clone(), loc.point.row)) {
            out.push(tag_target(&mut cache, loc, TargetSource::Tags));
        }
    }
    if out.len() < MAX_REFERENCES {
        let settings = project.settings();
        let walk = WalkOptions {
            hidden: settings.files.show_hidden,
            git_ignore: true,
            exclude: settings.files.exclude.clone(),
            max_depth: None,
        };
        let query = SearchQuery {
            pattern: word.clone(),
            regex: false,
            case: CaseMode::Sensitive,
            whole_word: true,
            include: Vec::new(),
            exclude: Vec::new(),
            max_results: MAX_REFERENCES,
        };
        let found: Mutex<Vec<TextHit>> = Mutex::new(Vec::new());
        let cancel = CancelToken::new();
        let _ = search_project(project.root(), &query, &walk, &cancel, |ev| {
            if let SearchEvent::File(f) = ev {
                let mut g = found.lock();
                for m in f.matches {
                    let r = m.ranges.first().cloned().unwrap_or(0..0);
                    g.push((project.root().join(&f.path), m.line, r.start, r.end, m.text));
                }
            }
        });
        let mut found = found.into_inner();
        found.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        for (path, row, c0, c1, text) in found {
            if out.len() >= MAX_REFERENCES {
                break;
            }
            if !seen.insert((path.clone(), row)) {
                continue;
            }
            out.push(Target {
                path,
                range: Point {
                    row,
                    column: c0 as u32,
                }..Point {
                    row,
                    column: c1 as u32,
                },
                preview: preview_line(&text),
                source: TargetSource::Text,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.range.start.cmp(&b.range.start)));
    Ok(out)
}

/// Hover markdown for the symbol at `offset`: language server, else (tags only) a code-fenced
/// signature line of the unique definition.
pub fn hover(project: &Project, doc: &Document, offset: usize) -> Result<Option<String>> {
    if project.lsp_open(doc) {
        if let (Some(lsp), Some(pos)) = (project.lsp(), lsp_position(doc, offset)) {
            let res = block_on_timeout(lsp.hover(doc.path(), pos), LSP_TIMEOUT);
            if let Some(Ok(Some(info))) = res {
                if !info.markdown.trim().is_empty() {
                    return Ok(Some(info.markdown));
                }
            }
        }
    }
    let Some((_, word)) = doc.word_at(offset) else {
        return Ok(None);
    };
    ensure_indexed(project, doc);
    let defs = project.symbols().definitions(&word);
    let [def] = defs.as_slice() else {
        return Ok(None);
    };
    let mut cache = TextCache::new(project, doc);
    let line = cache.preview(&def.path, def.point.row);
    if line.is_empty() {
        return Ok(None);
    }
    let lang = project
        .languages()
        .language_for_path(&def.path, None)
        .map(|l| l.lsp_id.clone())
        .unwrap_or_default();
    Ok(Some(format!("```{lang}\n{line}\n```")))
}

/// All known definitions for a symbol picker (tags index).
pub fn project_symbols(project: &Project) -> Vec<SymbolLocation> {
    project.symbols().all_definitions()
}
