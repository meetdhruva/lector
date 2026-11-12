//! Streaming fuzzy picker (nucleo scoring) built for huge candidate sets.
//!
//! nucleo's own `Nucleo` publishes a snapshot only after sorting *every*
//! match, which costs hundreds of milliseconds per keystroke on a million
//! paths. This picker keeps nucleo's `Pattern`/`Matcher` (identical scores and
//! highlight indices) but replaces everything around it:
//!
//! * **Arena storage** ([`crate::picker_store`]): haystacks live in contiguous
//!   per-chunk byte arenas next to dense side arrays (end offsets, a 128-bit
//!   character mask per path, boosts). About 22 bytes of overhead per path
//!   besides the text and the item itself.
//! * **Cheap rejection before scoring**: a path that lacks any query character
//!   fails one AND on its mask; for plain fuzzy queries an exact subsequence
//!   test (SIMD `memchr`) follows, so the match count is exact without
//!   running nucleo's scorer.
//! * **Score bound**: from the path's "characters at word boundaries" mask an
//!   upper bound of nucleo's score is derived ([`crate::picker_plan`]); a
//!   candidate whose bound cannot beat the current k-th best (score, shorter
//!   path, lower index) is never scored. With a bounded top-k heap this means
//!   short queries on huge sets only run the expensive scorer on the few
//!   candidates that can still enter the top [`MAX_RESULTS`].
//! * **Progressive publication**: results are published after small first
//!   batches (a few milliseconds) and then at most every ~12 ms, so the first
//!   results appear almost immediately while the full pass continues. All
//!   cores are used inside a batch and a new keystroke cancels the running
//!   pass at the next chunk.
//! * **Narrowing**: when the query extends an earlier completed query, only
//!   that query's matches are re-examined (the complete match sets of recent
//!   queries are kept, so backspacing is as cheap as typing).
//! * **Streaming items**: items injected while (or after) matching are
//!   examined incrementally; nothing is rescanned.
//!
//! `matches(range)` can only return the best [`MAX_RESULTS`] matches;
//! `matched_count` is exact once matching is complete.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str, Utf32String};
use parking_lot::Mutex;

use crate::picker_plan::{Mode, Plan};
use crate::picker_store::{Chunk, Store, NON_ASCII};

/// Number of best matches that are retained, ordered and available through
/// [`Picker::matches`].
pub const MAX_RESULTS: usize = 1000;
const HISTORY: usize = 16;
const HISTORY_MAX_INDICES: usize = 8_000_000;
const POLL: Duration = Duration::from_millis(40);
const PUBLISH_EVERY: Duration = Duration::from_millis(12);

/// Streaming fuzzy picker over items of type `T`.
///
/// Items are pushed (from any thread) through a [`PickerInjector`]; call
/// [`tick`](Self::tick) regularly (e.g. per frame) to wait for fresh results
/// and read the visible window with [`matches`](Self::matches). The `notify`
/// callback fires (from the matching thread) whenever new results were
/// published.
pub struct Picker<T: Send + Sync + 'static> {
    store: Arc<Store<T>>,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    config: Config,
    notify: Arc<dyn Fn() + Send + Sync>,
    matcher: Mutex<Matcher>,
    query: String,
    generation: u64,
}

/// Cloneable, `Send` handle that feeds items into a [`Picker`].
pub struct PickerInjector<T: Send + Sync + 'static> {
    inner: Arc<Store<T>>,
}

impl<T: Send + Sync + 'static> Clone for PickerInjector<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: Send + Sync + 'static> PickerInjector<T> {
    /// Adds `item`, matched against `haystack`.
    pub fn push(&self, item: T, haystack: &str) {
        self.push_boosted(item, haystack, 0);
    }

    /// Adds `item` with a score bonus (e.g. for recently opened files). The
    /// boost is added to the fuzzy score and orders items of an empty query.
    pub fn push_boosted(&self, item: T, haystack: &str, boost: u16) {
        self.inner.push(item, haystack, boost);
    }
}

/// One visible match.
#[derive(Debug)]
pub struct PickerMatch<'a, T> {
    /// The matched item.
    pub item: &'a T,
    /// Match score (higher is better).
    pub score: u32,
    /// Sorted, deduplicated matched char indices (chars, not bytes) in the haystack.
    pub indices: Vec<u32>,
}

