//! A trigram index over the text of a file set, to avoid reading every file
//! for a project-wide text search.
//!
//! The index maps every (ASCII-case-folded) 3-byte sequence to the sorted list
//! of files containing it. A query is reduced to a boolean plan over trigrams
//! (AND for concatenations, OR for alternations; classes, repetitions that may
//! match nothing and anything else unknown fall back to "no constraint"), which
//! yields a **superset** of the files that can match. [`crate::search_project`]
//! style confirmation then runs the real regex engine on the candidates only.
//!
//! Files that can never match a search (binary files, files over 10 MB) are
//! recorded (for staleness checks) but are never returned as candidates, just
//! like [`crate::search_project`] skips them.
//!
//! The index is plain data: wrap it in a lock to share it. It supports
//! per-file update/removal (stale ids are filtered at query time and dropped
//! by [`TrigramIndex::compact`]) and a compact versioned serialization
//! ([`TrigramIndex::to_bytes`] / [`TrigramIndex::from_bytes`]) with varint,
//! delta-coded posting lists and a checksum.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;

use regex_syntax::hir::{Hir, HirKind};

use crate::{CancelToken, SearchQuery};

/// Files larger than this are not indexed (and never searched).
pub const MAX_INDEXED_FILE_SIZE: u64 = 10 * 1024 * 1024;
const FORMAT_MAGIC: &[u8; 4] = b"LTRI";
/// Current serialization version.
pub const FORMAT_VERSION: u32 = 1;
const BATCH: usize = 256;

/// Errors of the trigram index.
#[derive(Debug, thiserror::Error)]
pub enum TrigramError {
    /// The bytes do not start with the index magic.
    #[error("not a trigram index")]
    BadMagic,
    /// Written by an incompatible version.
    #[error("unsupported trigram index version {0}")]
    Version(u32),
    /// Truncated or inconsistent data.
    #[error("corrupt trigram index: {0}")]
    Corrupt(&'static str),
    /// The query is not a valid regular expression.
    #[error("invalid pattern: {0}")]
    Pattern(String),
}

/// Files that may contain a match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Candidates {
    /// The query could not be narrowed (or the index is not applicable):
    /// search every searchable file.
    All,
    /// Exactly these paths (sorted) must be searched.
    Files(Vec<String>),
}

impl Candidates {
    /// Number of candidate files (`None` for `All`).
    pub fn len(&self) -> Option<usize> {
        match self {
            Candidates::All => None,
            Candidates::Files(f) => Some(f.len()),
        }
    }

    /// True for `Files` with no entries.
    pub fn is_empty(&self) -> bool {
        matches!(self, Candidates::Files(f) if f.is_empty())
    }
}

/// What [`TrigramIndex::update_file`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// Size and modification time are unchanged.
    Unchanged,
    /// The file was (re)indexed.
    Indexed,
    /// The file no longer exists and was dropped from the index.
    Removed,
}

/// Counts reported by [`TrigramIndex::refresh`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefreshStats {
    /// Files new to the index.
    pub added: usize,
    /// Files whose size or modification time changed.
    pub updated: usize,
    /// Files dropped (no longer in the list or unreadable).
    pub removed: usize,
    /// Files left as they were.
    pub unchanged: usize,
}

#[derive(Default)]
struct TriHasher(u64);
impl Hasher for TriHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(0x517cc1b727220a95);
        }
    }
    fn write_u32(&mut self, i: u32) {
        self.0 = (i as u64).wrapping_mul(0x9E3779B97F4A7C15);
    }
}
type TriMap<V> = HashMap<u32, V, BuildHasherDefault<TriHasher>>;

#[derive(Debug, Clone)]
struct FileEntry {
    path: String,
    size: u64,
    mtime_ns: u64,
    /// Indexed (not binary / oversized): may be a candidate.
    searchable: bool,
}

/// Index of the trigrams of a set of files. See the module docs.
#[derive(Debug, Default)]
pub struct TrigramIndex {
    files: Vec<Option<FileEntry>>,
    by_path: HashMap<String, u32>,
    postings: TriMap<Vec<u32>>,
    /// Ids that were removed but may still be in posting lists.
    stale: usize,
}

struct Computed {
    path: String,
    size: u64,
    mtime_ns: u64,
    searchable: bool,
    trigrams: Vec<u32>,
}

#[inline]
fn tri(a: u8, b: u8, c: u8) -> u32 {
    (a.to_ascii_lowercase() as u32) << 16
        | (b.to_ascii_lowercase() as u32) << 8
        | c.to_ascii_lowercase() as u32
}

