//! Performance smoke tests on generated repositories. Ignored by default (slow to set up):
//! `cargo test -p lector-git --test perf -- --ignored --nocapture`
mod common;

use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::Fixture;
use lector_git::{Changeset, LogQuery};

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn time<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let value = f();
    (value, start.elapsed())
}

#[test]
#[ignore]
fn status_on_50k_files() {
    let fx = Fixture::new();
    // LECTOR_PERF_STATUS_DIRS=850 gives a Linux-kernel-sized tree (85k files).
    let dirs: usize = std::env::var("LECTOR_PERF_STATUS_DIRS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);
    for dir in 0..dirs {
        for file in 0..100 {
            fx.write(
                &format!("d{dir:03}/sub/f{file:03}.txt"),
                &format!("dir {dir} file {file}\n"),
            );
        }
    }
    fx.commit_all("50k files");
    // Let the index become older than the files' mtime granularity (no racy entries).
    std::thread::sleep(Duration::from_millis(1100));
    for i in 0..5 {
        fx.write(&format!("d{i:03}/sub/f000.txt"), "modified\n");
    }
    for i in 0..3 {
        fx.write(&format!("d{i:03}/untracked{i}.txt"), "new\n");
    }
    let repo = fx.repo();
    let (status, first) = time(|| repo.status().unwrap());
    assert_eq!(status.len(), 8);
    let samples: Vec<_> = (0..5).map(|_| time(|| repo.status().unwrap()).1).collect();
    let (_, git_time) = time(|| fx.git(&["status", "--porcelain", "-uall"]));
    let (wt, wt_time) = time(|| repo.changes(&Changeset::WorkingTree).unwrap());
    assert_eq!(wt.len(), 8);
    println!(
        "status {}k files: first {:?}, median {:?} (git status: {:?}); WorkingTree changes {:?}",
        dirs / 10,
        first,
        median(samples),
        git_time,
        wt_time
    );
}

#[test]
#[ignore]
fn log_10k_commits() {
    let fx = Fixture::new();
    // Build history quickly with fast-import.
    let mut stream = String::new();
    for i in 0..10_000u32 {
        let time = common::BASE_TIME + i as i64 * 60;
        let content = format!("revision {i}\n");
        stream.push_str(&format!(
            "commit refs/heads/main\nauthor A <a@x> {time} +0000\ncommitter C <c@x> {time} +0000\ndata {}\ncommit {i}\n",
            format!("commit {i}").len()
        ));
        stream.push_str(&format!(
            "M 100644 inline dir{}/file{}.txt\ndata {}\n{content}\n",
            i % 50,
            i % 7,
            content.len()
        ));
    }
    let mut child = fx
        .command(&["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stream.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
    fx.git(&["reset", "-q", "--hard", "main"]);

    let repo = fx.repo();
    let (log, first) = time(|| repo.log(&LogQuery::default()).unwrap());
    assert_eq!(log.len(), 10_000);
    let samples: Vec<_> = (0..3)
        .map(|_| time(|| repo.log(&LogQuery::default()).unwrap()).1)
        .collect();
    let (page, page_time) = time(|| {
        repo.log(&LogQuery {
            limit: 200,
            ..Default::default()
        })
        .unwrap()
    });
    assert_eq!(page.len(), 200);
    let (filtered, path_time) = time(|| {
        repo.log(&LogQuery {
            path: Some("dir7".into()),
            ..Default::default()
        })
        .unwrap()
    });
    assert_eq!(filtered.len(), 200);
    let (rows, graph_time) = time(|| lector_git::graph(&log));
    assert_eq!(rows.len(), 10_000);
    let (_, git_time) = time(|| fx.git(&["log", "--format=%H %P %an %ae %at %s"]));
    println!(
        "log 10k commits: first {:?}, median {:?} (git log: {:?}); first 200: {:?}; path-filtered: {:?}; graph: {:?}",
        first,
        median(samples),
        git_time,
        page_time,
        path_time,
        graph_time
    );
}
