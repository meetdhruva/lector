//! Risk-ordering evaluation: the labelled changesets in `tests/risk_eval/cases/*.json` are ranked
//! by the review queue (`lector_app::review_queue::analyze`, i.e. `lector_workspace::risk`) and the
//! rank of the file a reviewer must see first is scored (top-1, top-3, MRR).
//!
//! * The set has a fixed 70/30 tune/holdout split. The weights were fitted on the tune split only
//!   (`examples/risk_fit.rs`); the holdout split guards against regressions that overfit.
//! * `holdout_top1_does_not_regress` fails when the holdout top-1 drops below the measured value
//!   minus a margin. When you improve the model, raise the measured numbers; when a change loses
//!   accuracy on purpose, say so in `tests/risk_eval/README.md` and lower them.
//! * Run `cargo run -p lector-app --example risk_eval -- --misses` for the full table.

mod risk_eval {
    pub mod harness;
}
use risk_eval::harness;

use std::collections::BTreeSet;

use lector_syntax::LanguageRegistry;
use lector_workspace::risk::{self, EvidenceSide, RiskWeights};

/// Holdout top-1 measured when the set was frozen: 18 of 18.
const MEASURED_HOLDOUT_TOP1: usize = 18;
/// Allowed loss, in cases (about 11 percentage points of the 18 holdout cases).
const HOLDOUT_MARGIN: usize = 2;
/// Tune top-1 measured: 40 of 42; allowed loss 3 cases.
const MEASURED_TUNE_TOP1: usize = 40;
const TUNE_MARGIN: usize = 3;

fn evaluation() -> harness::Evaluation {
    let langs = LanguageRegistry::new();
    harness::evaluate(&harness::load_cases(), |c| harness::queue_order(c, &langs))
}

#[test]
fn the_set_is_large_diverse_and_well_formed() {
    let cases = harness::load_cases();
    assert!(cases.len() >= 60, "{} cases", cases.len());
    let ids: BTreeSet<&str> = cases.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids.len(), cases.len(), "ids are unique");
    let tune = cases.iter().filter(|c| c.split == "tune").count();
    let holdout = cases.iter().filter(|c| c.split == "holdout").count();
    assert_eq!(tune + holdout, cases.len(), "every case is tune or holdout");
    // A fixed 70/30 split.
    assert_eq!(tune, (cases.len() * 7 + 5) / 10, "{tune} tune cases");
    let share = holdout as f64 / cases.len() as f64;
    assert!((0.28..=0.32).contains(&share), "holdout share {share:.3}");
    for case in &cases {
        assert!(!case.risky.is_empty(), "{} has no label", case.id);
        assert!(
            case.files.len() >= 3,
            "{}: mixed changes need several files",
            case.id
        );
        for r in &case.risky {
            assert!(
                case.files.iter().any(|f| &f.path == r),
                "{}: unknown risky path {r}",
                case.id
            );
        }
        let paths: BTreeSet<&str> = case.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths.len(),
            case.files.len(),
            "{}: duplicate paths",
            case.id
        );
        assert!(
            case.files.len() > case.risky.len(),
            "{}: needs files that are not risky",
            case.id
        );
    }
    let langs: BTreeSet<&str> = cases.iter().map(|c| c.language.as_str()).collect();
    for want in ["rust", "typescript", "python", "go"] {
        assert!(langs.contains(want), "no {want} cases");
    }
    let categories: BTreeSet<&str> = cases.iter().map(|c| c.category.as_str()).collect();
    for want in [
        "api-break",
        "error-handling",
        "security",
        "unsafe-exec",
        "concurrency",
        "migration",
        "deps-ci",
        "tests-removed",
        "config-flip",
        "hard-negative",
    ] {
        assert!(categories.contains(want), "no {want} cases");
    }
    let hard = cases
        .iter()
        .filter(|c| c.category == "hard-negative")
        .count();
    assert!(hard >= 6, "{hard} hard negatives");
    // Mixed changes where the risky file is small: the risky file changes fewer lines than the
    // changeset's largest other file in most cases.
    let langs_reg = LanguageRegistry::new();
    let small = cases
        .iter()
        .filter(|c| {
            let diffs = harness::diffs_of(c, &langs_reg);
            let churn = |i: usize| {
                let s = diffs[i].stats();
                s.added + s.removed
            };
            let risky_max = (0..c.files.len())
                .filter(|&i| c.risky.contains(&c.files[i].path))
                .map(churn)
                .max()
                .unwrap_or(0);
            let other_max = (0..c.files.len())
                .filter(|&i| !c.risky.contains(&c.files[i].path))
                .map(churn)
                .max()
                .unwrap_or(0);
            risky_max < other_max
        })
        .count();
    assert!(
        small * 10 >= cases.len() * 9,
        "risky file is the smaller change in {small} cases"
    );
}

