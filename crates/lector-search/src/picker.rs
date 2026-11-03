use std::ops::Range;
use std::sync::Arc;

use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Injector, Matcher, Nucleo, Utf32String};
use parking_lot::Mutex;

/// Streaming fuzzy picker over items of type `T`, backed by `nucleo`.
///
/// Items are pushed (from any thread) through a [`PickerInjector`]; call
/// [`tick`](Self::tick) regularly (e.g. per frame) to refresh results and read the visible
/// window with [`matches`](Self::matches). The `notify` callback fires when new results may
/// be available.
pub struct Picker<T: Send + Sync + 'static> {
    nucleo: Nucleo<T>,
    matcher: Mutex<Matcher>,
    query: String,
}

/// Cloneable, `Send` handle that feeds items into a [`Picker`].
pub struct PickerInjector<T: Send + Sync + 'static> {
    inner: Injector<T>,
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
        self.inner.push(item, |_, cols| {
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
        Self {
            nucleo: Nucleo::new(config.clone(), notify, None, 1),
            matcher: Mutex::new(Matcher::new(config)),
            query: String::new(),
        }
    }

    /// Returns an injector for adding items.
    pub fn injector(&self) -> PickerInjector<T> {
        PickerInjector {
            inner: self.nucleo.injector(),
        }
    }

    /// Sets the query (smart-case, unicode-normalized, nucleo syntax: `'exact`, `^prefix`,
    /// `suffix$`, `!negated`). Extending the previous query reuses earlier work.
    pub fn set_query(&mut self, query: &str) {
        // Appending only narrows results if no negated atom or escape is involved.
        let append = query.len() >= self.query.len()
            && query.starts_with(&self.query)
            && !query.contains('!')
            && !query.contains('\\');
        self.nucleo
            .pattern
            .reparse(0, query, CaseMatching::Smart, Normalization::Smart, append);
        self.query.clear();
        self.query.push_str(query);
    }

    /// Lets the background matcher publish results, waiting up to `timeout_ms`.
    /// Returns true while matching is still in progress (call again soon).
    pub fn tick(&mut self, timeout_ms: u64) -> bool {
        self.nucleo.tick(timeout_ms).running
    }

    /// Number of items matching the current query in the latest snapshot.
    pub fn matched_count(&self) -> u32 {
        self.nucleo.snapshot().matched_item_count()
    }

    /// Number of items in the latest snapshot.
    pub fn total_count(&self) -> u32 {
        self.nucleo.snapshot().item_count()
    }

    /// Matches `range` (clamped to the matched count) with highlight indices, best first.
    pub fn matches(&self, range: Range<u32>) -> Vec<PickerMatch<'_, T>> {
        let snapshot = self.nucleo.snapshot();
        let total = snapshot.matched_item_count();
        let start = range.start.min(total);
        let end = range.end.min(total);
        if start >= end {
            return Vec::new();
        }
        let pattern = snapshot.pattern().column_pattern(0);
        let mut matcher = self.matcher.lock();
        let mut buf = Vec::new();
        snapshot
            .matched_items(start..end)
            .map(|item| {
                buf.clear();
                let hay = item.matcher_columns[0].slice(..);
                let score = pattern.indices(hay, &mut matcher, &mut buf).unwrap_or(0);
                buf.sort_unstable();
                buf.dedup();
                PickerMatch {
                    item: item.data,
                    score,
                    indices: buf.clone(),
                }
            })
            .collect()
    }

    /// Removes all items and disconnects previously created injectors.
    pub fn clear(&mut self) {
        self.nucleo.restart(true);
    }
}
