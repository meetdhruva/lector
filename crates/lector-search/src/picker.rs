//! Streaming fuzzy picker (nucleo scoring) built for huge candidate sets.
//!
//! nucleo's own `Nucleo` publishes a snapshot only after sorting *every*
//! match, which costs hundreds of milliseconds per keystroke on a million
//! paths. This picker keeps nucleo's `Pattern`/`Matcher` (identical scores and
//! highlight indices) but runs its own matching loop on a background thread:
//!
//! * **Top-k heap** instead of a full sort: only the best [`MAX_RESULTS`]
//!   matches are retained and ordered (score, then shorter haystack, then
//!   insertion order, with an optional per-item boost for recency).
//! * **Progressive publication**: results are published after small first
//!   batches (a few milliseconds) and then at most every ~12 ms, so the first
//!   results appear almost immediately while the full pass continues. All
//!   cores are used inside a batch.
//! * **Narrowing**: when the query extends an earlier completed query, only
//!   that query's matches are re-scored (the complete match sets of recent
//!   queries are kept, so backspacing is as cheap as typing).
//! * **Streaming items**: items injected while (or after) matching are
//!   scored incrementally; nothing is rescanned.
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
use nucleo::{Config, Injector, Matcher, Nucleo, Utf32String};
use parking_lot::Mutex;

/// Number of best matches that are retained, ordered and available through
/// [`Picker::matches`].
pub const MAX_RESULTS: usize = 1000;
const HISTORY: usize = 16;
const HISTORY_MAX_INDICES: usize = 8_000_000;
const POLL: Duration = Duration::from_millis(40);
const PUBLISH_EVERY: Duration = Duration::from_millis(12);

struct Entry<T> {
    data: T,
    boost: u16,
}

type Store<T> = Injector<Entry<T>>;

/// Streaming fuzzy picker over items of type `T`.
///
/// Items are pushed (from any thread) through a [`PickerInjector`]; call
/// [`tick`](Self::tick) regularly (e.g. per frame) to wait for fresh results
/// and read the visible window with [`matches`](Self::matches). The `notify`
/// callback fires (from the matching thread) whenever new results were
/// published.
pub struct Picker<T: Send + Sync + 'static> {
    // Used purely as the append-only, lock-free item store.
    nucleo: Nucleo<Entry<T>>,
    store: Store<T>,
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
    inner: Store<T>,
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
        self.inner.push(Entry { data: item, boost }, |_, cols| {
            cols[0] = Utf32String::from(haystack);
        });
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
        let nucleo = Nucleo::new(config.clone(), Arc::new(|| {}), Some(1), 1);
        let store = nucleo.injector();
        let shared = Shared::new();
        let worker = Some(spawn_worker(
            shared.clone(),
            store.clone(),
            config.clone(),
            notify.clone(),
        ));
        let mut picker = Self {
            nucleo,
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
        let total = self.store.injected_items();
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

    /// Number of items matching the current query (exact when complete, the
    /// count so far while matching is running).
    pub fn matched_count(&self) -> u32 {
        self.shared.current().matched
    }

    /// Number of items injected so far.
    pub fn total_count(&self) -> u32 {
        self.store.injected_items()
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
            let Some(item) = self.store.get(idx) else {
                continue;
            };
            buf.clear();
            if !results.pattern.atoms.is_empty() {
                let hay = item.matcher_columns[0].slice(..);
                results.pattern.indices(hay, &mut matcher, &mut buf);
                buf.sort_unstable();
                buf.dedup();
            }
            let entry: &Entry<T> = item.data;
            out.push(PickerMatch {
                item: &entry.data,
                score,
                indices: buf.clone(),
            });
        }
        out
    }

    /// Removes all items and disconnects previously created injectors.
    pub fn clear(&mut self) {
        self.stop_worker();
        self.nucleo.restart(true);
        self.store = self.nucleo.injector();
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
    /// Items below this index were scanned for this query.
    upto: u32,
}