/// How far the matching of the current query has come, see [`Picker::progress`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PickerProgress {
    /// Items examined for the current query.
    pub scanned: u32,
    /// Items pushed so far.
    pub total: u32,
    /// The pass over all pushed items finished: `matched_count` is exact and
    /// the ranking is final (until more items are pushed).
    pub complete: bool,
    /// The ranking is final (every pushed item was examined for it) although
    /// `matched_count` may still be counting up: the first page is exact.
    pub ranked: bool,
    /// The visible ranking already reflects the current query (partial
    /// results of an older query are not shown as the new one's).
    pub current: bool,
}

// ---------------------------------------------------------------------------
// Shared state between the picker and its matching thread

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    score: u32,
    inv_len: Reverse<u32>,
    inv_idx: Reverse<u32>,
}

struct Results {
    gen: u64,
    pattern: Arc<Pattern>,
    /// Best first.
    hits: Vec<(u32, u32)>,
    matched: u32,
    /// Items accounted for (`< total` while items are still pending).
    scanned: u32,
    /// Every pushed item was examined for the ranking.
    ranked: bool,
    complete: bool,
}

struct Job {
    gen: u64,
    query: String,
    pattern: Arc<Pattern>,
}

struct Shared {
    pending: StdMutex<Option<Job>>,
    wake: Condvar,
    shutdown: AtomicBool,
    latest_gen: AtomicU64,
    reset_history: AtomicBool,
    results: StdMutex<Arc<Results>>,
    published: Condvar,
}

impl Shared {
    fn new() -> Arc<Shared> {
        Arc::new(Shared {
            pending: StdMutex::new(None),
            wake: Condvar::new(),
            shutdown: AtomicBool::new(false),
            latest_gen: AtomicU64::new(0),
            reset_history: AtomicBool::new(false),
            results: StdMutex::new(Arc::new(Results {
                gen: 0,
                pattern: Arc::new(Pattern::parse(
                    "",
                    CaseMatching::Smart,
                    Normalization::Smart,
                )),
                hits: Vec::new(),
                matched: 0,
                scanned: 0,
                ranked: true,
                complete: true,
            })),
            published: Condvar::new(),
        })
    }

    fn publish(&self, r: Results, notify: &(dyn Fn() + Send + Sync)) {
        if let Ok(mut g) = self.results.lock() {
            *g = Arc::new(r);
        }
        self.published.notify_all();
        notify();
    }

    fn current(&self) -> Arc<Results> {
        match self.results.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }
}

