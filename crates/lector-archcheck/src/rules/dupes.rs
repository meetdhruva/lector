//! `dupes` (ADR 0005): one `CancelToken`, one fuzzy matcher.
//!
//! * Every `struct CancelToken` in non-test code is reported once there is more than one
//!   (when `crates/lector-base` defines one, every definition outside it is reported).
//! * A fuzzy matcher is recognised by name: a file or directory called `fuzzy`, an inline
//!   `mod fuzzy { ... }`, or a function named `fuzzy`, `fuzzy_*` or `*_fuzzy`. Only
//!   `crates/lector-base` and `crates/lector-search/src/picker*` may have one.
//!
//! What it cannot see: a matcher with an unrelated name (`score_subsequence`).

use crate::config::Config;
use crate::report::{RuleOutput, Unit, Violation};
use crate::source::SourceFile;

/// Rule name.
pub const RULE: &str = "dupes";

const BASE: &str = "crates/lector-base/";
const PICKER: &str = "crates/lector-search/src/picker";

fn allowed_home(path: &str) -> bool {
    path.starts_with(BASE) || path.starts_with(PICKER)
}

fn looks_like_matcher_fn(name: &str) -> bool {
    name == "fuzzy" || name.starts_with("fuzzy_") || name.ends_with("_fuzzy")
}

fn looks_like_matcher_file(path: &str) -> bool {
    let mut parts: Vec<&str> = path.split('/').collect();
    let file = parts.pop().unwrap_or("");
    let stem = file.strip_suffix(".rs").unwrap_or(file);
    stem == "fuzzy" || parts.contains(&"fuzzy")
}

/// Run the rule over all files.
pub fn check(files: &[SourceFile], _cfg: &Config) -> RuleOutput {
    let mut found = Vec::new();
    let mut tokens: Vec<(&str, u32)> = Vec::new();
    for f in files {
        if f.test_only {
            continue;
        }
        if looks_like_matcher_file(&f.path) && !allowed_home(&f.path) {
            found.push(Violation::new(
                RULE,
                &f.path,
                1,
                "fuzzy matcher module outside lector-base and the picker; use lector_base::fuzzy (ADR 0005)",
            ));
        }
        for i in 0..f.tokens().len() {
            if f.is_test(i) {
                continue;
            }
            if f.seq(i, &["struct", "CancelToken"]) {
                tokens.push((&f.path, f.line(i)));
            }
            if f.tok(i) == "fn" && looks_like_matcher_fn(f.tok(i + 1)) && !allowed_home(&f.path) {
                found.push(Violation::new(
                    RULE,
                    &f.path,
                    f.line(i),
                    format!(
                        "`fn {}` is a fuzzy matcher outside lector-base and the picker (ADR 0005)",
                        f.tok(i + 1)
                    ),
                ));
            }
            if f.seq(i, &["mod", "fuzzy", "{"]) && !allowed_home(&f.path) {
                found.push(Violation::new(
                    RULE,
                    &f.path,
                    f.line(i),
                    "inline `mod fuzzy` outside lector-base and the picker (ADR 0005)",
                ));
            }
        }
    }
    found.extend(cancel_token_findings(&tokens));
    RuleOutput::new(RULE, found, Unit::Count)
}

fn cancel_token_findings(defs: &[(&str, u32)]) -> Vec<Violation> {
    if defs.len() <= 1 {
        return Vec::new();
    }
    let in_base = defs.iter().any(|(p, _)| p.starts_with(BASE));
    let message = if in_base {
        "second `struct CancelToken`; use lector_base::CancelToken (ADR 0005)".to_string()
    } else {
        format!(
            "{} definitions of `struct CancelToken`; keep exactly one, in lector-base (ADR 0005)",
            defs.len()
        )
    };
    defs.iter()
        .filter(|(p, _)| !(in_base && p.starts_with(BASE)))
        .map(|(p, line)| Violation::new(RULE, p, *line, message.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::file;

    fn run(files: Vec<SourceFile>) -> RuleOutput {
        check(&files, &Config::default())
    }

    #[test]
    fn several_cancel_tokens_are_all_reported() {
        let out = run(vec![
            file(
                "crates/lector-process/src/a.rs",
                "pub struct CancelToken(u8);",
            ),
            file(
                "crates/lector-search/src/b.rs",
                "pub struct CancelToken { f: u8 }",
            ),
            file(
                "crates/lector-lsp/src/c.rs",
                "struct CancelTokenSource;\n// struct CancelToken\n",
            ),
        ]);
        assert_eq!(out.violations.len(), 2);
    }

    #[test]
    fn a_single_cancel_token_is_fine_and_tests_do_not_count() {
        let out = run(vec![
            file(
                "crates/lector-process/src/a.rs",
                "pub struct CancelToken(u8);",
            ),
            file(
                "crates/lector-search/src/b.rs",
                "#[cfg(test)]\nmod t { struct CancelToken; }",
            ),
            file("crates/lector-lsp/tests/c.rs", "struct CancelToken;"),
        ]);
        assert!(out.violations.is_empty());
    }

    #[test]
    fn base_is_the_canonical_home() {
        let out = run(vec![
            file(
                "crates/lector-base/src/cancel.rs",
                "pub struct CancelToken;",
            ),
            file(
                "crates/lector-process/src/a.rs",
                "pub struct CancelToken(u8);",
            ),
        ]);
        assert_eq!(out.violations.len(), 1);
        assert_eq!(out.violations[0].file, "crates/lector-process/src/a.rs");
    }

    #[test]
    fn fuzzy_modules_and_functions_outside_the_homes() {
        let out = run(vec![
            file(
                "crates/lector-lsp/src/fuzzy.rs",
                "pub fn score(a: &str) -> u32 { 0 }\npub fn fuzzy_match(q: &str) {}",
            ),
            file(
                "crates/lector-ui/src/x.rs",
                "fn rank_fuzzy() {}\nfn fuzzy() {}\nfn unfuzzy_ish() {}\nmod fuzzy { }",
            ),
            file("crates/lector-base/src/fuzzy.rs", "pub fn fuzzy_match() {}"),
            file("crates/lector-search/src/picker.rs", "fn fuzzy_score() {}"),
            file(
                "crates/lector-search/src/picker/index.rs",
                "fn fuzzy_score() {}",
            ),
            file("crates/lector-search/src/other.rs", "fn fuzzy_score() {}"),
        ]);
        let got: Vec<(&str, u32)> = out
            .violations
            .iter()
            .map(|v| (v.file.as_str(), v.line))
            .collect();
        assert_eq!(
            got,
            [
                ("crates/lector-lsp/src/fuzzy.rs", 1),
                ("crates/lector-lsp/src/fuzzy.rs", 2),
                ("crates/lector-search/src/other.rs", 1),
                ("crates/lector-ui/src/x.rs", 1),
                ("crates/lector-ui/src/x.rs", 2),
                ("crates/lector-ui/src/x.rs", 4),
            ]
        );
    }
}
