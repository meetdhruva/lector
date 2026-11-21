//! `hygiene`: `forbid(unsafe_code)` everywhere, and no new `unwrap`/`expect` in non-test code.
//!
//! * Every library and binary target root (from `cargo metadata`) must contain
//!   `#![forbid(unsafe_code)]`, except crates listed as `crate:<name>` in `arch/unsafe.allow`
//!   (today `lector-text`, for the memory map). In an allowed crate every `unsafe { ... }`
//!   block and `unsafe impl` needs a `// SAFETY:` comment on the same line or in the comment
//!   lines directly above it.
//! * `.unwrap()` and `.expect(` calls in non-test code are counted per file and ratcheted by
//!   the baseline (CLAUDE.md: no `unwrap`/`expect` on user data or I/O).
//!
//! What it cannot see: `unwrap` hidden in a macro definition, and whether an individual
//! `unwrap` is on user data (the ratchet only prevents growth).

use crate::config::Config;
use crate::meta::Workspace;
use crate::report::{RuleOutput, Unit, Violation};
use crate::source::SourceFile;

/// Rule name.
pub const RULE: &str = "hygiene";

/// Run the rule.
pub fn check(ws: &Workspace, files: &[SourceFile], cfg: &Config) -> RuleOutput {
    let allowed_unsafe: Vec<String> = cfg
        .allow("unsafe")
        .tagged("crate")
        .into_iter()
        .map(str::to_string)
        .collect();
    let mut found = Vec::new();
    for pkg in &ws.packages {
        let unsafe_ok = allowed_unsafe.contains(&pkg.name);
        for t in &pkg.targets {
            let is_root = t.kinds.iter().any(|k| {
                matches!(
                    k.as_str(),
                    "lib" | "rlib" | "cdylib" | "dylib" | "staticlib" | "proc-macro" | "bin"
                )
            });
            if !is_root || unsafe_ok {
                continue;
            }
            match files.iter().find(|f| f.path == t.src_path) {
                Some(f) if !has_forbid_unsafe(f) => found.push(
                    Violation::new(
                        RULE,
                        &f.path,
                        1,
                        format!("`{}` target root lacks #![forbid(unsafe_code)]", pkg.name),
                    )
                    .keyed(format!("{} [forbid_unsafe_code]", f.path)),
                ),
                Some(_) => {}
                None => found.push(
                    Violation::new(
                        RULE,
                        &t.src_path,
                        1,
                        format!("target root of `{}` not found under crates/", pkg.name),
                    )
                    .keyed(format!("{} [forbid_unsafe_code]", t.src_path)),
                ),
            }
        }
        for f in files
            .iter()
            .filter(|f| f.path.starts_with(&format!("{}/", pkg.dir)))
        {
            if unsafe_ok {
                found.extend(missing_safety_comments(f));
            }
            found.extend(unwrap_findings(f));
        }
    }
    RuleOutput::new(RULE, found, Unit::Count)
}

/// True when the file contains `#![forbid(unsafe_code)]` (alone or in a list).
pub fn has_forbid_unsafe(f: &SourceFile) -> bool {
    (0..f.tokens().len()).any(|i| {
        f.seq(i, &["#", "!", "[", "forbid", "("]) && {
            let close = f.close_of(i + 4);
            (i + 5..close).any(|k| f.tok(k) == "unsafe_code")
        }
    })
}

/// `unsafe` blocks and impls without a nearby `// SAFETY:` comment.
pub fn missing_safety_comments(f: &SourceFile) -> Vec<Violation> {
    let mut out = Vec::new();
    for i in 0..f.tokens().len() {
        if f.tok(i) != "unsafe" || f.is_test(i) {
            continue;
        }
        let needs = matches!(f.tok(i + 1), "{" | "impl");
        if needs && !has_safety_comment(f, f.line(i)) {
            out.push(
                Violation::new(
                    RULE,
                    &f.path,
                    f.line(i),
                    "`unsafe` without a `// SAFETY:` comment",
                )
                .keyed(format!("{} [safety_comment]", f.path)),
            );
        }
    }
    out
}

fn has_safety_comment(f: &SourceFile, line: u32) -> bool {
    let comments = &f.scanned.comments;
    let mentions = |c: &crate::lexer::Comment| {
        f.text
            .get(c.start..c.end)
            .is_some_and(|t| t.contains("SAFETY:"))
    };
    if comments
        .iter()
        .any(|c| c.start_line <= line && c.end_line >= line && mentions(c))
    {
        return true;
    }
    // Walk up through the comment-only lines directly above.
    let mut l = line;
    while l > 1 {
        l -= 1;
        let covering: Vec<_> = comments
            .iter()
            .filter(|c| c.start_line <= l && c.end_line >= l)
            .collect();
        let has_code = f.tokens().iter().any(|t| t.line == l);
        if covering.is_empty() || has_code {
            return false;
        }
        if covering.iter().any(|c| mentions(c)) {
            return true;
        }
    }
    false
}