struct Active {
    gen: u64,
    query: String,
    pattern: Arc<Pattern>,
    empty: bool,
    list: Option<Arc<Vec<u32>>>,
    list_pos: usize,
    range_pos: u32,
    heap: BinaryHeap<Reverse<Key>>,
    matched: Vec<u32>,
    matched_count: u32,
    batches: u32,
    last_publish: Instant,
    /// `range_pos` at the last complete publication.
    published_complete_at: Option<u32>,
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
    store: Store<T>,
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
    loop {
        if shared.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let job = shared.pending.lock().ok().and_then(|mut p| p.take());
        if let Some(job) = job {
            active = Some(start(job, &history));
        }
        let Some(a) = active.as_mut() else {
            wait(shared, None);
            continue;
        };
        let count = store.injected_items();
        let list_done = a.list.as_ref().is_none_or(|l| a.list_pos >= l.len());
        if list_done && a.range_pos >= count {
            if a.published_complete_at != Some(a.range_pos) {
                a.published_complete_at = Some(a.range_pos);
                publish(shared, a, true, notify);
                if !a.empty {
                    remember(&mut history, a);
                }
            }
            wait(shared, Some(POLL));
            continue;
        }
        let before = (a.list_pos, a.range_pos);
        step(a, store, &mut matchers, count);
        if (a.list_pos, a.range_pos) == before {
            // Only in-flight (not yet written) items remain: let the pushers finish.
            wait(shared, Some(Duration::from_millis(1)));
        }
        if a.batches <= 3 || a.last_publish.elapsed() >= PUBLISH_EVERY {
            // Cheap check first: do not publish for an already replaced query.
            if shared.latest_gen.load(Ordering::SeqCst) == a.gen {
                publish(shared, a, false, notify);
            }
        }
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

fn start(job: Job, history: &[HistoryEntry]) -> Active {
    let empty = job.pattern.atoms.is_empty();
    let base = if empty {
        None
    } else {
        history
            .iter()
            .filter(|h| narrows(&h.query, &job.query))
            .max_by_key(|h| h.query.len())
    };
    Active {
        gen: job.gen,
        query: job.query,
        pattern: job.pattern,
        empty,
        list: base.map(|h| h.matched.clone()),
        list_pos: 0,
        range_pos: base.map_or(0, |h| h.upto),
        heap: BinaryHeap::new(),
        matched: Vec::new(),
        matched_count: 0,
        batches: 0,
        last_publish: Instant::now(),
        published_complete_at: None,
    }
}

fn remember(history: &mut Vec<HistoryEntry>, a: &Active) {
    history.retain(|h| h.query != a.query);
    history.push(HistoryEntry {
        query: a.query.clone(),
        matched: Arc::new(a.matched.clone()),
        upto: a.range_pos,
    });
    while history.len() > HISTORY
        || (history.len() > 1
            && history.iter().map(|h| h.matched.len()).sum::<usize>() > HISTORY_MAX_INDICES)
    {
        history.remove(0);
    }
}

fn publish(shared: &Shared, a: &mut Active, complete: bool, notify: &(dyn Fn() + Send + Sync)) {
    a.last_publish = Instant::now();
    let mut keys: Vec<Key> = a.heap.iter().map(|k| k.0).collect();
    keys.sort_unstable_by(|x, y| y.cmp(x));
    shared.publish(
        Results {
            gen: a.gen,
            pattern: a.pattern.clone(),
            hits: keys.into_iter().map(|k| (k.inv_idx.0, k.score)).collect(),
            matched: a.matched_count,
            scanned: a.range_pos,
            complete,
        },
        notify,
    );
}

struct Partial {
    matched: Vec<u32>,
    count: u32,
    heap: BinaryHeap<Reverse<Key>>,
}

fn offer(heap: &mut BinaryHeap<Reverse<Key>>, key: Key) {
    if heap.len() < MAX_RESULTS {
        heap.push(Reverse(key));
    } else if heap.peek().is_some_and(|worst| key > worst.0) {
        heap.pop();
        heap.push(Reverse(key));
    }
}

fn score_indices<T: Send + Sync + 'static>(
    store: &Store<T>,
    pattern: &Pattern,
    empty: bool,
    matcher: &mut Matcher,
    indices: impl Iterator<Item = u32>,
) -> Partial {
    let mut p = Partial {
        matched: Vec::new(),
        count: 0,
        heap: BinaryHeap::new(),
    };
    for idx in indices {
        let Some(item) = store.get(idx) else {
            continue;
        };
        let boost = item.data.boost as u32;
        let hay = item.matcher_columns[0].slice(..);
        let (score, len) = if empty {
            (boost, 0)
        } else {
            match pattern.score(hay, matcher) {
                Some(s) => (s + boost, hay.len() as u32),
                None => continue,
            }
        };
        p.count += 1;
        if !empty {
            p.matched.push(idx);
        }
        offer(
            &mut p.heap,
            Key {
                score,
                inv_len: Reverse(len),
                inv_idx: Reverse(idx),
            },
        );
    }
    p
}

/// Scores one batch of candidates (list entries first, then new items).
fn step<T: Send + Sync + 'static>(
    a: &mut Active,
    store: &Store<T>,
    matchers: &mut [Matcher],
    count: u32,
) {
    let size = (2048usize << a.batches.min(5)).min(65536);
    a.batches += 1;
    let threads = matchers.len();
    // Candidate source for this batch.
    enum Src<'x> {
        List(&'x [u32]),
        Range(Range<u32>),
    }
    let (src, advance_list, advance_range) = match &a.list {
        Some(l) if a.list_pos < l.len() => {
            let end = (a.list_pos + size).min(l.len());
            (Src::List(&l[a.list_pos..end]), end - a.list_pos, 0)
        }
        _ => {
            let mut end = (a.range_pos as usize + size).min(count as usize) as u32;
            if end == count {
                // Slots right at the tail may be reserved but not yet written.
                let from = end.saturating_sub(4096).max(a.range_pos);
                if let Some(hole) = (from..end).find(|&i| store.get(i).is_none()) {
                    end = hole;
                }
            }
            (Src::Range(a.range_pos..end), 0, end - a.range_pos)
        }
    };
    let n = match &src {
        Src::List(s) => s.len(),
        Src::Range(r) => (r.end - r.start) as usize,
    };
    let parts = threads.min(n / 512 + 1).max(1);
    let per = n.div_ceil(parts);
    let (pattern, empty) = (&a.pattern, a.empty);
    let partials: Vec<Partial> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        let mut first = None;
        for (i, matcher) in matchers.iter_mut().enumerate().take(parts) {
            let (lo, hi) = (i * per, ((i + 1) * per).min(n));
            let src = &src;
            let run = move || match src {
                Src::List(s) => {
                    score_indices(store, pattern, empty, matcher, s[lo..hi].iter().copied())
                }
                Src::Range(r) => score_indices(
                    store,
                    pattern,
                    empty,
                    matcher,
                    (r.start + lo as u32)..(r.start + hi as u32),
                ),
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
    // Merge in part order (parts are ascending index ranges; the first part
    // ran on this thread but was pushed first).
    for p in partials {
        a.matched_count += p.count;
        a.matched.extend_from_slice(&p.matched);
        for Reverse(k) in p.heap {
            offer(&mut a.heap, k);
        }
    }
    a.list_pos += advance_list;
    a.range_pos += advance_range;
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
