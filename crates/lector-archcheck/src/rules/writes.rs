//! `writes` (ADR 0002, ADR 0010 item 8): only `lector-app` command modules write to the
//! repository, start processes or write files on the user's behalf.
//!
//! **The write API is everything in `lector-git` that is not listed as a read.** The rule scans
//! all of `crates/lector-git/src/**` (not a list of files). Every `pub fn` of a *handle type*
//! (`Repo`, `ShadowRepo`: the `handle:` entries of `arch/git-read.allow`; methods, associated
//! functions and trait impl methods alike) and every `pub fn` free function must be classified in
//! `arch/git-read.allow` (`Type::name` or `free::name`, or a bare `name` for any type) as a read;
//! whatever is not listed there is a write. A new function therefore fails closed: it is a write
//! until a reviewer reads it and lists it. An inherent `impl` with `pub fn`s on any other type
//! must be declared a `value:` type (data only) or a `handle:`, or it is reported.
//!
//! **`lector-app` wrappers.** A plain `pub fn` of `crates/lector-app/src/**` outside `commands/`
//! whose body calls the write API, directly or through a function of the same file (a private
//! helper), is itself part of the write API (by name and argument count), so
//! `lector_app::review_service::apply::apply_hunk_action` counts. A wrapper of a wrapper in
//! another file is not followed.
//!
//! **Callers.** In the non-test code of every crate except `lector-git` itself, a call of the
//! write API (method `.name(`, `Type::name(`, `lector_git::name(`, through `use .. as ..` and
//! `type Alias = Repo;`, see [`crate::alias`]) is reported unless the file is listed in
//! `arch/git-callers.allow` (the command modules of `lector-app`, each with its reason).
//! Ambiguous method names (`push`, `fetch`, ...: `arch/git-ambiguous.allow`) only match a
//! receiver whose name contains `repo`, `git` or `shadow`; a call is matched by name and argument
//! count.
//!
//! **UI crates** (`lector-ui*`) additionally may not start processes: `ProcessCommand`,
//! `Supervisor` and `std::process::Command`, through any alias.
//!
//! **Direct file writes** in the UI crates, `lector-mcp`, `lector-cli` and `lector-agent`:
//! `fs::{write, remove_file, remove_dir, remove_dir_all, rename, copy, create_dir,
//! create_dir_all, hard_link, symlink*, set_permissions}`, `File::create` / `create_new` /
//! `options`, `OpenOptions` (the file API one, not a same-named struct) and
//! `lector_fs::write_atomic` and the confined write of `lector_fs::confine` (found by its
//! `CreateDirs` argument), unless the file is listed in `arch/fs-writers.allow` with a reason.
//! `lector-agent`'s writes are listed there with the ADR 0010 "known gaps" reason, so they stay
//! visible and counted (the rule prints how many sites the allowances cover).
//!
//! **Handing out a handle** (report-only, counted in the baseline): a plain `pub fn` outside
//! `lector-git` whose return type names `Repo` or `ShadowRepo` (`&Repo`, `Arc<Repo>`,
//! `Result<Repo>`, ...) gives every caller the whole write API; narrowing it to read-only views
//! is a product change.
//!
//! What it cannot see: a write reached through a trait object, a generic bound or a closure
//! stored in a variable; a call inside a macro body that only expands to the call; generated code
//! and `include!`d files; a handle hidden behind a wrapper type of another crate that forwards to
//! the write API under another name (only the `lector-app` wrappers are derived); methods defined
//! by macros in `lector-git`; and `write_all` on a file handle opened elsewhere (opening for
//! write is what is flagged).

use std::collections::{BTreeMap, BTreeSet};

use crate::alias::{Globals, Resolver};
use crate::config::{AllowList, Config};
use crate::items::{fn_items, FnItem};
use crate::report::{RuleOutput, Unit, Violation};
use crate::rules::detect::{fs_writes, process_uses, sites, ProcessUse};
use crate::rules::is_ui_src;
use crate::source::SourceFile;

/// Rule name.
pub const RULE: &str = "writes";

const GIT_SRC: &str = "crates/lector-git/src/";
const DEFAULT_HANDLES: &[&str] = &["Repo", "ShadowRepo"];
const APP_SRC: &str = "crates/lector-app/src/";

/// The `lector-git` write API: names with the argument counts (excluding `self`) they are
/// defined with. A call is only reported when its argument count matches, so a UI method that
/// merely shares a name (`panel.switch_branch(name, window, cx)`) is not a hit.
#[derive(Debug, Clone, Default)]
pub struct WriteApi {
    /// Methods of handle types (taking `self`): name to argument counts.
    pub names: BTreeMap<String, BTreeSet<usize>>,
    /// Associated functions of handle types without `self` (`ShadowRepo::open`): name to counts.
    pub assoc: BTreeMap<String, BTreeSet<usize>>,
    /// Free `pub fn`s of `lector-git`: name to counts.
    pub free: BTreeMap<String, BTreeSet<usize>>,
    /// `lector-app` functions that run a write (derived, see the module docs).
    pub app: BTreeMap<String, BTreeSet<usize>>,
    /// Handle type names.
    pub handles: BTreeSet<String>,
}

