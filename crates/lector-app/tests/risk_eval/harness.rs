//! Shared by `tests/risk_eval.rs` and `examples/risk_eval.rs`: loads the labelled
//! changesets, builds their `FileDiff`s and scores a ranking function with
//! top-1, top-3 and mean reciprocal rank (MRR), overall, per split and per
//! category.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lector_diff::DiffOptions;
use lector_git::{ChangeKind, FileChange, Side};
use lector_syntax::LanguageRegistry;
use lector_workspace::review::{diff_bytes, FileDiff};
use serde::Deserialize;

/// One file of a changeset: base and head content (`null` = absent on that side).
#[derive(Debug, Clone, Deserialize)]
pub struct CaseFile {
    pub path: String,
    pub base: Option<String>,
    pub head: Option<String>,
}

/// One labelled changeset.
#[derive(Debug, Clone, Deserialize)]
pub struct Case {
    pub id: String,
    /// `tune` or `holdout`.
    pub split: String,
    pub category: String,
    pub language: String,
    pub description: String,
    /// Paths a reviewer must see first (any of them counts).
    pub risky: Vec<String>,
    pub files: Vec<CaseFile>,
}

/// `crates/lector-app/tests/risk_eval/cases`.
pub fn cases_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/risk_eval/cases")
}

/// `crates/lector-app/tests/risk_eval/local` (git-ignored, written by `scripts/risk-eval-import.sh`).
pub fn local_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/risk_eval/local")
}

/// All `*.json` cases of `dir`, sorted by file name.
pub fn load_dir(dir: &Path) -> Vec<Case> {
    let mut names: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
        .iter()
        .map(|p| {
            let text = std::fs::read_to_string(p)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()));
            serde_json::from_str::<Case>(&text)
                .unwrap_or_else(|e| panic!("bad case {}: {e}", p.display()))
        })
        .collect()
}

/// The committed cases.
pub fn load_cases() -> Vec<Case> {
    load_dir(&cases_dir())
}

/// Builds the file diffs of a case, in file order.
pub fn diffs_of(case: &Case, langs: &LanguageRegistry) -> Vec<Arc<FileDiff>> {
    case.files
        .iter()
        .map(|f| {
            let kind = match (&f.base, &f.head) {
                (None, _) => ChangeKind::Added,
                (_, None) => ChangeKind::Deleted,
                _ => ChangeKind::Modified,
            };
            let change = FileChange {
                old_path: f.base.as_ref().map(|_| f.path.clone()),
                new_path: f.head.as_ref().map(|_| f.path.clone()),
                kind,
                old: Side::None,
                new: Side::None,
                binary: None,
            };
            Arc::new(diff_bytes(
                change,
                f.base.as_ref().map(|s| s.as_bytes().to_vec()),
                f.head.as_ref().map(|s| s.as_bytes().to_vec()),
                langs,
                &DiffOptions::default(),
            ))
        })
        .collect()
}

/// Accuracy numbers of a set of cases.
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    pub n: usize,
    pub top1: usize,
    pub top3: usize,
    pub rr_sum: f64,
}

impl Metrics {
    pub fn add(&mut self, rank: usize) {
        self.n += 1;
        self.top1 += usize::from(rank == 1);
        self.top3 += usize::from(rank <= 3);
        self.rr_sum += 1.0 / rank as f64;
    }
    pub fn mrr(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.rr_sum / self.n as f64
        }
    }
    pub fn top1_rate(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.top1 as f64 / self.n as f64
        }
    }
    pub fn top3_rate(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.top3 as f64 / self.n as f64
        }
    }
    pub fn line(&self) -> String {
        format!(
            "n={:>3}  top-1 {:>2}/{:<2} ({:>5.1}%)  top-3 {:>2}/{:<2} ({:>5.1}%)  MRR {:.3}",
            self.n,
            self.top1,
            self.n,
            100.0 * self.top1_rate(),
            self.top3,
            self.n,
            100.0 * self.top3_rate(),
            self.mrr()
        )
    }
}