/// Sorted, deduplicated trigram codes of `bytes`.
fn trigrams_of(bytes: &[u8]) -> Vec<u32> {
    let mut v: Vec<u32> = bytes.windows(3).map(|w| tri(w[0], w[1], w[2])).collect();
    v.sort_unstable();
    v.dedup();
    v
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8000)].contains(&0)
}

fn compute_bytes(path: String, bytes: &[u8], size: u64, mtime_ns: u64) -> Computed {
    let searchable = size <= MAX_INDEXED_FILE_SIZE && !is_binary(bytes);
    Computed {
        path,
        size,
        mtime_ns,
        searchable,
        trigrams: if searchable {
            trigrams_of(bytes)
        } else {
            Vec::new()
        },
    }
}

fn mtime_ns(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn compute_file(root: &Path, rel: &str) -> Option<Computed> {
    let full = root.join(rel);
    let meta = std::fs::metadata(&full).ok()?;
    if !meta.is_file() {
        return None;
    }
    let size = meta.len();
    let mtime = mtime_ns(&meta);
    if size > MAX_INDEXED_FILE_SIZE {
        return Some(compute_bytes(rel.to_string(), &[], size, mtime).with_unsearchable());
    }
    let mut bytes = Vec::with_capacity(size as usize);
    std::fs::File::open(&full)
        .ok()?
        .take(MAX_INDEXED_FILE_SIZE + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(compute_bytes(rel.to_string(), &bytes, size, mtime))
}

impl Computed {
    fn with_unsearchable(mut self) -> Self {
        self.searchable = false;
        self.trigrams.clear();
        self
    }
}

/// Computes `rels` on all cores; the result is in input order.
fn compute_batch(root: &Path, rels: &[String], cancel: &CancelToken) -> Vec<Option<Computed>> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .min(8)
        .min(rels.len().max(1));
    let next = AtomicUsize::new(0);
    let mut slots: Vec<Option<Computed>> = Vec::new();
    slots.resize_with(rels.len(), || None);
    let results: Vec<Vec<(usize, Option<Computed>)>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut local = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= rels.len() || cancel.is_cancelled() {
                            break;
                        }
                        local.push((i, compute_file(root, &rels[i])));
                    }
                    local
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_default())
            .collect()
    });
    for (i, c) in results.into_iter().flatten() {
        slots[i] = c;
    }
    slots
}

