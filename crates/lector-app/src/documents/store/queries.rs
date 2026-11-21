//! Derived data of a document: incremental parsing, the versioned queries (structure, diff gutter,
//! blame, highlights, diagnostics) and the LSP sync.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use lector_base::CancelToken;
use lector_buffer::Transaction;
use lector_diff::{diff_lines, DiffOptions, GutterMark};
use lector_git::BlameHunk;
use lector_syntax::{OutlineItem, Syntax};
use lector_text::Rope;

use crate::documents::derive::{self, DocDiagnostic};
use crate::scheduler::{JobKind, Priority};
use crate::{Document, LineHighlights};

use super::{
    map_result, slot_result, syntax_edits, ChunkHighlights, DocEvent, DocHandle, DocState, Fx,
    QueryCounts, QueryKind, QueryResult, Structure, HIGHLIGHT_CHUNK,
};

impl DocHandle {
    pub(super) fn schedule_reparse(&self, st: &mut DocState, fx: &mut Fx) {
        if st.parse_running || st.pending_deltas.is_empty() {
            return;
        }
        let Some((old, _)) = st.parsed.clone() else {
            st.pending_deltas.clear();
            return;
        };
        let Some(text) = st.live.text().cloned() else {
            return;
        };
        let version = text.version();
        let edits = syntax_edits(&std::mem::take(&mut st.pending_deltas));
        st.parse_running = true;
        let gen = st.parse_gen;
        let weak = self.shared.me.clone();
        fx.spawn(
            Priority::Interactive,
            self.key(JobKind::DocumentParse),
            Box::new(move |_| {
                let started = std::time::Instant::now();
                let parsed = old
                    .edit_and_reparse(&text, &edits)
                    .or_else(|_| Syntax::parse(old.language(), &text));
                tracing::trace!(target: "lector_app::perf", elapsed_us = started.elapsed().as_micros() as u64, "incremental reparse");
                Self::with_weak(&weak, |h| h.parsed(gen, version, parsed.ok()));
            }),
        );
    }

    fn parsed(&self, gen: u64, version: u64, syntax: Option<Syntax>) {
        let mut fx = Fx::default();
        {
            let mut st = self.shared.state.lock();
            if st.parse_gen != gen {
                return;
            }
            st.parse_running = false;
            let Some(syntax) = syntax else {
                // Too large now: no more syntax for this buffer.
                st.parsed = None;
                st.pending_deltas.clear();
                self.syntax_settled(&mut st, &mut fx);
                drop(st);
                self.run(fx);
                return;
            };
            st.parsed = Some((syntax.clone(), version));
            if st.version() == version {
                if let Some(text) = st.live.text().cloned() {
                    st.live = st.live.with_text(text, Some(syntax));
                    fx.events.push(DocEvent::Reparsed { version });
                }
                self.syntax_settled(&mut st, &mut fx);
            } else {
                self.schedule_reparse(&mut st, &mut fx);
            }
        }
        self.run(fx);
    }

    /// The current version's syntax is final: run queries that waited for it.
    fn syntax_settled(&self, st: &mut DocState, fx: &mut Fx) {
        if std::mem::take(&mut st.structure.waiting_syntax) {
            st.structure.scheduled = None;
            self.structure_job(st, Duration::ZERO, fx);
        }
    }

    // ----- queries -------------------------------------------------------

    /// Outline + folds of the current version (scheduled when missing; the
    /// newest older result meanwhile). [`DocEvent::QueryReady`] announces it.
    pub fn structure(&self) -> QueryResult<Structure> {
        let mut fx = Fx::default();
        let r = {
            let mut st = self.shared.state.lock();
            let version = st.version();
            let delay = self.delays().structure;
            let current = st
                .structure
                .value
                .as_ref()
                .is_some_and(|(v, _)| *v == version);
            if !current {
                self.structure_job(&mut st, delay, &mut fx);
            }
            slot_result(&st.structure, version)
        };
        self.run(fx);
        r
    }

