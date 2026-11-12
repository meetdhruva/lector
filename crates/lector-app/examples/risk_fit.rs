//! Fits the risk weights on the tune split of the evaluation set:
//! `cargo run -p lector-app --example risk_fit --release`.
//!
//! Deterministic coordinate search. For each weight in declaration order the candidates are the
//! start value times a fixed grid of factors; a candidate is accepted when it improves, in this
//! order, tune top-1, MRR, or the hinge loss of the margin between the best risky file and the best
//! other file (margin 0.05) plus a small L1 pull towards the start weights, so that weights without
//! evidence stay where they are. The factors are bounded (0.6x to 1.5x of the start weight) so that
//! no signal is switched off or inverted by the quirks of a small set. Rounds repeat until nothing
//! changes (`--accuracy-only` ignores the hinge term). The holdout split is only scored, never used. Prints the fitted weights as
//! `name = value` lines for `weights.rs`. With `--flat` the search starts from a neutral set (every
//! raising weight 0.3, dampening 0.5) and unbounded factors, to show how much the signals alone carry.

#[path = "../tests/risk_eval/harness.rs"]
mod harness;

use harness::{Case, Metrics};
use lector_syntax::LanguageRegistry;
use lector_workspace::risk::{RiskFacts, RiskWeights};

const FACTORS: &[f32] = &[0.6, 0.8, 1.0, 1.25, 1.5];
const MARGIN: f32 = 0.05;
const PULL: f64 = 0.02;
const MAX_ROUNDS: usize = 6;

struct Prepared {
    case: Case,
    facts: Vec<RiskFacts>,
}

#[derive(Clone, Copy, Debug)]
struct Objective {
    top1: usize,
    hinge: f64,
    mrr: f64,
    use_hinge: bool,
}

impl Objective {
    fn better_than(&self, other: &Objective) -> bool {
        if self.top1 != other.top1 {
            return self.top1 > other.top1;
        }
        if (self.mrr - other.mrr).abs() > 1e-9 {
            return self.mrr > other.mrr;
        }
        self.hinge < other.hinge - 1e-9 && self.use_hinge
    }
}

fn objective(set: &[&Prepared], w: &RiskWeights, start: &RiskWeights) -> Objective {
    let mut m = Metrics::default();
    let mut hinge = 0.0f64;
    for p in set {
        let order = harness::queue_order_of(&p.facts, w);
        m.add(harness::rank_of_best(&p.case, &order));
        let mut best_risky = 0.0f32;
        let mut best_other = 0.0f32;
        for (i, f) in p.facts.iter().enumerate() {
            let s = f.score(w).score;
            if p.case.risky.contains(&p.case.files[i].path) {
                best_risky = best_risky.max(s);
            } else {
                best_other = best_other.max(s);
            }
        }
        hinge += f64::from((MARGIN - (best_risky - best_other)).max(0.0));
    }
    let pull: f64 = (0..RiskWeights::COUNT)
        .map(|i| f64::from((w.get(i) - start.get(i)).abs()))
        .sum();
    Objective {
        top1: m.top1,
        hinge: hinge + PULL * pull,
        mrr: m.mrr(),
        use_hinge: !std::env::args().any(|a| a == "--accuracy-only"),
    }
}

/// Orderings that must hold whatever the data says: removing an exported item is at least as risky
/// as changing its signature, and a signal never outweighs a stronger version of itself.
fn plausible(w: &RiskWeights) -> bool {
    w.api_removed >= w.api_changed && w.api_changed >= w.api_added && w.dep_script >= w.dep_added
}

fn evaluate(set: &[&Prepared], w: &RiskWeights) -> Metrics {
    let mut m = Metrics::default();
    for p in set {
        m.add(harness::rank_of_best(
            &p.case,
            &harness::queue_order_of(&p.facts, w),
        ));
    }
    m
}

fn main() {
    let langs = LanguageRegistry::new();
    let prepared: Vec<Prepared> = harness::load_cases()
        .into_iter()
        .map(|case| Prepared {
            facts: harness::facts_of(&case, &langs),
            case,
        })
        .collect();
    let tune: Vec<&Prepared> = prepared.iter().filter(|p| p.case.split == "tune").collect();
    let holdout: Vec<&Prepared> = prepared
        .iter()
        .filter(|p| p.case.split == "holdout")
        .collect();
    let flat = std::env::args().any(|a| a == "--flat");
    let priors = RiskWeights::baseline();
    let mut start = priors.clone();
    if flat {
        for i in 0..RiskWeights::COUNT {
            start.set(
                i,
                if RiskWeights::NAMES[i].starts_with("damp_") {
                    0.5
                } else {
                    0.3
                },
            );
        }
    }
    let factors: Vec<f32> = if flat {
        vec![0.0, 0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 2.5]
    } else {
        FACTORS.to_vec()
    };
    let mut w = start.clone();
    println!(
        "start   tune {}\n        holdout {}",
        evaluate(&tune, &w).line(),
        evaluate(&holdout, &w).line()
    );
    let mut best = objective(&tune, &w, &start);
    for round in 0..MAX_ROUNDS {
        let mut changed = false;
        for i in 0..RiskWeights::COUNT {
            let base = start.get(i);
            let current = w.get(i);
            let mut pick = current;
            for f in &factors {
                let cand = (base * f).clamp(0.0, 0.9);
                if (cand - current).abs() < 1e-6 {
                    continue;
                }
                let mut trial = w.clone();
                trial.set(i, cand);
                if !flat && !plausible(&trial) {
                    continue;
                }
                let obj = objective(&tune, &trial, &start);
                if obj.better_than(&best) {
                    best = obj;
                    pick = cand;
                }
            }
            if (pick - current).abs() > 1e-6 {
                w.set(i, pick);
                changed = true;
                println!(
                    "round {round}: {} {current:.3} -> {pick:.3}",
                    RiskWeights::NAMES[i]
                );
            }
        }
        if !changed {
            break;
        }
    }
    println!(
        "fitted  tune {}\n        holdout {}",
        evaluate(&tune, &w).line(),
        evaluate(&holdout, &w).line()
    );
    println!("\n// fitted weights (changed ones marked)");
    for i in 0..RiskWeights::COUNT {
        let mark = if (w.get(i) - start.get(i)).abs() > 1e-6 {
            "  // was"
        } else {
            ""
        };
        if mark.is_empty() {
            println!("{} = {:.2}", RiskWeights::NAMES[i], w.get(i));
        } else {
            println!(
                "{} = {:.2}{mark} {:.2}",
                RiskWeights::NAMES[i],
                w.get(i),
                start.get(i)
            );
        }
    }
}
