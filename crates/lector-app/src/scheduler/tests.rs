use super::*;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

fn key(kind: JobKind, n: u64) -> JobKey {
    JobKey::new(kind, Subject::View(n))
}

/// A non-replacing compute kind, so every test job runs.
fn any(n: u64) -> JobKey {
    key(JobKind::DocumentHighlight, n)
}

const SECS5: Duration = Duration::from_secs(5);

/// Occupies every general worker with a job that waits for its release; returns the senders.
fn block_general(s: &Scheduler) -> Vec<mpsc::Sender<()>> {
    let (started_tx, started_rx) = mpsc::channel();
    let mut senders = Vec::new();
    for i in 0..s.workers() {
        let (tx, rx) = mpsc::channel::<()>();
        let started = started_tx.clone();
        let _ = s.spawn(any(1000 + i as u64), Priority::Idle, move |_| {
            let _ = started.send(());
            let _ = rx.recv();
        });
        senders.push(tx);
    }
    for _ in 0..s.workers() {
        started_rx.recv_timeout(SECS5).unwrap();
    }
    senders
}

/// Occupies the reserved interactive worker too.
fn block_reserved(s: &Scheduler) -> mpsc::Sender<()> {
    let (started_tx, started_rx) = mpsc::channel();
    let (tx, rx) = mpsc::channel::<()>();
    let _ = s.spawn(
        key(JobKind::PickerRank, 999),
        Priority::Interactive,
        move |_| {
            let _ = started_tx.send(());
            let _ = rx.recv();
        },
    );
    started_rx.recv_timeout(SECS5).unwrap();
    tx
}

#[test]
fn runs_and_returns() {
    let s = Scheduler::new(2);
    let h = s.spawn(any(1), Priority::Visible, |_| 21 * 2);
    assert_eq!(h.wait(), Some(42));
}

#[test]
fn priority_order_on_the_general_workers() {
    let s = Scheduler::new(1);
    let release = block_general(&s);
    let reserved = block_reserved(&s);
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for (i, p) in [
        Priority::Idle,
        Priority::Background,
        Priority::Visible,
        Priority::Interactive,
        Priority::Idle,
        Priority::Interactive,
    ]
    .into_iter()
    .enumerate()
    {
        let order = order.clone();
        let kind = if p == Priority::Interactive {
            JobKind::FindInFile
        } else {
            JobKind::DocumentHighlight
        };
        handles.push(s.spawn(key(kind, i as u64), p, move |_| order.lock().push((p, i))));
    }
    for r in release {
        r.send(()).unwrap();
    }
    for h in handles {
        assert!(h.wait().is_some());
    }
    reserved.send(()).unwrap();
    assert_eq!(
        *order.lock(),
        vec![
            (Priority::Interactive, 3),
            (Priority::Interactive, 5),
            (Priority::Visible, 2),
            (Priority::Background, 1),
            (Priority::Idle, 0),
            (Priority::Idle, 4),
        ]
    );
}

#[test]
fn an_interactive_job_runs_while_every_general_worker_is_busy_with_background_work() {
    let s = Scheduler::new(2);
    let release = block_general(&s);
    // More background work queued behind the busy workers.
    let queued: Vec<_> = (0..20)
        .map(|i| s.spawn(any(i), Priority::Background, |_| ()))
        .collect();
    let started = Instant::now();
    let h = s.spawn(key(JobKind::PickerRank, 1), Priority::Interactive, |_| 7);
    assert_eq!(h.wait(), Some(7));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    for r in release {
        r.send(()).unwrap();
    }
    for h in queued {
        assert_eq!(h.wait(), Some(()));
    }
}

#[test]
fn only_interactive_kinds_get_the_reserved_worker() {
    let s = Scheduler::new(1);
    let release = block_general(&s);
    // A diff load asking for Interactive runs at Visible: it waits for a general worker.
    let mut h = s.spawn(key(JobKind::GitChangeset, 1), Priority::Interactive, |_| 1);
    thread::sleep(Duration::from_millis(150));
    assert!(!h.is_finished());
    assert_eq!(h.try_take(), None);
    for r in release {
        r.send(()).unwrap();
    }
    assert_eq!(h.wait(), Some(1));
    assert_eq!(
        Priority::Interactive.for_kind(JobKind::GitChangeset),
        Priority::Visible
    );
    assert_eq!(
        Priority::Interactive.for_kind(JobKind::PickerRank),
        Priority::Interactive
    );
}