fn rel_string(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

impl TrigramIndex {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds an index over `files` (paths relative to `root`), reading and
    /// indexing them on all cores. Stops early when `cancel` fires (the
    /// partial index is returned).
    pub fn build(root: &Path, files: &[PathBuf], cancel: &CancelToken) -> TrigramIndex {
        let mut index = TrigramIndex::new();
        let rels: Vec<String> = files.iter().map(|p| rel_string(p)).collect();
        for chunk in rels.chunks(BATCH) {
            if cancel.is_cancelled() {
                break;
            }
            for c in compute_batch(root, chunk, cancel).into_iter().flatten() {
                index.insert(c);
            }
        }
        index
    }

    /// Builds an index from in-memory contents (`path`, `bytes`); sequential.
    pub fn from_contents(items: impl IntoIterator<Item = (String, Vec<u8>)>) -> TrigramIndex {
        let mut index = TrigramIndex::new();
        for (path, bytes) in items {
            index.update_bytes(&path, &bytes, 0);
        }
        index
    }

    /// Indexes (or re-indexes) `path` from in-memory `bytes`; `mtime_ns` is
    /// stored for staleness checks.
    pub fn update_bytes(&mut self, path: &str, bytes: &[u8], mtime_ns: u64) {
        self.insert(compute_bytes(
            path.to_string(),
            bytes,
            bytes.len() as u64,
            mtime_ns,
        ));
    }

    /// Re-reads `rel` under `root` if its size or modification time changed.
    pub fn update_file(&mut self, root: &Path, rel: &Path) -> std::io::Result<UpdateOutcome> {
        let rel = rel_string(rel);
        let meta = match std::fs::metadata(root.join(&rel)) {
            Ok(m) if m.is_file() => m,
            Ok(_) => {
                return Ok(if self.remove_file(&rel) {
                    UpdateOutcome::Removed
                } else {
                    UpdateOutcome::Unchanged
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(if self.remove_file(&rel) {
                    UpdateOutcome::Removed
                } else {
                    UpdateOutcome::Unchanged
                })
            }
            Err(e) => return Err(e),
        };
        if self.is_current(&rel, &meta) {
            return Ok(UpdateOutcome::Unchanged);
        }
        match compute_file(root, &rel) {
            Some(c) => {
                self.insert(c);
                Ok(UpdateOutcome::Indexed)
            }
            None => Ok(if self.remove_file(&rel) {
                UpdateOutcome::Removed
            } else {
                UpdateOutcome::Unchanged
            }),
        }
    }

    /// Brings the index in line with the current file list: drops files that
    /// are gone, (re)indexes new and modified ones (size / mtime), in parallel.
    pub fn refresh(
        &mut self,
        root: &Path,
        files: &[PathBuf],
        cancel: &CancelToken,
    ) -> RefreshStats {
        let mut stats = RefreshStats::default();
        let wanted: Vec<String> = files.iter().map(|p| rel_string(p)).collect();
        let keep: std::collections::HashSet<&str> = wanted.iter().map(String::as_str).collect();
        let gone: Vec<String> = self
            .by_path
            .keys()
            .filter(|p| !keep.contains(p.as_str()))
            .cloned()
            .collect();
        for p in gone {
            if self.remove_file(&p) {
                stats.removed += 1;
            }
        }
        let mut todo = Vec::new();
        for rel in &wanted {
            match std::fs::metadata(root.join(rel)) {
                Ok(m) if m.is_file() => {
                    if self.is_current(rel, &m) {
                        stats.unchanged += 1;
                    } else {
                        todo.push(rel.clone());
                    }
                }
                _ => {
                    if self.remove_file(rel) {
                        stats.removed += 1;
                    }
                }
            }
        }
        for chunk in todo.chunks(BATCH) {
            if cancel.is_cancelled() {
                break;
            }
            for (rel, c) in chunk.iter().zip(compute_batch(root, chunk, cancel)) {
                match c {
                    Some(c) => {
                        if self.by_path.contains_key(rel) {
                            stats.updated += 1;
                        } else {
                            stats.added += 1;
                        }
                        self.insert(c);
                    }
                    None => {
                        if self.remove_file(rel) {
                            stats.removed += 1;
                        }
                    }
                }
            }
        }
        stats
    }

    fn is_current(&self, rel: &str, meta: &std::fs::Metadata) -> bool {
        self.by_path
            .get(rel)
            .and_then(|&id| self.files[id as usize].as_ref())
            .is_some_and(|e| e.size == meta.len() && e.mtime_ns == mtime_ns(meta))
    }

    fn insert(&mut self, c: Computed) {
        self.remove_file(&c.path);
        let id = self.files.len() as u32;
        for t in &c.trigrams {
            self.postings.entry(*t).or_default().push(id);
        }
        self.by_path.insert(c.path.clone(), id);
        self.files.push(Some(FileEntry {
            path: c.path,
            size: c.size,
            mtime_ns: c.mtime_ns,
            searchable: c.searchable,
        }));
    }

    /// Removes `path`; returns whether it was indexed. Stale posting entries
    /// are dropped lazily (see [`compact`](Self::compact)).
    pub fn remove_file(&mut self, path: &str) -> bool {
        let path = path.replace('\\', "/");
        let Some(id) = self.by_path.remove(&path) else {
            return false;
        };
        self.files[id as usize] = None;
        self.stale += 1;
        if self.stale > 64 && self.stale * 4 > self.files.len() {
            self.compact();
        }
        true
    }

    /// Drops stale ids from the posting lists and empty postings.
    pub fn compact(&mut self) {
        if self.stale == 0 {
            return;
        }
        let files = &self.files;
        self.postings.retain(|_, ids| {
            ids.retain(|&id| files[id as usize].is_some());
            !ids.is_empty()
        });
        self.stale = 0;
    }

    /// Number of indexed files (including unsearchable ones).
    pub fn file_count(&self) -> usize {
        self.by_path.len()
    }

    /// Number of files that can be candidates.
    pub fn searchable_count(&self) -> usize {
        self.files.iter().flatten().filter(|e| e.searchable).count()
    }

    /// Number of distinct trigrams.
    pub fn trigram_count(&self) -> usize {
        self.postings.len()
    }

    /// Whether `path` is in the index.
    pub fn contains(&self, path: &str) -> bool {
        self.by_path.contains_key(path)
    }

    /// Size (bytes) and modification time (ns) recorded for `path`.
    pub fn stamp(&self, path: &str) -> Option<(u64, u64)> {
        let id = *self.by_path.get(path)?;
        self.files[id as usize]
            .as_ref()
            .map(|e| (e.size, e.mtime_ns))
    }

    // ------------------------------------------------------------------
    // Queries

    /// Sorted paths of every file that can be a candidate (indexed text files).
    pub fn searchable_paths(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .files
            .iter()
            .flatten()
            .filter(|e| e.searchable)
            .map(|e| e.path.clone())
            .collect();
        v.sort();
        v
    }

    fn paths_of(&self, mut ids: Vec<u32>) -> Candidates {
        ids.retain(|&id| {
            self.files[id as usize]
                .as_ref()
                .is_some_and(|e| e.searchable)
        });
        let mut paths: Vec<String> = ids
            .into_iter()
            .filter_map(|id| self.files[id as usize].as_ref().map(|e| e.path.clone()))
            .collect();
        paths.sort();
        Candidates::Files(paths)
    }

    /// Candidates for a literal string (case-insensitive superset).
    pub fn candidates_literal(&self, literal: &str) -> Candidates {
        self.run(&literal_plan(literal.as_bytes()))
    }

    /// Candidates for a regex pattern. Alternations are OR-ed, concatenations
    /// AND-ed, everything unknown (classes, `*`, `?`, look-around, inline
    /// flags) imposes no constraint.
    pub fn candidates_regex(&self, pattern: &str) -> Result<Candidates, TrigramError> {
        let hir = regex_syntax::Parser::new()
            .parse(pattern)
            .map_err(|e| TrigramError::Pattern(e.to_string()))?;
        Ok(self.run(&plan_of(&hir)))
    }

    /// Candidates for a [`SearchQuery`] (`regex` flag, literal otherwise).
    pub fn candidates(&self, query: &SearchQuery) -> Result<Candidates, TrigramError> {
        if query.regex {
            self.candidates_regex(&query.pattern)
        } else {
            Ok(self.candidates_literal(&query.pattern))
        }
    }

    fn run(&self, plan: &Plan) -> Candidates {
        match self.eval(plan) {
            Set::All => Candidates::All,
            Set::Slice(s) => self.paths_of(s.to_vec()),
            Set::Owned(v) => self.paths_of(v),
        }
    }

    fn eval<'a>(&'a self, plan: &Plan) -> Set<'a> {
        match plan {
            Plan::All => Set::All,
            Plan::Tri(t) => match self.postings.get(t) {
                Some(v) => Set::Slice(v),
                None => Set::Owned(Vec::new()),
            },
            Plan::And(children) => {
                let mut sets: Vec<Set<'a>> = children
                    .iter()
                    .map(|c| self.eval(c))
                    .filter(|s| !matches!(s, Set::All))
                    .collect();
                if sets.is_empty() {
                    return Set::All;
                }
                sets.sort_by_key(|s| s.len());
                let mut it = sets.into_iter();
                let mut acc = it.next().unwrap_or(Set::All);
                for s in it {
                    if acc.len() == 0 {
                        break;
                    }
                    acc = Set::Owned(intersect(acc.as_slice(), s.as_slice()));
                }
                acc
            }
            Plan::Or(children) => {
                let mut all = Vec::new();
                for c in children {
                    match self.eval(c) {
                        Set::All => return Set::All,
                        s => all.extend_from_slice(s.as_slice()),
                    }
                }
                all.sort_unstable();
                all.dedup();
                Set::Owned(all)
            }
        }
    }

    // ------------------------------------------------------------------
    // Serialization

    /// Serializes the live part of the index (ids are renumbered densely).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut remap = vec![u32::MAX; self.files.len()];
        let mut live = 0u32;
        for (i, f) in self.files.iter().enumerate() {
            if f.is_some() {
                remap[i] = live;
                live += 1;
            }
        }
        let mut out = Vec::with_capacity(64 + self.postings.len() * 8);
        out.extend_from_slice(FORMAT_MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        put_varint(&mut out, live as u64);
        for e in self.files.iter().flatten() {
            put_varint(&mut out, e.path.len() as u64);
            out.extend_from_slice(e.path.as_bytes());
            put_varint(&mut out, e.size);
            put_varint(&mut out, e.mtime_ns);
            out.push(u8::from(e.searchable));
        }
        let mut keys: Vec<u32> = self.postings.keys().copied().collect();
        keys.sort_unstable();
        // Count postings that survive.
        let mut body = Vec::new();
        let mut n_postings = 0u64;
        let mut prev_key = 0u32;
        let mut ids: Vec<u32> = Vec::new();
        for k in keys {
            ids.clear();
            ids.extend(
                self.postings[&k]
                    .iter()
                    .map(|&id| remap[id as usize])
                    .filter(|&id| id != u32::MAX),
            );
            if ids.is_empty() {
                continue;
            }
            put_varint(&mut body, (k - prev_key) as u64);
            prev_key = k;
            put_varint(&mut body, ids.len() as u64);
            let mut prev = 0u32;
            for (i, &id) in ids.iter().enumerate() {
                put_varint(&mut body, if i == 0 { id } else { id - prev } as u64);
                prev = id;
            }
            n_postings += 1;
        }
        put_varint(&mut out, n_postings);
        out.extend_from_slice(&body);
        let sum = checksum(&out);
        out.extend_from_slice(&sum.to_le_bytes());
        out
    }

    /// Reads an index written by [`to_bytes`](Self::to_bytes). Never panics on
    /// malformed input.
    pub fn from_bytes(bytes: &[u8]) -> Result<TrigramIndex, TrigramError> {
        if bytes.len() < 4 || &bytes[..4] != FORMAT_MAGIC {
            return Err(TrigramError::BadMagic);
        }
        if bytes.len() < 16 {
            return Err(TrigramError::Corrupt("truncated header"));
        }
        let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if version != FORMAT_VERSION {
            return Err(TrigramError::Version(version));
        }
        let (body, tail) = bytes.split_at(bytes.len() - 8);
        let mut sum = [0u8; 8];
        sum.copy_from_slice(tail);
        if u64::from_le_bytes(sum) != checksum(body) {
            return Err(TrigramError::Corrupt("checksum mismatch"));
        }
        let mut r = Reader { b: body, at: 8 };
        let n_files = r.varint()? as usize;
        if n_files > r.remaining() {
            return Err(TrigramError::Corrupt("file count"));
        }
        let mut index = TrigramIndex::new();
        for _ in 0..n_files {
            let len = r.varint()? as usize;
            let path = std::str::from_utf8(r.take(len)?)
                .map_err(|_| TrigramError::Corrupt("path is not utf-8"))?
                .to_string();
            let size = r.varint()?;
            let mtime_ns = r.varint()?;
            let flags = r.take(1)?[0];
            let id = index.files.len() as u32;
            if index.by_path.insert(path.clone(), id).is_some() {
                return Err(TrigramError::Corrupt("duplicate path"));
            }
            index.files.push(Some(FileEntry {
                path,
                size,
                mtime_ns,
                searchable: flags & 1 == 1,
            }));
        }
        let n_postings = r.varint()? as usize;
        if n_postings > r.remaining() {
            return Err(TrigramError::Corrupt("posting count"));
        }
        let mut key = 0u64;
        for i in 0..n_postings {
            let delta = r.varint()?;
            key = key
                .checked_add(delta)
                .filter(|k| *k <= 0xFF_FFFF)
                .ok_or(TrigramError::Corrupt("trigram key"))?;
            if i > 0 && delta == 0 {
                return Err(TrigramError::Corrupt("unsorted trigrams"));
            }
            let count = r.varint()? as usize;
            if count == 0 || count > r.remaining() || count > n_files {
                return Err(TrigramError::Corrupt("posting length"));
            }
            let mut ids = Vec::with_capacity(count);
            let mut prev = 0u64;
            for j in 0..count {
                let d = r.varint()?;
                if j > 0 && d == 0 {
                    return Err(TrigramError::Corrupt("unsorted posting"));
                }
                prev = prev
                    .checked_add(d)
                    .filter(|id| (*id as usize) < n_files)
                    .ok_or(TrigramError::Corrupt("file id out of range"))?;
                ids.push(prev as u32);
            }
            index.postings.insert(key as u32, ids);
        }
        if r.remaining() != 0 {
            return Err(TrigramError::Corrupt("trailing data"));
        }
        Ok(index)
    }
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.b.len() - self.at
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], TrigramError> {
        if n > self.remaining() {
            return Err(TrigramError::Corrupt("unexpected end"));
        }
        let s = &self.b[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }
    fn varint(&mut self) -> Result<u64, TrigramError> {
        let mut v = 0u64;
        for shift in (0..70).step_by(7) {
            let byte = self.take(1)?[0];
            if shift == 63 && byte > 1 {
                return Err(TrigramError::Corrupt("varint overflow"));
            }
            v |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(TrigramError::Corrupt("varint too long"))
    }
}

// ---------------------------------------------------------------------------
// Query plans

enum Set<'a> {
    All,
    Slice(&'a [u32]),
    Owned(Vec<u32>),
}

impl Set<'_> {
    fn len(&self) -> usize {
        match self {
            Set::All => usize::MAX,
            Set::Slice(s) => s.len(),
            Set::Owned(v) => v.len(),
        }
    }
    fn as_slice(&self) -> &[u32] {
        match self {
            Set::All => &[],
            Set::Slice(s) => s,
            Set::Owned(v) => v,
        }
    }
}

/// Intersection of two sorted lists (galloping when the sizes differ a lot).
fn intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::with_capacity(small.len());
    if small.len() * 16 < large.len() {
        let mut lo = 0;
        for &x in small {
            match large[lo..].binary_search(&x) {
                Ok(i) => {
                    out.push(x);
                    lo += i + 1;
                }
                Err(i) => lo += i,
            }
            if lo >= large.len() {
                break;
            }
        }
    } else {
        let (mut i, mut j) = (0, 0);
        while i < small.len() && j < large.len() {
            match small[i].cmp(&large[j]) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    out.push(small[i]);
                    i += 1;
                    j += 1;
                }
            }
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Plan {
    All,
    Tri(u32),
    And(Vec<Plan>),
    Or(Vec<Plan>),
}

fn and(parts: Vec<Plan>) -> Plan {
    let mut tris: Vec<u32> = Vec::new();
    let mut others: Vec<Plan> = Vec::new();
    let add = |p: Plan, tris: &mut Vec<u32>, others: &mut Vec<Plan>| match p {
        Plan::All => {}
        Plan::Tri(t) => tris.push(t),
        o => others.push(o),
    };
    for p in parts {
        match p {
            Plan::And(v) => {
                for q in v {
                    add(q, &mut tris, &mut others);
                }
            }
            other => add(other, &mut tris, &mut others),
        }
    }
    tris.sort_unstable();
    tris.dedup();
    let mut flat: Vec<Plan> = tris.into_iter().map(Plan::Tri).collect();
    flat.extend(others);
    match flat.len() {
        0 => Plan::All,
        1 => flat.remove(0),
        _ => Plan::And(flat),
    }
}

fn or(parts: Vec<Plan>) -> Plan {
    let mut flat = Vec::new();
    for p in parts {
        match p {
            Plan::All => return Plan::All,
            Plan::Or(v) => flat.extend(v),
            other => flat.push(other),
        }
    }
    flat.dedup();
    match flat.len() {
        0 => Plan::All,
        1 => flat.remove(0),
        _ => Plan::Or(flat),
    }
}

/// AND of the trigrams of the ASCII runs of `lit` (non-ASCII bytes break runs:
/// the index only folds ASCII case).
fn literal_plan(lit: &[u8]) -> Plan {
    let mut parts = Vec::new();
    for run in lit.split(|b| !b.is_ascii()) {
        for w in run.windows(3) {
            parts.push(Plan::Tri(tri(w[0], w[1], w[2])));
        }
    }
    and(parts)
}

fn plan_of(hir: &Hir) -> Plan {
    match hir.kind() {
        HirKind::Literal(l) => literal_plan(&l.0),
        HirKind::Concat(v) => and(v.iter().map(plan_of).collect()),
        HirKind::Alternation(v) => or(v.iter().map(plan_of).collect()),
        HirKind::Capture(c) => plan_of(&c.sub),
        HirKind::Repetition(r) if r.min >= 1 => plan_of(&r.sub),
        HirKind::Empty | HirKind::Class(_) | HirKind::Look(_) | HirKind::Repetition(_) => Plan::All,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn index(files: &[(&str, &str)]) -> TrigramIndex {
        TrigramIndex::from_contents(
            files
                .iter()
                .map(|(p, c)| (p.to_string(), c.as_bytes().to_vec())),
        )
    }

    fn files(c: Candidates) -> Vec<String> {
        match c {
            Candidates::Files(f) => f,
            Candidates::All => panic!("expected files"),
        }
    }

    fn corpus() -> TrigramIndex {
        index(&[
            ("a.rs", "fn main() { println!(\"Hello world\"); }"),
            ("b.rs", "struct Hello { name: String }"),
            ("c.txt", "nothing relevant here"),
            ("d.py", "def hello_world():\n    return 'HELLO'\n"),
            ("e.txt", "ab"),
        ])
    }

    #[test]
    fn literal_candidates_are_case_insensitive_supersets() {
        let idx = corpus();
        assert_eq!(
            files(idx.candidates_literal("Hello")),
            vec!["a.rs", "b.rs", "d.py"]
        );
        assert_eq!(files(idx.candidates_literal("hello_world")), vec!["d.py"]);
        assert_eq!(files(idx.candidates_literal("zzzzz")), Vec::<String>::new());
        // Shorter than a trigram: cannot narrow.
        assert_eq!(idx.candidates_literal("he"), Candidates::All);
        assert_eq!(idx.candidates_literal(""), Candidates::All);
    }

    #[test]
    fn regex_plans() {
        let idx = corpus();
        assert_eq!(
            files(idx.candidates_regex("Hello.*world").unwrap()),
            vec!["a.rs", "d.py"]
        );
        // alternation -> OR
        assert_eq!(
            files(idx.candidates_regex("struct|def ").unwrap()),
            vec!["b.rs", "d.py"]
        );
        // a class cannot be narrowed, but literals around it can
        assert_eq!(
            files(idx.candidates_regex("def\\s+hello").unwrap()),
            vec!["d.py"]
        );
        // nothing to extract
        assert_eq!(idx.candidates_regex("[a-z]+").unwrap(), Candidates::All);
        assert_eq!(idx.candidates_regex(".*").unwrap(), Candidates::All);
        assert_eq!(idx.candidates_regex("(?i)hello").unwrap(), Candidates::All);
        // optional parts impose nothing
        assert_eq!(
            files(idx.candidates_regex("nothing( relevant)?").unwrap()),
            vec!["c.txt"]
        );
        // an alternative without literals widens the whole alternation
        assert_eq!(
            idx.candidates_regex("struct|[0-9]+").unwrap(),
            Candidates::All
        );
        assert!(idx.candidates_regex("(unclosed").is_err());
    }

    #[test]
    fn non_ascii_is_conservative() {
        let idx = index(&[
            ("a.txt", "ÉCOLE normale"),
            ("b.txt", "école normale"),
            ("c.txt", "x"),
        ]);
        // Non-ASCII chars never produce trigrams, so both files stay candidates.
        let c = files(idx.candidates_literal("école normale"));
        assert_eq!(c, vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn binary_and_tiny_files() {
        let mut idx = TrigramIndex::new();
        idx.update_bytes("bin.dat", b"hello\0world", 0);
        idx.update_bytes("t.txt", b"hello", 0);
        assert!(idx.contains("bin.dat"));
        assert_eq!(files(idx.candidates_literal("hello")), vec!["t.txt"]);
        assert_eq!(idx.searchable_count(), 1);
    }

    #[test]
    fn update_and_remove() {
        let mut idx = corpus();
        idx.update_bytes("c.txt", b"now it says hello", 1);
        assert_eq!(
            files(idx.candidates_literal("hello")),
            vec!["a.rs", "b.rs", "c.txt", "d.py"]
        );
        assert_eq!(
            files(idx.candidates_literal("relevant")),
            Vec::<String>::new()
        );
        assert!(idx.remove_file("a.rs"));
        assert!(!idx.remove_file("a.rs"));
        assert_eq!(
            files(idx.candidates_literal("println")),
            Vec::<String>::new()
        );
        idx.compact();
        assert_eq!(idx.file_count(), 4);
        assert_eq!(
            files(idx.candidates_literal("hello")),
            vec!["b.rs", "c.txt", "d.py"]
        );
    }

    #[test]
    fn serialization_roundtrip_and_validation() {
        let mut idx = corpus();
        idx.remove_file("b.rs");
        let bytes = idx.to_bytes();
        let back = TrigramIndex::from_bytes(&bytes).unwrap();
        assert_eq!(back.file_count(), 4);
        for q in ["hello", "world", "struct", "nothing", "zzz"] {
            assert_eq!(idx.candidates_literal(q), back.candidates_literal(q), "{q}");
        }
        assert_eq!(back.stamp("c.txt"), idx.stamp("c.txt"));
        // Version and corruption are rejected without panicking.
        assert!(matches!(
            TrigramIndex::from_bytes(b"nope"),
            Err(TrigramError::BadMagic)
        ));
        let mut v = bytes.clone();
        v[4] = 9;
        assert!(matches!(
            TrigramIndex::from_bytes(&v),
            Err(TrigramError::Version(9))
        ));
        let mut bad = bytes.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 0xff;
        assert!(TrigramIndex::from_bytes(&bad).is_err());
        for cut in 0..bytes.len().min(40) {
            assert!(TrigramIndex::from_bytes(&bytes[..cut]).is_err());
        }
    }

    #[test]
    fn varint_roundtrip() {
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut b = Vec::new();
            put_varint(&mut b, v);
            assert_eq!(Reader { b: &b, at: 0 }.varint().unwrap(), v);
        }
    }

    #[test]
    fn build_refresh_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn alpha_function() {}").unwrap();
        std::fs::write(root.join("b.txt"), "beta text").unwrap();
        std::fs::write(root.join("bin"), b"\0\0\0binary alpha_function").unwrap();
        let list: Vec<PathBuf> = ["src/a.rs", "b.txt", "bin"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let cancel = CancelToken::new();
        let mut idx = TrigramIndex::build(root, &list, &cancel);
        assert_eq!(idx.file_count(), 3);
        assert_eq!(
            files(idx.candidates_literal("alpha_function")),
            vec!["src/a.rs"]
        );

        // Unchanged refresh is a no-op.
        let s = idx.refresh(root, &list, &cancel);
        assert_eq!(
            s,
            RefreshStats {
                unchanged: 3,
                ..Default::default()
            }
        );

        // Modify, add, remove.
        std::fs::write(root.join("b.txt"), "beta text with alpha_function inside!").unwrap();
        std::fs::write(root.join("new.txt"), "brand new").unwrap();
        let list2: Vec<PathBuf> = ["src/a.rs", "b.txt", "new.txt"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let s = idx.refresh(root, &list2, &cancel);
        assert_eq!((s.added, s.updated, s.removed), (1, 1, 1), "{s:?}");
        assert_eq!(
            files(idx.candidates_literal("alpha_function")),
            vec!["b.txt", "src/a.rs"]
        );
        assert_eq!(files(idx.candidates_literal("brand new")), vec!["new.txt"]);

        assert_eq!(
            idx.update_file(root, Path::new("new.txt")).unwrap(),
            UpdateOutcome::Unchanged
        );
        std::fs::remove_file(root.join("new.txt")).unwrap();
        assert_eq!(
            idx.update_file(root, Path::new("new.txt")).unwrap(),
            UpdateOutcome::Removed
        );
        assert!(!idx.contains("new.txt"));
    }

    fn arb_content() -> impl Strategy<Value = String> {
        proptest::collection::vec("[a-d ]{0,12}", 0..8).prop_map(|v| v.join("\n"))
    }

    proptest! {
        /// Every file that matches a literal is among its candidates.
        #[test]
        fn literal_candidates_are_a_superset(
            contents in proptest::collection::vec(arb_content(), 1..8),
            needle in "[a-dA-D ]{1,8}",
        ) {
            let items: Vec<(String, Vec<u8>)> = contents
                .iter()
                .enumerate()
                .map(|(i, c)| (format!("f{i}.txt"), c.as_bytes().to_vec()))
                .collect();
            let idx = TrigramIndex::from_contents(items.clone());
            let cands = idx.candidates_literal(&needle);
            let lower = needle.to_lowercase();
            for (path, bytes) in &items {
                let text = String::from_utf8_lossy(bytes).to_lowercase();
                if text.contains(&lower) {
                    match &cands {
                        Candidates::All => {}
                        Candidates::Files(f) => prop_assert!(f.contains(path), "{path} missing for {needle:?}"),
                    }
                }
            }
        }

        #[test]
        fn regex_candidates_are_a_superset(
            contents in proptest::collection::vec(arb_content(), 1..8),
            a in "[a-d]{1,5}",
            b in "[a-d]{1,5}",
            op in 0usize..4,
        ) {
            let pattern = match op {
                0 => format!("{a}.*{b}"),
                1 => format!("{a}|{b}"),
                2 => format!("({a}|{b})[a-d]+{a}"),
                _ => format!("{a}(x)?{b}"),
            };
            let re = regex::RegexBuilder::new(&pattern).case_insensitive(true).multi_line(true).build().unwrap();
            let items: Vec<(String, Vec<u8>)> = contents
                .iter()
                .enumerate()
                .map(|(i, c)| (format!("f{i}.txt"), c.as_bytes().to_vec()))
                .collect();
            let idx = TrigramIndex::from_contents(items.clone());
            let cands = idx.candidates_regex(&pattern).unwrap();
            for (path, bytes) in &items {
                if re.is_match(&String::from_utf8_lossy(bytes)) {
                    if let Candidates::Files(f) = &cands {
                        prop_assert!(f.contains(path), "{path} missing for /{pattern}/");
                    }
                }
            }
        }

        #[test]
        fn serialization_roundtrips(contents in proptest::collection::vec(arb_content(), 0..10), needle in "[a-d]{3,6}") {
            let idx = TrigramIndex::from_contents(
                contents.iter().enumerate().map(|(i, c)| (format!("f{i}"), c.as_bytes().to_vec())),
            );
            let back = TrigramIndex::from_bytes(&idx.to_bytes()).unwrap();
            prop_assert_eq!(idx.candidates_literal(&needle), back.candidates_literal(&needle));
            prop_assert_eq!(idx.file_count(), back.file_count());
        }

        #[test]
        fn from_bytes_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
            let _ = TrigramIndex::from_bytes(&bytes);
            let mut with_magic = FORMAT_MAGIC.to_vec();
            with_magic.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
            with_magic.extend(bytes);
            let _ = TrigramIndex::from_bytes(&with_magic);
        }
    }
}
