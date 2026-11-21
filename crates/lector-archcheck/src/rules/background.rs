//! `background` (ADR 0003): background work starts through the scheduler and the one UI bridge.
//!
//! In non-test code outside the files in `arch/background.allow` (the `lector-ui` bridge, which
//! holds the executors of its caller's GPUI context) the rule flags, through any alias
//! (`use gpui::BackgroundExecutor as Ex;`, a re-export in another file, a glob import):
//!
//! * a GPUI executor **type named or stored**: `BackgroundExecutor`, `ForegroundExecutor` (a
//!   field, a parameter, a `let` type, a generic argument: a handle that can spawn);
//! * `background_spawn(` and `background_executor(` calls in `crates/lector-ui*/src`
//!   (`cx.background_executor().timer(..)` is exempt: ADR 0003 leaves timers on GPUI);
//! * **another executor**: any path into `tokio`, `smol`, `async_std`, `async_executor`,
//!   `async_global_executor`, `blocking`, `futures_executor`, `threadpool`, `scoped_threadpool`;
//!   a `ThreadPool` / `LocalPool` / `LocalSpawner` of `futures`, `futures_executor`, `threadpool`
//!   or `rayon`; `rayon::spawn`, `rayon::spawn_fifo` and `rayon::ThreadPoolBuilder` (a detached
//!   spawn or a global pool). `rayon::scope`, `par_iter` and friends are allowed: they finish
//!   before they return, like `std::thread::scope`;
//! * a workspace crate with a **direct dependency** (normal or build) on one of the executor
//!   crates above (decision: `rayon` itself is allowed as a dependency for data parallelism, its
//!   detached `spawn` is not; the check is on the manifest line, key `Cargo.toml`).
//!
//! What it cannot see: a spawn through a handle obtained from a function of another crate whose
//! return type is inferred (`let ex = lector_x::executor();` then `ex.spawn(..)`: the type is
//! never named here, so only the crate-level dependency check would catch a new executor), and
//! calls made by macros.

use crate::alias::{Globals, Resolver};
use crate::config::{AllowList, Config};
use crate::meta::{DepKind, Workspace};
use crate::report::{RuleOutput, Unit, Violation};
use crate::rules::detect::{executor_uses, EXECUTOR_CRATES};
use crate::rules::is_ui_src;
use crate::source::SourceFile;

/// Rule name.
pub const RULE: &str = "background";

/// Run the rule over all files and, when `ws` is given, the workspace manifests.
pub fn check(files: &[SourceFile], cfg: &Config, ws: Option<&Workspace>) -> RuleOutput {
    let allow = cfg.allow("background");
    let globals = Globals::build(files);
    let mut found: Vec<Violation> = files
        .iter()
        .flat_map(|f| scan_file_with(f, &allow, &globals))
        .collect();
    if let Some(ws) = ws {
        found.extend(manifest_findings(ws));
    }
    found.extend(allow.stale_violations(RULE, &|_| false));
    RuleOutput::new(RULE, found, Unit::Count)
}

/// Direct dependencies on executor crates.
pub fn manifest_findings(ws: &Workspace) -> Vec<Violation> {
    let mut out = Vec::new();
    for p in &ws.packages {
        for d in &p.deps {
            if d.kind != DepKind::Dev && EXECUTOR_CRATES.contains(&d.name.as_str()) {
                out.push(Violation::new(
                    RULE,
                    &p.manifest,
                    p.dep_line(&d.name),
                    format!("`{}` depends on the executor crate `{}`; background work goes through the lector-app scheduler (ADR 0003)", p.name, d.name),
                ));
            }
        }
    }
    out
}

/// Findings in one file (only the file's own names resolve).
pub fn scan_file(f: &SourceFile, allow: &AllowList) -> Vec<Violation> {
    scan_file_with(f, allow, &Globals::default())
}

