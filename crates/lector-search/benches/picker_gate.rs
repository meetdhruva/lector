//! Fuzzy file finder gate: short queries over 1M paths.
//! `cargo bench -p lector-search --bench picker_gate`
//! (custom harness; `PICKER_GATE_REPS=n` sets repetitions, `PICKER_GATE_PATHS=n` the corpus size)
//!
//! Two corpora: `uniform` (the legacy synthetic set where *every* path contains
//! `module_`, i.e. the worst case for short queries: everything matches) and
//! `varied` (words, depths and extensions drawn from a vocabulary, closer to a
//! real monorepo). For each query ladder the gate reports, per query length,
//! p50 / p95 / max over repeated runs of
//!
//! * `cold`: the query typed with no earlier work to narrow from,
//! * `extend`: one more character after the shorter query settled,
//! * `back`: one character deleted after the longer query settled,
//!
//! as time from `set_query` until the pass is complete (exact count, final
//! ranking), plus for `cold` the time until the first page is exact ("ranked":
//! the top results are final, the match count may still be counting up).

use std::sync::Arc;
use std::time::{Duration, Instant};

use lector_search::Picker;

/// Gate: settled time per keystroke (ms).
const GATE_MS: f64 = 50.0;

fn synthetic(n: usize) -> Vec<String> {
    let dirs = [
        "src", "crates", "tests", "docs", "vendor", "lib", "app", "core",
    ];
    (0..n)
        .map(|i| {
            format!(
                "{}/{}/{}/module_{}_{}.rs",
                dirs[i % 8],
                dirs[(i / 8) % 8],
                dirs[(i / 64) % 8],
                i % 1000,
                i
            )
        })
        .collect()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn varied(n: usize) -> Vec<String> {
    const WORDS: &[&str] = &[
        "src",
        "lib",
        "app",
        "core",
        "util",
        "utils",
        "config",
        "parser",
        "lexer",
        "render",
        "view",
        "model",
        "store",
        "cache",
        "index",
        "search",
        "picker",
        "diff",
        "git",
        "text",
        "buffer",
        "syntax",
        "theme",
        "icons",
        "network",
        "client",
        "server",
        "handler",
        "router",
        "service",
        "worker",
        "queue",
        "event",
        "state",
        "action",
        "command",
        "editor",
        "window",
        "panel",
        "button",
        "input",
        "output",
        "error",
        "result",
        "context",
        "session",
        "project",
        "package",
        "module",
        "plugin",
        "driver",
        "device",
        "kernel",
        "memory",
        "thread",
        "task",
        "schema",
        "query",
        "table",
        "column",
        "record",
        "field",
        "token",
        "stream",
        "format",
        "decode",
        "encode",
        "compress",
        "archive",
        "backup",
        "restore",
        "deploy",
        "release",
        "build",
        "test",
        "bench",
        "example",
        "fixture",
        "mock",
        "stub",
        "helper",
        "common",
        "shared",
        "internal",
        "public",
        "private",
        "legacy",
        "compat",
        "vendor",
        "third_party",
        "generated",
        "proto",
        "api",
        "rest",
        "graphql",
        "auth",
        "oauth",
        "crypto",
        "hash",
    ];
    const EXTS: &[&str] = &[
        "rs", "ts", "tsx", "js", "py", "go", "md", "json", "toml", "yaml", "css", "html", "txt",
        "lock", "png", "svg", "c", "h", "cpp", "java",
    ];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut out = Vec::with_capacity(n);
    // A pool of directory chains so that siblings share prefixes like real trees.
    let mut dirs: Vec<String> = Vec::new();
    for _ in 0..20_000 {
        let depth = 1 + rng.below(6);
        let mut d = String::new();
        for _ in 0..depth {
            if !d.is_empty() {
                d.push('/');
            }
            d.push_str(WORDS[rng.below(WORDS.len())]);
        }
        dirs.push(d);
    }
    for i in 0..n {
        let d = &dirs[rng.below(dirs.len())];
        let w1 = WORDS[rng.below(WORDS.len())];
        let w2 = WORDS[rng.below(WORDS.len())];
        let ext = EXTS[rng.below(EXTS.len())];
        let stem = match rng.below(4) {
            0 => format!("{w1}_{w2}"),
            1 => format!("{w1}-{w2}"),
            2 => format!("{w1}{}", w2.to_uppercase().chars().next().unwrap_or('X')),
            _ => format!("{w1}_{}", i % 977),
        };
        out.push(format!("{d}/{stem}.{ext}"));
    }
    out
}

fn build(paths: &[String]) -> (Picker<u32>, Duration) {
    let mut p: Picker<u32> = Picker::new_for_paths(Arc::new(|| {}));
    let inj = p.injector();
    let t0 = Instant::now();
    for (i, s) in paths.iter().enumerate() {
        inj.push(i as u32, s);
    }
    let inject = t0.elapsed();
    p.set_query("");
    while p.tick(5) {}
    (p, inject)
}

fn settle(p: &mut Picker<u32>) -> Duration {
    let t = Instant::now();
    while p.tick(1) {
        assert!(t.elapsed() < Duration::from_secs(60), "did not settle");
    }
    t.elapsed()
}

/// (first non-empty page, exact ranking, settled)
fn timed(p: &mut Picker<u32>, q: &str) -> (Duration, Duration, Duration) {
    let t = Instant::now();
    p.set_query(q);
    let (mut first, mut ranked) = (None, None);
    loop {
        let running = p.tick(1);
        if first.is_none() && !p.matches(0..1).is_empty() {
            first = Some(t.elapsed());
        }
        if ranked.is_none() && p.progress().ranked {
            ranked = Some(t.elapsed());
        }
        if !running {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(60), "did not settle");
    }
    let settled = t.elapsed();
    (first.unwrap_or(settled), ranked.unwrap_or(settled), settled)
}

/// CPU time (user + system, all threads) of this process in ms; 10 ms ticks.
fn cpu_ms() -> f64 {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return f64::NAN;
    };
    let Some(rest) = stat.rsplit(')').next() else {
        return f64::NAN;
    };
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks = |i: usize| {
        f.get(i)
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(f64::NAN)
    };
    // After the closing paren: state is field 0, utime is 11, stime is 12.
    (ticks(11) + ticks(12)) * 10.0
}