impl<T: Send + Sync + 'static> Picker<T> {
    /// Creates a picker with default (general text) matching.
    pub fn new(notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self::with_config(notify, Config::DEFAULT)
    }

    /// Creates a picker tuned for file paths: `/` is a word boundary and
    /// file-name matches are preferred.
    pub fn new_for_paths(notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self::with_config(notify, Config::DEFAULT.match_paths())
    }

    /// Creates a picker with an explicit nucleo [`Config`].
    pub fn with_config(notify: Arc<dyn Fn() + Send + Sync>, config: Config) -> Self {
        let store = Arc::new(Store::new());
        let shared = Shared::new();
        let worker = Some(spawn_worker(
            shared.clone(),
            store.clone(),
            config.clone(),
            notify.clone(),
        ));
        let mut picker = Self {
            store,
            shared,
            worker,
            matcher: Mutex::new(Matcher::new(config.clone())),
            config,
            notify,
            query: String::new(),
            generation: 0,
        };
        picker.post_query("");
        picker
    }

    /// Returns an injector for adding items.
    pub fn injector(&self) -> PickerInjector<T> {
        PickerInjector {
            inner: self.store.clone(),
        }
    }

    /// Sets the query (smart-case, unicode-normalized, nucleo syntax: `'exact`, `^prefix`,
    /// `suffix$`, `!negated`). Extending the previous query reuses earlier work.
    /// Matching happens in the background; use [`tick`](Self::tick).
    pub fn set_query(&mut self, query: &str) {
        if query == self.query {
            return;
        }
        self.post_query(query);
    }

    fn post_query(&mut self, query: &str) {
        self.generation += 1;
        self.query.clear();
        self.query.push_str(query);
        let pattern = Arc::new(Pattern::parse(
            query,
            CaseMatching::Smart,
            Normalization::Smart,
        ));
        self.shared
            .latest_gen
            .store(self.generation, Ordering::SeqCst);
        if let Ok(mut p) = self.shared.pending.lock() {
            *p = Some(Job {
                gen: self.generation,
                query: query.to_string(),
                pattern,
            });
        }
        self.shared.wake.notify_one();
    }

    /// Waits up to `timeout_ms` for matching to finish. Partial results are
    /// readable meanwhile (and after this returns). Returns true while
    /// matching is still in progress (call again soon).
    pub fn tick(&mut self, timeout_ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let total = self.store.injected();
        // Wake an idle worker so it notices new items right away.
        self.shared.wake.notify_one();
        let mut guard = match self.shared.results.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        loop {
            let done = guard.gen == self.generation && guard.complete && guard.scanned >= total;
            if done {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            guard = match self.shared.published.wait_timeout(guard, deadline - now) {
                Ok((g, _)) => g,
                Err(p) => p.into_inner().0,
            };
        }
    }

    /// Progress of the current query without waiting. The ranking readable
    /// through [`matches`](Self::matches) is partial until `complete`; the UI
    /// can show the first page as soon as `current` is true and say "N+
    /// matches" until the count is final.
    pub fn progress(&self) -> PickerProgress {
        let r = self.shared.current();
        let total = self.store.injected();
        PickerProgress {
            scanned: r.scanned,
            total,
            complete: r.gen == self.generation && r.complete && r.scanned >= total,
            ranked: r.gen == self.generation && r.ranked,
            current: r.gen == self.generation,
        }
    }

    /// Forgets the remembered match sets of earlier queries (frees their
    /// memory; the next query is matched against all items). Takes effect
    /// with the next query.
    pub fn drop_caches(&self) {
        self.shared.reset_history.store(true, Ordering::SeqCst);
    }

    /// Number of items matching the current query (exact when complete, the
    /// count so far while matching is running).
    pub fn matched_count(&self) -> u32 {
        self.shared.current().matched
    }

    /// Number of items injected so far.
    pub fn total_count(&self) -> u32 {
        self.store.injected()
    }

    /// Heap bytes held by the item store (haystack arenas, masks, offsets,
    /// boosts and the items themselves, not what an item owns on the heap).
    pub fn heap_bytes(&self) -> usize {
        self.store.heap_bytes()
    }

    /// Matches `range` (clamped to the retained best [`MAX_RESULTS`]) with
    /// highlight indices, best first.
    pub fn matches(&self, range: Range<u32>) -> Vec<PickerMatch<'_, T>> {
        let results = self.shared.current();
        let total = results.hits.len() as u32;
        let start = range.start.min(total);
        let end = range.end.min(total);
        if start >= end {
            return Vec::new();
        }
        let mut matcher = self.matcher.lock();
        let mut buf = Vec::new();
        let mut out = Vec::with_capacity((end - start) as usize);
        for &(idx, score) in &results.hits[start as usize..end as usize] {
            let Some((chunk, j)) = self.store.locate(idx) else {
                continue;
            };
            buf.clear();
            if !results.pattern.atoms.is_empty() {
                let hay = chunk.hay(j);
                if chunk.masks[j].any & NON_ASCII == 0 {
                    results
                        .pattern
                        .indices(Utf32Str::Ascii(hay), &mut matcher, &mut buf);
                } else {
                    let text = Utf32String::from(std::str::from_utf8(hay).unwrap_or(""));
                    results
                        .pattern
                        .indices(text.slice(..), &mut matcher, &mut buf);
                }
                buf.sort_unstable();
                buf.dedup();
            }
            out.push(PickerMatch {
                item: &chunk.items[j],
                score,
                indices: std::mem::take(&mut buf),
            });
        }
        out
    }

    /// Removes all items and disconnects previously created injectors.
    pub fn clear(&mut self) {
        self.stop_worker();
        self.store = Arc::new(Store::new());
        self.shared = Shared::new();
        self.generation = 0;
        self.query.clear();
        self.worker = Some(spawn_worker(
            self.shared.clone(),
            self.store.clone(),
            self.config.clone(),
            self.notify.clone(),
        ));
        self.post_query("");
    }

    fn stop_worker(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        self.shared.wake.notify_all();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

impl<T: Send + Sync + 'static> Drop for Picker<T> {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        self.shared.wake.notify_all();
        // The worker exits on its own; do not block the (UI) thread on it.
        drop(self.worker.take());
    }
}

// ---------------------------------------------------------------------------
// Matching thread

struct HistoryEntry {
    query: String,
    matched: Arc<Vec<u32>>,
    /// Items below this index were examined for this query.
    upto: u32,
    /// The final top-k, best first, and the exact match count: replayed as is
    /// when the same query comes back (backspace) and nothing was added.
    keys: Arc<Vec<Key>>,
    matched_count: u32,
    /// For single plain fuzzy atoms: needle, case mode and, per matched item,
    /// where the leftmost match ended (so extending the query continues from
    /// there instead of searching again).
    atom: Option<(Vec<u8>, bool)>,
    ends: Option<Arc<Vec<u16>>>,
}

