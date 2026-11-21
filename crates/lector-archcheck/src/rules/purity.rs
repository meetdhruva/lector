//! `purity` (ADR 0001): `crates/lector-review` is stateless.
//!
//! Applies to the non-test code of `crates/lector-review` once it exists (until then the rule
//! prints a notice and finds nothing). Flags:
//!
//! * `static mut`,
//! * any `static` item whose name is not in `arch/review-statics.allow` (regex and
//!   tree-sitter query tables in `OnceLock` are allowed by name), and `OnceLock`, `LazyLock`,
//!   `Lazy`, `OnceCell` or `LazyCell` anywhere outside such an allowed item,
//! * `thread::spawn`, `thread::Builder`, `.spawn_scoped(`,
//! * file writes: `fs::write`, `fs::remove_file`, `fs::remove_dir`, `fs::remove_dir_all`,
//!   `fs::rename`, `fs::copy`, `fs::create_dir`, `fs::create_dir_all`, `File::create`,
//!   `File::create_new`, `OpenOptions`.
//!
//! What it cannot see: state hidden in a dependency, interior mutability in a `const`, and
//! writes through a handle passed in (reading through the `lector-git`/`lector-fs` handles is
//! allowed by the ADR).

use crate::config::{AllowList, Config};
use crate::report::{RuleOutput, Unit, Violation};
use crate::rules::thread_spawn_at;
use crate::source::SourceFile;

/// Rule name.
pub const RULE: &str = "purity";

/// Directory of the crate the rule applies to.
pub const CRATE_DIR: &str = "crates/lector-review/";

const FS_WRITES: &[&str] = &[
    "write",
    "remove_file",
    "remove_dir",
    "remove_dir_all",
    "rename",
    "copy",
    "create_dir",
    "create_dir_all",
];
const LAZY_TYPES: &[&str] = &["OnceLock", "LazyLock", "Lazy", "OnceCell", "LazyCell"];

/// Run the rule over all files.
pub fn check(files: &[SourceFile], cfg: &Config) -> RuleOutput {
    if !files.iter().any(|f| f.path.starts_with(CRATE_DIR)) {
        return RuleOutput::new(RULE, Vec::new(), Unit::Count).with_notice(
            "crates/lector-review does not exist yet; purity is a no-op until it does",
        );
    }
    let allow = cfg.allow("review-statics");
    let found = files
        .iter()
        .filter(|f| f.path.starts_with(CRATE_DIR))
        .flat_map(|f| scan_file(f, &allow))
        .collect();
    RuleOutput::new(RULE, found, Unit::Count)
}

/// Findings in one file of the review crate.
pub fn scan_file(f: &SourceFile, allowed_statics: &AllowList) -> Vec<Violation> {
    let mut out = Vec::new();
    let n = f.tokens().len();
    let mut i = 0;
    while i < n {
        if f.is_test(i) {
            i += 1;
            continue;
        }
        let t = f.tok(i);
        let mut v = |what: String| out.push(Violation::new(RULE, &f.path, f.line(i), what));
        if t == "static" && f.tokens()[i].kind == crate::lexer::TokKind::Ident {
            if f.tok(i + 1) == "mut" {
                v("`static mut` in lector-review (ADR 0001)".to_string());
            } else {
                let name = f.tok(i + 1).to_string();
                if allowed_statics.contains(&name) {
                    // Skip the whole allowed item, including its `OnceLock` type.
                    while i < n && f.tok(i) != ";" {
                        i += 1;
                    }
                } else {
                    v(format!("`static {name}` in lector-review; the crate is stateless (ADR 0001); allow immutable tables by name in arch/review-statics.allow"));
                }
            }
        } else if LAZY_TYPES.contains(&t) {
            v(format!(
                "`{t}` in lector-review outside an allowed static (ADR 0001)"
            ));
        } else if let Some(what) = thread_spawn_at(f, i) {
            v(format!("`{what}` in lector-review: no threads (ADR 0001)"));
        } else if f.seq(i, &["fs", "::"])
            && FS_WRITES.contains(&f.tok(i + 2))
            && f.tok(i + 3) == "("
        {
            v(format!(
                "`fs::{}` in lector-review: no writes (ADR 0001)",
                f.tok(i + 2)
            ));
        } else if f.seq(i, &["File", "::", "create"]) || f.seq(i, &["File", "::", "create_new"]) {
            v("`File::create` in lector-review: no writes (ADR 0001)".to_string());
        } else if t == "OpenOptions" && f.tok(i + 1) == "::" {
            v("`OpenOptions` in lector-review: no writes (ADR 0001)".to_string());
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::file;

    fn run(src: &str, allow: &[&str]) -> Vec<(u32, String)> {
        scan_file(
            &file("crates/lector-review/src/a.rs", src),
            &AllowList::from_entries(allow.iter().copied()),
        )
        .into_iter()
        .map(|v| (v.line, v.message))
        .collect()
    }

    #[test]
    fn no_op_until_the_crate_exists() {
        let out = check(
            &[file("crates/lector-app/src/a.rs", "static mut X: u8 = 0;")],
            &Config::default(),
        );
        assert!(out.violations.is_empty());
        assert!(out.notice.is_some());
    }

    #[test]
    fn applies_to_the_crate_only() {
        let files = [
            file("crates/lector-app/src/a.rs", "static mut X: u8 = 0;"),
            file("crates/lector-review/src/a.rs", "static mut X: u8 = 0;"),
        ];
        let out = check(&files, &Config::default());
        assert_eq!(out.violations.len(), 1);
        assert!(out.notice.is_none());
    }

    #[test]
    fn statics_and_lazy_state() {
        let src = "static mut COUNTER: u32 = 0;\nstatic RE: OnceLock<Regex> = OnceLock::new();\nstatic CACHE: LazyLock<Mutex<Map>> = LazyLock::new(|| m());\nfn f() { let c = OnceLock::new(); }\nconst K: u8 = 1;\nfn g(x: &'static str) {}";
        let v = run(src, &["RE"]);
        assert_eq!(
            v.iter().map(|(l, _)| *l).collect::<Vec<_>>(),
            [1, 3, 3, 3, 4]
        );
    }

    #[test]
    fn threads_and_writes() {
        let src = "fn f() {\n thread::spawn(g);\n std::fs::write(p, b);\n fs::remove_file(p);\n let f = File::create(p);\n OpenOptions::new();\n fs::read(p);\n}";
        let v = run(src, &[]);
        assert_eq!(
            v.iter().map(|(l, _)| *l).collect::<Vec<_>>(),
            [2, 3, 4, 5, 6]
        );
    }

    #[test]
    fn tests_comments_and_strings_are_ignored() {
        let src = "// static mut X\nfn f() { let s = \"fs::write(\"; }\n#[cfg(test)]\nmod t { static mut Y: u8 = 0; fn g() { fs::write(p, b); } }";
        assert!(run(src, &[]).is_empty());
    }
}