#[test]
fn a_newer_job_replaces_an_older_one_with_the_same_key() {
    let s = Scheduler::new(2);
    let release = block_general(&s);
    let ran = Arc::new(AtomicU64::new(0));
    let handles: Vec<_> = (0..5u64)
        .map(|i| {
            let ran = ran.clone();
            s.spawn(
                key(JobKind::GitChangeset, 1),
                Priority::Visible,
                move |_| {
                    ran.fetch_add(1, Ordering::SeqCst);
                    i
                },
            )
        })
        .collect();
    // Another subject is not affected.
    let other = s.spawn(key(JobKind::GitChangeset, 2), Priority::Visible, |_| 9);
    for r in release {
        r.send(()).unwrap();
    }
    let results: Vec<_> = handles.into_iter().map(JobHandle::wait_outcome).collect();
    assert_eq!(
        results,
        vec![
            Err(JobError::Cancelled),
            Err(JobError::Cancelled),
            Err(JobError::Cancelled),
            Err(JobError::Cancelled),
            Ok(4)
        ]
    );
    assert_eq!(ran.load(Ordering::SeqCst), 1, "replaced jobs never run");
    assert_eq!(other.wait(), Some(9));
}

#[test]
fn kinds_that_do_not_replace_accumulate() {
    let s = Scheduler::new(2);
    let handles: Vec<_> = (0..5u64)
        .map(|i| s.spawn(key(JobKind::GitWrite, 1), Priority::Visible, move |_| i))
        .collect();
    let results: Vec<_> = handles.into_iter().map(JobHandle::wait).collect();
    assert_eq!(results, vec![Some(0), Some(1), Some(2), Some(3), Some(4)]);
}