/// End positions of an earlier, shorter query of the same atom.
struct Inc {
    ends: Arc<Vec<u16>>,
    prefix: usize,
}

/// Position in the candidate stream: the previous match list (if any) followed
/// by the items pushed after that list was built.
#[derive(Clone, Copy, Default)]
struct Cursor {
    list_pos: usize,
    range_pos: u32,
}

impl Cursor {
    fn done(&self, list_len: usize, sealed: u32) -> bool {
        self.list_pos >= list_len && self.range_pos >= sealed
    }
}

/// Which half of the work a scan batch does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// Rank candidates: cheap bound first, exact scoring only for those that
    /// can still enter the top-k. Does the counting too unless the query is
    /// two-phase.
    Rank,
    /// Two-phase queries only: exact membership test, counts and records the
    /// matches (the previous work ranked everything already).
    Count,
}

struct Active {
    gen: u64,
    query: String,
    pattern: Arc<Pattern>,
    plan: Plan,
    list: Option<Arc<Vec<u32>>>,
    /// Ranking progress.
    rank: Cursor,
    /// Counting progress (mirrors `rank` for queries that count while ranking).
    count: Cursor,
    heap: BinaryHeap<Reverse<Key>>,
    matched: Vec<u32>,
    /// Match end positions, parallel to `matched` (single-atom queries).
    ends: Vec<u16>,
    inc: Option<Inc>,
    record_ends: bool,
    matched_count: u32,
    batches: u32,
    last_publish: Instant,
    /// `count.range_pos` at the last complete publication.
    published_complete_at: Option<u32>,
    ranked_published: bool,
    /// Replayed from the history: publish at once.
    restored: bool,
}

/// A query that only adds constraints to `old` (every match of `new` matches `old`).
fn narrows(old: &str, new: &str) -> bool {
    if old.is_empty() || !new.starts_with(old) {
        return false;
    }
    if old.contains('$') || old.contains('\\') {
        return false;
    }
    if old.ends_with(char::is_whitespace) {
        return true;
    }
    // A trailing negative atom is loosened (not tightened) by extending it.
    let last = old.rsplit(char::is_whitespace).next().unwrap_or("");
    !last.starts_with('!')
}

fn worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(1, 8)
}

fn spawn_worker<T: Send + Sync + 'static>(
    shared: Arc<Shared>,
    store: Arc<Store<T>>,
    config: Config,
    notify: Arc<dyn Fn() + Send + Sync>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("lector-picker".into())
        .spawn(move || run_worker(&shared, &store, config, &*notify))
        .unwrap_or_else(|_| std::thread::spawn(|| {}))
}

fn run_worker<T: Send + Sync + 'static>(
    shared: &Shared,
    store: &Store<T>,
    config: Config,
    notify: &(dyn Fn() + Send + Sync),
) {
    let threads = worker_count();
    let mut matchers: Vec<Matcher> = (0..threads).map(|_| Matcher::new(config.clone())).collect();
    let mut history: Vec<HistoryEntry> = Vec::new();
    let mut active: Option<Active> = None;
    let mut last_flush = Instant::now();
    loop {
        if shared.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let job = shared.pending.lock().ok().and_then(|mut p| p.take());
        if shared.reset_history.swap(false, Ordering::SeqCst) {
            history.clear();
        }
        if let Some(job) = job {
            let mut a = start(
                job,
                &history,
                &config,
                store.sealed_items(),
                store.pending(),
            );
            if a.restored {
                a.published_complete_at = Some(a.count.range_pos);
                publish(shared, &mut a, true, true, notify);
            }
            active = Some(a);
        }
        let Some(a) = active.as_mut() else {
            wait(shared, None);
            continue;
        };
        let sealed = store.sealed_items();
        let list_len = a.list.as_ref().map_or(0, |l| l.len());
        let two_phase = a.plan.two_phase();
        if !a.rank.done(list_len, sealed) {
            let finished = step(a, store, &mut matchers, shared, Pass::Rank);
            if !two_phase {
                a.count = a.rank;
            }
            if finished {
                let ranked = a.rank.done(list_len, store.sealed_items());
                if ranked && two_phase && !a.ranked_published {
                    // The first page is exact now: say so right away.
                    a.ranked_published = true;
                    publish(shared, a, false, true, notify);
                } else if a.batches <= 3 || a.last_publish.elapsed() >= PUBLISH_EVERY {
                    publish_if_current(shared, a, ranked && two_phase, notify);
                }
            }
            continue;
        }
        if !a.count.done(list_len, sealed) {
            if two_phase && a.count.range_pos == 0 && a.count.list_pos == 0 {
                a.ranked_published = true;
            }
            let finished = step(a, store, &mut matchers, shared, Pass::Count);
            if finished && a.last_publish.elapsed() >= PUBLISH_EVERY {
                publish_if_current(shared, a, true, notify);
            }
            continue;
        }
        if store.pending() > 0 {
            // Seal the open tail chunk so it can be scanned. Under a slow
            // trickle of items this makes tiny chunks; throttle it once
            // the chunk table is getting crowded.
            let crowded = store.chunk_count() > 50_000;
            if !crowded
                || store.pending() >= 1024
                || last_flush.elapsed() > Duration::from_millis(200)
            {
                store.flush();
                last_flush = Instant::now();
            } else {
                wait(shared, Some(Duration::from_millis(5)));
            }
            continue;
        }
        if a.published_complete_at != Some(a.count.range_pos) {
            a.published_complete_at = Some(a.count.range_pos);
            publish(shared, a, true, true, notify);
            if a.plan.mode != Mode::Empty {
                remember(&mut history, a);
            }
        }
        wait(shared, Some(POLL));
    }
}

