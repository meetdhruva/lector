//! `threads` (ADR 0003): background threads are created in `lector-base` and the scheduler only.
//!
//! Flags `thread::spawn`, `thread::Builder` and `.spawn_scoped(` in non-test code outside the
//! files listed in `arch/threads.allow`, **through any alias**: `use std::thread as t; t::spawn(..)`,
//! `use std::thread::spawn as go; go(..)`, `use std::thread::{self as th, Builder as B}`, a
//! re-export (`pub use std::thread as t;` in another file) and a glob import (`use std::thread::*;`)
//! all resolve to the target (see [`crate::alias`]). A name that cannot be resolved but ends in
//! `spawn` or `Builder` and could have come from a glob import is reported as a guess. Also checks
//! that every `spawn_named(kind, ...)` call (through any alias of `spawn_named`) names a kind from
//! the closed list (`kind:<name>` entries of the same file); a kind listed as `test-only:<name>`
//! may only be spawned from the files named by `test-file:<glob>` entries (test-support code that
//! is compiled into a library, such as the mock forge server).
//!
//! Allowance hygiene: a file entry that exempts no finding, a `kind:` entry no non-test code
//! names, and a `test-only` / `test-file` entry that matches nothing are violations.
//!
//! What it cannot see: `thread::scope` (bounded, not flagged on purpose), `spawn_named` calls
//! whose kind is a variable, a spawn inside a macro body that only expands to the call,
//! generated code, `include!`d files, and a function pointer to `spawn` passed around under a
//! name the file never imports (`let f = other_crate::reexported_spawn;`).

use std::collections::BTreeSet;

use crate::alias::{Globals, Resolver};
use crate::config::{AllowList, Config};
use crate::report::{RuleOutput, Unit, Violation};
use crate::rules::detect::{sites, thread_spawns};
use crate::rules::str_value;
use crate::source::SourceFile;

/// Rule name.
pub const RULE: &str = "threads";

/// Run the rule over all files.
pub fn check(files: &[SourceFile], cfg: &Config) -> RuleOutput {
    let allow = cfg.allow("threads");
    let globals = Globals::build(files);
    let mut mentioned: BTreeSet<String> = BTreeSet::new();
    let mut found: Vec<Violation> = Vec::new();
    for f in files {
        found.extend(scan_file_with(f, &allow, &globals));
        if !allow.matches_path(&f.path) {
            for i in 0..f.tokens().len() {
                if !f.is_test(i) && f.seq(i, &["ThreadKind", "::"]) {
                    mentioned.insert(normalize_kind(f.tok(i + 2)));
                }
            }
        }
    }
    let test_only = allow.tagged("test-only");
    let test_files = allow.tagged("test-file");
    for kind in allow.tagged("kind") {
        let norm = normalize_kind(kind);
        if mentioned.contains(&norm) || test_only.iter().any(|t| normalize_kind(t) == norm) {
            allow.mark_tagged("kind", kind);
        }
    }
    for k in &test_only {
        let norm = normalize_kind(k);
        if mentioned.contains(&norm) || files.iter().any(|f| !f.test_only && spawns_kind(f, &norm))
        {
            allow.mark_tagged("test-only", k);
        }
    }
    for g in &test_files {
        if files.iter().any(|f| crate::config::glob_match(g, &f.path)) {
            allow.mark_tagged("test-file", g);
        }
    }
    found.extend(allow.stale_violations(RULE, &|_| false));
    RuleOutput::new(RULE, found, Unit::Count)
}

fn spawns_kind(f: &SourceFile, norm: &str) -> bool {
    (0..f.tokens().len()).any(|i| {
        !f.is_test(i)
            && f.tok(i) == "spawn_named"
            && f.tok(i + 1) == "("
            && kind_argument(f, i + 1).is_some_and(|k| normalize_kind(&k) == norm)
    })
}

