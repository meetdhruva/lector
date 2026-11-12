//! Instruction-count gate for `lector-search`: the fuzzy file picker over 100k synthetic paths.

use std::hint::black_box;
use std::process::ExitCode;
use std::sync::Arc;

use lector_perf::harness::{gate_main, work, Bench, Work};
use lector_perf::inputs::project_paths;
use lector_search::Picker;

/// Inject 100k paths, type `mod12` key by key and wait for each result set to settle.
fn picker_100k_paths() -> Work {
    let paths = project_paths(100_000, 41);
    work(move || {
        let mut picker: Picker<u32> = Picker::new_for_paths(Arc::new(|| {}));
        let injector = picker.injector();
        for (i, path) in paths.iter().enumerate() {
            injector.push(i as u32, path);
        }
        let query = "mod12";
        for end in 1..=query.len() {
            picker.set_query(&query[..end]);
            while picker.tick(5) {}
        }
        let top = picker.matches(0..20);
        black_box((picker.matched_count(), top.len()));
    })
}

fn main() -> ExitCode {
    gate_main(
        "search",
        &[Bench::threaded(
            "search/picker_100k_paths",
            picker_100k_paths,
        )],
    )
}