fn wait(shared: &Shared, timeout: Option<Duration>) {
    let Ok(guard) = shared.pending.lock() else {
        return;
    };
    if guard.is_some() || shared.shutdown.load(Ordering::SeqCst) {
        return;
    }
    match timeout {
        Some(t) => {
            let _ = shared.wake.wait_timeout(guard, t);
        }
        None => {
            let _ = shared.wake.wait_timeout(guard, Duration::from_secs(1));
        }
    }
}

fn start(job: Job, history: &[HistoryEntry], config: &Config, sealed: u32, pending: u32) -> Active {
    let plan = Plan::new(&job.pattern, config);
    let record_ends = plan.two_phase() && plan.single_atom().is_some();
    let mut a = Active {
        gen: job.gen,
        query: job.query,
        pattern: job.pattern,
        plan,
        list: None,
        rank: Cursor::default(),
        count: Cursor::default(),
        heap: BinaryHeap::new(),
        matched: Vec::new(),
        ends: Vec::new(),
        inc: None,
        record_ends,
        matched_count: 0,
        batches: 0,
        last_publish: Instant::now(),
        published_complete_at: None,
        ranked_published: false,
        restored: false,
    };
    if a.plan.mode == Mode::Empty {
        return a;
    }
    // Same query again with nothing new: replay the remembered result.
    if let Some(h) = history
        .iter()
        .find(|h| h.query == a.query && h.upto == sealed && pending == 0)
    {
        let cursor = Cursor {
            list_pos: h.matched.len(),
            range_pos: h.upto,
        };
        a.list = Some(h.matched.clone());
        a.rank = cursor;
        a.count = cursor;
        a.heap = h.keys.iter().map(|k| Reverse(*k)).collect();
        a.matched = h.matched.to_vec();
        a.ends = h.ends.as_ref().map(|e| e.to_vec()).unwrap_or_default();
        a.matched_count = h.matched_count;
        a.restored = true;
        return a;
    }
    let base = history
        .iter()
        .filter(|h| h.query != a.query && narrows(&h.query, &a.query))
        .max_by_key(|h| h.query.len());
    if let Some(h) = base {
        a.list = Some(h.matched.clone());
        let cursor = Cursor {
            list_pos: 0,
            range_pos: h.upto,
        };
        a.rank = cursor;
        a.count = cursor;
        // Continue from the old match ends when the new atom extends the old one.
        if let (Some((new, new_ic)), Some((old, old_ic)), Some(ends)) =
            (a.plan.single_atom(), h.atom.as_ref(), h.ends.as_ref())
        {
            if record_ends
                && new_ic == *old_ic
                && new.starts_with(old)
                && ends.len() == h.matched.len()
            {
                a.inc = Some(Inc {
                    ends: ends.clone(),
                    prefix: old.len(),
                });
            }
        }
    }
    a
}

