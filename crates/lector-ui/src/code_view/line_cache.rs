//! LRU cache of shaped lines keyed by a hash of (text, runs, font size).

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use gpui::{Pixels, ShapedLine, TextRun};

/// Shaped-line cache. Entries untouched for the longest are evicted when
/// the cache exceeds its capacity.
pub(crate) struct LineCache {
    entries: HashMap<u64, (ShapedLine, u64)>,
    capacity: usize,
    clock: u64,
    hits: u64,
    misses: u64,
}

/// Cache key for a line shaped with `runs` at `font_size`.
pub(crate) fn line_key(text: &str, runs: &[TextRun], font_size: Pixels) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    f32::from(font_size).to_bits().hash(&mut h);
    for run in runs {
        run.len.hash(&mut h);
        run.font.family.hash(&mut h);
        run.font.features.0.len().hash(&mut h);
        run.font.weight.0.to_bits().hash(&mut h);
        (run.font.style as u8).hash(&mut h);
        run.color.h.to_bits().hash(&mut h);
        run.color.s.to_bits().hash(&mut h);
        run.color.l.to_bits().hash(&mut h);
        run.color.a.to_bits().hash(&mut h);
        run.underline.is_some().hash(&mut h);
    }
    h.finish()
}

impl LineCache {
    pub(crate) fn new(capacity: usize) -> Self {
        LineCache {
            entries: HashMap::with_capacity(capacity),
            capacity,
            clock: 0,
            hits: 0,
            misses: 0,
        }
    }

    /// Returns the cached line for `key` (verifying the text), or shapes it
    /// with `shape` and caches the result.
    pub(crate) fn get_or_shape(
        &mut self,
        key: u64,
        text: &str,
        shape: impl FnOnce() -> ShapedLine,
    ) -> ShapedLine {
        self.clock += 1;
        if let Some((line, used)) = self.entries.get_mut(&key) {
            if line.text.as_ref() == text {
                *used = self.clock;
                self.hits += 1;
                return line.clone();
            }
        }
        self.misses += 1;
        let line = shape();
        self.entries.insert(key, (line.clone(), self.clock));
        if self.entries.len() > self.capacity {
            self.evict();
        }
        line
    }

    fn evict(&mut self) {
        // Drop the least recently used half.
        let mut ages: Vec<u64> = self.entries.values().map(|(_, used)| *used).collect();
        let mid = ages.len() / 2;
        let (_, cutoff, _) = ages.select_nth_unstable(mid);
        let cutoff = *cutoff;
        self.entries.retain(|_, (_, used)| *used > cutoff);
    }

    /// `(hits, misses)` since creation.
    #[allow(dead_code)]
    pub(crate) fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }
}
