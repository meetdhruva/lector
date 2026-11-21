//! `purity` (ADR 0001, ADR 0002): `crates/lector-review` is stateless and writes nothing.
//!
//! Applies to the non-test code of `crates/lector-review` once it exists (until then the rule
//! prints a notice and finds nothing). Flags:
//!
//! * **every `static` item**, `static mut` and `static ref` (`lazy_static!`) and the statics
//!   inside `thread_local!` included, whatever its type (`OnceLock`, `LazyLock`, `Mutex`,
//!   `RwLock`, atomics, `Cell`, a plain `u32`): it needs an entry `NAME: Type` in
//!   `arch/review-statics.allow`, and **both the name and the type must match** (types compare
//!   without whitespace), so a second static reusing an allowed name, or an allowed name whose
//!   type was changed to something mutable, is reported. An entry that matches no static is a
//!   violation too. `OnceLock`, `LazyLock`, `Lazy`, `OnceCell` or `LazyCell` anywhere outside an
//!   allowed item are reported as before,
//! * `thread::spawn`, `thread::Builder`, `.spawn_scoped(`,
//! * **writes through a handle** (step 2), in three sub-rules:
//!   * (a) a call of the `lector-git` write API: the list `writes --list-api` prints, matched by
//!     name and argument count, with the `arch/git-ambiguous.allow` names needing a repo-like
//!     receiver, exactly as the `writes` rule matches it;
//!   * (b) a write to `lector-config` state: any mention of a `lector-config` type that has a
//!     writing method (`ReviewStore`, `SessionStore`, `RecentProjects`, `ReviewBackend`, ...:
//!     a handle that can write must not reach the analysis at all), a call `.name(` of a
//!     derived store-writing method (a `pub fn` of `crates/lector-config/src` that takes
//!     `&mut self` or is named like `save*`, `write*`, `set*`, `remove*`, `clear*`, `delete*`,
//!     `put*`, `insert*`, `persist*`, `record*`, `add*`; names in `arch/store-ambiguous.allow`
//!     need a store-like receiver) and a call of a `lector-config` free `write_*` / `save*`
//!     function through a path;
//!   * (c) `std::process::Command` and `ProcessCommand` / `Supervisor` (the `writes` rule's
//!     detection),
//! * file writes (d): `fs::write`, `fs::remove_file`, `fs::remove_dir`, `fs::remove_dir_all`,
//!   `fs::rename`, `fs::copy`, `fs::create_dir`, `fs::create_dir_all`, `fs::hard_link`,
//!   `fs::soft_link`, `fs::symlink`, `fs::set_permissions` (as a call, an import or a value),
//!   `File::create`, `File::create_new`, `File::options` and any mention of `OpenOptions`.
//!
//! What it cannot see: state hidden in a dependency, interior mutability in a `const`, a write
//! through a trait object or a closure stored in a variable, `write_all` on a file handle that
//! was opened elsewhere (opening for write is flagged where it happens), and calls hidden by a
//! macro. Calls renamed on import (`use x as y`) are resolved (see `crate::alias`). Reading through the `lector-git`/`lector-fs` handles is allowed by
//! the ADR. Test code is skipped; when the lexer cannot tell whether code is test-only it is
//! treated as production.

use std::collections::{BTreeMap, BTreeSet};

use crate::alias::{Globals, Resolver};
use crate::config::{AllowList, Config};
use crate::lexer::TokKind;
use crate::report::{RuleOutput, Unit, Violation};
use crate::rules::detect::{fs_writes, process_uses, thread_spawns, ProcessUse};
use crate::rules::writes::{
    arg_count, derive_write_api, git_write_calls, param_count, receiver_name_contains, takes_self,
    WriteApi,
};
use crate::source::SourceFile;

/// Rule name.
pub const RULE: &str = "purity";

/// Directory of the crate the rule applies to.
pub const CRATE_DIR: &str = "crates/lector-review/";

