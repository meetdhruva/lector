//! Ranking a picker's candidates for a query with `lector_base::fuzzy` without ever stalling the
//! UI thread (ADR 0005).
//!
//! A list of at most [`INLINE_MAX`] candidates is ranked inline: measured at well under a
//! millisecond (0.3-0.7 ms for 5,000 names in a dev build on the workspace profile). A larger one
//! is ranked on the `lector-app` scheduler under a per-list [`JobKey`], so a newer query cancels
//! the one still running (the ranking checks its [`CancelToken`](lector_base::CancelToken)
//! every 1,024 candidates) and the result comes back in one piece. The caller awaits
//! [`Pending::wait`] in a foreground task and hands the result to [`ListRanker::is_current`]
//! first, so a result that was overtaken is dropped.
//!
//! This module does not touch GPUI and starts no thread of its own.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use lector_app::scheduler::{JobHandle, JobKey, Priority, Scheduler};
use lector_base::fuzzy::{Matcher, Query, Ranked};

/// Lists up to this many candidates are ranked on the calling thread.
pub const INLINE_MAX: usize = 5_000;

/// One ranked row: the candidate's index and the byte offsets to highlight.
pub type Row = (usize, Vec<usize>);

fn rows(ranked: Vec<Ranked>) -> Vec<Row> {
    ranked.into_iter().map(|r| (r.index, r.positions)).collect()
}

/// The outcome of [`ListRanker::set_query`].
pub enum Ranking {
    /// Ranked right away.
    Ready(Vec<Row>),
    /// Ranking runs off the UI thread; await it and check [`ListRanker::is_current`].
    Pending(Pending),
}

/// A ranking in flight.
pub struct Pending {
    generation: u64,
    handle: JobHandle<Option<Vec<Ranked>>>,
}

impl Pending {
    /// Waits for the ranking. Returns the query generation it was started for and the rows, or
    /// `None` rows when it was cancelled by a newer query.
    pub async fn wait(self) -> (u64, Option<Vec<Row>>) {
        let generation = self.generation;
        let ranked = self.handle.into_future().await.flatten();
        (generation, ranked.map(rows))
    }
}

/// Ranks the (fixed) candidates of one picker for successive queries.
pub struct ListRanker<T: Send + Sync + 'static> {
    items: Arc<Vec<T>>,
    text: fn(&T) -> &str,
    limit: usize,
    key: JobKey,
    matcher: Matcher,
    generation: u64,
}

impl<T: Send + Sync + 'static> ListRanker<T> {
    /// A ranker over `items`, matching the text `text(item)` and keeping the best `limit`.
    pub fn new(items: Arc<Vec<T>>, text: fn(&T) -> &str, limit: usize) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        ListRanker {
            items,
            text,
            limit,
            key: JobKey::new(format!(
                "picker-rank:{}",
                NEXT.fetch_add(1, Ordering::Relaxed)
            )),
            matcher: Matcher::new(),
            generation: 0,
        }
    }

    /// Whether ranking this list happens off the UI thread.
    pub fn is_off_thread(&self) -> bool {
        self.items.len() > INLINE_MAX
    }

    /// Starts ranking for `query`, superseding any earlier query.
    pub fn set_query(&mut self, query: &str) -> Ranking {
        self.generation += 1;
        let query = Query::new(query);
        if !self.is_off_thread() {
            return Ranking::Ready(rows(self.matcher.rank_by(
                &query,
                &self.items,
                self.text,
                self.limit,
            )));
        }
        let (items, text, limit) = (self.items.clone(), self.text, self.limit);
        let handle = Scheduler::global().spawn(
            Priority::Interactive,
            Some(self.key.clone()),
            move |cancel| {
                Matcher::new()
                    .rank_by_cancellable(&query, &items, text, limit, cancel)
                    .ok()
            },
        );
        Ranking::Pending(Pending {
            generation: self.generation,
            handle,
        })
    }

    /// Drops the work for the current query (a ranking in flight is cancelled and its result is
    /// no longer current), e.g. when the query becomes empty.
    pub fn cancel(&mut self) {
        self.generation += 1;
        if self.is_off_thread() {
            Scheduler::global().cancel_key(&self.key);
        }
    }

    /// Whether a result tagged `generation` is still the answer to the latest query.
    pub fn is_current(&self, generation: u64) -> bool {
        generation == self.generation
    }
}

impl<T: Send + Sync + 'static> Drop for ListRanker<T> {
    fn drop(&mut self) {
        if self.is_off_thread() {
            Scheduler::global().cancel_key(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::ThreadId;

    fn names(n: usize) -> Arc<Vec<String>> {
        Arc::new((0..n).map(|i| format!("item_{i}_value")).collect())
    }

    fn text(s: &String) -> &str {
        s
    }

    fn wait(p: Pending) -> (u64, Option<Vec<Row>>) {
        futures::executor::block_on(p.wait())
    }

    #[test]
    fn small_lists_are_ranked_inline() {
        let mut r = ListRanker::new(names(INLINE_MAX), text, 10);
        assert!(!r.is_off_thread());
        let Ranking::Ready(rows) = r.set_query("item_42_") else {
            panic!("expected an inline ranking");
        };
        assert_eq!(rows.first().map(|r| r.0), Some(42));
    }

    #[test]
    fn a_300k_list_is_ranked_off_the_calling_thread() {
        static WORKER: std::sync::Mutex<Option<ThreadId>> = std::sync::Mutex::new(None);
        fn spying_text(s: &String) -> &str {
            *WORKER.lock().unwrap() = Some(std::thread::current().id());
            s
        }
        let mut r = ListRanker::new(names(300_000), spying_text, 500);
        assert!(r.is_off_thread());
        let started = std::time::Instant::now();
        let Ranking::Pending(p) = r.set_query("value 299") else {
            panic!("a 300k list must not be ranked inline");
        };
        // Handing the work off is cheap; the ranking itself takes much longer than that.
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
        let (generation, rows) = wait(p);
        assert!(r.is_current(generation));
        let rows = rows.expect("not cancelled");
        assert!(!rows.is_empty());
        let on = WORKER.lock().unwrap().expect("the scorer ran");
        assert_ne!(on, std::thread::current().id());
        // Same answer as ranking inline.
        let items = names(300_000);
        let inline = Matcher::new().rank_by(&Query::new("value 299"), &items, text, 500);
        assert_eq!(rows, super::rows(inline));
    }

    #[test]
    fn a_newer_query_supersedes_an_older_one() {
        let mut r = ListRanker::new(names(300_000), text, 500);
        let Ranking::Pending(first) = r.set_query("item") else {
            panic!("off thread");
        };
        let Ranking::Pending(second) = r.set_query("item_7_val") else {
            panic!("off thread");
        };
        let (g2, rows2) = wait(second);
        assert!(r.is_current(g2));
        let rows2 = rows2.expect("the newest query completes");
        assert_eq!(rows2.first().map(|r| r.0), Some(7));
        // The older one was cancelled (or, if it had finished first, is not current): either
        // way its result can never replace the newer one.
        let (g1, rows1) = wait(first);
        assert!(!r.is_current(g1));
        assert!(g1 < g2);
        let _ = rows1;
    }

    #[test]
    fn cancel_makes_pending_results_stale() {
        let mut r = ListRanker::new(names(300_000), text, 500);
        let Ranking::Pending(p) = r.set_query("item") else {
            panic!("off thread");
        };
        r.cancel();
        let (generation, _) = wait(p);
        assert!(!r.is_current(generation));
    }
}