/// Count of `.unwrap()` / `.expect(` in non-test code, as one finding per call.
pub fn unwrap_findings(f: &SourceFile) -> Vec<Violation> {
    let mut out = Vec::new();
    for i in 0..f.tokens().len() {
        if f.tok(i) != "." || f.is_test(i) {
            continue;
        }
        let what = if f.seq(i, &[".", "unwrap", "(", ")"]) {
            Some("unwrap()")
        } else if f.seq(i, &[".", "expect", "("]) {
            Some("expect(")
        } else {
            None
        };
        if let Some(what) = what {
            out.push(Violation::new(
                RULE,
                &f.path,
                f.line(i + 1),
                format!("`.{what}` in non-test code (no unwrap/expect on user data or I/O)"),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{Package, Target};
    use crate::rules::file;

    fn ws(name: &str, root: &str) -> Workspace {
        let mut p = Package::bare(name);
        p.targets.push(Target {
            kinds: vec!["lib".into()],
            src_path: root.into(),
        });
        Workspace { packages: vec![p] }
    }

    #[test]
    fn forbid_detection() {
        assert!(has_forbid_unsafe(&file(
            "a.rs",
            "#![forbid(unsafe_code)]\nfn f() {}"
        )));
        assert!(has_forbid_unsafe(&file(
            "a.rs",
            "#![deny(missing_docs)]\n#![forbid(unsafe_code, clippy::x)]"
        )));
        assert!(!has_forbid_unsafe(&file("a.rs", "#![deny(unsafe_code)]")));
        assert!(!has_forbid_unsafe(&file(
            "a.rs",
            "// #![forbid(unsafe_code)]\nfn f() {}"
        )));
        assert!(!has_forbid_unsafe(&file(
            "a.rs",
            "const S: &str = \"#![forbid(unsafe_code)]\";"
        )));
    }

    #[test]
    fn missing_forbid_is_reported_unless_allowlisted() {
        let files = [file("crates/a/src/lib.rs", "fn f() {}")];
        let out = check(&ws("a", "crates/a/src/lib.rs"), &files, &Config::default());
        assert_eq!(out.violations.len(), 1);
        let mut cfg = Config::default();
        cfg.set_allow(
            "unsafe",
            crate::config::AllowList::from_entries(["crate:a"]),
        );
        assert!(check(&ws("a", "crates/a/src/lib.rs"), &files, &cfg)
            .violations
            .is_empty());
    }

    #[test]
    fn safety_comments_must_be_adjacent() {
        let ok = "fn f() {\n    // SAFETY: the map is read-only.\n    // More explanation.\n    let m = unsafe { map() };\n    let n = unsafe { map() }; // SAFETY: same\n}";
        assert!(missing_safety_comments(&file("a.rs", ok)).is_empty());
        let bad = "fn f() {\n    // SAFETY: this one is fine\n    let a = unsafe { x() };\n\n    // unrelated\n    let m = unsafe { map() };\n    let k = 1;\n    let z = unsafe { y() };\n}";
        let lines: Vec<u32> = missing_safety_comments(&file("a.rs", bad))
            .iter()
            .map(|v| v.line)
            .collect();
        assert_eq!(lines, [6, 8]);
    }

    #[test]
    fn unsafe_fn_declarations_do_not_need_a_comment() {
        assert!(
            missing_safety_comments(&file("a.rs", "unsafe fn f() {}\nunsafe trait T {}"))
                .is_empty()
        );
        assert_eq!(
            missing_safety_comments(&file("a.rs", "unsafe impl Send for X {}")).len(),
            1
        );
    }

    #[test]
    fn unwrap_counting_skips_tests_and_lookalikes() {
        let src = "fn f() {\n a.unwrap();\n b.expect(\"x\");\n c.unwrap_or(1);\n d.unwrap_err();\n e.expect_err(\"x\");\n // f.unwrap()\n let s = \".unwrap()\";\n}\n#[cfg(test)]\nmod t { fn g() { a.unwrap(); } }\n#[test]\nfn h() { a.expect(\"y\"); }";
        let v = unwrap_findings(&file("crates/a/src/x.rs", src));
        assert_eq!(v.iter().map(|v| v.line).collect::<Vec<_>>(), [2, 3]);
        assert!(unwrap_findings(&file("crates/a/tests/x.rs", "fn f() { a.unwrap(); }")).is_empty());
    }

    #[test]
    fn unwraps_are_counted_per_file_in_the_crate() {
        let files = [
            file(
                "crates/a/src/lib.rs",
                "#![forbid(unsafe_code)]\nfn f() { a.unwrap(); b.unwrap(); }",
            ),
            file("crates/b/src/lib.rs", "fn f() { a.unwrap(); }"),
        ];
        let out = check(&ws("a", "crates/a/src/lib.rs"), &files, &Config::default());
        assert_eq!(out.entries["crates/a/src/lib.rs"], 2);
        assert!(!out.entries.contains_key("crates/b/src/lib.rs"));
    }
}