    /// Outline of the current version (see [`DocHandle::structure`]).
    pub fn outline(&self) -> QueryResult<Arc<Vec<OutlineItem>>> {
        map_result(self.structure(), |s| s.outline)
    }

    /// Fold ranges of the current version (see [`DocHandle::structure`]).
    pub fn folds(&self) -> QueryResult<Arc<Vec<Range<u32>>>> {
        map_result(self.structure(), |s| s.folds)
    }

    /// The newest document whose outline is computed (sticky headers,
    /// breadcrumbs): at most a few hundred milliseconds behind typing.
    pub fn settled_document(&self) -> Document {
        let st = self.shared.state.lock();
        st.structure
            .value
            .as_ref()
            .map(|(_, s)| s.document.clone())
            .unwrap_or_else(|| st.live.clone())
    }

    /// Enclosing scopes at `row` from the settled document (sticky headers,
    /// breadcrumbs), outermost first.
    pub fn scopes_at_row(&self, row: u32) -> Vec<OutlineItem> {
        self.settled_document().scopes_at_row(row)
    }

    fn structure_job(&self, st: &mut DocState, delay: Duration, fx: &mut Fx) {
        let version = st.version();
        if st.structure.scheduled == Some(version) {
            return;
        }
        st.structure.scheduled = Some(version);
        let weak = self.shared.me.clone();
        fx.debounce(
            self.key(JobKind::DocumentStructure),
            delay,
            Priority::Visible,
            Box::new(move |cancel| {
                Self::with_weak(&weak, |h| h.compute_structure(version, cancel));
            }),
        );
    }

    fn compute_structure(&self, version: u64, cancel: &CancelToken) {
        let doc = {
            let mut st = self.shared.state.lock();
            if st.version() != version || cancel.is_cancelled() {
                if st.structure.scheduled == Some(version) {
                    st.structure.scheduled = None;
                }
                return;
            }
            if st.syntax_pending() {
                st.structure.waiting_syntax = true;
                return;
            }
            st.live.clone()
        };
        let outline = doc.outline();
        let folds = doc.fold_ranges();
        let mut fx = Fx::default();
        {
            let mut st = self.shared.state.lock();
            st.structure.computes += 1;
            if st.structure.scheduled == Some(version) {
                st.structure.scheduled = None;
            }
            let newer = st
                .structure
                .value
                .as_ref()
                .is_some_and(|(v, _)| *v == st.version());
            if !newer {
                st.structure.value = Some((
                    version,
                    Structure {
                        document: doc,
                        outline,
                        folds,
                    },
                ));
                fx.events.push(DocEvent::QueryReady {
                    kind: QueryKind::Structure,
                    version,
                });
            }
        }
        self.run(fx);
    }

    /// Diff gutter vs HEAD for the current version (scheduled when missing).
    pub fn gutter(&self) -> QueryResult<Arc<Vec<GutterMark>>> {
        let mut fx = Fx::default();
        let r = {
            let mut st = self.shared.state.lock();
            let version = st.version();
            let current = st.gutter.value.as_ref().is_some_and(|(v, _)| *v == version);
            if !current && st.gutter_enabled {
                let delay = self.delays().gutter;
                self.gutter_job(&mut st, delay, &mut fx);
            }
            slot_result(&st.gutter, version)
        };
        self.run(fx);
        r
    }

    /// Re-reads HEAD and recomputes the gutter now (git state changed).
    pub fn refresh_gutter(&self) {
        let mut fx = Fx::default();
        {
            let mut st = self.shared.state.lock();
            st.gutter_base = None;
            st.gutter.scheduled = None;
            if st.gutter_enabled {
                self.gutter_job(&mut st, Duration::ZERO, &mut fx);
            }
        }
        self.run(fx);
    }

    /// Turns the gutter query on or off (off: empty marks).
    pub fn set_gutter_enabled(&self, enabled: bool) {
        let mut st = self.shared.state.lock();
        st.gutter_enabled = enabled;
        if !enabled {
            let version = st.version();
            st.gutter.value = Some((version, Arc::new(Vec::new())));
        } else {
            st.gutter.value = None;
        }
    }

