//! Prints the risk-ordering evaluation: `cargo run -p lector-app --example risk_eval`.
//! Add `-- --misses` to list the cases whose risky file is not first, and
//! `-- --local` to also score the importer's cases (`tests/risk_eval/local`).

#[path = "../tests/risk_eval/harness.rs"]
mod harness;

use lector_syntax::LanguageRegistry;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let langs = LanguageRegistry::new();
    let mut cases = harness::load_cases();
    if args.iter().any(|a| a == "--local") {
        cases.extend(harness::load_dir(&harness::local_dir()));
    }
    let ev = harness::evaluate(&cases, |c| harness::queue_order(c, &langs));
    harness::print_report(
        "review queue order (lector_app::review_queue::analyze)",
        &ev,
    );
    if let Some(ix) = args.iter().position(|a| a == "--explain") {
        for id in &args[ix + 1..] {
            let Some(case) = cases.iter().find(|c| c.id.contains(id.as_str())) else {
                continue;
            };
            println!("-- {} (risky: {:?})", case.id, case.risky);
            let diffs = harness::diffs_of(case, &langs);
            for (i, report) in lector_workspace::risk::rank_files_with(&diffs, &langs) {
                println!("  {:.3} {}", report.score, case.files[i].path);
                for r in report.reasons.iter().take(5) {
                    println!("        {:.2} {:?}: {}", r.weight, r.kind, r.message);
                }
            }
        }
    }
    if args.iter().any(|a| a == "--misses") {
        for (id, split, cat, rank) in &ev.ranks {
            if *rank > 1 {
                println!("  miss: {id} [{split}, {cat}] rank {rank}");
            }
        }
    }
}
