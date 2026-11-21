//! `writes` (ADR 0002): UI crates do not write to the repository or spawn processes.
//!
//! The write API is derived from the source of `crates/lector-git/src`: every `pub fn` taking
//! `self` in an `impl Repo` / `impl ShadowRepo` block of `write.rs`, `sync.rs`, `branch.rs`,
//! `conflict.rs` or `shadow.rs`, minus the names in `arch/git-read.allow` (methods that only
//! read). In non-test code of `crates/lector-ui*/src` the rule flags:
//!
//! * a call `.<name>(` or `Repo::<name>(` / `ShadowRepo::<name>(` of a write-API name,
//! * any use of the identifiers `ProcessCommand` or `Supervisor` (from `lector_process`),
//! * `std::process::Command` (path, import, or `Command::new(` when it is imported).
//!
//! Heuristics, and what the rule cannot see: calls are matched by method *name*, so an
//! unrelated type with a method of the same name is a false positive (names that clash with
//! std methods, such as `push`, are listed in `arch/git-ambiguous.allow` and only match when
//! the receiver's name contains `repo`, `git` or `shadow`); a write reached through a
//! trait object, a closure stored in a variable or a wrapper in another crate is invisible;
//! and a call made via a different name in `lector-app` is, by design, not a violation.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::{AllowList, Config};
use crate::report::{RuleOutput, Unit, Violation};
use crate::rules::is_ui_src;
use crate::source::SourceFile;

/// Rule name.
pub const RULE: &str = "writes";

const GIT_FILES: &[&str] = &["write", "sync", "branch", "conflict", "shadow"];
const RECEIVER_TYPES: &[&str] = &["Repo", "ShadowRepo"];

/// The `lector-git` write API: method names with the argument counts (excluding `self`) they
/// are defined with. A call is only reported when its argument count matches, so a UI method
/// that merely shares a name (`panel.switch_branch(name, window, cx)`) is not a hit.
#[derive(Debug, Clone, Default)]
pub struct WriteApi {
    /// Method name to the set of argument counts.
    pub names: BTreeMap<String, BTreeSet<usize>>,
}

impl WriteApi {
    fn has(&self, name: &str, args: usize) -> bool {
        self.names.get(name).is_some_and(|a| a.contains(&args))
    }
}

/// Derive the write API from the `lector-git` sources.
pub fn derive_write_api(files: &[SourceFile], read_only: &AllowList) -> WriteApi {
    let mut names: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for f in files {
        let is_git_file = GIT_FILES
            .iter()
            .any(|g| f.path == format!("crates/lector-git/src/{g}.rs"));
        if !is_git_file {
            continue;
        }
        for body in impl_bodies(f) {
            for i in body.0..body.1 {
                if f.tok(i) == "fn"
                    && i > 0
                    && f.tok(i - 1) == "pub"
                    && !f.is_test(i)
                    && f.tok(i + 2) == "("
                    && takes_self(f, i + 2)
                    && !read_only.contains(f.tok(i + 1))
                {
                    names
                        .entry(f.tok(i + 1).to_string())
                        .or_default()
                        .insert(param_count(f, i + 2));
                }
            }
        }
    }
    WriteApi { names }
}