const LAZY_TYPES: &[&str] = &["OnceLock", "LazyLock", "Lazy", "OnceCell", "LazyCell"];

/// Receiver names (substrings) that make a call of an ambiguous store-writing name a hit.
const STORE_RECEIVERS: &[&str] = &[
    "store", "config", "settings", "session", "recent", "backend",
];

/// Name prefixes (and exact names) of methods that write when they are not `&mut self`
/// (interior mutability behind `&self`, as `ReviewStore::set_viewed` has).
const WRITE_WORDS: &[&str] = &[
    "save", "write", "set", "remove", "clear", "delete", "put", "insert", "persist", "record",
    "add",
];

fn is_write_name(name: &str) -> bool {
    WRITE_WORDS.iter().any(|w| {
        name == *w
            || name
                .strip_prefix(w)
                .is_some_and(|rest| rest.starts_with('_'))
    })
}

/// The writing part of `lector-config`'s API, derived from its sources.
#[derive(Debug, Clone, Default)]
pub struct StoreApi {
    /// Writing method names to the argument counts (excluding `self`) they are defined with.
    pub methods: BTreeMap<String, BTreeSet<usize>>,
    /// Types with at least one writing method (store handles).
    pub types: BTreeSet<String>,
    /// Free `pub fn`s named like writes (`write_*`, `save*`).
    pub free_fns: BTreeSet<String>,
}

/// Everything `purity` needs besides the file itself.
#[derive(Debug, Clone, Default)]
pub struct PurityApis {
    /// Statics `lector-review` may declare: `NAME: Type` entries.
    pub statics: AllowList,
    /// The `lector-git` write API (the `writes` rule's).
    pub git: WriteApi,
    /// `lector-git` write names that need a repo-like receiver.
    pub git_ambiguous: AllowList,
    /// The `lector-config` write API.
    pub store: StoreApi,
    /// Store-writing names that need a store-like receiver.
    pub store_ambiguous: AllowList,
}

impl PurityApis {
    /// Derive the APIs from the sources and the allow lists.
    pub fn derive(files: &[SourceFile], cfg: &Config) -> PurityApis {
        PurityApis {
            statics: cfg.allow("review-statics"),
            git: derive_write_api(files, &cfg.allow("git-read")),
            git_ambiguous: cfg.allow("git-ambiguous"),
            store: derive_store_api(files),
            store_ambiguous: cfg.allow("store-ambiguous"),
        }
    }
}

