//! Ranking a picker's candidates for a query with `lector_base::fuzzy` without ever stalling the
//! UI thread (ADR 0005).
//!
//! What may run on the calling thread is decided by the **work** the ranking does, not by the
//! number of candidates (the cost is queried characters x candidate characters): the ranking is
//! attempted inline under [`INTERACTIVE_WORK_BUDGET`] (about 1 to 1.6 ms in a dev build, see
//! `lector_base::fuzzy`), and when it would exceed that, or obviously will, it runs on the
//! `lector-app` scheduler under a per-list [`JobKey`], so a newer query cancels the one still
//! running (the ranking checks its [`CancelToken`](lector_base::CancelToken) every
//! `CANCEL_CHECK_WORK` units of work) and the result comes back in one piece. The caller awaits
//! [`Pending::wait`] in a foreground task and hands the result to [`ListRanker::land`], so a
//! result that was overtaken is dropped.
//!
//! While a ranking is pending the owner still shows the rows of the previous query. They must
//! not be confirmed: [`ListRanker::confirm_or_defer`] holds an Enter back until the rows of the
//! latest query have landed ([`ListRanker::take_deferred_confirm`]), and drops it when the query
//! changes again or the picker is dismissed.
//!
//! This module does not touch GPUI and starts no thread of its own.

use std::sync::Arc;

use lector_app::scheduler::{JobHandle, JobKey, JobKind, Priority, Scheduler, Subject};
use lector_base::fuzzy::{Matcher, Query, Ranked, INTERACTIVE_WORK_BUDGET, WORK_PER_CANDIDATE};

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
    /// The generation whose rows the owner is showing (see [`ListRanker::land`]).
    landed: u64,
    /// An Enter arrived while the rows were still those of an older query.
    confirm_waiting: bool,
    /// A job may be running or queued under `key` (it was started and not cancelled since).
    spawned: bool,
    /// The last query that did not fit the inline budget: queries that extend it are handed to
    /// the scheduler without a wasted inline attempt.
    over_budget: Option<Query>,
}