fn remember(history: &mut Vec<HistoryEntry>, a: &Active) {
    history.retain(|h| h.query != a.query);
    history.push(HistoryEntry {
        query: a.query.clone(),
        matched: Arc::new(a.matched.clone()),
        upto: a.count.range_pos,
        keys: Arc::new(sorted_keys(a)),
        matched_count: a.matched_count,
        atom: a
            .plan
            .single_atom()
            .filter(|_| a.record_ends)
            .map(|(n, ic)| (n.to_vec(), ic)),
        ends: a.record_ends.then(|| Arc::new(a.ends.clone())),
    });
    while history.len() > HISTORY
        || (history.len() > 1
            && history.iter().map(|h| h.matched.len()).sum::<usize>() > HISTORY_MAX_INDICES)
    {
        history.remove(0);
    }
}

/// Publishes unless the query was already replaced.
fn publish_if_current(
    shared: &Shared,
    a: &mut Active,
    ranked: bool,
    notify: &(dyn Fn() + Send + Sync),
) {
    // Cheap check first: do not publish for an already replaced query.
    if shared.latest_gen.load(Ordering::SeqCst) == a.gen {
        publish(shared, a, false, ranked, notify);
    }
}

fn publish(
    shared: &Shared,
    a: &mut Active,
    complete: bool,
    ranked: bool,
    notify: &(dyn Fn() + Send + Sync),
) {
    a.last_publish = Instant::now();
    let keys = sorted_keys(a);
    // While the count is still running do not claim fewer matches than the
    // page that is already shown.
    let matched = a.matched_count.max(keys.len() as u32);
    shared.publish(
        Results {
            gen: a.gen,
            pattern: a.pattern.clone(),
            hits: keys.into_iter().map(|k| (k.inv_idx.0, k.score)).collect(),
            matched,
            scanned: a.count.range_pos,
            ranked,
            complete,
        },
        notify,
    );
}

/// The heap's keys, best first.
fn sorted_keys(a: &Active) -> Vec<Key> {
    let mut keys: Vec<Key> = a.heap.iter().map(|k| k.0).collect();
    keys.sort_unstable_by(|x, y| y.cmp(x));
    keys
}

// ---------------------------------------------------------------------------
// Scan kernel

struct ScanCtx<'a, T> {
    store: &'a Store<T>,
    pattern: &'a Pattern,
    plan: &'a Plan,
    pass: Pass,
    /// Key of the current k-th best (full heap): candidates that cannot beat
    /// it are skipped.
    floor: Option<Key>,
    record: bool,
    record_ends: bool,
    /// Needle chars of the single atom already matched by `prior` ends.
    inc_prefix: usize,
    latest_gen: &'a AtomicU64,
    gen: u64,
}

impl<T> ScanCtx<'_, T> {
    #[inline]
    fn cancelled(&self) -> bool {
        self.latest_gen.load(Ordering::Relaxed) != self.gen
    }
}

struct Partial {
    matched: Vec<u32>,
    ends: Vec<u16>,
    count: u32,
    heap: BinaryHeap<Reverse<Key>>,
    /// Best-known lower limit for entering the heap (floor or heap minimum).
    thr: Option<Key>,
}

impl Partial {
    fn new(floor: Option<Key>) -> Self {
        Partial {
            matched: Vec::new(),
            ends: Vec::new(),
            count: 0,
            heap: BinaryHeap::new(),
            thr: floor,
        }
    }

    /// Can a candidate with (at best) this key still enter the top-k?
    #[inline]
    fn admits(&self, key: &Key) -> bool {
        self.thr.as_ref().is_none_or(|t| key > t)
    }

    fn offer(&mut self, key: Key) {
        if !self.admits(&key) {
            return;
        }
        if self.heap.len() >= MAX_RESULTS {
            self.heap.pop();
        }
        self.heap.push(Reverse(key));
        if self.heap.len() >= MAX_RESULTS {
            if let Some(min) = self.heap.peek() {
                if self.thr.as_ref().is_none_or(|t| min.0 > *t) {
                    self.thr = Some(min.0);
                }
            }
        }
    }

    #[inline]
    fn count_match(&mut self, record: bool, idx: u32) {
        self.count += 1;
        if record {
            self.matched.push(idx);
        }
    }
}

