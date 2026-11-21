//! Content-addressed caches for derived data, so browsing history and review
//! is instant the second time.
//!
//! [`ContentCache`] holds four thread-safe LRU caches with byte budgets, keyed
//! by *what the content is* (a git blob id, or a hash of the bytes), never by
//! path or revision:
//!
//! * parsed [`Syntax`] trees (per language),
//! * outlines and tags of a tree,
//! * line diffs (a pair of content ids plus the diff options).
//!
//! Sizes are estimates (tree-sitter does not report its memory). Hit/miss
//! statistics are available through [`ContentCache::stats`]. Computation runs
//! outside the locks, so two threads may compute the same value concurrently;
//! the first stored result wins.
//!
//! The stateless `lector-review` functions take a [`DerivedCache`]; the app passes
//! [`ContentCache::global`], which implements it.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::sync::{Arc, OnceLock};

use lector_diff::{DiffOptions, LineDiff};
use lector_review::{ContentId, DerivedCache};
use lector_syntax::{Language, OutlineItem, Syntax, SyntaxError, Tag};
use lector_text::Text;
use parking_lot::Mutex;

/// Byte budgets of a [`ContentCache`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheConfig {
    /// Budget for syntax trees (estimated size).
    pub syntax_bytes: usize,
    /// Budget for outlines.
    pub outline_bytes: usize,
    /// Budget for tags.
    pub tags_bytes: usize,
    /// Budget for line diffs.
    pub diff_bytes: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            syntax_bytes: 192 << 20,
            outline_bytes: 16 << 20,
            tags_bytes: 16 << 20,
            diff_bytes: 32 << 20,
        }
    }
}

/// Hit/miss/size statistics of one cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KindStats {
    /// Lookups answered from the cache.
    pub hits: u64,
    /// Lookups that had to compute.
    pub misses: u64,
    /// Entries dropped to stay within the budget.
    pub evictions: u64,
    /// Entries currently stored.
    pub entries: usize,
    /// Estimated bytes currently stored.
    pub bytes: usize,
    /// Budget in bytes.
    pub budget: usize,
}

impl KindStats {
    /// `hits / (hits + misses)`, 0 without lookups.
    pub fn hit_rate(&self) -> f64 {
        let n = self.hits + self.misses;
        if n == 0 {
            0.0
        } else {
            self.hits as f64 / n as f64
        }
    }
}

/// Statistics of all caches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Syntax trees.
    pub syntax: KindStats,
    /// Outlines.
    pub outline: KindStats,
    /// Tags.
    pub tags: KindStats,
    /// Line diffs.
    pub diffs: KindStats,
}

struct Slot<V> {
    value: V,
    size: usize,
    tick: u64,
}

/// LRU with a byte budget (`BTreeMap` of recency ticks: O(log n) per operation).
struct Lru<K, V> {
    map: HashMap<K, Slot<V>>,
    order: BTreeMap<u64, K>,
    tick: u64,
    stats: KindStats,
}

impl<K: Clone + Eq + Hash, V: Clone> Lru<K, V> {
    fn new(budget: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: BTreeMap::new(),
            tick: 0,
            stats: KindStats {
                budget,
                ..KindStats::default()
            },
        }
    }

    fn get(&mut self, key: &K) -> Option<V> {
        self.tick += 1;
        let tick = self.tick;
        match self.map.get_mut(key) {
            Some(slot) => {
                self.order.remove(&slot.tick);
                slot.tick = tick;
                self.order.insert(tick, key.clone());
                self.stats.hits += 1;
                Some(slot.value.clone())
            }
            None => {
                self.stats.misses += 1;
                None
            }
        }
    }

    fn insert(&mut self, key: K, value: V, size: usize) {
        if size > self.stats.budget {
            return;
        }
        self.tick += 1;
        if let Some(old) = self.map.remove(&key) {
            self.order.remove(&old.tick);
            self.stats.bytes -= old.size;
        }
        self.order.insert(self.tick, key.clone());
        self.map.insert(
            key,
            Slot {
                value,
                size,
                tick: self.tick,
            },
        );
        self.stats.bytes += size;
        while self.stats.bytes > self.stats.budget {
            let Some((&oldest, _)) = self.order.iter().next() else {
                break;
            };
            if let Some(k) = self.order.remove(&oldest) {
                if let Some(slot) = self.map.remove(&k) {
                    self.stats.bytes -= slot.size;
                    self.stats.evictions += 1;
                }
            }
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.stats.bytes = 0;
    }

    fn stats(&self) -> KindStats {
        KindStats {
            entries: self.map.len(),
            ..self.stats
        }
    }

    fn reset_stats(&mut self) {
        self.stats.hits = 0;
        self.stats.misses = 0;
        self.stats.evictions = 0;
    }
}