/// Token ranges (inside the braces) of inherent `impl Repo` / `impl ShadowRepo` blocks.
fn impl_bodies(f: &SourceFile) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for i in 0..f.tokens().len() {
        if f.tok(i) != "impl" {
            continue;
        }
        let mut j = i + 1;
        if f.tok(j) == "<" {
            let mut depth = 0usize;
            while j < f.tokens().len() {
                match f.tok(j) {
                    "<" => depth += 1,
                    ">" => {
                        depth -= 1;
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }
        let ty = f.tok(j).to_string();
        let mut k = j;
        let mut is_trait_impl = false;
        while k < f.tokens().len() && f.tok(k) != "{" && f.tok(k) != ";" {
            if f.tok(k) == "for" {
                is_trait_impl = true;
            }
            k += 1;
        }
        if f.tok(k) == "{" && !is_trait_impl && RECEIVER_TYPES.contains(&ty.as_str()) {
            out.push((k + 1, f.close_of(k)));
        }
    }
    out
}

/// Number of parameters after `self` of the parameter list opening at `open`.
pub(crate) fn param_count(f: &SourceFile, open: usize) -> usize {
    let close = f.close_of(open);
    let (mut depth, mut angle) = (0usize, 0usize);
    let mut segments = 0usize;
    let mut at_start = true;
    for k in open + 1..close {
        match f.tok(k) {
            "," if depth == 0 && angle == 0 => {
                at_start = true;
                continue;
            }
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => depth = depth.saturating_sub(1),
            "<" if depth == 0 => angle += 1,
            ">" if depth == 0 => angle = angle.saturating_sub(1),
            _ => {}
        }
        if at_start {
            segments += 1;
            at_start = false;
        }
    }
    segments.saturating_sub(1)
}

/// Number of arguments of the call whose `(` is at `open` (closure parameter lists and nested
/// brackets do not split arguments).
pub(crate) fn arg_count(f: &SourceFile, open: usize) -> usize {
    let close = f.close_of(open);
    let mut depth = 0usize;
    let mut args = 0usize;
    let mut at_start = true;
    let mut k = open + 1;
    while k < close {
        let t = f.tok(k);
        if at_start && t == "|" {
            // Closure parameters: skip to the closing pipe.
            k += 1;
            while k < close && f.tok(k) != "|" {
                k += 1;
            }
        } else if depth == 0 && t == "," {
            at_start = true;
            k += 1;
            continue;
        } else {
            match t {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        if at_start {
            args += 1;
            at_start = false;
        }
        k += 1;
    }
    args
}

/// True when the parameter list opening at `open` starts with `self`, `&self`, `&mut self`.
pub(crate) fn takes_self(f: &SourceFile, open: usize) -> bool {
    let mut j = open + 1;
    if f.tok(j) == "&" {
        j += 1;
        if f.tok(j).starts_with('\'') {
            j += 1;
        }
    }
    if f.tok(j) == "mut" {
        j += 1;
    }
    f.tok(j) == "self"
}

/// Calls of the write API in the non-test code of `f`: the token index of the called name and
/// the name. Matching is by name and argument count; ambiguous names also need a repo-like
/// receiver (see the module docs). Shared with the `purity` rule.
pub(crate) fn git_write_calls(
    f: &SourceFile,
    api: &WriteApi,
    ambiguous: &AllowList,
) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for i in 0..f.tokens().len() {
        if f.is_test(i) {
            continue;
        }
        let t = f.tok(i);
        // `.name(` and `Repo::name(`
        if t == "." && f.tok(i + 2) == "(" && api.has(f.tok(i + 1), arg_count(f, i + 2)) {
            let name = f.tok(i + 1);
            if !ambiguous.contains(name) || receiver_looks_like_repo(f, i) {
                out.push((i + 1, name.to_string()));
            }
        } else if RECEIVER_TYPES.contains(&t)
            && f.tok(i + 1) == "::"
            && f.tok(i + 3) == "("
            && api.has(f.tok(i + 2), arg_count(f, i + 3).saturating_sub(1))
        {
            out.push((i + 2, f.tok(i + 2).to_string()));
        }
    }
    out
}

/// A process-spawning construct found by [`process_uses`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProcessUse {
    /// `ProcessCommand` or `Supervisor` (from `lector_process`).
    Process(String),
    /// `std::process::Command` (path, import, or `Command::new(` when it is imported).
    StdCommand,
}

/// Process-spawning constructs in the non-test code of `f`, with their token index. Shared with
/// the `purity` rule.
pub(crate) fn process_uses(f: &SourceFile) -> Vec<(usize, ProcessUse)> {
    let mut out = Vec::new();
    let n = f.tokens().len();
    let mut imports_command = false;
    for i in 0..n {
        if f.is_test(i) {
            continue;
        }
        let t = f.tok(i);
        if (t == "ProcessCommand" || t == "Supervisor")
            && !(i > 0
                && matches!(
                    f.tok(i - 1),
                    "fn" | "struct" | "enum" | "mod" | "type" | "trait"
                ))
        {
            out.push((i, ProcessUse::Process(t.to_string())));
        }
        if f.seq(i, &["process", "::", "Command"]) {
            imports_command = true;
            out.push((i, ProcessUse::StdCommand));
        } else if f.seq(i, &["process", "::", "{"]) {
            let close = f.close_of(i + 2);
            if (i + 3..close).any(|k| f.tok(k) == "Command") {
                imports_command = true;
                out.push((i, ProcessUse::StdCommand));
            }
        }
    }
    if imports_command {
        for i in 0..n {
            if !f.is_test(i)
                && f.seq(i, &["Command", "::", "new", "("])
                && (i == 0 || f.tok(i - 1) != "::")
            {
                out.push((i, ProcessUse::StdCommand));
            }
        }
    }
    out
}

/// Findings in one file (a no-op outside UI crates' non-test sources).
pub fn scan_file(f: &SourceFile, api: &WriteApi, ambiguous: &AllowList) -> Vec<Violation> {
    if !is_ui_src(&f.path) {
        return Vec::new();
    }
    let mut out: Vec<Violation> = git_write_calls(f, api, ambiguous)
        .into_iter()
        .map(|(i, name)| write_call(f, i, &name))
        .collect();
    for (i, hit) in process_uses(f) {
        out.push(match hit {
            ProcessUse::Process(t) => Violation::new(
                RULE,
                &f.path,
                f.line(i),
                format!(
                    "`{t}` in a UI crate; processes are started by lector-app commands (ADR 0002)"
                ),
            ),
            ProcessUse::StdCommand => command_violation(f, i),
        });
    }
    out
}

fn write_call(f: &SourceFile, name_idx: usize, name: &str) -> Violation {
    Violation::new(
        RULE,
        &f.path,
        f.line(name_idx),
        format!("`{name}(` is a lector-git write; call it from a lector-app registry command (ADR 0002)"),
    )
}

fn command_violation(f: &SourceFile, i: usize) -> Violation {
    Violation::new(
        RULE,
        &f.path,
        f.line(i),
        "`std::process::Command` in a UI crate; spawn processes from lector-app (ADR 0002)",
    )
}

/// The identifier a method call is made on (`repo` in `repo.push(`, `repo` in `x.repo().push(`).
fn receiver_looks_like_repo(f: &SourceFile, dot: usize) -> bool {
    receiver_name_contains(f, dot, &["repo", "git", "shadow"])
}

/// Whether the identifier a method call at `dot` is made on contains one of `hints` (case
/// insensitive). Shared with the `purity` rule.
pub(crate) fn receiver_name_contains(f: &SourceFile, dot: usize, hints: &[&str]) -> bool {
    let mut p = dot;
    loop {
        if p == 0 {
            return false;
        }
        p -= 1;
        if f.tok(p) == "?" {
            continue;
        }
        if f.tok(p) == ")" {
            let mut depth = 0usize;
            loop {
                match f.tok(p) {
                    ")" => depth += 1,
                    "(" => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                if p == 0 {
                    return false;
                }
                p -= 1;
            }
            if p == 0 {
                return false;
            }
            p -= 1;
        }
        let name = f.tok(p).to_ascii_lowercase();
        return hints.iter().any(|h| name.contains(h));
    }
}

/// Run the rule over all files.
pub fn check(files: &[SourceFile], cfg: &Config) -> RuleOutput {
    let api = derive_write_api(files, &cfg.allow("git-read"));
    let ambiguous = cfg.allow("git-ambiguous");
    let found = files
        .iter()
        .flat_map(|f| scan_file(f, &api, &ambiguous))
        .collect();
    RuleOutput::new(RULE, found, Unit::Count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::file;

    const GIT_WRITE: &str = "impl Repo {\n    pub fn stage_paths(&self, paths: &[&str]) -> R { }\n    pub fn last_commit_message(&self) -> R { }\n    fn private(&self) {}\n    pub(crate) fn crate_only(&self) {}\n    pub fn constructor(path: &Path) -> Self { }\n    pub fn push(&self, opts: &O) {}\n    #[cfg(test)]\n    pub fn test_helper(&self) {}\n}\nimpl Drop for LockGuard { fn drop(&mut self) {} }\npub fn free_fn(r: &Repo) {}";
    const GIT_SHADOW: &str = "impl ShadowRepo {\n    pub fn restore_paths(&self, s: &S) {}\n    pub fn snapshots(&self) {}\n    pub fn open(d: &Path) {}\n}";

    fn api() -> WriteApi {
        derive_write_api(
            &[
                file("crates/lector-git/src/write.rs", GIT_WRITE),
                file("crates/lector-git/src/shadow.rs", GIT_SHADOW),
                file(
                    "crates/lector-git/src/status.rs",
                    "impl Repo { pub fn status_all(&self) {} }",
                ),
            ],
            &AllowList::from_entries(["last_commit_message", "snapshots"]),
        )
    }

    fn run(path: &str, src: &str) -> Vec<Violation> {
        scan_file(&file(path, src), &api(), &AllowList::from_entries(["push"]))
    }

    #[test]
    fn api_derivation() {
        let api = api();
        let names: Vec<&String> = api.names.keys().collect();
        assert_eq!(names, ["push", "restore_paths", "stage_paths"]);
        assert!(api.has("stage_paths", 1));
        assert!(!api.has("stage_paths", 3));
        assert!(api.has("push", 1));
    }

    #[test]
    fn flags_method_and_path_calls_in_ui_crates() {
        let src = "fn f(repo: &Repo) {\n repo.stage_paths(&[]);\n Repo::stage_paths(repo, &[]);\n self.shadow.restore_paths(&s);\n repo.last_commit_message();\n}";
        let v = run("crates/lector-ui-git/src/x.rs", src);
        assert_eq!(v.iter().map(|v| v.line).collect::<Vec<_>>(), [2, 3, 4]);
    }

    #[test]
    fn same_name_with_another_argument_count_is_not_a_hit() {
        let src = "fn f() {\n panel.stage_paths(paths, window, cx);\n c.update(cx, |c, cx| c.stage_paths(a, window, cx));\n repo.stage_paths(&[\"a\", \"b\"]);\n repo.stage_paths(&v.iter().map(|a, b| a).collect::<Vec<_>>());\n}";
        let v = run("crates/lector-ui-git/src/x.rs", src);
        assert_eq!(v.iter().map(|v| v.line).collect::<Vec<_>>(), [4, 5]);
    }

    #[test]
    fn argument_counting() {
        let f = file(
            "a.rs",
            "x(); x(a); x(a, b,); x(|p, q| p + q, c); x(f(a, b), [1, 2], {3}); x(move |a| a);",
        );
        let opens: Vec<usize> = (0..f.tokens().len())
            .filter(|i| f.tok(*i) == "x")
            .map(|i| i + 1)
            .collect();
        let counts: Vec<usize> = opens.iter().map(|o| arg_count(&f, *o)).collect();
        assert_eq!(counts, [0, 1, 2, 2, 3, 1]);
    }

    #[test]
    fn parameter_counting_ignores_generic_commas() {
        let f = file("a.rs", "impl Repo { pub fn a(&self, m: HashMap<K, V>, x: u8) {} pub fn b(&self) {} pub fn c(&mut self, f: impl Fn(A, B) -> C,) {} }");
        let api = derive_write_api(
            &[file("crates/lector-git/src/write.rs", &f.text)],
            &AllowList::default(),
        );
        assert_eq!(api.names["a"], BTreeSet::from([2]));
        assert_eq!(api.names["b"], BTreeSet::from([0]));
        assert_eq!(api.names["c"], BTreeSet::from([1]));
    }

    #[test]
    fn ambiguous_names_need_a_repo_like_receiver() {
        let src = "fn f() {\n v.push(1);\n self.items.push(x);\n repo.push(&opts);\n self.repo.push(&o);\n self.repo()?.push(&o);\n git_handle.push(&o);\n}";
        let v = run("crates/lector-ui/src/x.rs", src);
        assert_eq!(v.iter().map(|v| v.line).collect::<Vec<_>>(), [4, 5, 6, 7]);
    }

    #[test]
    fn non_ui_code_tests_comments_and_strings_are_ignored() {
        let src = "// repo.stage_paths(x)\nfn f() { let s = \"repo.stage_paths(x)\"; }\n#[cfg(test)]\nmod t { fn g(repo: &Repo) { repo.stage_paths(&[]); } }";
        assert!(run("crates/lector-ui/src/x.rs", src).is_empty());
        assert!(run(
            "crates/lector-app/src/x.rs",
            "fn f(r: &Repo) { r.stage_paths(&[]); }"
        )
        .is_empty());
        assert!(run(
            "crates/lector-ui/tests/x.rs",
            "fn f(r: &Repo) { r.stage_paths(&[]); }"
        )
        .is_empty());
    }

    #[test]
    fn process_types_and_std_command() {
        let src = "use lector_process::{ProcessCommand, Supervisor};\nuse std::process::{self, Command};\nfn f() {\n let c = ProcessCommand::new(\"git\");\n let d = Command::new(\"x\");\n let e = std::process::Command::new(\"y\");\n}\nstruct Supervisor;";
        let v = run("crates/lector-ui-git/src/x.rs", src);
        let mut lines: Vec<u32> = v.iter().map(|v| v.line).collect();
        lines.sort_unstable();
        assert_eq!(lines, [1, 1, 2, 4, 5, 6], "{v:?}");
    }

    #[test]
    fn command_without_a_process_import_is_ignored() {
        let src = "use clap::Command;\nfn f() { let c = Command::new(\"x\"); }";
        assert!(run("crates/lector-ui/src/x.rs", src).is_empty());
    }
}