/// Bodies (inside the braces) of inherent `impl Type` and `trait Name` blocks:
/// `(type name, is_trait, first token, closing token)`.
fn type_bodies(f: &SourceFile) -> Vec<(String, bool, usize, usize)> {
    let mut out = Vec::new();
    let n = f.tokens().len();
    for i in 0..n {
        let is_impl = f.tok(i) == "impl";
        let is_trait = f.tok(i) == "trait" && f.tokens()[i].kind == TokKind::Ident;
        if !is_impl && !is_trait {
            continue;
        }
        let mut j = i + 1;
        if is_impl && f.tok(j) == "<" {
            let mut depth = 0usize;
            while j < n {
                match f.tok(j) {
                    "<" => depth += 1,
                    ">" => {
                        depth = depth.saturating_sub(1);
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
        let name = f.tok(j).to_string();
        let mut k = j;
        let mut trait_impl = false;
        while k < n && f.tok(k) != "{" && f.tok(k) != ";" {
            if f.tok(k) == "for" {
                trait_impl = true;
            }
            k += 1;
        }
        if f.tok(k) == "{" && !(is_impl && trait_impl) {
            out.push((name, is_trait, k + 1, f.close_of(k)));
        }
    }
    out
}

/// True when the parameter list opening at `open` starts with `&mut self` or `mut self`.
fn takes_mut_self(f: &SourceFile, open: usize) -> bool {
    let mut j = open + 1;
    if f.tok(j) == "&" {
        j += 1;
        if f.tok(j).starts_with('\'') {
            j += 1;
        }
    }
    f.tok(j) == "mut" && f.tok(j + 1) == "self"
}

/// Derive the writing API of `lector-config` from `crates/lector-config/src`.
pub fn derive_store_api(files: &[SourceFile]) -> StoreApi {
    let mut api = StoreApi::default();
    for f in files
        .iter()
        .filter(|f| f.path.starts_with("crates/lector-config/src/"))
    {
        let bodies = type_bodies(f);
        for (ty, is_trait, start, end) in &bodies {
            for i in *start..*end {
                if f.tok(i) != "fn" || f.is_test(i) || f.tok(i + 2) != "(" || !takes_self(f, i + 2)
                {
                    continue;
                }
                // Inherent methods must be `pub`; trait methods are all callable.
                if !*is_trait && !(i > 0 && f.tok(i - 1) == "pub") {
                    continue;
                }
                let name = f.tok(i + 1);
                if takes_mut_self(f, i + 2) || is_write_name(name) {
                    api.methods
                        .entry(name.to_string())
                        .or_default()
                        .insert(param_count(f, i + 2));
                    api.types.insert(ty.clone());
                }
            }
        }
        for i in 1..f.tokens().len() {
            if f.tok(i) == "fn"
                && f.tok(i - 1) == "pub"
                && !f.is_test(i)
                && !bodies.iter().any(|b| i >= b.2 && i < b.3)
                && f.tok(i + 2) == "("
                && (f.tok(i + 1).starts_with("write") || f.tok(i + 1).starts_with("save"))
            {
                api.free_fns.insert(f.tok(i + 1).to_string());
            }
        }
    }
    api
}

/// Run the rule over all files.
pub fn check(files: &[SourceFile], cfg: &Config) -> RuleOutput {
    if !files.iter().any(|f| f.path.starts_with(CRATE_DIR)) {
        return RuleOutput::new(RULE, Vec::new(), Unit::Count).with_notice(
            "crates/lector-review does not exist yet; purity is a no-op until it does",
        );
    }
    let apis = PurityApis::derive(files, cfg);
    let globals = Globals::build(files);
    let mut found: Vec<Violation> = files
        .iter()
        .filter(|f| f.path.starts_with(CRATE_DIR))
        .flat_map(|f| scan_file_with(f, &apis, &globals))
        .collect();
    found.extend(apis.statics.stale_violations(RULE, &|_| false));
    RuleOutput::new(RULE, found, Unit::Count)
}

/// Findings in one file of the review crate (only the file's own names resolve).
pub fn scan_file(f: &SourceFile, apis: &PurityApis) -> Vec<Violation> {
    scan_file_with(f, apis, &Globals::default())
}

fn normalize_type(s: &str) -> String {
    s.split_whitespace().collect()
}

/// The declared type of the `static` whose name is at `name_idx` (tokens after `:` up to the `=`
/// or `;`), and the index of the token that ends the item (`;`, at brace depth zero).
fn static_type_and_end(f: &SourceFile, name_idx: usize) -> (String, usize) {
    let n = f.tokens().len();
    let mut j = name_idx + 1;
    let mut ty = String::new();
    if f.tok(j) == ":" {
        j += 1;
        let mut angle = 0usize;
        while j < n {
            match f.tok(j) {
                "<" => angle += 1,
                ">" => angle = angle.saturating_sub(1),
                "=" if angle == 0 => break,
                ";" => break,
                _ => {}
            }
            ty.push_str(f.tok(j));
            ty.push(' ');
            j += 1;
        }
    }
    let mut depth = 0usize;
    while j < n {
        match f.tok(j) {
            "{" | "(" | "[" => depth += 1,
            "}" | ")" | "]" => depth = depth.saturating_sub(1),
            ";" if depth == 0 => break,
            _ => {}
        }
        j += 1;
    }
    (normalize_type(&ty), j)
}

/// Findings in one file of the review crate, with the workspace's re-export table.
pub fn scan_file_with(f: &SourceFile, apis: &PurityApis, globals: &Globals) -> Vec<Violation> {
    let r = Resolver::new(f, globals);
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
        if t == "static" && f.tokens()[i].kind == TokKind::Ident {
            let mut j = i + 1;
            let is_mut = f.tok(j) == "mut";
            if is_mut || f.tok(j) == "ref" {
                j += 1;
            }
            let name = f.tok(j).to_string();
            let (ty, end) = static_type_and_end(f, j);
            let listed = apis
                .statics
                .pairs()
                .iter()
                .any(|(n2, t2)| *n2 == name && normalize_type(t2) == ty);
            if listed {
                apis.statics.mark_pair(&name, &ty);
                // Skip the whole allowed item, including its type and initialiser.
                i = end;
            } else {
                let kind = if is_mut { "static mut" } else { "static" };
                v(format!("`{kind} {name}: {ty}` in lector-review; the crate is stateless (ADR 0001); an immutable table needs the entry `{name}: {ty}` (name and type) in arch/review-statics.allow"));
            }
        } else if LAZY_TYPES.contains(&t) {
            v(format!(
                "`{t}` in lector-review outside an allowed static (ADR 0001)"
            ));
        } else if apis.store.types.contains(t) && f.tokens()[i].kind == TokKind::Ident {
            v(format!(
                "`{t}` in lector-review: a store handle can write; viewed marks and settings come in as values (ADR 0001)"
            ));
        } else if t == "."
            && f.tok(i + 2) == "("
            && apis
                .store
                .methods
                .get(f.tok(i + 1))
                .is_some_and(|counts| counts.contains(&arg_count(f, i + 2)))
            && (!apis.store_ambiguous.contains(f.tok(i + 1))
                || receiver_name_contains(f, i, STORE_RECEIVERS))
        {
            v(format!(
                "`.{}(` writes lector-config state; lector-review writes nothing (ADR 0001)",
                f.tok(i + 1)
            ));
        } else if apis.store.free_fns.contains(t)
            && i > 0
            && f.tok(i - 1) == "::"
            && f.tok(i + 1) == "("
        {
            v(format!(
                "`{t}(` writes lector-config state; lector-review writes nothing (ADR 0001)"
            ));
        }
        i += 1;
    }
    for h in thread_spawns(f, &r) {
        out.push(Violation::new(
            RULE,
            &f.path,
            f.line(h.tok),
            format!("{} in lector-review: no threads (ADR 0001)", h.describe()),
        ));
    }
    for h in fs_writes(f, &r, true) {
        out.push(Violation::new(
            RULE,
            &f.path,
            f.line(h.tok),
            format!("{} in lector-review: no writes (ADR 0001)", h.describe()),
        ));
    }
    for (i, name) in git_write_calls(f, &apis.git, &apis.git_ambiguous, &r) {
        out.push(Violation::new(
            RULE,
            &f.path,
            f.line(i),
            format!("`{name}(` is a lector-git write; lector-review computes, lector-app applies (ADR 0001, ADR 0002)"),
        ));
    }
    for (i, hit, guess) in process_uses(f, &r) {
        let what = match hit {
            ProcessUse::Process(t) => t,
            ProcessUse::StdCommand => "std::process::Command".to_string(),
        };
        let note = if guess {
            " (unresolved name, treated as a hit)"
        } else {
            ""
        };
        out.push(Violation::new(
            RULE,
            &f.path,
            f.line(i),
            format!("`{what}`{note} in lector-review: no processes (ADR 0001)"),
        ));
    }
    out.sort_by_key(|v| v.line);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::file;

    const GIT_WRITE: &str = "impl Repo {\n    pub fn stage_paths(&self, paths: &[&str]) -> R { }\n    pub fn apply_patch(&self, patch: &[u8], target: T, reverse: bool) -> R { }\n    pub fn last_commit_message(&self) -> R { }\n    pub fn push(&self, opts: &O) {}\n}";
    const CONFIG_REVIEW: &str = "pub struct ReviewStore { }\nimpl ReviewStore {\n    pub fn open(f: P) -> Self { }\n    pub fn is_viewed(&self, r: &Path, c: &str, p: &str, h: &str) -> bool { }\n    pub fn set_viewed(&self, r: &Path, c: &str, p: &str, h: &str, v: bool) -> R { }\n    pub fn remove(&self, r: &Path) -> R { }\n    pub fn bump(&mut self) {}\n    fn set_private(&self) {}\n    #[cfg(test)]\n    pub fn set_in_test(&self) {}\n}\npub trait ReviewBackend { fn get(&self, a: &str) -> O; fn set(&self, a: &str, h: O) -> R; }\npub struct DiffSettings { }\nimpl DiffSettings { pub fn clamped(&self) -> u8 { } }\nimpl Default for Other { fn set_default(&mut self) {} }\npub fn write_atomic_file(p: &Path) -> R { }\npub fn read_all(p: &Path) -> R { }";

    fn apis(allow: &[&str]) -> PurityApis {
        let files = [
            file("crates/lector-git/src/write.rs", GIT_WRITE),
            file("crates/lector-config/src/review.rs", CONFIG_REVIEW),
        ];
        let mut cfg = Config::default();
        cfg.set_allow(
            "review-statics",
            AllowList::from_entries(allow.iter().copied()),
        );
        cfg.set_allow("git-read", AllowList::from_entries(["last_commit_message"]));
        cfg.set_allow("git-ambiguous", AllowList::from_entries(["push"]));
        cfg.set_allow(
            "store-ambiguous",
            AllowList::from_entries(["remove", "set"]),
        );
        PurityApis::derive(&files, &cfg)
    }

    fn run(src: &str, allow: &[&str]) -> Vec<(u32, String)> {
        scan_file(&file("crates/lector-review/src/a.rs", src), &apis(allow))
            .into_iter()
            .map(|v| (v.line, v.message))
            .collect()
    }

    fn lines(src: &str) -> Vec<u32> {
        run(src, &[]).iter().map(|(l, _)| *l).collect()
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
        let v = run(src, &["RE: OnceLock<Regex>"]);
        assert_eq!(
            v.iter().map(|(l, _)| *l).collect::<Vec<_>>(),
            [1, 3, 3, 3, 4]
        );
    }

    #[test]
    fn threads_are_flagged_scoped_spawns_on_a_scope_are_not() {
        let src = "fn f() {\n thread::spawn(g);\n std::thread::scope(|s| { s.spawn(|| 1); });\n}";
        assert_eq!(lines(src), [2]);
    }

    // (a) lector-git write API

    #[test]
    fn git_write_calls_positive() {
        let src = "fn f(repo: &Repo) {\n repo.stage_paths(&[]);\n repo.apply_patch(&p, t, false);\n Repo::stage_paths(repo, &[]);\n self.repo.push(&o);\n}";
        assert_eq!(lines(src), [2, 3, 4, 5]);
    }

    #[test]
    fn git_write_calls_negative() {
        // read names, other argument counts, ambiguous names without a repo-like receiver,
        // definitions, and the same name on another type.
        let src = "fn stage_paths(a: u8) {}\nfn f(repo: &Repo) {\n repo.last_commit_message();\n repo.stage_paths(&[], 1);\n repo.apply_patch(&p);\n v.push(1);\n let x = repo.status();\n}";
        assert!(lines(src).is_empty(), "{:?}", run(src, &[]));
    }

    // (b) lector-config store writes

    #[test]
    fn store_api_is_derived() {
        let api = apis(&[]).store;
        let names: Vec<&String> = api.methods.keys().collect();
        assert_eq!(names, ["bump", "remove", "set", "set_viewed"]);
        assert!(api.methods["set_viewed"].contains(&5));
        assert!(api.methods["set"].contains(&2));
        assert_eq!(
            api.types.iter().map(String::as_str).collect::<Vec<_>>(),
            ["ReviewBackend", "ReviewStore"]
        );
        assert_eq!(
            api.free_fns.iter().map(String::as_str).collect::<Vec<_>>(),
            ["write_atomic_file"]
        );
    }

    #[test]
    fn store_writes_positive() {
        let src = "use lector_config::ReviewStore;\nfn f(s: &Marks) {\n s.bump();\n store.set_viewed(&r, c, p, h, true);\n review_store.remove(&r);\n lector_config::write_atomic_file(&p);\n}\nfn g(b: &dyn ReviewBackend) {}";
        assert_eq!(lines(src), [1, 3, 4, 5, 6, 8]);
    }

    #[test]
    fn store_writes_negative() {
        // reads, other argument counts, ambiguous names on a non-store receiver, plain
        // settings types, and private or test-only methods.
        let src = "use lector_config::{content_hash, DiffSettings};\nfn f(settings: &DiffSettings) {\n store.is_viewed(&r, c, p, h);\n store.set_viewed(&r);\n map.remove(&k);\n items.set(1, 2);\n store.set_private();\n store.set_in_test();\n read_all(&p);\n}";
        assert!(lines(src).is_empty(), "{:?}", run(src, &[]));
    }

    // (c) processes

    #[test]
    fn processes_positive_and_negative() {
        let src = "use std::process::{self, Command};\nfn f() {\n let c = Command::new(\"git\");\n let p = ProcessCommand::new(\"x\");\n}";
        assert_eq!(lines(src), [1, 3, 4]);
        let clap = "use clap::Command;\nfn f() { let c = Command::new(\"x\"); }";
        assert!(lines(clap).is_empty());
    }

    // (d) file writes

    #[test]
    fn file_writes_positive() {
        let src = "fn f() {\n std::fs::write(p, b);\n fs::remove_file(p);\n fs::rename(a, b);\n fs::create_dir_all(p);\n fs::create_dir(p);\n let f = File::create(p);\n let g = std::fs::File::create_new(p);\n let h = File::options();\n OpenOptions::new();\n let o: OpenOptions;\n files.iter().for_each(fs::remove_dir_all);\n fs::set_permissions(p, perm);\n}\nuse std::fs::{self, copy};";
        assert_eq!(lines(src), [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 15]);
    }

    #[test]
    fn file_writes_negative() {
        let src = "fn f() {\n let a = fs::read(p);\n let b = std::fs::metadata(p);\n let c = File::open(p);\n let d = fs::read_to_string(p);\n let e = lector_fs::decode(b);\n let g = fs::read_dir(p);\n}\nuse std::fs::{self, File};";
        assert!(lines(src).is_empty(), "{:?}", run(src, &[]));
    }

    // Lexer invariants

    #[test]
    fn tests_comments_and_strings_are_ignored() {
        let src = "// static mut X\nfn f() { let s = \"fs::write(\"; }\n#[cfg(test)]\nmod t { static mut Y: u8 = 0; fn g() { fs::write(p, b); repo.stage_paths(&[]); store.set_viewed(&r, c, p, h, true); } }";
        assert!(run(src, &[]).is_empty());
    }

    #[test]
    fn code_that_may_be_production_is_checked() {
        // `cfg(not(test))`, `cfg(any(test, ...))` and `cfg_attr(test, ..)` are not test-only.
        let src = "#[cfg(not(test))]\nfn a() { fs::write(p, b); }\n#[cfg(any(test, feature = \"x\"))]\nfn b() { fs::remove_file(p); }\n#[cfg_attr(test, derive(Debug))]\nfn c() { fs::rename(a, b); }\n#[cfg(test)]\nfn d() { fs::copy(a, b); }";
        assert_eq!(lines(src), [2, 4, 6]);
    }

    #[test]
    fn unsure_extent_is_production() {
        // A test attribute on something the lexer cannot bound fails closed.
        let src = "#[cfg(test)]\n";
        assert!(run(src, &[]).is_empty());
        let src = "#[cfg(test)] fn a() {}\nfn b() { fs::write(p, b); }";
        assert_eq!(lines(src), [2]);
    }

    // Static allowances are `NAME: Type`; every static needs one.

    fn statics_run(src: &str, entries: &[&str]) -> Vec<(u32, String)> {
        let mut a = apis(&[]);
        a.statics = AllowList::from_entries(entries.iter().copied());
        scan_file(&file("crates/lector-review/src/a.rs", src), &a)
            .into_iter()
            .map(|v| (v.line, v.message))
            .collect()
    }

    #[test]
    fn a_static_needs_its_name_and_its_type() {
        let src = "static RE: OnceLock<Regex> = OnceLock::new();";
        assert!(statics_run(src, &["RE: OnceLock<Regex>"]).is_empty());
        // whitespace in the entry does not matter
        assert!(statics_run(src, &["RE:   OnceLock < Regex >"]).is_empty());
        // right name, wrong type: a mutable static reusing an allowed name
        let v = statics_run(
            "static RE: Mutex<Vec<String>> = Mutex::new(Vec::new());",
            &["RE: OnceLock<Regex>"],
        );
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].1.contains("RE: Mutex<Vec<String>>"), "{}", v[0].1);
        // right type, wrong name
        let wrong_name = statics_run(src, &["OTHER: OnceLock<Regex>"]);
        assert!(
            wrong_name.iter().any(|(_, m)| m.contains("static RE")),
            "{wrong_name:?}"
        );
    }

    #[test]
    fn every_kind_of_static_needs_an_entry() {
        let src = "static A: AtomicU32 = AtomicU32::new(0);\nstatic mut B: u8 = 0;\nstatic C: RwLock<u8> = RwLock::new(0);\nstatic D: Cell<u8> = x;\nthread_local! { static E: RefCell<u8> = RefCell::new(0); }\nlazy_static! { static ref F: Mutex<u8> = Mutex::new(0); }\nstatic G: u32 = 1;";
        let v = statics_run(src, &[]);
        let lines: Vec<u32> = v.iter().map(|(l, _)| *l).collect();
        assert_eq!(lines, [1, 2, 3, 4, 5, 6, 7], "{v:?}");
        let ok = statics_run(
            "static A: AtomicU32 = AtomicU32::new(0);\nthread_local! { static E: RefCell<u8> = RefCell::new(0); }",
            &["A: AtomicU32", "E: RefCell<u8>"],
        );
        assert!(ok.is_empty(), "{ok:?}");
    }

    #[test]
    fn an_entry_that_matches_no_static_is_a_violation() {
        let files = [file(
            "crates/lector-review/src/a.rs",
            "static RE: OnceLock<Regex> = OnceLock::new();",
        )];
        let mut cfg = Config::default();
        cfg.set_allow(
            "review-statics",
            AllowList::from_entries([
                "RE: OnceLock<Regex>",
                "GONE: OnceLock<Regex>",
                "RE: OnceLock<Other>",
            ]),
        );
        let out = check(&files, &cfg);
        assert_eq!(out.violations.len(), 2, "{:?}", out.violations);
        assert!(out
            .violations
            .iter()
            .all(|v| v.message.contains("stale allowance")));
    }

    // Alias bypasses.

    #[test]
    fn bypass_renamed_imports_in_the_review_crate() {
        let src = "use std::fs as f;\nuse std::thread as t;\nuse std::process as p;\nfn g() {\n f::write(a, b);\n t::spawn(h);\n p::Command::new(\"x\");\n}";
        assert_eq!(lines(src), [5, 6, 7]);
    }
}