#[derive(Default)]
struct Samples(Vec<f64>);
impl Samples {
    fn add(&mut self, d: Duration) {
        self.0.push(d.as_secs_f64() * 1000.0);
    }
    fn pct(&self, q: f64) -> f64 {
        let mut v = self.0.clone();
        v.sort_by(|a, b| a.total_cmp(b));
        if v.is_empty() {
            return f64::NAN;
        }
        let rank = ((q * v.len() as f64).ceil() as usize).clamp(1, v.len());
        v[rank - 1]
    }
    fn cell(&self) -> String {
        format!(
            "{:>6.1}/{:>6.1}/{:>6.1}",
            self.pct(0.5),
            self.pct(0.95),
            self.pct(1.0)
        )
    }
}

fn run_corpus(name: &str, paths: &[String], ladders: &[&str], reps: usize) -> bool {
    let (mut p, inject) = build(paths);
    let bytes: usize = paths.iter().map(|s| s.len()).sum();
    let heap = p.heap_bytes();
    let n = paths.len();
    println!(
        "\n== corpus `{name}`: {n} paths, avg {:.1} bytes, inject {inject:?}",
        bytes as f64 / n as f64
    );
    println!(
        "   store heap {:.1} MB = {:.1} bytes/path, of which {:.1} bytes/path overhead on top of the path text and the 4-byte item",
        heap as f64 / 1e6,
        heap as f64 / n as f64,
        (heap as f64 - bytes as f64) / n as f64 - 4.0
    );
    println!(
        "   cells are p50/p95/max in ms over {reps} runs; settled = exact count + final ranking"
    );
    println!(
        "{:>8} {:>3} {:>22} {:>22} {:>22} {:>16} {:>8} {:>8} {:>9}",
        "query",
        "len",
        "cold settled",
        "extend settled",
        "back settled",
        "cold ranked",
        "cold cpu",
        "ext cpu",
        "matched"
    );
    let mut pass = true;
    for ladder in ladders {
        for len in [1usize, 2, 3, 6] {
            if len > ladder.len() {
                continue;
            }
            let q = &ladder[..len];
            let prev = &ladder[..len - 1];
            let (mut cold, mut cold_first, mut ext, mut back) = (
                Samples::default(),
                Samples::default(),
                Samples::default(),
                Samples::default(),
            );
            let mut matched = 0;
            let (mut cold_cpu, mut ext_cpu) = (0.0, 0.0);
            for _ in 0..reps {
                // cold: no cached match sets to narrow from.
                p.set_query("");
                p.drop_caches();
                settle(&mut p);
                let c0 = cpu_ms();
                let (_, r, s) = timed(&mut p, q);
                cold_cpu += cpu_ms() - c0;
                cold.add(s);
                cold_first.add(r);
                matched = p.matched_count();
                // extend: the shorter query is cached.
                p.set_query("");
                p.drop_caches();
                settle(&mut p);
                if !prev.is_empty() {
                    timed(&mut p, prev);
                }
                let c0 = cpu_ms();
                let (_, _, s) = timed(&mut p, q);
                ext_cpu += cpu_ms() - c0;
                ext.add(s);
                // back: the longer query was just typed, now delete a char.
                let (_, _, s) = timed(&mut p, prev);
                back.add(s);
            }
            let worst = cold.pct(0.95).max(ext.pct(0.95)).max(back.pct(0.95));
            let ok = worst < GATE_MS;
            pass &= ok;
            println!(
                "{:>8} {:>3} {:>22} {:>22} {:>22} {:>8.1}/{:>6.1} {:>7.1} {:>7.1} {:>9} {}",
                q,
                len,
                cold.cell(),
                ext.cell(),
                back.cell(),
                cold_first.pct(0.5),
                cold_first.pct(0.95),
                cold_cpu / reps as f64,
                ext_cpu / reps as f64,
                matched,
                if ok { "ok" } else { "MISS" }
            );
        }
    }
    pass
}

fn main() {
    let reps: usize = std::env::var("PICKER_GATE_REPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15);
    let n: usize = std::env::var("PICKER_GATE_PATHS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let mut all = true;
    all &= run_corpus("uniform", &synthetic(n), &["module", "crates"], reps);
    all &= run_corpus("varied", &varied(n), &["config", "parser", "src/ma"], reps);
    println!(
        "\ngate (p95 < {GATE_MS} ms for cold, extend and backspace at 1, 2, 3 and 6 characters): {}",
        if all { "PASS" } else { "MISS" }
    );
}
