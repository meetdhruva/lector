//! The file finder's candidate list: `lector_search::Picker` (nucleo, path-tuned) keyed by the
//! relative path, fed from a background thread. File ranking is the picker's business (ADR 0005:
//! the picker keeps its own index and scoring); other pickers use `lector_base::fuzzy`.

use std::sync::Arc;

/// One ranked match of a [`PathList`].
#[derive(Clone, Debug)]
pub(super) struct PathMatch {
    /// The relative path.
    pub item: Arc<str>,
    /// Matched byte positions in the path.
    pub positions: Vec<usize>,
}

/// A ranked list of relative paths backed by `lector_search::Picker`: paths can be pushed from
/// any thread; matching runs on the picker's worker threads.
pub(super) struct PathList {
    picker: lector_search::Picker<Arc<str>>,
    query: String,
    /// Counts the queries set so far; see [`PathList::stamp`].
    queries: u64,
}

impl PathList {
    /// Empty list.
    pub fn new() -> Self {
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        PathList {
            picker: lector_search::Picker::new_for_paths(notify),
            query: String::new(),
            queries: 0,
        }
    }

    /// A sendable handle that adds paths.
    pub fn injector(&self) -> lector_search::PickerInjector<Arc<str>> {
        self.picker.injector()
    }

    /// Sets the query (nucleo syntax).
    pub fn set_query(&mut self, query: &str) {
        if query != self.query {
            self.picker.set_query(query);
            self.query = query.to_string();
            self.queries += 1;
        }
    }

    /// Identifies the current query: `Some(stamp)` when the matcher has published results for
    /// it (what [`PathList::top`] returns now answers it, possibly only the first part of a
    /// pass still running), `None` while it still returns those of an earlier query. A caller
    /// that keeps rows compares the stamp it got when it read them with the stamp now.
    pub fn stamp(&self) -> Option<u64> {
        self.picker.progress().current.then_some(self.queries)
    }

    /// The number of the latest query, to tell whether rows read under a stamp are still the
    /// answer to it.
    pub fn latest(&self) -> u64 {
        self.queries
    }

    /// Lets matching progress for up to `ms`; true while still running.
    pub fn tick(&mut self, ms: u64) -> bool {
        self.picker.tick(ms)
    }

    /// Number of matches / paths.
    pub fn counts(&self) -> (u32, u32) {
        (self.picker.matched_count(), self.picker.total_count())
    }

    /// The best `limit` matches with byte positions.
    pub fn top(&self, limit: u32) -> Vec<PathMatch> {
        self.picker
            .matches(0..limit)
            .into_iter()
            .map(|m| PathMatch {
                item: m.item.clone(),
                positions: char_to_byte_positions(m.item, &m.indices),
            })
            .collect()
    }
}

/// Converts sorted char indices in `text` to byte offsets.
fn char_to_byte_positions(text: &str, chars: &[u32]) -> Vec<usize> {
    let mut out = Vec::with_capacity(chars.len());
    let mut wanted = chars.iter().peekable();
    for (ci, (bi, _)) in text.char_indices().enumerate() {
        match wanted.peek() {
            Some(&&w) if w as usize == ci => {
                out.push(bi);
                wanted.next();
            }
            Some(_) => {}
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_and_positions() {
        let mut list = PathList::new();
        let injector = list.injector();
        for p in ["src/main.rs", "crates/x/src/lib.rs", "README.md"] {
            injector.push(Arc::from(p), p);
        }
        list.set_query("main");
        let start = std::time::Instant::now();
        while list.tick(5) && start.elapsed().as_millis() < 500 {}
        let top = list.top(10);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].item.as_ref(), "src/main.rs");
        assert_eq!(top[0].positions, vec![4, 5, 6, 7]);
        assert_eq!(char_to_byte_positions("aé b", &[0, 1, 3]), vec![0, 1, 4]);
    }
}