    fn gutter_job(&self, st: &mut DocState, delay: Duration, fx: &mut Fx) {
        let version = st.version();
        if st.gutter.scheduled == Some(version) {
            return;
        }
        st.gutter.scheduled = Some(version);
        let weak = self.shared.me.clone();
        fx.debounce(
            self.key(JobKind::DocumentGutter),
            delay,
            Priority::Background,
            Box::new(move |cancel| {
                Self::with_weak(&weak, |h| h.compute_gutter(version, cancel));
            }),
        );
    }

    fn compute_gutter(&self, version: u64, cancel: &CancelToken) {
        let (doc, base, project) = {
            let mut st = self.shared.state.lock();
            if st.version() != version || cancel.is_cancelled() {
                if st.gutter.scheduled == Some(version) {
                    st.gutter.scheduled = None;
                }
                return;
            }
            (st.live.clone(), st.gutter_base.clone(), st.project.clone())
        };
        let base =
            base.unwrap_or_else(|| Arc::new(doc.text().and_then(|_| doc.gutter_base(&project))));
        let marks = match base.as_ref() {
            Some(b) => doc.gutter_against(b),
            None => Vec::new(),
        };
        let mut fx = Fx::default();
        {
            let mut st = self.shared.state.lock();
            st.gutter.computes += 1;
            st.gutter_base = Some(base);
            if st.gutter.scheduled == Some(version) {
                st.gutter.scheduled = None;
            }
            if st.version() == version {
                st.gutter.value = Some((version, Arc::new(marks)));
                fx.events.push(DocEvent::QueryReady {
                    kind: QueryKind::Gutter,
                    version,
                });
            }
        }
        self.run(fx);
    }

    /// Blame of the file on disk mapped onto the current text (edited lines
    /// are `uncommitted`). `None` value outside a repository.
    pub fn blame_map(&self) -> QueryResult<Arc<Vec<BlameHunk>>> {
        let mut fx = Fx::default();
        let r = {
            let mut st = self.shared.state.lock();
            let version = st.version();
            let current = st.blame.value.as_ref().is_some_and(|(v, _)| *v == version);
            if !current && st.blame.scheduled != Some(version) {
                st.blame.scheduled = Some(version);
                let weak = self.shared.me.clone();
                fx.debounce(
                    self.key(JobKind::DocumentBlame),
                    self.delays().blame,
                    Priority::Background,
                    Box::new(move |cancel| {
                        Self::with_weak(&weak, |h| h.compute_blame(version, cancel));
                    }),
                );
            }
            slot_result(&st.blame, version)
        };
        self.run(fx);
        r
    }

    fn compute_blame(&self, version: u64, cancel: &CancelToken) {
        let (project, path, text) = {
            let mut st = self.shared.state.lock();
            if st.version() != version || cancel.is_cancelled() {
                if st.blame.scheduled == Some(version) {
                    st.blame.scheduled = None;
                }
                return;
            }
            let text = match &st.buffer {
                Some(b) => b.text().to_string(),
                None => st.live.text().map(|t| t.to_string()).unwrap_or_default(),
            };
            (st.project.clone(), st.path.clone(), text)
        };
        let mapped = project.relative_path(&path).and_then(|rel| {
            let hunks = project.blame(&rel).ok()?;
            let disk = std::fs::read(&path).ok()?;
            let disk = derive::unix_lines(String::from_utf8_lossy(&disk).into_owned());
            let current = derive::unix_lines(text);
            if disk == current {
                return Some(hunks);
            }
            let diff = diff_lines(&disk, &current, &DiffOptions::default());
            Some(Arc::new(derive::remap_blame(&hunks, &diff)))
        });
        let mut fx = Fx::default();
        {
            let mut st = self.shared.state.lock();
            st.blame.computes += 1;
            if st.blame.scheduled == Some(version) {
                st.blame.scheduled = None;
            }
            if st.version() == version {
                st.blame.value = Some((version, mapped.unwrap_or_default()));
                fx.events.push(DocEvent::QueryReady {
                    kind: QueryKind::Blame,
                    version,
                });
            }
        }
        self.run(fx);
    }

