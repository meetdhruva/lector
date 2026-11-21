//! `size` (ADR 0006): hand-written `.rs` files stay at or under 1,500 lines.
//!
//! The baseline stores each oversized file's current line count (the key's value is the line
//! count, not a number of violations): a baselined file may only shrink, a new file may not
//! exceed the limit. Generated files are exempt by path in `arch/generated.allow`.

use crate::config::Config;
use crate::report::{RuleOutput, Unit, Violation};
use crate::source::SourceFile;

/// Rule name.
pub const RULE: &str = "size";

/// Maximum lines in a hand-written source file.
pub const LIMIT: usize = 1500;

/// Run the rule over all files.
pub fn check(files: &[SourceFile], cfg: &Config) -> RuleOutput {
    let generated = cfg.allow("generated");
    let mut found = Vec::new();
    for f in files {
        let lines = f.line_count();
        if lines > LIMIT && !generated.exempt_path(&f.path) {
            found.push(
                Violation::new(
                    RULE,
                    &f.path,
                    1,
                    format!("{lines} lines (limit {LIMIT}); split along model/input/render seams (ADR 0006)"),
                )
                .weighted(lines as u64),
            );
        }
    }
    found.extend(generated.stale_violations(RULE, &|_| false));
    RuleOutput::new(RULE, found, Unit::Files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AllowList;
    use crate::rules::file;

    fn lines(n: usize) -> String {
        "fn f() {}\n".repeat(n)
    }

    #[test]
    fn limit_is_inclusive_and_crlf_counts_like_lf() {
        let ok = file("crates/a/src/ok.rs", &lines(1500));
        let big = file("crates/a/src/big.rs", &lines(1501).replace('\n', "\r\n"));
        let out = check(&[ok, big], &Config::default());
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.entries["crates/a/src/big.rs"], 1501);
        assert_eq!(out.total(), 1);
    }

    #[test]
    fn generated_files_are_exempt() {
        let mut cfg = Config::default();
        cfg.set_allow(
            "generated",
            AllowList::from_entries(["crates/lector-icons/src/generated.rs"]),
        );
        let g = file("crates/lector-icons/src/generated.rs", &lines(5000));
        assert!(check(&[g], &cfg).violations.is_empty());
    }
}