impl<T: Send + Sync + 'static> ListRanker<T> {
    /// A ranker over `items`, matching the text `text(item)` and keeping the best `limit`.
    pub fn new(items: Arc<Vec<T>>, text: fn(&T) -> &str, limit: usize) -> Self {
        ListRanker {
            items,
            text,
            limit,
            key: JobKey::new(JobKind::PickerRank, Subject::unique()),
            matcher: Matcher::new(),
            generation: 0,
            landed: 0,
            confirm_waiting: false,
            spawned: false,
            over_budget: None,
        }
    }

    /// Starts ranking for `query`, superseding any earlier query. Inline when it fits the work
    /// budget, otherwise on the scheduler.
    pub fn set_query(&mut self, query: &str) -> Ranking {
        self.generation += 1;
        self.confirm_waiting = false;
        let query = Query::new(query);
        if let Some(rows) = self.rank_inline(&query) {
            self.landed = self.generation;
            return Ranking::Ready(rows);
        }
        let (items, text, limit) = (self.items.clone(), self.text, self.limit);
        self.spawned = true;
        let handle = Scheduler::global().spawn(
            self.key.clone(),
            Priority::Interactive,
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

    /// Ranks on the calling thread if that fits the budget; `None` when it does not (nothing
    /// is kept, at most the budget was spent).
    fn rank_inline(&mut self, query: &Query) -> Option<Vec<Row>> {
        let hopeless = !query.is_empty()
            && (self.items.len() as u64).saturating_mul(WORK_PER_CANDIDATE)
                > INTERACTIVE_WORK_BUDGET;
        let extends_a_slow_query = self
            .over_budget
            .as_ref()
            .is_some_and(|slow| slow.is_prefix_of(query));
        if hopeless || extends_a_slow_query {
            self.over_budget = Some(query.clone());
            return None;
        }
        let ranked = self.matcher.rank_by_budgeted(
            query,
            &self.items,
            self.text,
            self.limit,
            INTERACTIVE_WORK_BUDGET,
        );
        self.over_budget = match ranked {
            Some(_) => None,
            None => Some(query.clone()),
        };
        ranked.map(rows)
    }

    /// Drops the work for the current query (a ranking in flight is cancelled and its result is
    /// no longer current), e.g. when the query becomes empty. The caller shows its own rows for
    /// that state right away, so they count as landed.
    pub fn cancel(&mut self) {
        self.generation += 1;
        self.landed = self.generation;
        self.confirm_waiting = false;
        self.cancel_job();
    }

    /// The picker was dismissed: cancels the work in flight and any deferred Enter, and makes
    /// every pending result stale.
    pub fn dismiss(&mut self) {
        self.generation += 1;
        self.confirm_waiting = false;
        self.cancel_job();
    }

    fn cancel_job(&mut self) {
        if std::mem::take(&mut self.spawned) {
            Scheduler::global().cancel_key(&self.key);
        }
    }

    /// Whether a result tagged `generation` is still the answer to the latest query.
    pub fn is_current(&self, generation: u64) -> bool {
        generation == self.generation
    }

    /// Accepts the result tagged `generation` if it still answers the latest query and records
    /// that the owner now shows its rows. Returns `false` (and changes nothing) for a result that
    /// was overtaken: drop it.
    pub fn land(&mut self, generation: u64) -> bool {
        if !self.is_current(generation) {
            return false;
        }
        self.landed = generation;
        true
    }

    /// Whether the rows the owner shows answer the latest query. They do not between
    /// [`ListRanker::set_query`] returning [`Ranking::Pending`] and [`ListRanker::land`]: the
    /// rows of the previous query are still on screen then.
    pub fn rows_are_current(&self) -> bool {
        self.landed == self.generation
    }

    /// For the owner's confirm (Enter). Returns `true` when the rows shown answer the latest
    /// query and the selection may be confirmed now. Otherwise the Enter is remembered and
    /// `false` is returned: confirm nothing yet. When the new rows land
    /// ([`ListRanker::take_deferred_confirm`]) the Enter applies to them, unless the query
    /// changed again or the picker was dismissed first.
    pub fn confirm_or_defer(&mut self) -> bool {
        if self.rows_are_current() {
            return true;
        }
        self.confirm_waiting = true;
        false
    }

    /// Whether an Enter is waiting for the rows just landed; clears it.
    pub fn take_deferred_confirm(&mut self) -> bool {
        std::mem::take(&mut self.confirm_waiting)
    }
}

impl<T: Send + Sync + 'static> Drop for ListRanker<T> {
    fn drop(&mut self) {
        self.cancel_job();
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

    /// `n` names of 256 characters made of `a`: the worst case for the scorer.
    fn long_names(n: usize) -> Arc<Vec<String>> {
        Arc::new(
            (0..n)
                .map(|i| format!("{}{}", "a".repeat(250), i % 1000))
                .collect(),
        )
    }

    #[test]
    fn long_names_and_long_queries_are_not_ranked_on_the_calling_thread() {
        // 5,000 names is not "small" for the scorer when they are 256 characters long: the cost
        // is queried characters x name characters, not the number of names. Whatever the query
        // length (an alignment up to 64 characters, a substring search beyond), ranking either
        // finishes inline within the budget or is handed to the scheduler, and the calling
        // thread is never held for long.
        for qlen in [6, 64, 100, 200] {
            let q = "a".repeat(qlen);
            let mut r = ListRanker::new(long_names(5_000), text, 500);
            let started = std::time::Instant::now();
            let ranking = r.set_query(&q);
            let held = started.elapsed();
            assert!(
                held < std::time::Duration::from_millis(40),
                "{qlen}: {held:?}"
            );
            let Ranking::Pending(pending) = ranking else {
                panic!("{qlen}-character query on 5,000 x 256 characters was ranked inline");
            };
            // The scheduler's answer is the plain ranking.
            let (generation, rows) = wait(pending);
            assert!(r.land(generation));
            let items = long_names(5_000);
            let inline = Matcher::new().rank_by(&Query::new(&q), &items, text, 500);
            assert_eq!(rows.expect("not cancelled"), super::rows(inline), "{qlen}");
        }
    }

    #[test]
    fn what_fits_the_budget_stays_inline_whatever_the_count() {
        // 5,000 short names and a short query: well inside the budget.
        let mut r = ListRanker::new(names(5_000), text, 10);
        assert!(matches!(r.set_query("it"), Ranking::Ready(_)));
        // The empty query keeps the first rows and costs nothing, even for a huge list.
        let mut r = ListRanker::new(names(300_000), text, 10);
        let Ranking::Ready(rows) = r.set_query("") else {
            panic!("the empty query is free");
        };
        assert_eq!(rows.len(), 10);
        assert!(r.rows_are_current());
    }

    #[test]
    fn a_query_that_extends_one_over_budget_is_not_tried_inline_again() {
        thread_local! {
            /// Candidates read on this (the calling) thread: an inline attempt reads them.
            static READ_HERE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        }
        fn counting_text(s: &String) -> &str {
            READ_HERE.with(|n| n.set(n.get() + 1));
            s
        }
        let mut r = ListRanker::new(long_names(5_000), counting_text, 500);
        let Ranking::Pending(first) = r.set_query(&"a".repeat(70)) else {
            panic!("over budget");
        };
        assert!(
            READ_HERE.with(|n| n.get()) > 0,
            "the first query is tried inline"
        );
        READ_HERE.with(|n| n.set(0));
        let Ranking::Pending(second) = r.set_query(&"a".repeat(71)) else {
            panic!("still over budget");
        };
        // No wasted inline attempt: not one candidate was read on this thread.
        assert_eq!(READ_HERE.with(|n| n.get()), 0);
        // A query that does not extend it is tried again.
        let Ranking::Pending(third) = r.set_query("b") else {
            panic!("rejecting 5,000 names of 256 characters is still over budget");
        };
        assert!(
            READ_HERE.with(|n| n.get()) > 0,
            "a new query is tried inline"
        );
        let _ = (wait(first), wait(second), wait(third));
    }

    #[test]
    fn small_lists_are_ranked_inline() {
        let mut r = ListRanker::new(names(5_000), text, 10);
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
    fn rows_are_current_only_once_the_pending_ranking_has_landed() {
        let mut r = ListRanker::new(names(300_000), text, 500);
        assert!(r.rows_are_current(), "nothing was asked yet");
        let Ranking::Pending(p) = r.set_query("item_1") else {
            panic!("off thread");
        };
        assert!(!r.rows_are_current());
        let (generation, rows) = wait(p);
        assert!(rows.is_some());
        assert!(r.land(generation));
        assert!(r.rows_are_current());
        // A small list is current at once.
        let mut small = ListRanker::new(names(10), text, 5);
        let Ranking::Ready(_) = small.set_query("item") else {
            panic!("inline");
        };
        assert!(small.rows_are_current());
    }

    #[test]
    fn a_stale_result_cannot_land() {
        let mut r = ListRanker::new(names(300_000), text, 500);
        let Ranking::Pending(old) = r.set_query("item_1") else {
            panic!("off thread");
        };
        let Ranking::Pending(new) = r.set_query("item_2") else {
            panic!("off thread");
        };
        let (g_old, _) = wait(old);
        assert!(!r.land(g_old));
        assert!(!r.rows_are_current());
        let (g_new, _) = wait(new);
        assert!(r.land(g_new));
        assert!(r.rows_are_current());
    }

    #[test]
    fn enter_is_deferred_until_the_rows_of_the_latest_query_land() {
        let mut r = ListRanker::new(names(300_000), text, 500);
        assert!(r.confirm_or_defer(), "nothing pending: confirm now");
        let Ranking::Pending(p) = r.set_query("item_1") else {
            panic!("off thread");
        };
        assert!(!r.confirm_or_defer(), "rows are the previous query's");
        let (generation, _) = wait(p);
        assert!(r.land(generation));
        assert!(r.take_deferred_confirm());
        assert!(!r.take_deferred_confirm(), "applies once");
        assert!(r.confirm_or_defer());
    }

    #[test]
    fn typing_again_or_dismissing_drops_a_deferred_enter() {
        let mut r = ListRanker::new(names(300_000), text, 500);
        let Ranking::Pending(_) = r.set_query("item_1") else {
            panic!("off thread");
        };
        assert!(!r.confirm_or_defer());
        let Ranking::Pending(p) = r.set_query("item_2") else {
            panic!("off thread");
        };
        let (generation, _) = wait(p);
        assert!(r.land(generation));
        assert!(!r.take_deferred_confirm(), "the query changed again");

        let Ranking::Pending(p) = r.set_query("item_3") else {
            panic!("off thread");
        };
        assert!(!r.confirm_or_defer());
        r.dismiss();
        let (generation, _) = wait(p);
        assert!(!r.land(generation), "a dismissed picker accepts nothing");
        assert!(!r.take_deferred_confirm());
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
