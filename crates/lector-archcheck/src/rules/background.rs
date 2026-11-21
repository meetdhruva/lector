//! `background` (ADR 0003): UI code starts background work through the one bridge only.
//!
//! Flags `background_spawn(` and `background_executor(` in non-test code of `crates/lector-ui*/src`
//! outside the files in `arch/background.allow` (the future `lector-ui` bridge module).
//!
//! `cx.background_executor().timer(..)` is exempt: ADR 0003 leaves timers on GPUI.
//!
//! What it cannot see: a spawn through a stored executor handle (`let ex = ...; ex.spawn(...)`
//! is not matched, only the two entry points are), and calls made by macros.

use crate::config::{AllowList, Config};
use crate::report::{RuleOutput, Unit, Violation};
use crate::rules::is_ui_src;
use crate::source::SourceFile;

/// Rule name.
pub const RULE: &str = "background";

/// Run the rule over all files.
pub fn check(files: &[SourceFile], cfg: &Config) -> RuleOutput {
    let allow = cfg.allow("background");
    let found = files.iter().flat_map(|f| scan_file(f, &allow)).collect();
    RuleOutput::new(RULE, found, Unit::Count)
}

/// Findings in one file.
pub fn scan_file(f: &SourceFile, allow: &AllowList) -> Vec<Violation> {
    if !is_ui_src(&f.path) || allow.matches_path(&f.path) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..f.tokens().len() {
        let name = f.tok(i);
        if (name == "background_spawn" || name == "background_executor")
            && f.tok(i + 1) == "("
            && (i == 0 || f.tok(i - 1) != "fn")
            && !f.is_test(i)
            && !(name == "background_executor" && f.seq(i + 1, &["(", ")", ".", "timer", "("]))
        {
            out.push(Violation::new(
                RULE,
                &f.path,
                f.line(i),
                format!("`{name}(` in UI code; use the lector-ui background bridge (ADR 0003)"),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::file;

    fn run(path: &str, src: &str, allow: &[&str]) -> Vec<Violation> {
        scan_file(
            &file(path, src),
            &AllowList::from_entries(allow.iter().copied()),
        )
    }

    #[test]
    fn flags_both_entry_points_in_ui_crates() {
        let src = "fn f(cx: &App) {\n cx.background_spawn(async {});\n let e = cx.background_executor().clone();\n}";
        let v = run("crates/lector-ui-git/src/x.rs", src, &[]);
        assert_eq!(v.iter().map(|v| v.line).collect::<Vec<_>>(), [2, 3]);
    }

    #[test]
    fn timers_on_the_executor_are_exempt() {
        let src = "fn f(cx: &App) {\n cx.background_executor().timer(d).await;\n cx.background_executor()\n    .timer(d)\n    .await;\n let e = cx.background_executor();\n}";
        let v = run("crates/lector-ui/src/x.rs", src, &[]);
        assert_eq!(v.iter().map(|v| v.line).collect::<Vec<_>>(), [6]);
    }

    #[test]
    fn non_ui_crates_comments_strings_tests_and_definitions_are_ignored() {
        let src = "// cx.background_spawn(x)\nfn f() { let s = \"background_executor()\"; }\nfn background_spawn(x: u8) {}\n#[cfg(test)]\nmod t { fn g(cx: &App) { cx.background_spawn(a); } }";
        assert!(run("crates/lector-ui/src/x.rs", src, &[]).is_empty());
        assert!(run(
            "crates/lector-app/src/x.rs",
            "fn f() { cx.background_spawn(a); }",
            &[]
        )
        .is_empty());
    }

    #[test]
    fn bridge_module_is_allowed() {
        let src = "fn f() { cx.background_spawn(a); }";
        assert!(run(
            "crates/lector-ui/src/background.rs",
            src,
            &["crates/lector-ui/src/background.rs"]
        )
        .is_empty());
    }
}
