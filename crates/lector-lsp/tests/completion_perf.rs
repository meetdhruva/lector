//! Keystroke latency of the completion session with 10,000 items.
//! `cargo test -p lector-lsp --test completion_perf -- --nocapture` prints the numbers.

use std::time::{Duration, Instant};

use lector_lsp::{
    CompletionItem, CompletionKind, CompletionResponse, CompletionSession, InsertFormat,
    InsertTextMode, OpenReason, PointUtf16, SessionOptions, Update,
};
use lector_text::Rope;
use serde_json::Value;

/// Deterministic pseudo-random numbers (xorshift).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const WORDS: &[&str] = &[
    "get", "set", "user", "name", "count", "index", "value", "buffer", "string", "vector", "map",
    "iter", "next", "from", "into", "parse", "format", "write", "read", "file", "path", "line",
    "text", "range", "start", "end", "len", "push", "pop", "insert", "remove", "clear", "default",
    "new", "with", "capacity", "slice", "chunk", "window", "select", "filter", "fold", "sort",
];

fn identifier(rng: &mut Rng) -> String {
    let parts = 1 + rng.below(4);
    let camel = rng.below(2) == 0;
    let mut out = String::new();
    for i in 0..parts {
        let w = WORDS[rng.below(WORDS.len())];
        if i == 0 {
            out.push_str(w);
        } else if camel {
            let mut cs = w.chars();
            if let Some(c) = cs.next() {
                out.extend(c.to_uppercase());
                out.push_str(cs.as_str());
            }
        } else {
            out.push('_');
            out.push_str(w);
        }
    }
    out
}

fn items(n: usize) -> Vec<CompletionItem> {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    (0..n)
        .map(|i| {
            let label = identifier(&mut rng);
            CompletionItem {
                label: label.clone(),
                label_details: None,
                kind: CompletionKind::Function,
                detail: Some("fn() -> T".into()),
                documentation: None,
                deprecated: i % 50 == 0,
                preselect: i == 17,
                sort_text: Some(format!("{:04}{label}", rng.below(10))),
                filter_text: (i % 7 == 0).then(|| label.clone()),
                insert_text: Some(format!("{label}()")),
                format: InsertFormat::PlainText,
                insert_mode: InsertTextMode::AdjustIndentation,
                edit: None,
                additional_edits: vec![],
                commit_characters: vec![],
                command: None,
                resolved: false,
                raw: Value::Null,
            }
        })
        .collect()
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[test]
fn ten_thousand_items_filter_in_well_under_5ms_per_keystroke() {
    const N: usize = 10_000;
    let items = items(N);
    let typing = [
        "g", "ge", "get", "get_", "get_u", "get_us", "get_use", "get_user",
    ];
    let mut per_step: Vec<Vec<Duration>> = vec![vec![]; typing.len()];
    let mut backspace = vec![];
    let mut ingest = vec![];
    let mut matched = vec![0usize; typing.len()];
    for _ in 0..30 {
        let empty = Rope::from_str("");
        let mut s =
            CompletionSession::open(&empty, 0, OpenReason::Manual, SessionOptions::default());
        let response = CompletionResponse {
            items: items.clone(),
            is_incomplete: false,
            request_pos: PointUtf16 { row: 0, column: 0 },
            doc_version: 1,
            tag: None,
            resolve_supported: false,
        };
        let t = Instant::now();
        assert!(s.set_response(&empty, 0, response));
        ingest.push(t.elapsed());
        assert_eq!(s.matches().len(), N);

        for (i, text) in typing.iter().enumerate() {
            let rope = Rope::from_str(text);
            let t = Instant::now();
            let update = s.update(&rope, text.len());
            per_step[i].push(t.elapsed());
            assert_eq!(update, Update::Continue, "{text}");
            matched[i] = s.matches().len();
        }
        // Backspace: the query shrinks, so the whole list is searched again.
        let rope = Rope::from_str("get_use");
        let t = Instant::now();
        assert_eq!(s.update(&rope, 7), Update::Continue);
        backspace.push(t.elapsed());
    }
    println!(
        "items: {N}; ingest (set_response) median {:?}",
        median(ingest)
    );
    let mut worst = Duration::ZERO;
    for (i, text) in typing.iter().enumerate() {
        let m = median(per_step[i].clone());
        worst = worst.max(*per_step[i].iter().max().unwrap());
        println!("typed {text:<9} -> {:>5} matches, median {m:?}", matched[i]);
        assert!(m < Duration::from_millis(5), "{text}: {m:?}");
    }
    let back = median(backspace);
    println!("backspace (full re-filter) median {back:?}; worst keystroke {worst:?}");
    assert!(back < Duration::from_millis(5), "{back:?}");
    assert!(worst < Duration::from_millis(30), "{worst:?}");
}