fn counts_has(m: &BTreeMap<String, BTreeSet<usize>>, name: &str, args: usize) -> bool {
    m.get(name).is_some_and(|a| a.contains(&args))
}

impl WriteApi {
    fn has(&self, name: &str, args: usize) -> bool {
        counts_has(&self.names, name, args) || counts_has(&self.app, name, args)
    }

    /// `Type::name(args)` where `Type` is a handle: an associated function with `args`
    /// arguments, or a method called with `self` as its first argument.
    fn has_path_call(&self, name: &str, args: Option<usize>) -> bool {
        match args {
            Some(n) => {
                counts_has(&self.assoc, name, n) || (n > 0 && counts_has(&self.names, name, n - 1))
            }
            None => self.names.contains_key(name) || self.assoc.contains_key(name),
        }
    }

    /// True when `name` is a write name of any kind.
    pub fn has_name(&self, name: &str) -> bool {
        self.names.contains_key(name)
            || self.assoc.contains_key(name)
            || self.free.contains_key(name)
            || self.app.contains_key(name)
    }

    /// Every write name, sorted, with its kind (`method`, `assoc`, `free`, `app`).
    pub fn listing(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (kind, m) in [
            ("method", &self.names),
            ("assoc", &self.assoc),
            ("free", &self.free),
            ("app", &self.app),
        ] {
            for (name, counts) in m {
                let c: Vec<String> = counts.iter().map(usize::to_string).collect();
                out.push(format!("{kind:<7}{name}/{}", c.join(",")));
            }
        }
        out
    }
}

/// What deriving the surface of `lector-git` found.
#[derive(Debug, Clone, Default)]
pub struct GitSurface {
    /// The write API.
    pub api: WriteApi,
    /// Inherent `impl`s with `pub fn`s on a type that is neither `handle:` nor `value:`:
    /// `(file, line, type)`.
    pub unclassified: Vec<(String, u32, String)>,
}

fn is_read(classes: &AllowList, owner: &str, name: &str) -> bool {
    let q = classes.contains(&format!("{owner}::{name}"));
    let b = classes.contains(name);
    q || b
}

/// Derive the write API from the `lector-git` sources and the classification in
/// `arch/git-read.allow`.
pub fn derive_surface(files: &[SourceFile], classes: &AllowList) -> GitSurface {
    let mut handles: BTreeSet<String> = DEFAULT_HANDLES.iter().map(|s| (*s).to_string()).collect();
    handles.extend(classes.tagged("handle").into_iter().map(String::from));
    let values: BTreeSet<String> = classes
        .tagged("value")
        .into_iter()
        .map(String::from)
        .collect();
    let mut surface = GitSurface::default();
    let mut seen_types: BTreeSet<String> = BTreeSet::new();
    for f in files {
        if !f.path.starts_with(GIT_SRC) || f.test_only {
            continue;
        }
        let items = fn_items(f);
        let bodies: Vec<(usize, usize)> = items.iter().filter_map(|i| i.body).collect();
        let mut reported: BTreeSet<String> = BTreeSet::new();
        for it in &items {
            if f.is_test(it.name_idx) {
                continue;
            }
            match it.impl_ty.as_deref() {
                Some(ty) if handles.contains(ty) => {
                    seen_types.insert(ty.to_string());
                    if !(it.is_pub || it.trait_impl) || is_read(classes, ty, &it.name) {
                        continue;
                    }
                    let target = if it.takes_self {
                        &mut surface.api.names
                    } else {
                        &mut surface.api.assoc
                    };
                    target.entry(it.name.clone()).or_default().insert(it.params);
                }
                Some(ty) => {
                    seen_types.insert(ty.to_string());
                    if it.is_pub
                        && !it.trait_impl
                        && !values.contains(ty)
                        && reported.insert(ty.to_string())
                    {
                        surface.unclassified.push((
                            f.path.clone(),
                            f.line(it.name_idx),
                            ty.to_string(),
                        ));
                    }
                }
                None => {
                    let nested = bodies
                        .iter()
                        .any(|b| b.0 <= it.name_idx && it.name_idx < b.1);
                    if it.is_pub && !nested && !is_read(classes, "free", &it.name) {
                        surface
                            .api
                            .free
                            .entry(it.name.clone())
                            .or_default()
                            .insert(it.params + usize::from(it.takes_self));
                    }
                }
            }
        }
    }
    for t in handles.iter().chain(values.iter()) {
        if seen_types.contains(t) {
            classes.mark_tagged("handle", t);
            classes.mark_tagged("value", t);
        }
    }
    surface.api.handles = handles;
    surface
}

/// Derive the write API (the `lector-git` part only).
pub fn derive_write_api(files: &[SourceFile], classes: &AllowList) -> WriteApi {
    derive_surface(files, classes).api
}