type LangKey = (ContentId, String);

struct Inner {
    syntax: Mutex<Lru<LangKey, Syntax>>,
    outline: Mutex<Lru<LangKey, Arc<Vec<OutlineItem>>>>,
    tags: Mutex<Lru<LangKey, Arc<Vec<Tag>>>>,
    diffs: Mutex<Lru<(ContentId, ContentId, DiffOptions), Arc<LineDiff>>>,
}

/// Thread-safe, content-addressed caches (see the module docs). Cheap to
/// clone: clones share the same storage.
#[derive(Clone)]
pub struct ContentCache {
    inner: Arc<Inner>,
}

impl Default for ContentCache {
    fn default() -> Self {
        Self::new(CacheConfig::default())
    }
}

impl std::fmt::Debug for ContentCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentCache")
            .field("stats", &self.stats())
            .finish()
    }
}

impl ContentCache {
    /// Creates caches with the given budgets.
    pub fn new(config: CacheConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                syntax: Mutex::new(Lru::new(config.syntax_bytes)),
                outline: Mutex::new(Lru::new(config.outline_bytes)),
                tags: Mutex::new(Lru::new(config.tags_bytes)),
                diffs: Mutex::new(Lru::new(config.diff_bytes)),
            }),
        }
    }

    /// The process-wide cache the app passes to the `lector-review` functions.
    pub fn global() -> &'static ContentCache {
        static GLOBAL: OnceLock<ContentCache> = OnceLock::new();
        GLOBAL.get_or_init(ContentCache::default)
    }

    /// Cached `Syntax::parse(lang, text)` for the content `id`.
    pub fn syntax(
        &self,
        id: ContentId,
        lang: &Arc<Language>,
        text: &Text,
    ) -> Result<Syntax, SyntaxError> {
        let key = (id, lang.name.clone());
        if let Some(hit) = self.inner.syntax.lock().get(&key) {
            return Ok(hit);
        }
        let syntax = Syntax::parse(lang, text)?;
        self.inner
            .syntax
            .lock()
            .insert(key, syntax.clone(), text.len() * 12 + 8 * 1024);
        Ok(syntax)
    }

    /// Cached outline of the tree of content `id`.
    pub fn outline(&self, id: ContentId, syntax: &Syntax, text: &Text) -> Arc<Vec<OutlineItem>> {
        let key = (id, syntax.language().name.clone());
        if let Some(hit) = self.inner.outline.lock().get(&key) {
            return hit;
        }
        let items = Arc::new(syntax.outline(text));
        let size = 64 + items.iter().map(|i| 96 + i.name.len()).sum::<usize>();
        self.inner.outline.lock().insert(key, items.clone(), size);
        items
    }

    /// Cached tags of the tree of content `id`.
    pub fn tags(&self, id: ContentId, syntax: &Syntax, text: &Text) -> Arc<Vec<Tag>> {
        let key = (id, syntax.language().name.clone());
        if let Some(hit) = self.inner.tags.lock().get(&key) {
            return hit;
        }
        let tags = Arc::new(syntax.tags(text));
        let size = 64 + tags.iter().map(|t| 80 + t.name.len()).sum::<usize>();
        self.inner.tags.lock().insert(key, tags.clone(), size);
        tags
    }

    /// Cached line diff between two contents; `compute` runs on a miss.
    pub fn line_diff(
        &self,
        old: ContentId,
        new: ContentId,
        opts: &DiffOptions,
        compute: impl FnOnce() -> LineDiff,
    ) -> Arc<LineDiff> {
        let key = (old, new, *opts);
        if let Some(hit) = self.inner.diffs.lock().get(&key) {
            return hit;
        }
        let diff = Arc::new(compute());
        let size = 96 + diff.hunks.len() * 48;
        self.inner.diffs.lock().insert(key, diff.clone(), size);
        diff
    }

    /// Statistics of all caches.
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            syntax: self.inner.syntax.lock().stats(),
            outline: self.inner.outline.lock().stats(),
            tags: self.inner.tags.lock().stats(),
            diffs: self.inner.diffs.lock().stats(),
        }
    }

    /// Zeroes the hit/miss/eviction counters (entries stay).
    pub fn reset_stats(&self) {
        self.inner.syntax.lock().reset_stats();
        self.inner.outline.lock().reset_stats();
        self.inner.tags.lock().reset_stats();
        self.inner.diffs.lock().reset_stats();
    }

    /// Drops every entry.
    pub fn clear(&self) {
        self.inner.syntax.lock().clear();
        self.inner.outline.lock().clear();
        self.inner.tags.lock().clear();
        self.inner.diffs.lock().clear();
    }
}