/// Examines item `j` of `chunk` (global index `idx`) for the active query.
#[inline(always)]
fn visit<T>(
    ctx: &ScanCtx<'_, T>,
    chunk: &Chunk<T>,
    j: usize,
    idx: u32,
    prior_end: Option<u16>,
    matcher: &mut Matcher,
    p: &mut Partial,
) {
    let plan = ctx.plan;
    let boost = chunk.boosts[j] as u32;
    if plan.mode == Mode::Empty {
        p.count_match(false, idx);
        p.offer(Key {
            score: boost,
            inv_len: Reverse(0),
            inv_idx: Reverse(idx),
        });
        return;
    }
    let mask = chunk.masks[j];
    let hay = chunk.hay(j);
    let ascii = mask.any & NON_ASCII == 0;
    if ascii && !plan.mask_ok(&mask) {
        return;
    }
    if ctx.pass == Pass::Count {
        // Two-phase queries only (Fast mode): ranking is done, count the matches.
        let mut end = u16::MAX;
        let hit = if !ascii {
            let text = Utf32String::from(std::str::from_utf8(hay).unwrap_or(""));
            ctx.pattern.score(text.slice(..), matcher).is_some()
        } else if plan.mask_is_exact() {
            true
        } else if ctx.record_ends {
            let found = match prior_end {
                Some(e) if e != u16::MAX => plan.extend_end(hay, e as usize, ctx.inc_prefix),
                _ => plan.match_end(hay),
            };
            if let Some(e) = found {
                end = u16::try_from(e).unwrap_or(u16::MAX);
            }
            found.is_some()
        } else {
            plan.subsequence(hay)
        };
        if hit {
            p.count_match(ctx.record, idx);
            if ctx.record_ends {
                p.ends.push(end);
            }
        }
        return;
    }
    let (score, len) = if ascii {
        let len = hay.len() as u32;
        if plan.two_phase() {
            // Bound first (valid for matching paths; non-matching paths that
            // slip through are rejected by the membership test below).
            let best = Key {
                score: plan.upper_bound(mask.boundary).saturating_add(boost),
                inv_len: Reverse(len),
                inv_idx: Reverse(idx),
            };
            if !p.admits(&best) || !(plan.mask_is_exact() || plan.subsequence(hay)) {
                return;
            }
            match ctx.pattern.score(Utf32Str::Ascii(hay), matcher) {
                Some(s) => (s, len),
                None => return,
            }
        } else {
            match ctx.pattern.score(Utf32Str::Ascii(hay), matcher) {
                Some(s) => {
                    p.count_match(ctx.record, idx);
                    (s, len)
                }
                None => return,
            }
        }
    } else {
        let text = Utf32String::from(std::str::from_utf8(hay).unwrap_or(""));
        match ctx.pattern.score(text.slice(..), matcher) {
            Some(s) => {
                if !plan.two_phase() {
                    p.count_match(ctx.record, idx);
                }
                (s, text.len() as u32)
            }
            None => return,
        }
    };
    p.offer(Key {
        score: score.saturating_add(boost),
        inv_len: Reverse(len),
        inv_idx: Reverse(idx),
    });
}

fn scan_range<T>(ctx: &ScanCtx<'_, T>, matcher: &mut Matcher, lo: u32, hi: u32) -> Partial {
    let mut p = Partial::new(ctx.floor);
    let Some(mut no) = ctx.store.chunk_no_of(lo) else {
        return p;
    };
    let mut idx = lo;
    while idx < hi {
        let Some(chunk) = ctx.store.chunk(no) else {
            break;
        };
        if ctx.cancelled() {
            break;
        }
        let j0 = (idx - chunk.base) as usize;
        let j1 = ((hi - chunk.base) as usize).min(chunk.len());
        for j in j0..j1 {
            visit(ctx, chunk, j, chunk.base + j as u32, None, matcher, &mut p);
        }
        idx = chunk.base + chunk.len() as u32;
        no += 1;
    }
    p
}

fn scan_list<T>(
    ctx: &ScanCtx<'_, T>,
    matcher: &mut Matcher,
    list: &[u32],
    ends: Option<&[u16]>,
) -> Partial {
    let mut p = Partial::new(ctx.floor);
    let mut cur: Option<&Chunk<T>> = None;
    for (n, &idx) in list.iter().enumerate() {
        if n % 4096 == 0 && ctx.cancelled() {
            break;
        }
        let chunk = match cur {
            Some(c) if idx >= c.base && ((idx - c.base) as usize) < c.len() => c,
            _ => {
                let Some(c) = ctx
                    .store
                    .chunk_no_of(idx)
                    .and_then(|no| ctx.store.chunk(no))
                else {
                    continue;
                };
                cur = Some(c);
                c
            }
        };
        let prior = ends.and_then(|e| e.get(n).copied());
        visit(
            ctx,
            chunk,
            (idx - chunk.base) as usize,
            idx,
            prior,
            matcher,
            &mut p,
        );
    }
    p
}