/// Plain `pub fn`s of `lector-app` (outside `commands/`) whose body calls the write API.
pub fn derive_app_wrappers(
    files: &[SourceFile],
    api: &WriteApi,
    ambiguous: &AllowList,
    globals: &Globals,
) -> BTreeMap<String, BTreeSet<usize>> {
    let mut out: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for f in files {
        if !f.path.starts_with(APP_SRC)
            || f.test_only
            || f.path.starts_with("crates/lector-app/src/commands/")
        {
            continue;
        }
        let r = Resolver::new(f, globals);
        let calls = git_write_calls(f, api, ambiguous, &r);
        if calls.is_empty() {
            continue;
        }
        let items: Vec<_> = fn_items(f)
            .into_iter()
            .filter(|it| it.body.is_some() && !f.is_test(it.name_idx))
            .collect();
        // A function runs a write when its body calls the write API, or calls (by name and
        // argument count) another function of the same file that does: a public function that
        // reaches the write through a private helper is still a wrapper.
        let mut tainted: Vec<bool> = items
            .iter()
            .map(|it| {
                let (a, b) = it.body.unwrap_or((0, 0));
                calls.iter().any(|(i, _)| *i >= a && *i < b)
            })
            .collect();
        loop {
            let mut changed = false;
            for k in 0..items.len() {
                if tainted[k] {
                    continue;
                }
                let (a, b) = items[k].body.unwrap_or((0, 0));
                let reaches = (a..b).any(|i| {
                    f.tok(i + 1) == "("
                        && items.iter().enumerate().any(|(m, other)| {
                            tainted[m]
                                && m != k
                                && f.tok(i) == other.name
                                && arg_count(f, i + 1)
                                    == other.params
                                        + usize::from(
                                            other.takes_self && f.tok(i.wrapping_sub(1)) != ".",
                                        )
                        })
                });
                if reaches {
                    tainted[k] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for (it, t) in items.iter().zip(&tainted) {
            if *t && it.is_pub && !it.trait_impl {
                out.entry(it.name.clone()).or_default().insert(it.params);
            }
        }
    }
    out
}

/// The print-out of `writes --list-api`.
pub fn list_api(files: &[SourceFile], cfg: &Config) -> Vec<String> {
    let classes = cfg.allow("git-read");
    let ambiguous = cfg.allow("git-ambiguous");
    let globals = Globals::build(files);
    let mut api = derive_write_api(files, &classes);
    api.app = derive_app_wrappers(files, &api, &ambiguous, &globals);
    api.listing()
}

/// Number of parameters after `self` of the parameter list opening at `open`.
pub(crate) fn param_count(f: &SourceFile, open: usize) -> usize {
    crate::items::param_list(f, open).1
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
    crate::items::param_list(f, open).0
}

/// Calls of the write API in the non-test code of `f`: the token index of the called name and
/// the name. Matching is by name and argument count; ambiguous names also need a repo-like
/// receiver (see the module docs). Shared with the `purity` rule.
pub(crate) fn git_write_calls(
    f: &SourceFile,
    api: &WriteApi,
    ambiguous: &AllowList,
    r: &Resolver<'_>,
) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for i in 0..f.tokens().len() {
        if f.is_test(i) {
            continue;
        }
        // `.name(`
        if f.tok(i) == "." && f.tok(i + 2) == "(" && api.has(f.tok(i + 1), arg_count(f, i + 2)) {
            let name = f.tok(i + 1);
            if !ambiguous.contains(name) || receiver_looks_like_repo(f, i) {
                out.push((i + 1, name.to_string()));
            }
        }
    }
    for s in sites(f, r) {
        if s.import {
            continue;
        }
        let args = s.call.then(|| arg_count(f, s.end));
        for p in &s.res.paths {
            let Some(name) = p.last().map(String::as_str) else {
                continue;
            };
            let typed = p.len() >= 2
                && api.handles.contains(p[p.len() - 2].as_str())
                && api.has_path_call(name, args);
            let free = p.first().is_some_and(|x| x == "lector_git")
                && args.map_or(api.free.contains_key(name), |n| {
                    counts_has(&api.free, name, n)
                });
            let app = p.len() >= 2
                && p.first().is_some_and(|x| {
                    x == "lector_app"
                        || (f.path.starts_with(APP_SRC)
                            && matches!(x.as_str(), "crate" | "self" | "super"))
                })
                && args.map_or(api.app.contains_key(name), |n| {
                    counts_has(&api.app, name, n)
                });
            if typed || free || app {
                out.push((s.end - 1, name.to_string()));
            }
        }
    }
    out.sort();
    out.dedup();
    out
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

/// Crates whose own sources may not write files directly (see the module docs).
fn fs_scope(path: &str) -> bool {
    is_ui_src(path)
        || ["lector-mcp", "lector-cli", "lector-agent"]
            .iter()
            .any(|c| path.starts_with(&format!("crates/{c}/src/")))
}

fn skips_git_scan(path: &str) -> bool {
    path.starts_with(GIT_SRC) || path.starts_with("crates/lector-archcheck/")
}

/// Per-file results of the rule: the findings and the number of file-write sites an allowance
/// covered.
pub struct FileScan {
    /// Findings that stand.
    pub violations: Vec<Violation>,
    /// File-write sites exempted by `arch/fs-writers.allow`.
    pub fs_allowed: usize,
    /// Of those, in `lector-agent`.
    pub fs_allowed_agent: usize,
}

/// Findings in one file.
pub fn scan_file_full(
    f: &SourceFile,
    api: &WriteApi,
    ambiguous: &AllowList,
    callers: &AllowList,
    fs_writers: &AllowList,
    globals: &Globals,
) -> FileScan {
    let mut scan = FileScan {
        violations: Vec::new(),
        fs_allowed: 0,
        fs_allowed_agent: 0,
    };
    if skips_git_scan(&f.path) || f.test_only {
        return scan;
    }
    let r = Resolver::new(f, globals);
    let mut git: Vec<Violation> = git_write_calls(f, api, ambiguous, &r)
        .into_iter()
        .map(|(i, name)| write_call(f, i, &name, api))
        .collect();
    if !git.is_empty() && callers.exempt_path(&f.path) {
        git.clear();
    }
    scan.violations.extend(git);
    if is_ui_src(&f.path) {
        for (i, hit, guess) in process_uses(f, &r) {
            scan.violations.push(match hit {
                ProcessUse::Process(t) => Violation::new(
                    RULE,
                    &f.path,
                    f.line(i),
                    format!(
                        "`{t}` in a UI crate; processes are started by lector-app commands (ADR 0002)"
                    ),
                ),
                ProcessUse::StdCommand => command_violation(f, i, guess),
            });
        }
    }
    if fs_scope(&f.path) {
        let mut writes: Vec<Violation> = fs_writes(f, &r, false)
            .into_iter()
            .map(|h| {
                Violation::new(
                    RULE,
                    &f.path,
                    f.line(h.tok),
                    format!("{} writes files outside lector-app commands; use a command, or list the file in arch/fs-writers.allow with a reason (ADR 0002, ADR 0010)", h.describe()),
                )
            })
            .collect();
        if !writes.is_empty() && fs_writers.exempt_path(&f.path) {
            scan.fs_allowed += writes.len();
            if f.path.starts_with("crates/lector-agent/") {
                scan.fs_allowed_agent += writes.len();
            }
            writes.clear();
        }
        scan.violations.extend(writes);
    }
    scan.violations.extend(handle_accessors(f, api, &r));
    scan
}

/// Findings in one file with the allow lists of the repository (tests use this for the call and
/// process checks).
pub fn scan_file(f: &SourceFile, api: &WriteApi, ambiguous: &AllowList) -> Vec<Violation> {
    scan_file_full(
        f,
        api,
        ambiguous,
        &AllowList::default(),
        &AllowList::default(),
        &Globals::default(),
    )
    .violations
}

fn write_call(f: &SourceFile, name_idx: usize, name: &str, api: &WriteApi) -> Violation {
    let message = if api.app.contains_key(name)
        && !api.names.contains_key(name)
        && !api.free.contains_key(name)
        && !api.assoc.contains_key(name)
    {
        format!("`{name}(` runs a lector-git write inside lector-app (a derived wrapper); call it from a lector-app registry command (ADR 0002)")
    } else {
        format!("`{name}(` is a lector-git write; call it from a lector-app registry command (ADR 0002)")
    };
    Violation::new(RULE, &f.path, f.line(name_idx), message)
}

fn command_violation(f: &SourceFile, i: usize, guess: bool) -> Violation {
    let what = if guess {
        "`Command` (name cannot be resolved: a glob import may hide `std::process::Command`; treated as a hit)"
    } else {
        "`std::process::Command`"
    };
    Violation::new(
        RULE,
        &f.path,
        f.line(i),
        format!("{what} in a UI crate; spawn processes from lector-app (ADR 0002)"),
    )
}

/// Report-only findings: plain `pub fn`s outside `lector-git` whose return type names a handle.
fn handle_accessors(f: &SourceFile, api: &WriteApi, r: &Resolver<'_>) -> Vec<Violation> {
    let mut out = Vec::new();
    for it in fn_items(f) {
        if !it.is_pub || f.is_test(it.name_idx) {
            continue;
        }
        if let Some(h) = returned_handle(f, &it, api, r) {
            out.push(Violation::new(
                RULE,
                &f.path,
                f.line(it.name_idx),
                format!("[report-only] `pub fn {}` hands out a `{h}`: every caller gets the whole lector-git write API; return a read-only view instead (ADR 0010, product change)", it.name),
            ));
        }
    }
    out
}

fn returned_handle(
    f: &SourceFile,
    it: &FnItem,
    api: &WriteApi,
    r: &Resolver<'_>,
) -> Option<String> {
    (it.ret.0..it.ret.1).find_map(|k| {
        let t = f.tok(k);
        if k > 0 && f.tok(k - 1) == "::" {
            return None;
        }
        let res = r.resolve(&[t.to_string()]);
        api.handles
            .iter()
            .find(|h| {
                t == h.as_str() || res.paths.iter().any(|p| p.last().is_some_and(|x| x == *h))
            })
            .cloned()
    })
}

/// Run the rule over all files.
pub fn check(files: &[SourceFile], cfg: &Config) -> RuleOutput {
    let classes = cfg.allow("git-read");
    let ambiguous = cfg.allow("git-ambiguous");
    let callers = cfg.allow("git-callers");
    let fs_writers = cfg.allow("fs-writers");
    let globals = Globals::build(files);
    let surface = derive_surface(files, &classes);
    let mut api = surface.api;
    for name in api
        .names
        .keys()
        .chain(api.assoc.keys())
        .chain(api.free.keys())
    {
        ambiguous.contains(name);
    }
    api.app = derive_app_wrappers(files, &api, &ambiguous, &globals);
    let mut found: Vec<Violation> = surface
        .unclassified
        .iter()
        .map(|(path, line, ty)| {
            Violation::new(
                RULE,
                path,
                *line,
                format!("`impl {ty}` has `pub fn`s but `{ty}` is not classified: add `handle:{ty}` (it can write: every pub fn is then a write unless listed as a read) or `value:{ty}` (data only) to arch/git-read.allow"),
            )
        })
        .collect();
    let (mut allowed, mut allowed_agent) = (0, 0);
    for f in files {
        let s = scan_file_full(f, &api, &ambiguous, &callers, &fs_writers, &globals);
        allowed += s.fs_allowed;
        allowed_agent += s.fs_allowed_agent;
        found.extend(s.violations);
    }
    for (name, list) in [
        ("git-read", &classes),
        ("git-ambiguous", &ambiguous),
        ("git-callers", &callers),
        ("fs-writers", &fs_writers),
    ] {
        let _ = name;
        found.extend(list.stale_violations(RULE, &|_| false));
    }
    let mut out = RuleOutput::new(RULE, found, Unit::Count);
    if allowed > 0 {
        out = out.with_notice(format!(
            "{allowed} file-write sites are covered by arch/fs-writers.allow ({allowed_agent} in lector-agent, the known second write path of ADR 0010)"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::file;

    const GIT_WRITE: &str = "impl Repo {\n    pub fn stage_paths(&self, paths: &[&str]) -> R { }\n    pub fn last_commit_message(&self) -> R { }\n    fn private(&self) {}\n    pub(crate) fn crate_only(&self) {}\n    pub fn constructor(path: &Path) -> Self { }\n    pub fn push(&self, opts: &O) {}\n    #[cfg(test)]\n    pub fn test_helper(&self) {}\n}\nimpl Drop for LockGuard { fn drop(&mut self) {} }\npub fn free_fn(r: &Repo) {}\npub fn pure_fn(s: &str) -> String { s }";
    const GIT_SHADOW: &str = "impl ShadowRepo {\n    pub fn restore_paths(&self, s: &S) {}\n    pub fn snapshots(&self) {}\n    pub fn open(d: &Path) {}\n}";
    /// A write method in a file the old rule did not list, and one in a trait impl.
    const GIT_TAG: &str = "impl Repo {\n    pub fn create_tag(&self, name: &str, target: &str) -> R {}\n    pub fn tags_of(&self) -> R {}\n}\nimpl Amend for Repo {\n    fn amend(&self, message: &str) {}\n}";

    fn read_list() -> AllowList {
        AllowList::from_entries([
            "Repo::last_commit_message",
            "ShadowRepo::snapshots",
            "Repo::tags_of",
            "ShadowRepo::open",
            "free::pure_fn",
            "value:LockGuard",
        ])
    }

    fn git_files() -> Vec<SourceFile> {
        vec![
            file("crates/lector-git/src/write.rs", GIT_WRITE),
            file("crates/lector-git/src/shadow.rs", GIT_SHADOW),
            file("crates/lector-git/src/tag.rs", GIT_TAG),
            file(
                "crates/lector-git/src/status.rs",
                "impl Repo { pub fn status_all(&self) {} }",
            ),
        ]
    }

    fn api() -> WriteApi {
        derive_write_api(&git_files(), &read_list())
    }

    fn run(path: &str, src: &str) -> Vec<Violation> {
        scan_file(&file(path, src), &api(), &AllowList::from_entries(["push"]))
    }

    fn lines(v: &[Violation]) -> Vec<u32> {
        v.iter().map(|v| v.line).collect()
    }

    #[test]
    fn api_derivation_scans_every_lector_git_file_and_fails_closed() {
        let api = api();
        let names: Vec<&String> = api.names.keys().collect();
        // `status_all` (status.rs) is a write until it is listed as a read; so are the new
        // `create_tag` (a file the old rule did not list) and the trait impl method `amend`.
        assert_eq!(
            names,
            [
                "amend",
                "create_tag",
                "push",
                "restore_paths",
                "stage_paths",
                "status_all"
            ]
        );
        assert!(api.has("stage_paths", 1));
        assert!(!api.has("stage_paths", 3));
        assert!(api.has("push", 1));
        assert!(api.has("amend", 1));
        assert!(api.assoc.contains_key("constructor"));
        assert!(!api.assoc.contains_key("open"));
        assert_eq!(api.free.keys().collect::<Vec<_>>(), ["free_fn"]);
        // private, pub(crate), test-only and Drop-of-another-type items are not API
        for hidden in ["private", "crate_only", "test_helper", "drop"] {
            assert!(!api.has_name(hidden), "{hidden}");
        }
    }

    #[test]
    fn a_read_listed_under_another_type_does_not_hide_a_write() {
        // `ShadowRepo::snapshots` is a read; a `Repo::snapshots` is not listed, so it is a write.
        let files = [
            file(
                "crates/lector-git/src/shadow.rs",
                "impl ShadowRepo { pub fn snapshots(&self) {} }",
            ),
            file(
                "crates/lector-git/src/x.rs",
                "impl Repo { pub fn snapshots(&self) {} }",
            ),
        ];
        let api = derive_write_api(&files, &AllowList::from_entries(["ShadowRepo::snapshots"]));
        assert!(api.names.contains_key("snapshots"));
    }

    #[test]
    fn unclassified_types_with_pub_fns_are_reported() {
        let files = [file(
            "crates/lector-git/src/worktree.rs",
            "impl Worktree { pub fn remove(&self) {} }\nimpl Oid { pub fn short(&self) {} }",
        )];
        let s = derive_surface(&files, &AllowList::from_entries(["value:Oid"]));
        assert_eq!(
            s.unclassified,
            [(
                "crates/lector-git/src/worktree.rs".to_string(),
                1,
                "Worktree".to_string()
            )]
        );
    }

    #[test]
    fn flags_method_and_path_calls_in_ui_crates() {
        let src = "fn f(repo: &Repo) {\n repo.stage_paths(&[]);\n Repo::stage_paths(repo, &[]);\n self.shadow.restore_paths(&s);\n repo.last_commit_message();\n}";
        let v = run("crates/lector-ui-git/src/x.rs", src);
        assert_eq!(lines(&v), [2, 3, 4]);
    }

    #[test]
    fn same_name_with_another_argument_count_is_not_a_hit() {
        let src = "fn f() {\n panel.stage_paths(paths, window, cx);\n c.update(cx, |c, cx| c.stage_paths(a, window, cx));\n repo.stage_paths(&[\"a\", \"b\"]);\n repo.stage_paths(&v.iter().map(|a, b| a).collect::<Vec<_>>());\n}";
        let v = run("crates/lector-ui-git/src/x.rs", src);
        assert_eq!(lines(&v), [4, 5]);
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
        let src = "impl Repo { pub fn a(&self, m: HashMap<K, V>, x: u8) {} pub fn b(&self) {} pub fn c(&mut self, f: impl Fn(A, B) -> C,) {} }";
        let api = derive_write_api(
            &[file("crates/lector-git/src/write.rs", src)],
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
        assert_eq!(lines(&v), [4, 5, 6, 7]);
    }

    #[test]
    fn tests_comments_and_strings_are_ignored_and_git_itself_is_not_scanned() {
        let src = "// repo.stage_paths(x)\nfn f() { let s = \"repo.stage_paths(x)\"; }\n#[cfg(test)]\nmod t { fn g(repo: &Repo) { repo.stage_paths(&[]); } }";
        assert!(run("crates/lector-ui/src/x.rs", src).is_empty());
        assert!(run(
            "crates/lector-ui/tests/x.rs",
            "fn f(r: &Repo) { r.stage_paths(&[]); }"
        )
        .is_empty());
        assert!(run(
            "crates/lector-git/src/x.rs",
            "fn f(r: &Repo) { r.stage_paths(&[]); }"
        )
        .is_empty());
    }

    // Bypasses the audit demonstrated: each of these used to pass.

    #[test]
    fn bypass_process_module_alias() {
        let src = "use std::process as p;\nfn f() { let c = p::Command::new(\"git\"); }";
        let v = run("crates/lector-ui-git/src/x.rs", src);
        assert_eq!(lines(&v), [2], "{v:?}");
    }

    #[test]
    fn bypass_process_command_renamed_and_glob() {
        let src = "use std::process::Command as C;\nfn f() { C::new(\"x\"); }";
        assert_eq!(lines(&run("crates/lector-ui/src/x.rs", src)), [1, 2]);
        let src = "use std::process::*;\nfn f() { Command::new(\"x\"); }";
        assert_eq!(lines(&run("crates/lector-ui/src/x.rs", src)), [2]);
        // a Command of another crate is fine
        let src = "use clap::Command as C;\nfn f() { C::new(\"x\"); }";
        assert!(run("crates/lector-ui/src/x.rs", src).is_empty());
    }

    #[test]
    fn bypass_type_alias_of_the_handle() {
        let src =
            "use lector_git::Repo;\ntype Rp = Repo;\nfn f(r: &Rp) { Rp::stage_paths(r, &[]); }";
        let v = run("crates/lector-ui-git/src/x.rs", src);
        assert_eq!(lines(&v), [3], "{v:?}");
    }

    #[test]
    fn bypass_new_write_method_outside_the_old_file_list_and_trait_impl_methods() {
        let src = "fn f(repo: &Repo) { repo.create_tag(\"v1\", \"HEAD\"); repo.amend(\"m\"); }";
        let v = run("crates/lector-ui-git/src/x.rs", src);
        assert_eq!(lines(&v), [1, 1], "{v:?}");
    }

    #[test]
    fn bypass_lector_git_free_function_and_assoc_function() {
        let src = "use lector_git::free_fn as ff;\nfn f(r: &Repo) { ff(r); lector_git::free_fn(r); Repo::constructor(p); }";
        let v = run("crates/lector-ui/src/x.rs", src);
        assert_eq!(lines(&v), [2, 2, 2], "{v:?}");
    }

    const APP_APPLY: &str = "pub fn apply_hunk_action(repo: &Repo, action: A) -> R {\n    repo.stage_paths(&[])?;\n    Ok(())\n}\nfn internal(repo: &Repo) { repo.stage_paths(&[]); }\npub fn reads(repo: &Repo) { repo.last_commit_message(); }";

    fn app_files() -> Vec<SourceFile> {
        let mut files = git_files();
        files.push(file(
            "crates/lector-app/src/review_service/apply.rs",
            APP_APPLY,
        ));
        files
    }

    fn app_api() -> WriteApi {
        let files = app_files();
        let mut api = derive_write_api(&files, &read_list());
        let amb = AllowList::from_entries(["push"]);
        api.app = derive_app_wrappers(&files, &api, &amb, &Globals::build(&files));
        api
    }

    #[test]
    fn bypass_lector_app_wrapper_that_runs_a_git_write() {
        let api = app_api();
        assert_eq!(api.app.keys().collect::<Vec<_>>(), ["apply_hunk_action"]);
        let amb = AllowList::from_entries(["push"]);
        let src = "use lector_app::review_service::apply::apply_hunk_action;\nfn f(r: &Repo) { apply_hunk_action(r, a); lector_app::review_service::apply::apply_hunk_action(r, a); }";
        let v = scan_file(&file("crates/lector-ui-git/src/x.rs", src), &api, &amb);
        assert_eq!(lines(&v), [2, 2], "{v:?}");
        assert!(v[0].message.contains("wrapper"));
        // a call with another argument count is another function
        let src = "fn f(r: &Repo) { apply_hunk_action(r); }";
        assert!(scan_file(&file("crates/lector-ui-git/src/x.rs", src), &api, &amb).is_empty());
    }

    #[test]
    fn a_public_function_reaching_a_write_through_a_private_helper_is_a_wrapper() {
        let src = "pub fn outer(repo: &Repo, a: u8) { helper(repo, a); }\nfn helper(repo: &Repo, a: u8) { repo.stage_paths(&[]); }\npub fn unrelated(repo: &Repo) { helper(repo); }\npub fn reads(repo: &Repo) { other(repo); }\nfn other(r: &Repo) { r.last_commit_message(); }";
        let mut files = git_files();
        files.push(file("crates/lector-app/src/review_service/x.rs", src));
        let api = derive_write_api(&files, &read_list());
        let amb = AllowList::from_entries(["push"]);
        let wrappers = derive_app_wrappers(&files, &api, &amb, &Globals::build(&files));
        assert_eq!(
            wrappers.keys().collect::<Vec<_>>(),
            ["outer"],
            "{wrappers:?}"
        );
    }

    #[test]
    fn callers_in_every_crate_above_lector_git_are_checked_and_command_modules_are_allowed() {
        let api = api();
        let amb = AllowList::from_entries(["push"]);
        let src = "fn f(repo: &Repo) { repo.stage_paths(&[]); }";
        let callers = AllowList::from_entries(["crates/lector-app/src/commands/**"]);
        let scan = |path: &str| {
            scan_file_full(
                &file(path, src),
                &api,
                &amb,
                &callers,
                &AllowList::default(),
                &Globals::default(),
            )
            .violations
            .len()
        };
        for hit in [
            "crates/lector-agent/src/session.rs",
            "crates/lector-mcp/src/tools.rs",
            "crates/lector-cli/src/run.rs",
            "crates/lector-review/src/lib.rs",
            "crates/lector-ui-agent/src/x.rs",
            "crates/lector-app/src/project.rs",
            "crates/lector/src/main.rs",
        ] {
            assert_eq!(scan(hit), 1, "{hit}");
        }
        for ok in [
            "crates/lector-app/src/commands/git_write.rs",
            "crates/lector-git/src/write.rs",
            "crates/lector-agent/tests/x.rs",
        ] {
            assert_eq!(scan(ok), 0, "{ok}");
        }
    }

    #[test]
    fn handing_out_a_handle_is_a_report_only_finding() {
        let src = "impl Project {\n    pub fn repo(&self) -> &Repo { &self.r }\n    pub fn shared(&self) -> Arc<Repo> { x }\n    pub fn open(&self) -> Result<ShadowRepo, E> { x }\n    pub fn name(&self) -> &str { n }\n    pub(crate) fn hidden(&self) -> &Repo { r }\n    fn private(&self) -> &Repo { r }\n}\ntype Rp = lector_git::Repo;\npub fn via_alias() -> Option<&'static Rp> { None }";
        let v = run("crates/lector-app/src/project.rs", src);
        assert_eq!(lines(&v), [2, 3, 4, 10], "{v:?}");
        assert!(v.iter().all(|v| v.message.starts_with("[report-only]")));
        assert!(run("crates/lector-git/src/repo.rs", src).is_empty());
    }

    fn fs_run(path: &str, src: &str, allow: &[&str]) -> (Vec<Violation>, usize) {
        let s = scan_file_full(
            &file(path, src),
            &api(),
            &AllowList::default(),
            &AllowList::default(),
            &AllowList::from_entries(allow.iter().copied()),
            &Globals::default(),
        );
        (s.violations, s.fs_allowed)
    }

    #[test]
    fn bypass_direct_file_writes_from_ui_mcp_cli_and_agent() {
        let src = "fn f(p: &Path) {\n std::fs::write(p, b);\n lector_fs::write_atomic(p, b);\n std::fs::remove_file(p);\n let f = File::create(p);\n OpenOptions::new().append(true);\n}";
        for krate in [
            "lector-ui",
            "lector-ui-git",
            "lector-mcp",
            "lector-cli",
            "lector-agent",
        ] {
            let (v, _) = fs_run(&format!("crates/{krate}/src/x.rs"), src, &[]);
            assert_eq!(lines(&v), [2, 3, 4, 5], "{krate}: {v:?}");
        }
        // lector-app (command modules write files by design) and the headless binary are out of scope
        let (v, _) = fs_run("crates/lector-app/src/documents/save.rs", src, &[]);
        assert!(v.is_empty());
    }

    #[test]
    fn bypass_direct_file_writes_through_aliases() {
        let src = "use std::fs as f;\nuse std::fs::{remove_dir_all as rm, File as F};\nuse lector_fs::write_atomic as wa;\nuse lector_fs as lf;\nfn g(p: &Path) {\n f::write(p, b);\n rm(p);\n F::create(p);\n wa(p, b);\n lf::write_atomic(p, b);\n}";
        let (v, _) = fs_run("crates/lector-mcp/src/x.rs", src, &[]);
        // the imports of `remove_dir_all` and `write_atomic` and the five uses
        assert_eq!(lines(&v), [2, 3, 6, 7, 8, 9, 10], "{v:?}");
    }

    #[test]
    fn an_unrelated_open_options_struct_is_not_a_file_write() {
        let src = "use lector_app::OpenOptions;\nfn f() { let o = OpenOptions { x: 1 }; }";
        assert!(fs_run("crates/lector-ui/src/x.rs", src, &[]).0.is_empty());
    }

    #[test]
    fn fs_allowances_exempt_a_file_and_are_counted() {
        let src = "fn f(p: &Path) { std::fs::write(p, b); std::fs::create_dir_all(p); }";
        let (v, allowed) = fs_run(
            "crates/lector-agent/src/session.rs",
            src,
            &["crates/lector-agent/src/session.rs"],
        );
        assert!(v.is_empty());
        assert_eq!(allowed, 2);
    }

    #[test]
    fn stale_allowances_and_unused_read_entries_fail() {
        let mut files = git_files();
        files.push(file(
            "crates/lector-ui/src/x.rs",
            "fn f(repo: &Repo) { repo.stage_paths(&[]); }",
        ));
        files.push(file(
            "crates/lector-app/src/commands/a.rs",
            "fn f(repo: &Repo) { repo.stage_paths(&[]); }",
        ));
        let mut cfg = Config::default();
        cfg.set_allow(
            "git-read",
            AllowList::from_entries([
                "Repo::last_commit_message",
                "Repo::gone_method",
                "value:LockGuard",
                "value:Ghost",
            ]),
        );
        cfg.set_allow(
            "git-ambiguous",
            AllowList::from_entries(["push", "prune_nothing"]),
        );
        cfg.set_allow(
            "git-callers",
            AllowList::from_entries([
                "crates/lector-app/src/commands/**",
                "crates/lector-app/src/old.rs",
            ]),
        );
        cfg.set_allow(
            "fs-writers",
            AllowList::from_entries(["crates/lector-ui/src/never_writes.rs"]),
        );
        let out = check(&files, &cfg);
        let stale: Vec<&str> = out
            .violations
            .iter()
            .filter(|v| v.message.starts_with("stale allowance"))
            .map(|v| v.message.as_str())
            .collect();
        for gone in [
            "Repo::gone_method",
            "value:Ghost",
            "prune_nothing",
            "crates/lector-app/src/old.rs",
            "never_writes",
        ] {
            assert!(
                stale.iter().any(|m| m.contains(gone)),
                "{gone} not reported stale: {stale:?}"
            );
        }
        for live in [
            "Repo::last_commit_message`",
            "push`",
            "commands/**`",
            "value:LockGuard",
        ] {
            assert!(
                !stale.iter().any(|m| m.contains(live)),
                "{live} wrongly stale: {stale:?}"
            );
        }
        // The UI call stands, the command module's call is exempt.
        let calls: Vec<&str> = out
            .violations
            .iter()
            .filter(|v| v.message.contains("lector-git write"))
            .map(|v| v.file.as_str())
            .collect();
        assert_eq!(calls, ["crates/lector-ui/src/x.rs"]);
    }

    #[test]
    fn process_types_and_std_command() {
        let src = "use lector_process::{ProcessCommand, Supervisor};\nuse std::process::{self, Command};\nfn f() {\n let c = ProcessCommand::new(\"git\");\n let d = Command::new(\"x\");\n let e = std::process::Command::new(\"y\");\n}\nstruct Supervisor;";
        let v = run("crates/lector-ui-git/src/x.rs", src);
        let mut l = lines(&v);
        l.sort_unstable();
        assert_eq!(l, [1, 1, 2, 4, 5, 6], "{v:?}");
    }

    #[test]
    fn command_without_a_process_import_is_ignored() {
        let src = "use clap::Command;\nfn f() { let c = Command::new(\"x\"); }";
        assert!(run("crates/lector-ui/src/x.rs", src).is_empty());
    }

    #[test]
    fn list_api_prints_every_kind() {
        let listing = api().listing();
        assert!(listing
            .iter()
            .any(|l| l.starts_with("method") && l.contains("stage_paths/1")));
        assert!(listing
            .iter()
            .any(|l| l.starts_with("assoc") && l.contains("constructor/1")));
        assert!(listing
            .iter()
            .any(|l| l.starts_with("free") && l.contains("free_fn/1")));
    }
}