impl DerivedCache for ContentCache {
    fn syntax(
        &self,
        id: ContentId,
        lang: &Arc<Language>,
        text: &Text,
    ) -> Result<Syntax, SyntaxError> {
        ContentCache::syntax(self, id, lang, text)
    }

    fn line_diff(
        &self,
        old: ContentId,
        new: ContentId,
        opts: &DiffOptions,
        compute: &dyn Fn() -> LineDiff,
    ) -> LineDiff {
        (*ContentCache::line_diff(self, old, new, opts, compute)).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lector_syntax::LanguageRegistry;
    use proptest::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn lru_evicts_least_recently_used_within_budget() {
        let mut lru: Lru<u32, u32> = Lru::new(100);
        lru.insert(1, 10, 40);
        lru.insert(2, 20, 40);
        assert_eq!(lru.get(&1), Some(10)); // 1 is now newer than 2
        lru.insert(3, 30, 40); // evicts 2
        assert_eq!(lru.get(&2), None);
        assert_eq!(lru.get(&1), Some(10));
        assert_eq!(lru.get(&3), Some(30));
        let s = lru.stats();
        assert_eq!((s.entries, s.bytes, s.evictions), (2, 80, 1));
        // Too big for the budget: not stored.
        lru.insert(9, 9, 1000);
        assert_eq!(lru.get(&9), None);
        // Replacing an entry updates the byte count.
        lru.insert(1, 11, 10);
        assert_eq!(lru.stats().bytes, 50);
    }

    proptest! {
        #[test]
        fn lru_never_exceeds_budget(ops in proptest::collection::vec((0u32..20, 1usize..60, any::<bool>()), 1..200)) {
            let mut lru: Lru<u32, u32> = Lru::new(150);
            for (k, size, get) in ops {
                if get { let _ = lru.get(&k); } else { lru.insert(k, k, size); }
                let s = lru.stats();
                prop_assert!(s.bytes <= 150);
                prop_assert_eq!(s.bytes, lru.map.values().map(|v| v.size).sum::<usize>());
                prop_assert_eq!(lru.map.len(), lru.order.len());
            }
        }
    }

    #[test]
    fn syntax_outline_tags_are_cached() {
        let cache = ContentCache::new(CacheConfig::default());
        let langs = LanguageRegistry::new();
        let lang = langs.by_name("rust").unwrap();
        let text = Text::from("pub fn alpha() {}\nfn beta() { alpha(); }\n");
        let id = ContentId::of_text(&text);
        let s1 = cache.syntax(id, &lang, &text).unwrap();
        let s2 = cache.syntax(id, &lang, &text).unwrap();
        assert_eq!(s1.debug_sexp(), s2.debug_sexp());
        let o1 = cache.outline(id, &s1, &text);
        let o2 = cache.outline(id, &s1, &text);
        assert!(Arc::ptr_eq(&o1, &o2));
        assert_eq!(o1.len(), 2);
        let t1 = cache.tags(id, &s1, &text);
        assert!(Arc::ptr_eq(&t1, &cache.tags(id, &s1, &text)));
        let st = cache.stats();
        assert_eq!((st.syntax.hits, st.syntax.misses), (1, 1));
        assert_eq!((st.outline.hits, st.outline.misses), (1, 1));
        assert!((st.syntax.hit_rate() - 0.5).abs() < 1e-9);
        // Different language key is a different entry.
        let other = langs.by_name("typescript").unwrap();
        cache.syntax(id, &other, &text).unwrap();
        assert_eq!(cache.stats().syntax.misses, 2);
        cache.clear();
        assert_eq!(cache.stats().syntax.entries, 0);
    }

    #[test]
    fn line_diff_computes_once() {
        let cache = ContentCache::default();
        let calls = AtomicUsize::new(0);
        let (a, b) = (ContentId::of_bytes(b"a\n"), ContentId::of_bytes(b"b\n"));
        let opts = DiffOptions::default();
        for _ in 0..3 {
            let d = cache.line_diff(a, b, &opts, || {
                calls.fetch_add(1, Ordering::SeqCst);
                lector_diff::diff_lines("a\n", "b\n", &opts)
            });
            assert_eq!(d.stats().added, 1);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Options are part of the key.
        let ws = DiffOptions {
            whitespace: lector_diff::Whitespace::IgnoreAll,
            ..opts
        };
        cache.line_diff(a, b, &ws, || {
            calls.fetch_add(1, Ordering::SeqCst);
            LineDiff::default()
        });
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(cache.stats().diffs.hits, 2);
    }

    #[test]
    fn concurrent_use_is_safe() {
        let cache = ContentCache::default();
        let langs = LanguageRegistry::new();
        let lang = langs.by_name("rust").unwrap();
        std::thread::scope(|s| {
            for t in 0..4 {
                let (cache, lang) = (&cache, &lang);
                s.spawn(move || {
                    for i in 0..20 {
                        let text = Text::from(format!("fn f{}() {{}}\n", (i + t) % 5));
                        let id = ContentId::of_text(&text);
                        let syn = cache.syntax(id, lang, &text).unwrap();
                        assert_eq!(cache.outline(id, &syn, &text).len(), 1);
                    }
                });
            }
        });
        let st = cache.stats();
        assert_eq!(st.syntax.hits + st.syntax.misses, 80);
        assert!(st.syntax.entries <= 5);
    }

    /// The `lector-review` seam: the app's cache and `NoCache` give identical file diffs, on a
    /// miss and on a hit, and the review functions really go through the cache.
    #[test]
    fn cached_and_uncached_file_diffs_are_identical() {
        use lector_git::{ChangeKind, FileChange, Side};
        use lector_review::review::{diff_bytes, side_from_bytes, FileDiff};
        use lector_review::NoCache;

        let langs = LanguageRegistry::new();
        let old = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n";
        let new = "fn main() {\n    let x = 2;\n    println!(\"{x}\");\n}\nfn added() {}\n";
        let change = FileChange {
            old_path: Some("lib.rs".into()),
            new_path: Some("lib.rs".into()),
            kind: ChangeKind::Modified,
            old: Side::Workdir("lib.rs".into()),
            new: Side::Workdir("lib.rs".into()),
            binary: None,
        };
        let opts = DiffOptions::default();
        let diff = |cache: &dyn DerivedCache| {
            let (o, n) = (old.as_bytes().to_vec(), new.as_bytes().to_vec());
            diff_bytes(change.clone(), Some(o), Some(n), &langs, &opts, cache)
        };
        let view = |fd: &FileDiff| {
            let sexp =
                |s: &lector_review::review::DiffSide| s.syntax.as_ref().map(|t| t.debug_sexp());
            (
                fd.kind.clone(),
                fd.diff.clone(),
                fd.split.clone(),
                fd.unified.clone(),
                (sexp(&fd.old), sexp(&fd.new)),
                (fd.old.line_highlights(0..5), fd.new.line_highlights(0..6)),
                fd.inline_for_old(1).map(|i| (*i).clone()),
            )
        };
        let cache = ContentCache::default();
        let uncached = view(&diff(&NoCache));
        assert_eq!(view(&diff(&cache)), uncached, "miss");
        assert_eq!(view(&diff(&cache)), uncached, "hit");
        let st = cache.stats();
        assert_eq!((st.syntax.misses, st.syntax.hits), (2, 2));
        assert_eq!((st.diffs.misses, st.diffs.hits), (1, 1));

        let side = |cache: &dyn DerivedCache| {
            let s = side_from_bytes("lib.rs", old.as_bytes().to_vec(), &langs, cache);
            (s.syntax.as_ref().map(|t| t.debug_sexp()), s.line_count())
        };
        assert_eq!(side(&cache), side(&NoCache));
        assert_eq!(cache.stats().syntax.hits, 3);
    }
}
