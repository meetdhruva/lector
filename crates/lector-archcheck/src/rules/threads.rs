//! `threads` (ADR 0003): background threads are created in `lector-base` and the scheduler only.
//!
//! Flags `thread::spawn`, `thread::Builder` and `.spawn_scoped(` in non-test code outside the
//! files listed in `arch/threads.allow`. Also checks that every `spawn_named(kind, ...)` call
//! names a kind from the closed list (`kind:<name>` entries of the same file).
//!
//! What it cannot see: a spawn through a re-exported or renamed import
//! (`use std::thread::spawn as go;`), `thread::scope` (bounded, not flagged on purpose),
//! `rayon::spawn`, and `spawn_named` calls whose kind is a variable.

use crate::config::{AllowList, Config};
use crate::report::{RuleOutput, Unit, Violation};
use crate::rules::{str_value, thread_spawn_at};
use crate::source::SourceFile;

/// Rule name.
pub const RULE: &str = "threads";

/// Run the rule over all files.
pub fn check(files: &[SourceFile], cfg: &Config) -> RuleOutput {
    let allow = cfg.allow("threads");
    let found = files.iter().flat_map(|f| scan_file(f, &allow)).collect();
    RuleOutput::new(RULE, found, Unit::Count)
}

fn normalize_kind(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Findings in one file.
pub fn scan_file(f: &SourceFile, allow: &AllowList) -> Vec<Violation> {
    let allowed_file = allow.matches_path(&f.path);
    let kinds: Vec<String> = allow
        .tagged("kind")
        .into_iter()
        .map(normalize_kind)
        .collect();
    let mut out = Vec::new();
    for i in 0..f.tokens().len() {
        if f.is_test(i) {
            continue;
        }
        if !allowed_file {
            if let Some(what) = thread_spawn_at(f, i) {
                out.push(Violation::new(
                    RULE,
                    &f.path,
                    f.line(i),
                    format!("`{what}` outside the thread owners; use lector_base::thread::spawn_named or the lector-app scheduler (ADR 0003)"),
                ));
            }
        }
        if f.tok(i) == "spawn_named" && f.tok(i + 1) == "(" && (i == 0 || f.tok(i - 1) != "fn") {
            if let Some(kind) = kind_argument(f, i + 1) {
                if !kinds.contains(&normalize_kind(&kind)) {
                    out.push(Violation::new(
                        RULE,
                        &f.path,
                        f.line(i),
                        format!("spawn_named kind `{kind}` is not in arch/threads.allow (the list of kinds is closed; add `kind:<name>` with a reason)"),
                    ));
                }
            }
        }
    }
    out
}

/// The kind named by the first argument of the call whose `(` is at `open`, when it is a
/// string literal or a path (`ThreadKind::PtyReader`); `None` for variables and expressions.
fn kind_argument(f: &SourceFile, open: usize) -> Option<String> {
    let close = f.close_of(open);
    let mut end = open + 1;
    let mut depth = 0usize;
    while end < close {
        match f.tok(end) {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => depth = depth.saturating_sub(1),
            "," if depth == 0 => break,
            _ => {}
        }
        end += 1;
    }
    let arg: Vec<usize> = (open + 1..end).collect();
    match arg.as_slice() {
        [one] if f.tokens()[*one].kind == crate::lexer::TokKind::Str => {
            Some(str_value(f.tok(*one)).to_string())
        }
        many if many.iter().any(|i| f.tok(*i) == "::") => many
            .iter()
            .rev()
            .find(|i| f.tokens()[**i].kind == crate::lexer::TokKind::Ident)
            .map(|i| f.tok(*i).to_string()),
        _ => None,
    }
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
    fn flags_each_spawn_form() {
        let src = "fn f() {\n std::thread::spawn(|| {});\n let b = thread::Builder::new();\n s.spawn_scoped(a, || {});\n}";
        let v = run("crates/a/src/x.rs", src, &[]);
        assert_eq!(v.iter().map(|v| v.line).collect::<Vec<_>>(), [2, 3, 4]);
    }

    #[test]
    fn comments_strings_and_tests_are_ignored() {
        let src = "// thread::spawn(\nfn f() { let s = \"thread::spawn(x)\"; /* thread::spawn */ }\n#[cfg(test)]\nmod t { fn g() { std::thread::spawn(|| {}); } }\n#[test]\nfn h() { thread::spawn(f); }";
        assert!(run("crates/a/src/x.rs", src, &[]).is_empty());
    }

    #[test]
    fn test_only_files_are_ignored() {
        assert!(run("crates/a/tests/t.rs", "fn f() { thread::spawn(g); }", &[]).is_empty());
    }

    #[test]
    fn allowed_files_may_spawn() {
        let src = "fn f() { thread::spawn(g); }";
        assert!(run(
            "crates/lector-base/src/thread.rs",
            src,
            &["crates/lector-base/src/thread.rs"]
        )
        .is_empty());
        assert_eq!(
            run(
                "crates/lector-app/src/other.rs",
                src,
                &["crates/lector-base/src/thread.rs"]
            )
            .len(),
            1
        );
    }

    #[test]
    fn similar_names_do_not_match() {
        let src = "fn f() { my_thread::spawn(g); thread::sleep(d); pool.spawn(g); thread::spawn_named(k, n, f); }";
        assert!(run("crates/a/src/x.rs", src, &[]).is_empty());
    }

    #[test]
    fn spawn_named_kinds_are_checked() {
        let src = "fn f() {\n spawn_named(ThreadKind::PtyReader, \"pty\", g);\n spawn_named(ThreadKind::Mystery, \"m\", g);\n spawn_named(\"lsp-reader\", \"l\", g);\n spawn_named(kind, n, g);\n}\npub fn spawn_named(kind: K) {}";
        let v = run(
            "crates/a/src/x.rs",
            src,
            &["kind:pty-reader", "kind:lsp_reader"],
        );
        assert_eq!(v.len(), 1, "{v:?}");
        assert_eq!(v[0].line, 3);
        assert!(v[0].message.contains("Mystery"));
    }
}