#[test]
fn the_target_of_seven_in_ten_is_met_overall() {
    let ev = evaluation();
    assert!(ev.all.top1_rate() >= 0.7, "{}", ev.all.line());
    assert!(ev.tune.top1_rate() >= 0.7, "{}", ev.tune.line());
    assert!(ev.holdout.top1_rate() >= 0.7, "{}", ev.holdout.line());
}

#[test]
fn holdout_top1_does_not_regress() {
    let ev = evaluation();
    let floor = MEASURED_HOLDOUT_TOP1 - HOLDOUT_MARGIN;
    assert!(
        ev.holdout.top1 >= floor,
        "holdout top-1 {} of {} is below the floor {floor} (measured {MEASURED_HOLDOUT_TOP1} minus {HOLDOUT_MARGIN}): {}",
        ev.holdout.top1,
        ev.holdout.n,
        ev.holdout.line()
    );
    assert!(ev.holdout.mrr() >= 0.9, "{}", ev.holdout.line());
    assert!(ev.holdout.top3_rate() >= 0.9, "{}", ev.holdout.line());
    let floor = MEASURED_TUNE_TOP1 - TUNE_MARGIN;
    assert!(
        ev.tune.top1 >= floor,
        "tune top-1 below {floor}: {}",
        ev.tune.line()
    );
}

#[test]
fn every_category_keeps_most_of_its_accuracy() {
    let ev = evaluation();
    for (cat, m) in &ev.by_category {
        assert!(
            m.top3 == m.n || m.n >= 5 && m.top3 + 1 >= m.n,
            "{cat}: {}",
            m.line()
        );
        assert!(m.mrr() >= 0.75, "{cat}: {}", m.line());
    }
}

#[test]
fn fitting_machinery_agrees_with_the_review_queue() {
    // The fit works on cached facts; the product path is `analyze`. They must order alike.
    let langs = LanguageRegistry::new();
    let weights = RiskWeights::default();
    for case in harness::load_cases() {
        let via_queue = harness::queue_order(&case, &langs);
        let via_facts = harness::queue_order_of(&harness::facts_of(&case, &langs), &weights);
        assert_eq!(via_queue, via_facts, "{}", case.id);
    }
}

#[test]
fn reasons_of_risky_files_point_at_evidence() {
    let langs = LanguageRegistry::new();
    let cases = harness::load_cases();
    let mut with_evidence = 0;
    let mut risky_total = 0;
    for case in &cases {
        let diffs = harness::diffs_of(case, &langs);
        let ranked = risk::rank_files_with(&diffs, &langs);
        for (i, report) in ranked {
            let file = &case.files[i];
            for reason in &report.reasons {
                let Some(ev) = &reason.evidence else { continue };
                assert_eq!(ev.path, file.path, "{}", case.id);
                let text = match ev.side {
                    EvidenceSide::Old => file.base.as_deref(),
                    EvidenceSide::New => file.head.as_deref(),
                }
                .unwrap_or("");
                let lines = text.lines().count() as u32;
                assert!(
                    ev.lines.start < ev.lines.end && ev.lines.end <= lines.max(1),
                    "{}: {:?} outside {lines} lines of {}",
                    case.id,
                    ev,
                    file.path
                );
            }
            if case.risky.contains(&file.path) {
                risky_total += 1;
                if report.top_reason().is_some_and(|r| r.evidence.is_some()) {
                    with_evidence += 1;
                }
            }
        }
    }
    // Most top reasons of risky files can show their lines (path-only and churn signals cannot).
    assert!(
        with_evidence * 10 >= risky_total * 8,
        "{with_evidence} of {risky_total} top reasons carry evidence"
    );
}

#[test]
fn local_cases_are_scored_only_when_present() {
    // `tests/risk_eval/local` is git-ignored and empty in a clean checkout.
    let local = harness::load_dir(&harness::local_dir());
    let langs = LanguageRegistry::new();
    let ev = harness::evaluate(&local, |c| harness::queue_order(c, &langs));
    assert_eq!(
        ev.all.n, 0,
        "local cases never count towards the committed set"
    );
}