/// Findings in one file, with the workspace's re-export table.
pub fn scan_file_with(f: &SourceFile, allow: &AllowList, globals: &Globals) -> Vec<Violation> {
    if f.test_only {
        return Vec::new();
    }
    let r = Resolver::new(f, globals);
    let mut out: Vec<Violation> = executor_uses(f, &r)
        .into_iter()
        .map(|h| {
            Violation::new(
                RULE,
                &f.path,
                f.line(h.tok),
                format!("{} is an executor outside the bridge; use the lector-app scheduler or the lector-ui background bridge (ADR 0003)", h.describe()),
            )
        })
        .collect();
    if is_ui_src(&f.path) {
        for i in 0..f.tokens().len() {
            let name = f.tok(i);
            if (name == "background_spawn" || name == "background_executor")
                && f.tok(i + 1) == "("
                && i > 0
                && f.tok(i - 1) == "."
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
    }
    if !out.is_empty() && allow.exempt_path(&f.path) {
        out.clear();
    }
    out.sort_by_key(|v| v.line);
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

    // Bypasses the audit demonstrated: each of these used to pass.

    #[test]
    fn a_stored_or_named_executor_handle_is_flagged_through_aliases() {
        let src = "use gpui::{BackgroundExecutor as Ex, ForegroundExecutor};\nstruct Holder { ex: Ex, fg: ForegroundExecutor }\nfn f(e: &gpui::BackgroundExecutor) {}\nfn g(h: &Holder) { h.ex.spawn(async {}).detach(); }";
        let v = run("crates/lector-ui/src/x.rs", src, &[]);
        assert_eq!(
            v.iter().map(|v| v.line).collect::<Vec<_>>(),
            [1, 1, 2, 2, 3],
            "{v:?}"
        );
    }

    #[test]
    fn executor_types_are_flagged_outside_ui_crates_too() {
        let src = "fn f(e: BackgroundExecutor) {}";
        assert_eq!(run("crates/lector-app/src/x.rs", src, &[]).len(), 1);
        assert!(run("crates/lector-app/tests/x.rs", src, &[]).is_empty());
    }

    #[test]
    fn other_executors_and_detached_pools_are_flagged() {
        let src = "use async_std::task;\nuse futures::executor::ThreadPool;\nfn f() {\n smol::spawn(a).detach();\n tokio::spawn(b);\n task::spawn(c);\n let p = ThreadPool::new();\n rayon::spawn(d);\n let q = rayon::ThreadPoolBuilder::new();\n}";
        let v = run("crates/lector-app/src/x.rs", src, &[]);
        assert_eq!(
            v.iter().map(|v| v.line).collect::<Vec<_>>(),
            [1, 2, 4, 5, 6, 7, 8, 9],
            "{v:?}"
        );
    }

    #[test]
    fn scoped_and_data_parallel_rayon_is_fine() {
        let src = "use rayon::prelude::*;\nfn f(v: &[u8]) {\n rayon::scope(|s| { s.spawn(|_| {}); });\n v.par_iter().for_each(|x| {});\n std::thread::scope(|s| {});\n}";
        assert!(run("crates/lector-text/src/x.rs", src, &[]).is_empty());
    }

    #[test]
    fn dependencies_on_executor_crates_are_flagged() {
        use crate::meta::{DepKind, Package, Workspace};
        let ws = Workspace {
            packages: vec![
                Package::bare("a")
                    .with_dep("smol", DepKind::Normal)
                    .with_dep("tokio", DepKind::Dev)
                    .with_dep("rayon", DepKind::Normal),
                Package::bare("b").with_dep("async-executor", DepKind::Build),
            ],
        };
        let v = manifest_findings(&ws);
        let msgs: Vec<&str> = v.iter().map(|v| v.message.as_str()).collect();
        assert_eq!(v.len(), 2, "{msgs:?}");
        assert!(msgs[0].contains("smol") && msgs[1].contains("async-executor"));
    }

    #[test]
    fn an_allowance_that_exempts_nothing_is_stale() {
        let files = [file("crates/lector-ui/src/background.rs", "fn f() {}")];
        let mut cfg = Config::default();
        cfg.set_allow(
            "background",
            AllowList::from_entries(["crates/lector-ui/src/background.rs"]),
        );
        let out = check(&files, &cfg, None);
        assert_eq!(out.violations.len(), 1);
        assert!(out.violations[0].message.contains("stale allowance"));
        let files = [file(
            "crates/lector-ui/src/background.rs",
            "fn f(e: BackgroundExecutor) {}",
        )];
        assert!(check(&files, &cfg, None).violations.is_empty());
    }
}