#[test]
fn a_running_job_sees_cancellation_and_its_result_is_not_delivered() {
    let s = Scheduler::new(2);
    let (tx, rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel::<()>();
    let observed = Arc::new(AtomicBool::new(false));
    let seen = observed.clone();
    let first = s.spawn(
        key(JobKind::GitChangeset, 1),
        Priority::Visible,
        move |tok| {
            tx.send(()).unwrap();
            while !tok.is_cancelled() {
                thread::sleep(Duration::from_millis(1));
            }
            seen.store(true, Ordering::SeqCst);
            // Finishes with a value after the cancellation: a stale result.
            let _ = finish_rx.recv_timeout(Duration::from_millis(50));
            1
        },
    );
    rx.recv_timeout(SECS5).unwrap();
    let second = s.spawn(key(JobKind::GitChangeset, 1), Priority::Visible, |_| 2);
    drop(finish_tx);
    assert_eq!(first.wait_outcome(), Err(JobError::Cancelled));
    assert!(observed.load(Ordering::SeqCst));
    assert_eq!(second.wait(), Some(2));
}

#[test]
fn cancel_subject_cancels_queued_running_and_debounced_jobs_but_not_writes() {
    let s = Scheduler::new(1);
    let release = block_general(&s);
    let doc = Subject::Document(7);
    let other = Subject::Document(8);
    let ran = Arc::new(AtomicU64::new(0));
    let count = |ran: &Arc<AtomicU64>| {
        let ran = ran.clone();
        move |_: &CancelToken| {
            ran.fetch_add(1, Ordering::SeqCst);
        }
    };
    let queued = s.spawn(
        JobKey::new(JobKind::DocumentStructure, doc.clone()),
        Priority::Visible,
        count(&ran),
    );
    let accumulating = s.spawn(
        JobKey::new(JobKind::DocumentHighlight, doc.clone()),
        Priority::Visible,
        count(&ran),
    );
    let save = s.spawn(
        JobKey::new(JobKind::DocumentSave, doc.clone()),
        Priority::Visible,
        count(&ran),
    );
    let unrelated = s.spawn(
        JobKey::new(JobKind::DocumentStructure, other),
        Priority::Visible,
        count(&ran),
    );
    let debounced_ran = Arc::new(AtomicBool::new(false));
    let d = debounced_ran.clone();
    s.debounce(
        JobKey::new(JobKind::DocumentGutter, doc.clone()),
        Duration::from_millis(30),
        Priority::Background,
        move |_| d.store(true, Ordering::SeqCst),
    );
    assert_eq!(s.cancel_subject(&doc), 3);
    for r in release {
        r.send(()).unwrap();
    }
    assert_eq!(queued.wait_outcome(), Err(JobError::Cancelled));
    assert_eq!(accumulating.wait_outcome(), Err(JobError::Cancelled));
    assert_eq!(
        save.wait_outcome(),
        Ok(()),
        "a save survives its document closing"
    );
    assert_eq!(unrelated.wait_outcome(), Ok(()));
    assert!(s.wait_idle(SECS5));
    thread::sleep(Duration::from_millis(60));
    assert!(!debounced_ran.load(Ordering::SeqCst));
    assert_eq!(ran.load(Ordering::SeqCst), 2);
}

#[test]
fn cancel_subject_reaches_a_running_job() {
    let s = Scheduler::new(2);
    let (tx, rx) = mpsc::channel();
    let repo = Subject::repo("/tmp/some-repo");
    let h = s.spawn(
        JobKey::new(JobKind::GitLog, repo.clone()),
        Priority::Background,
        move |tok| {
            tx.send(()).unwrap();
            while !tok.is_cancelled() {
                thread::sleep(Duration::from_millis(1));
            }
            1
        },
    );
    rx.recv_timeout(SECS5).unwrap();
    assert_eq!(s.cancel_subject(&repo), 1);
    assert_eq!(h.wait_outcome(), Err(JobError::Cancelled));
}

#[test]
fn explicit_cancel_and_try_take() {
    let s = Scheduler::new(1);
    let release = block_general(&s);
    let h = s.spawn(any(1), Priority::Idle, |_| 1);
    h.cancel();
    let mut h2 = s.spawn(any(2), Priority::Idle, |_| 2);
    assert_eq!(h2.try_take(), None);
    for r in release {
        r.send(()).unwrap();
    }
    assert_eq!(h.wait(), None);
    assert!(s.wait_idle(SECS5));
    assert!(h2.is_finished());
    assert_eq!(h2.try_take(), Some(2));
    assert_eq!(h2.try_take(), None);
}

#[test]
fn debounce_coalesces() {
    let s = Scheduler::new(2);
    let runs = Arc::new(Mutex::new(Vec::new()));
    let k = key(JobKind::GitRefresh, 1);
    for i in 0..10 {
        let runs = runs.clone();
        s.debounce(
            k.clone(),
            Duration::from_millis(60),
            Priority::Background,
            move |_| runs.lock().push(i),
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(s.wait_idle(SECS5));
    assert_eq!(*runs.lock(), vec![9]);
    let r = runs.clone();
    s.debounce(
        k,
        Duration::from_millis(10),
        Priority::Background,
        move |_| r.lock().push(100),
    );
    assert!(s.wait_idle(SECS5));
    assert_eq!(*runs.lock(), vec![9, 100]);
}

#[test]
fn debounce_waits_for_quiet_period() {
    let s = Scheduler::new(2);
    let start = Instant::now();
    let (tx, rx) = mpsc::channel();
    s.debounce(
        key(JobKind::GitRefresh, 1),
        Duration::from_millis(80),
        Priority::Visible,
        move |_| tx.send(Instant::now()).unwrap(),
    );
    let at = rx.recv_timeout(SECS5).unwrap();
    assert!(at.duration_since(start) >= Duration::from_millis(80));
}

#[test]
fn no_lost_or_duplicated_jobs_under_contention() {
    let s = Scheduler::new(4);
    let counts = Arc::new(Mutex::new(HashMap::<u64, u32>::new()));
    let threads: Vec<_> = (0..8u64)
        .map(|t| {
            let s = s.clone();
            let counts = counts.clone();
            thread::spawn(move || {
                let mut handles = Vec::new();
                for i in 0..250u64 {
                    let id = t * 1000 + i;
                    let counts = counts.clone();
                    let prio = Priority::ALL[(id % 4) as usize];
                    let kind = if prio == Priority::Interactive {
                        JobKind::DocumentParse
                    } else {
                        JobKind::DocumentHighlight
                    };
                    handles.push(s.spawn(key(kind, id), prio, move |_| {
                        *counts.lock().entry(id).or_default() += 1;
                        id
                    }));
                }
                handles.into_iter().map(|h| h.wait()).collect::<Vec<_>>()
            })
        })
        .collect();
    let mut total = 0;
    for (t, th) in threads.into_iter().enumerate() {
        let results = th.join().unwrap();
        for (i, r) in results.into_iter().enumerate() {
            assert_eq!(r, Some(t as u64 * 1000 + i as u64));
            total += 1;
        }
    }
    assert_eq!(total, 2000);
    let counts = counts.lock();
    assert_eq!(counts.len(), 2000);
    assert!(counts.values().all(|&c| c == 1));
    // A waiter can see its result just before the worker unregisters the job.
    assert!(s.wait_idle(SECS5));
    assert_eq!(s.live_jobs(), 0, "every finished job is unregistered");
}

#[test]
fn keyed_contention_runs_newest_at_most_once_each() {
    let s = Scheduler::new(3);
    let ran = Arc::new(AtomicU64::new(0));
    let handles: Vec<_> = (0..500u64)
        .map(|i| {
            let ran = ran.clone();
            s.spawn(
                key(JobKind::GitChangeset, i % 7),
                Priority::Visible,
                move |_| {
                    ran.fetch_add(1, Ordering::SeqCst);
                    i
                },
            )
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(JobHandle::wait).collect();
    for k in 0..7u64 {
        let last = (0..500u64).rev().find(|i| i % 7 == k).unwrap();
        assert_eq!(results[last as usize], Some(last));
    }
    let delivered = results.iter().filter(|r| r.is_some()).count() as u64;
    assert!(delivered <= ran.load(Ordering::SeqCst));
}

#[test]
fn a_panicking_job_reports_to_its_waiter_and_the_worker_survives() {
    let s = Scheduler::new(1);
    let h = s.spawn(any(1), Priority::Visible, |_| -> u32 {
        panic!("expected test panic")
    });
    assert_eq!(
        h.wait_outcome(),
        Err(JobError::Panicked("expected test panic".into()))
    );
    // Same single general worker, still alive.
    assert_eq!(s.spawn(any(2), Priority::Visible, |_| 7).wait(), Some(7));
    let b = s.spawn(key(JobKind::ForgeRead, 1), Priority::Visible, |_| -> u32 {
        panic!("expected test panic")
    });
    assert!(matches!(b.wait_outcome(), Err(JobError::Panicked(_))));
    assert_eq!(
        s.spawn(key(JobKind::ForgeRead, 2), Priority::Visible, |_| 8)
            .wait(),
        Some(8)
    );
}

#[test]
fn blocking_jobs_never_occupy_the_compute_workers() {
    let s = Scheduler::with_limits(1, 3);
    let (release_tx, release_rx) = crossbeam_like();
    let (started_tx, started_rx) = mpsc::channel();
    let running = Arc::new(AtomicU64::new(0));
    let peak = Arc::new(AtomicU64::new(0));
    let blocked: Vec<_> = (0..6u64)
        .map(|i| {
            let rx = release_rx.clone();
            let started = started_tx.clone();
            let (running, peak) = (running.clone(), peak.clone());
            s.spawn(key(JobKind::ForgeRead, i), Priority::Visible, move |_| {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                let _ = started.send(());
                let _ = rx.lock().recv_timeout(SECS5);
                running.fetch_sub(1, Ordering::SeqCst);
            })
        })
        .collect();
    for _ in 0..3 {
        started_rx.recv_timeout(SECS5).unwrap();
    }
    // The blocking lane is full (3 of 6 waiting); compute work still runs at once.
    assert_eq!(s.spawn(any(1), Priority::Background, |_| 5).wait(), Some(5));
    assert_eq!(peak.load(Ordering::SeqCst), 3, "bounded by blocking_max");
    assert!(s.inner().blocking_threads() <= 3);
    for _ in 0..6 {
        let _ = release_tx.send(());
    }
    for h in blocked {
        assert_eq!(h.wait(), Some(()));
    }
    assert!(peak.load(Ordering::SeqCst) <= 3);
}

/// An mpsc receiver shared by several jobs.
fn crossbeam_like() -> (mpsc::Sender<()>, Arc<Mutex<mpsc::Receiver<()>>>) {
    let (tx, rx) = mpsc::channel();
    (tx, Arc::new(Mutex::new(rx)))
}

#[test]
fn a_job_waiting_for_another_job_on_a_one_worker_pool_does_not_deadlock() {
    let s = Scheduler::new(1);
    let s2 = s.clone();
    let outer = s.spawn(any(1), Priority::Visible, move |_| {
        // The only general worker runs this job; the inner job is queued behind it.
        let inner = s2.spawn(any(2), Priority::Background, |_| 20);
        inner.wait().map(|v| v + 1)
    });
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || tx.send(outer.wait()));
    assert_eq!(rx.recv_timeout(SECS5).unwrap(), Some(Some(21)));
}

#[test]
fn shutdown_cancels_queued_and_running_jobs_and_stops_the_threads_promptly() {
    let s = Scheduler::with_limits(1, 2);
    let (tx, rx) = mpsc::channel();
    let running = s.spawn(any(1), Priority::Background, move |tok| {
        tx.send(()).unwrap();
        while !tok.is_cancelled() {
            thread::sleep(Duration::from_millis(1));
        }
    });
    rx.recv_timeout(SECS5).unwrap();
    let queued: Vec<_> = (0..10)
        .map(|i| s.spawn(any(10 + i), Priority::Idle, |_| ()))
        .collect();
    let (btx, brx) = mpsc::channel();
    let blocking = s.spawn(key(JobKind::ForgeRead, 1), Priority::Visible, move |tok| {
        btx.send(()).unwrap();
        while !tok.is_cancelled() {
            thread::sleep(Duration::from_millis(1));
        }
    });
    brx.recv_timeout(SECS5).unwrap();
    s.debounce(
        key(JobKind::GitRefresh, 1),
        Duration::from_secs(60),
        Priority::Background,
        |_| (),
    );
    let started = Instant::now();
    assert!(s.shutdown(SECS5));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(s.inner().threads_alive(), 0);
    assert_eq!(running.wait_outcome(), Err(JobError::Cancelled));
    assert_eq!(blocking.wait_outcome(), Err(JobError::Cancelled));
    for h in queued {
        assert_eq!(h.wait_outcome(), Err(JobError::Cancelled));
    }
    // Later work is cancelled at once instead of hanging.
    assert_eq!(
        s.spawn(any(99), Priority::Visible, |_| 1).wait_outcome(),
        Err(JobError::Cancelled)
    );
    assert!(s.is_shut_down());
}

#[test]
fn shutdown_is_bounded_when_a_job_ignores_its_token() {
    let s = Scheduler::new(1);
    let (tx, rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let _stuck = s.spawn(any(1), Priority::Visible, move |_| {
        tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(SECS5);
    });
    rx.recv_timeout(SECS5).unwrap();
    let started = Instant::now();
    assert!(!s.shutdown(Duration::from_millis(100)));
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(100) && took < Duration::from_secs(2),
        "{took:?}"
    );
    release_tx.send(()).unwrap();
}

#[test]
fn a_runner_scheduler_keeps_keys_subjects_and_results() {
    let queue: Arc<Mutex<Vec<(Duration, Body)>>> = Arc::default();
    let q = queue.clone();
    let s = Scheduler::with_runner(Arc::new(move |delay, body| q.lock().push((delay, body))));
    let first = s.spawn(key(JobKind::GitChangeset, 1), Priority::Visible, |_| 1);
    let second = s.spawn(key(JobKind::GitChangeset, 1), Priority::Visible, |_| 2);
    let other = s.spawn(key(JobKind::GitLog, 2), Priority::Visible, |_| 3);
    s.debounce(
        key(JobKind::GitRefresh, 3),
        Duration::from_millis(40),
        Priority::Background,
        |_| (),
    );
    assert_eq!(s.cancel_subject(&Subject::View(2)), 1);
    let jobs: Vec<_> = std::mem::take(&mut *queue.lock());
    assert_eq!(jobs.len(), 4);
    assert_eq!(jobs[3].0, Duration::from_millis(40));
    for (_, body) in jobs {
        body();
    }
    assert_eq!(first.wait_outcome(), Err(JobError::Cancelled));
    assert_eq!(second.wait(), Some(2));
    assert_eq!(other.wait_outcome(), Err(JobError::Cancelled));
    assert_eq!(s.live_jobs(), 0);
}

#[test]
fn default_workers_at_least_two() {
    assert!(default_workers() >= 2);
}

/// `n` blocking-lane leaf jobs that wait for their release; returns once all have started.
fn fill_blocking_lane(s: &Scheduler, n: usize) -> Vec<mpsc::Sender<()>> {
    let (started_tx, started_rx) = mpsc::channel();
    let mut senders = Vec::new();
    for i in 0..n {
        let (tx, rx) = mpsc::channel::<()>();
        let started = started_tx.clone();
        let _ = s.spawn(
            key(JobKind::ForgeRead, 5000 + i as u64),
            Priority::Visible,
            move |_| {
                started.send(()).ok();
                let _ = rx.recv_timeout(Duration::from_secs(30));
            },
        );
        senders.push(tx);
    }
    for _ in 0..n {
        started_rx.recv_timeout(SECS5).unwrap();
    }
    senders
}

#[test]
fn a_full_blocking_lane_never_delays_what_others_wait_on() {
    let s = Scheduler::new(1);
    let release = fill_blocking_lane(&s, BLOCKING_MAX);
    // One more leaf job queues: the lane really is full.
    let mut queued = s.spawn(key(JobKind::LspHover, 1), Priority::Visible, |_| ());
    thread::sleep(Duration::from_millis(50));
    assert_eq!(queued.try_take(), None);
    let started = Instant::now();
    let prompt = s.spawn(key(JobKind::AskpassPrompt, 1), Priority::Visible, |_| {
        "answer"
    });
    let control = s.spawn(key(JobKind::AgentControl, 1), Priority::Visible, |_| {
        "cancelled"
    });
    let terminal = s.spawn(key(JobKind::TerminalKill, 1), Priority::Background, |_| {
        "killed"
    });
    assert_eq!(prompt.wait(), Some("answer"));
    assert_eq!(control.wait(), Some("cancelled"));
    assert_eq!(terminal.wait(), Some("killed"));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    for r in release {
        r.send(()).ok();
    }
    assert_eq!(queued.wait(), Some(()));
}

#[test]
fn a_command_waiting_for_a_prompt_completes_while_the_lane_is_otherwise_full() {
    let s = Scheduler::new(1);
    let release = fill_blocking_lane(&s, BLOCKING_MAX - 1);
    let (asked_tx, asked_rx) = mpsc::channel();
    let (answer_tx, answer_rx) = mpsc::channel::<&'static str>();
    let answer_rx = Arc::new(Mutex::new(Some(answer_rx)));
    let s2 = s.clone();
    // The sixteenth leaf slot: a git command that needs a credential.
    let command = s.spawn(
        key(JobKind::NetworkCommand, 1),
        Priority::Visible,
        move |_| {
            let answers = answer_rx.clone();
            let prompt = s2.spawn(
                key(JobKind::AskpassPrompt, 1),
                Priority::Visible,
                move |_| {
                    asked_tx.send(()).ok();
                    let rx = answers.lock().take()?;
                    rx.recv_timeout(SECS5).ok()
                },
            );
            prompt.wait().flatten()
        },
    );
    // The prompt is shown although all sixteen leaf slots are taken; the user answers.
    asked_rx.recv_timeout(SECS5).expect("the prompt started");
    answer_tx.send("hunter2").unwrap();
    assert_eq!(command.wait(), Some(Some("hunter2")));
    for r in release {
        r.send(()).ok();
    }
}

#[test]
fn a_compute_job_waits_for_a_blocking_job_instead_of_running_it_inline() {
    let s = Scheduler::new(1);
    let s2 = s.clone();
    let outer = s.spawn(any(1), Priority::Visible, move |_| {
        let inner = s2.spawn(key(JobKind::ForgeRead, 1), Priority::Visible, |_| {
            thread::current().name().map(str::to_owned)
        });
        inner.wait().flatten()
    });
    let ran_on = outer.wait().flatten().expect("thread name");
    assert!(ran_on.contains("blocking"), "{ran_on}");
}