fn normalize_kind(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Findings in one file (no workspace table: only the file's own names resolve).
pub fn scan_file(f: &SourceFile, allow: &AllowList) -> Vec<Violation> {
    scan_file_with(f, allow, &Globals::default())
}

/// Findings in one file, with the workspace's re-export table.
pub fn scan_file_with(f: &SourceFile, allow: &AllowList, globals: &Globals) -> Vec<Violation> {
    let kinds: Vec<String> = allow
        .tagged("kind")
        .into_iter()
        .map(normalize_kind)
        .collect();
    let test_only: Vec<String> = allow
        .tagged("test-only")
        .into_iter()
        .map(normalize_kind)
        .collect();
    let test_files = allow.tagged("test-file");
    let r = Resolver::new(f, globals);
    let mut spawns: Vec<Violation> = thread_spawns(f, &r)
        .into_iter()
        .map(|h| {
            Violation::new(
                RULE,
                &f.path,
                f.line(h.tok),
                format!("{} outside the thread owners; use lector_base::thread::spawn_named or the lector-app scheduler (ADR 0003)", h.describe()),
            )
        })
        .collect();
    if !spawns.is_empty() && allow.exempt_path(&f.path) {
        spawns.clear();
    }
    let mut out = spawns;
    for s in sites(f, &r) {
        if s.import || !s.res.last_is("spawn_named") || f.tok(s.end) != "(" {
            continue;
        }
        let Some(kind) = kind_argument(f, s.end) else {
            continue;
        };
        let norm = normalize_kind(&kind);
        if !kinds.contains(&norm) {
            out.push(Violation::new(
                RULE,
                &f.path,
                f.line(s.tok),
                format!("spawn_named kind `{kind}` is not in arch/threads.allow (the list of kinds is closed; add `kind:<name>` with a reason)"),
            ));
        } else if test_only.contains(&norm)
            && !test_files
                .iter()
                .any(|g| crate::config::glob_match(g, &f.path))
        {
            out.push(Violation::new(
                RULE,
                &f.path,
                f.line(s.tok),
                format!("spawn_named kind `{kind}` is test-only (`test-only:` in arch/threads.allow) and may be spawned from the `test-file:` files only"),
            ));
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

    // Bypasses the audit demonstrated: each of these used to pass.

    #[test]
    fn bypass_thread_module_alias() {
        let src = "use std::thread as t;\nfn f() {\n t::spawn(|| {});\n}";
        let v = run("crates/a/src/x.rs", src, &[]);
        assert_eq!(v.iter().map(|v| v.line).collect::<Vec<_>>(), [3], "{v:?}");
    }

    #[test]
    fn renamed_function_import_and_group_aliases_are_caught() {
        let src = "use std::thread::spawn as go;\nuse std::thread::{self as th, Builder as B};\nfn f() { go(a); th::spawn(b); B::new(); }";
        let v = run("crates/a/src/x.rs", src, &[]);
        // the import of `spawn` and of `Builder`, and the three uses
        assert_eq!(
            v.iter().map(|v| v.line).collect::<Vec<_>>(),
            [1, 2, 3, 3, 3],
            "{v:?}"
        );
    }

    #[test]
    fn glob_imports_and_function_values_are_caught() {
        let src = "use std::thread::*;\nfn f() { spawn(a); let g = std::thread::spawn; }";
        let v = run("crates/a/src/x.rs", src, &[]);
        assert_eq!(
            v.iter().map(|v| v.line).collect::<Vec<_>>(),
            [2, 2],
            "{v:?}"
        );
    }

    #[test]
    fn a_name_that_cannot_be_resolved_is_reported_as_a_guess() {
        let src = "use crate::compat::*;\nfn f() { t::spawn(a); other::run(b); }";
        let v = run("crates/a/src/x.rs", src, &[]);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(
            v[0].message.contains("cannot be resolved"),
            "{}",
            v[0].message
        );
        // without a glob there is nothing that could hide a spawn
        assert!(run("crates/a/src/x.rs", "fn f() { t::spawn(a); }", &[]).is_empty());
    }

    #[test]
    fn a_reexport_in_another_file_is_followed() {
        let util = file("crates/a/src/util.rs", "pub use std::thread as t;");
        let user = file(
            "crates/a/src/x.rs",
            "use crate::util::t;\nfn f() { t::spawn(a); }",
        );
        let cfg = {
            let mut c = Config::default();
            c.set_allow("threads", AllowList::default());
            c
        };
        let out = check(&[util, user], &cfg);
        assert_eq!(out.violations.len(), 1, "{:?}", out.violations);
        assert_eq!(out.violations[0].file, "crates/a/src/x.rs");
    }

    #[test]
    fn other_thread_apis_are_not_spawns() {
        let src = "use std::thread as t;\nuse my::thread as mt;\nfn f() { t::sleep(d); t::scope(|s| {}); mt::run(g); t::current(); }";
        assert!(run("crates/a/src/x.rs", src, &[]).is_empty());
    }

    #[test]
    fn test_only_kinds_may_be_spawned_from_test_files_only() {
        let allow = [
            "kind:mock-server",
            "test-only:mock-server",
            "test-file:crates/f/src/mock.rs",
        ];
        let src = "fn f() { spawn_named(ThreadKind::MockServer, \"m\", g); }";
        assert!(run("crates/f/src/mock.rs", src, &allow).is_empty());
        let v = run("crates/f/src/other.rs", src, &allow);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].message.contains("test-only"));
        // and an aliased spawn_named is still checked
        let src = "use lector_base::thread::spawn_named as sn;\nfn f() { sn(ThreadKind::Mystery, \"m\", g); }";
        assert_eq!(run("crates/f/src/other.rs", src, &allow).len(), 1);
    }

    #[test]
    fn stale_file_entries_unused_kinds_and_unmatched_test_entries_fail() {
        let files = [
            file(
                "crates/a/src/x.rs",
                "fn f() { spawn_named(ThreadKind::PtyReader, \"p\", g); }",
            ),
            file("crates/a/src/clean.rs", "fn f() {}"),
            file("crates/b/src/owner.rs", "fn f() { std::thread::spawn(g); }"),
        ];
        let mut cfg = Config::default();
        cfg.set_allow(
            "threads",
            AllowList::from_entries([
                "crates/b/src/owner.rs",
                "crates/a/src/clean.rs",
                "kind:pty-reader",
                "kind:never-used",
                "test-only:ghost",
                "test-file:crates/zzz/**",
            ]),
        );
        let out = check(&files, &cfg);
        let stale: Vec<&str> = out
            .violations
            .iter()
            .filter(|v| v.message.starts_with("stale allowance"))
            .map(|v| v.message.as_str())
            .collect();
        assert_eq!(stale.len(), 4, "{stale:?}");
        for gone in [
            "crates/a/src/clean.rs",
            "never-used",
            "test-only:ghost",
            "crates/zzz/**",
        ] {
            assert!(stale.iter().any(|m| m.contains(gone)), "{gone}: {stale:?}");
        }
        assert!(out
            .violations
            .iter()
            .all(|v| v.file != "crates/b/src/owner.rs"));
    }
}