    /// Highlights of `chunk` (rows `chunk * HIGHLIGHT_CHUNK ..`) for the
    /// current version, if computed.
    pub fn highlight_chunk(&self, chunk: u32) -> Option<Arc<Vec<LineHighlights>>> {
        let st = self.shared.state.lock();
        (st.highlights.0 == st.version())
            .then(|| st.highlights.1.get(&chunk).cloned())
            .flatten()
    }

    /// Every computed chunk of the current version.
    pub fn cached_highlights(&self) -> (u64, Vec<ChunkHighlights>) {
        let st = self.shared.state.lock();
        let version = st.version();
        if st.highlights.0 != version {
            return (version, Vec::new());
        }
        (
            version,
            st.highlights
                .1
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect(),
        )
    }

    /// Highlights of `rows` if every chunk is computed; otherwise schedules
    /// the missing chunks ([`DocEvent::HighlightsReady`]) and returns `None`.
    pub fn highlights(&self, rows: Range<u32>) -> Option<Vec<LineHighlights>> {
        if rows.is_empty() {
            return Some(Vec::new());
        }
        let chunks: Vec<u32> =
            (rows.start / HIGHLIGHT_CHUNK..=(rows.end - 1) / HIGHLIGHT_CHUNK).collect();
        let mut out = Vec::new();
        let mut missing = Vec::new();
        for &chunk in &chunks {
            match self.highlight_chunk(chunk) {
                Some(lines) => out.extend(lines.iter().filter(|l| rows.contains(&l.row)).cloned()),
                None => missing.push(chunk),
            }
        }
        if missing.is_empty() {
            return Some(out);
        }
        self.request_highlights(&missing);
        None
    }

    /// Computes `chunks` (in the given order: put the visible ones first) for
    /// the current version. Returns false when the document has no syntax
    /// (yet): request again after [`DocEvent::Reparsed`].
    pub fn request_highlights(&self, chunks: &[u32]) -> bool {
        let mut fx = Fx::default();
        let ok = {
            let st = self.shared.state.lock();
            let doc = st.live.clone();
            if doc.syntax().is_none() {
                false
            } else {
                let version = st.version();
                let work: Vec<u32> = chunks
                    .iter()
                    .copied()
                    .filter(|c| st.highlights.0 != version || !st.highlights.1.contains_key(c))
                    .collect();
                if !work.is_empty() {
                    let weak = self.shared.me.clone();
                    fx.spawn(
                        Priority::Visible,
                        self.key(JobKind::DocumentHighlight),
                        Box::new(move |_| {
                            let computed: Vec<(u32, Arc<Vec<LineHighlights>>)> = work
                                .into_iter()
                                .map(|chunk| {
                                    let rows =
                                        chunk * HIGHLIGHT_CHUNK..(chunk + 1) * HIGHLIGHT_CHUNK;
                                    (chunk, Arc::new(doc.line_highlights(rows)))
                                })
                                .collect();
                            Self::with_weak(&weak, |h| h.highlights_done(version, computed));
                        }),
                    );
                }
                true
            }
        };
        self.run(fx);
        ok
    }

    fn highlights_done(&self, version: u64, chunks: Vec<(u32, Arc<Vec<LineHighlights>>)>) {
        {
            let mut st = self.shared.state.lock();
            st.highlight_computes += chunks.len() as u64;
            if st.version() == version {
                if st.highlights.0 != version {
                    st.highlights = (version, HashMap::new());
                }
                for (chunk, lines) in &chunks {
                    st.highlights.1.insert(*chunk, lines.clone());
                }
            }
        }
        self.shared
            .subs
            .emit(vec![DocEvent::HighlightsReady { version, chunks }]);
    }