/// Examines one batch of candidates (list entries first, then new items) for
/// `pass`. Returns false when the batch was abandoned because the query was
/// replaced.
fn step<T: Send + Sync + 'static>(
    a: &mut Active,
    store: &Store<T>,
    matchers: &mut [Matcher],
    shared: &Shared,
    pass: Pass,
) -> bool {
    let cur = if pass == Pass::Rank { a.rank } else { a.count };
    let size = (2048usize << a.batches.min(5)).min(65536);
    if pass == Pass::Rank || !a.plan.two_phase() {
        a.batches += 1;
    }
    let threads = matchers.len();
    enum Src<'x> {
        List(&'x [u32], Option<&'x [u16]>),
        Range(Range<u32>),
    }
    let (src, advance_list, advance_range) = match &a.list {
        Some(l) if cur.list_pos < l.len() => {
            let end = (cur.list_pos + size).min(l.len());
            let ends = match (&a.inc, pass) {
                (Some(inc), Pass::Count) => inc.ends.get(cur.list_pos..end),
                _ => None,
            };
            (
                Src::List(&l[cur.list_pos..end], ends),
                end - cur.list_pos,
                0,
            )
        }
        _ => {
            let end = (cur.range_pos as usize + size).min(store.sealed_items() as usize) as u32;
            (Src::Range(cur.range_pos..end), 0, end - cur.range_pos)
        }
    };
    let n = match &src {
        Src::List(s, _) => s.len(),
        Src::Range(r) => (r.end - r.start) as usize,
    };
    let parts = threads.min(n / 512 + 1).max(1);
    let per = n.div_ceil(parts);
    let floor = if pass == Pass::Rank && a.heap.len() >= MAX_RESULTS {
        a.heap.peek().map(|r| r.0)
    } else {
        None
    };
    // Matches are recorded (for narrowing) by the pass that counts them.
    let counts_here = pass == Pass::Count || !a.plan.two_phase();
    let ctx = ScanCtx {
        store,
        pattern: &a.pattern,
        plan: &a.plan,
        pass,
        floor,
        record: counts_here && a.plan.mode != Mode::Empty,
        record_ends: pass == Pass::Count && a.record_ends,
        inc_prefix: a.inc.as_ref().map_or(0, |i| i.prefix),
        latest_gen: &shared.latest_gen,
        gen: a.gen,
    };
    let partials: Vec<Partial> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        let mut first = None;
        for (i, matcher) in matchers.iter_mut().enumerate().take(parts) {
            let (lo, hi) = (i * per, ((i + 1) * per).min(n));
            let (src, ctx) = (&src, &ctx);
            let run = move || match src {
                Src::List(s, e) => scan_list(ctx, matcher, &s[lo..hi], e.map(|e| &e[lo..hi])),
                Src::Range(r) => scan_range(ctx, matcher, r.start + lo as u32, r.start + hi as u32),
            };
            if i == 0 {
                first = Some(run);
            } else {
                handles.push(scope.spawn(run));
            }
        }
        let mut out = Vec::new();
        if let Some(mut run) = first {
            out.push(run());
        }
        for h in handles {
            if let Ok(p) = h.join() {
                out.push(p);
            }
        }
        out
    });
    if ctx.cancelled() {
        return false;
    }
    // Merge in part order (parts are ascending index ranges; the first part
    // ran on this thread but was pushed first).
    for p in partials {
        a.matched_count += p.count;
        a.matched.extend_from_slice(&p.matched);
        a.ends.extend_from_slice(&p.ends);
        for Reverse(k) in p.heap {
            if a.heap.len() < MAX_RESULTS {
                a.heap.push(Reverse(k));
            } else if a.heap.peek().is_some_and(|worst| k > worst.0) {
                a.heap.pop();
                a.heap.push(Reverse(k));
            }
        }
    }
    let target = if pass == Pass::Rank {
        &mut a.rank
    } else {
        &mut a.count
    };
    target.list_pos += advance_list;
    target.range_pos += advance_range;
    true
}

#[cfg(test)]
mod tests {
    use super::narrows;

    #[test]
    fn narrowing_rules() {
        assert!(narrows("ab", "abc"));
        assert!(narrows("ab", "ab c"));
        assert!(narrows("rs ", "rs !lib"));
        assert!(narrows("rs", "rs !lib"));
        assert!(!narrows("rs !li", "rs !lib"));
        assert!(!narrows("rs !", "rs !x"));
        assert!(!narrows("abc", "ab"));
        assert!(!narrows("", "a"));
        assert!(!narrows("a$", "a$b"));
        assert!(!narrows("a\\", "a\\ b"));
    }
}