/// 1-based rank of the best-ranked risky file in `order` (indices into `case.files`, best first).
pub fn rank_of_best(case: &Case, order: &[usize]) -> usize {
    order
        .iter()
        .position(|&i| case.risky.contains(&case.files[i].path))
        .map_or(case.files.len() + 1, |p| p + 1)
}

/// Result of scoring a ranking over a set of cases.
#[derive(Debug, Default)]
pub struct Evaluation {
    pub all: Metrics,
    pub tune: Metrics,
    pub holdout: Metrics,
    /// Imported cases (`split: "local"`) that have labels.
    pub local: Metrics,
    pub by_category: BTreeMap<String, Metrics>,
    pub by_category_holdout: BTreeMap<String, Metrics>,
    /// (case id, split, category, rank) of every case.
    pub ranks: Vec<(String, String, String, usize)>,
}

/// Scores `rank` (returns indices into the case's files, best first) over `cases`.
pub fn evaluate<F: FnMut(&Case) -> Vec<usize>>(cases: &[Case], mut rank: F) -> Evaluation {
    let mut ev = Evaluation::default();
    for case in cases {
        if case.risky.is_empty() {
            continue;
        }
        let order = rank(case);
        let r = rank_of_best(case, &order);
        if case.split == "local" {
            ev.local.add(r);
            ev.ranks.push((
                case.id.clone(),
                case.split.clone(),
                case.category.clone(),
                r,
            ));
            continue;
        }
        ev.all.add(r);
        if case.split == "holdout" {
            ev.holdout.add(r);
            ev.by_category_holdout
                .entry(case.category.clone())
                .or_default()
                .add(r);
        } else {
            ev.tune.add(r);
        }
        ev.by_category
            .entry(case.category.clone())
            .or_default()
            .add(r);
        ev.ranks.push((
            case.id.clone(),
            case.split.clone(),
            case.category.clone(),
            r,
        ));
    }
    ev
}

/// The review queue order (`lector_app::review_queue::analyze`) of a case.
pub fn queue_order(case: &Case, langs: &LanguageRegistry) -> Vec<usize> {
    let diffs = diffs_of(case, langs);
    lector_app::review_queue::analyze(
        &diffs,
        &lector_app::review_queue::AnalysisOptions {
            detect_moved: false,
        },
    )
    .order
    .iter()
    .map(|e| e.index)
    .collect()
}

/// A printable report: overall, split and per-category lines.
pub fn print_report(title: &str, ev: &Evaluation) {
    println!("== {title}");
    println!("  all      {}", ev.all.line());
    println!("  tune     {}", ev.tune.line());
    println!("  holdout  {}", ev.holdout.line());
    if ev.local.n > 0 {
        println!("  local    {}", ev.local.line());
    }
    for (cat, m) in &ev.by_category {
        let h = ev.by_category_holdout.get(cat).cloned().unwrap_or_default();
        println!(
            "    {:<15} top-1 {:>2}/{:<2}  top-3 {:>2}/{:<2}  MRR {:.3}   (holdout {}/{})",
            cat,
            m.top1,
            m.n,
            m.top3,
            m.n,
            m.mrr(),
            h.top1,
            h.n
        );
    }
}

/// Facts of every file of a case (one analysis per file; the changeset context is applied).
pub fn facts_of(case: &Case, langs: &LanguageRegistry) -> Vec<lector_workspace::risk::RiskFacts> {
    let diffs = diffs_of(case, langs);
    let tests = Some(lector_workspace::risk::tests_changed_in(&diffs));
    diffs
        .iter()
        .map(|d| lector_workspace::risk::collect_facts(d, langs, tests))
        .collect()
}

/// The review-queue order for `weights` from already analyzed facts: the attention group by
/// descending score, then the low-value group (the same rule as `review_queue::analyze`).
pub fn queue_order_of(
    facts: &[lector_workspace::risk::RiskFacts],
    weights: &lector_workspace::risk::RiskWeights,
) -> Vec<usize> {
    let ranked = lector_workspace::risk::rank_facts(facts, weights);
    let (low, attention): (Vec<_>, Vec<_>) =
        ranked.into_iter().partition(|(_, r)| r.is_low_value());
    attention.into_iter().chain(low).map(|(i, _)| i).collect()
}