    /// How many times each query was computed (tests, diagnostics).
    pub fn compute_counts(&self) -> QueryCounts {
        let st = self.shared.state.lock();
        QueryCounts {
            structure: st.structure.computes,
            gutter: st.gutter.computes,
            blame: st.blame.computes,
            highlight_chunks: st.highlight_computes,
        }
    }

    // ----- diagnostics -----------------------------------------------------

    /// Installs published diagnostics: converted against the text they were
    /// computed for (its version is the LSP sync tag) and mapped through
    /// every later edit. `None` clears. Publishes for texts older than the
    /// history are ignored (the next publish replaces them).
    pub fn set_diagnostics(&self, published: Option<lector_lsp::DocumentDiagnostics>) {
        let changed = {
            let mut st = self.shared.state.lock();
            let entries = match published {
                None => Vec::new(),
                Some(p) => {
                    let Some(tag) = p.tag else { return };
                    let (text, later): (Rope, Vec<Arc<Transaction>>) = if st.version() == tag {
                        let Some(text) = self.current_rope(&st) else {
                            return;
                        };
                        (text, Vec::new())
                    } else {
                        let Some(ix) = st.history.iter().rposition(|e| e.version == tag) else {
                            return;
                        };
                        (
                            st.history[ix].text.clone(),
                            st.history
                                .iter()
                                .skip(ix + 1)
                                .map(|e| e.transaction.clone())
                                .collect(),
                        )
                    };
                    let mut entries = derive::convert_diagnostics(&text, p.diagnostics);
                    for e in &mut entries {
                        for tx in &later {
                            e.range = derive::map_range(&e.range, &tx.deltas);
                        }
                    }
                    entries.sort_by_key(|e| (e.range.start, e.severity));
                    entries
                }
            };
            if *st.diagnostics == entries {
                false
            } else {
                st.diagnostics = Arc::new(entries);
                true
            }
        };
        if changed {
            self.shared.subs.emit(vec![DocEvent::DiagnosticsChanged]);
        }
    }

    /// Diagnostics in the current text.
    pub fn diagnostics(&self) -> Arc<Vec<DocDiagnostic>> {
        self.shared.state.lock().diagnostics.clone()
    }

    fn current_rope(&self, st: &DocState) -> Option<Rope> {
        match &st.buffer {
            Some(b) => Some(b.text().clone()),
            None => st.live.text().and_then(|t| t.as_rope()).cloned(),
        }
    }

    // ----- LSP -------------------------------------------------------------

    pub(super) fn lsp_job(&self, st: &mut DocState, delay: Duration, fx: &mut Fx) {
        if st.project.lsp().is_none() {
            return;
        }
        let weak = self.shared.me.clone();
        fx.debounce(
            self.key(JobKind::DocumentLsp),
            delay,
            Priority::Background,
            Box::new(move |_| {
                Self::with_weak(&weak, |h| {
                    h.sync_lsp_now();
                })
            }),
        );
    }

    pub(super) fn schedule_lsp_sync(&self, delay: Duration) {
        let mut fx = Fx::default();
        {
            let mut st = self.shared.state.lock();
            self.lsp_job(&mut st, delay, &mut fx);
        }
        self.run(fx);
    }

    /// Sends the current text to the language server now (blocking-free:
    /// the manager queues it), tagged with the text version. Returns the
    /// version synced.
    pub fn sync_lsp_now(&self) -> Option<u64> {
        let (lsp, path, lang, text, version) = {
            let st = self.shared.state.lock();
            let lsp = st.project.lsp()?.clone();
            let lang = st.live.language()?.name.clone();
            let text = st.live.text()?;
            if text.is_mapped() || st.live.is_large() {
                return None;
            }
            if st.lsp_synced == Some(st.version()) {
                return st.lsp_synced;
            }
            (
                lsp,
                st.path.clone(),
                lang,
                self.current_rope(&st)?,
                st.version(),
            )
        };
        lsp.open_document_tagged(&path, &lang, &text.to_string(), version);
        self.shared.state.lock().lsp_synced = Some(version);
        Some(version)
    }
}
