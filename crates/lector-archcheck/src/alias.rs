//! Alias resolution shared by the rules.
//!
//! The rules used to match call sites by spelling (`thread::spawn`, `Repo::stage_paths`,
//! `process::Command`), which an import rename defeats (`use std::thread as t; t::spawn(..)`).
//! This module gives every rule the same answer to "what does this path resolve to" without a
//! parser:
//!
//! * a per-file pass records every `use` leaf (`use a::b;`, `use a::b as c;`,
//!   `use a::{b as c, d};`, `use a::b::{self}`, nested groups), `extern crate a as b;`, glob
//!   imports (`use a::*;`) and `type X = Y;` aliases;
//! * [`Globals`] adds the workspace-wide table of `pub use` re-exports and type aliases by name,
//!   so `use crate::util::t;` or `use crate::types::Rp;` resolve through the file that defines
//!   them (name based, one hop per pass, three passes);
//! * [`Resolver::resolve`] expands the first segment of a path through those tables and returns
//!   every candidate full path; [`path_occurrences`] lists the path expressions of a file.
//!
//! # Fail closed
//!
//! A name this pass cannot resolve is not ignored: [`Resolved::unresolved`] says that the path
//! starts at a name that could have come from a glob import (`use crate::compat::*;`) or from a
//! renamed import of a re-export the workspace table does not know. A rule that looks for a
//! distinctive last segment (`spawn`, `Command`, `write_atomic`, `OpenOptions`) then reports the
//! occurrence as a violation candidate. Everything the pass cannot see at all is listed in
//! `arch/README.md` (macros that expand to calls, `include!`, generated code, function pointers
//! held in variables).
//!
//! Aliases are file-wide, not scoped to the module that declared them; two modules of one file
//! that bind the same name differently both count (union), which can only add candidates.

use std::collections::{BTreeMap, BTreeSet};

use crate::lexer::TokKind;
use crate::source::SourceFile;

/// One name bound by a `use` (or `extern crate`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseLeaf {
    /// Full path as written (`["std", "thread", "spawn"]`).
    pub path: Vec<String>,
    /// The name it binds in this file (`go` for `use std::thread::spawn as go`); `_` for `as _`.
    pub local: String,
    /// Token index of the leaf's first identifier (for line numbers).
    pub tok: usize,
    /// Declared `pub` (any restriction).
    pub public: bool,
}

/// Everything one file declares about names.
#[derive(Debug, Clone, Default)]
pub struct FileNames {
    /// `use` leaves of non-test code.
    pub leaves: Vec<UseLeaf>,
    /// Glob imports: the path before `::*`.
    pub globs: Vec<Vec<String>>,
    /// `type X = path;` aliases of non-test code: name to the right-hand path as written.
    pub types: Vec<(String, Vec<String>)>,
    /// Token ranges `[use, ;]` of `use` statements (path occurrences skip them).
    pub use_ranges: Vec<(usize, usize)>,
    /// Names the file defines itself (`fn x`, `struct X`, `mod x`, `const X`, ...): a path that
    /// starts at one of them is not hidden behind a glob import.
    pub defined: BTreeSet<String>,
}

fn ident(f: &SourceFile, i: usize) -> bool {
    f.tokens().get(i).is_some_and(|t| t.kind == TokKind::Ident)
}

/// True when the token before `i` is `pub` or a `pub(...)` restriction.
fn is_pub_before(f: &SourceFile, i: usize) -> bool {
    if i == 0 {
        return false;
    }
    match f.tok(i - 1) {
        "pub" => true,
        ")" => {
            let mut k = i - 1;
            while k > 0 && f.tok(k) != "(" {
                k -= 1;
            }
            k > 0 && f.tok(k - 1) == "pub"
        }
        _ => false,
    }
}

fn is_start_of_item(f: &SourceFile, i: usize) -> bool {
    i == 0 || !matches!(f.tok(i - 1), "::" | "." | "$")
}

/// Parse one use tree starting at token `*j`; leaves go to `out`.
fn parse_tree(
    f: &SourceFile,
    j: &mut usize,
    prefix: &mut Vec<String>,
    out: &mut FileNames,
    public: bool,
) {
    if f.tok(*j) == "::" {
        *j += 1;
    }
    let base = prefix.len();
    loop {
        match f.tok(*j) {
            "{" => {
                *j += 1;
                loop {
                    parse_tree(f, j, prefix, out, public);
                    match f.tok(*j) {
                        "," => {
                            *j += 1;
                            if f.tok(*j) == "}" {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
                if f.tok(*j) == "}" {
                    *j += 1;
                }
                prefix.truncate(base);
                return;
            }
            "*" => {
                out.globs.push(prefix.clone());
                *j += 1;
                prefix.truncate(base);
                return;
            }
            name if ident(f, *j) => {
                let tok = *j;
                prefix.push(name.to_string());
                *j += 1;
                if f.tok(*j) == "::" {
                    *j += 1;
                    continue;
                }
                let mut path = prefix.clone();
                if name == "self" {
                    path.pop();
                }
                let mut local = path.last().cloned().unwrap_or_default();
                if f.tok(*j) == "as" && ident(f, *j + 1) {
                    local = f.tok(*j + 1).to_string();
                    *j += 2;
                }
                if !path.is_empty() {
                    out.leaves.push(UseLeaf {
                        path,
                        local,
                        tok,
                        public,
                    });
                }
                prefix.truncate(base);
                return;
            }
            _ => {
                prefix.truncate(base);
                return;
            }
        }
    }
}

/// Collect the names a file declares (non-test code only).
pub fn collect_names(f: &SourceFile) -> FileNames {
    let mut out = FileNames::default();
    let n = f.tokens().len();
    let mut i = 0;
    while i < n {
        if f.is_test(i) || !ident(f, i) || !is_start_of_item(f, i) {
            i += 1;
            continue;
        }
        if matches!(
            f.tok(i),
            "fn" | "struct" | "enum" | "trait" | "union" | "mod" | "const" | "static" | "type"
        ) && ident(f, i + 1)
        {
            out.defined.insert(f.tok(i + 1).to_string());
        }
        match f.tok(i) {
            "use" => {
                let public = is_pub_before(f, i);
                let mut j = i + 1;
                let mut prefix = Vec::new();
                parse_tree(f, &mut j, &mut prefix, &mut out, public);
                let mut end = j;
                while end < n && f.tok(end) != ";" {
                    end += 1;
                }
                out.use_ranges.push((i, end));
                i = end + 1;
            }
            "extern" if f.tok(i + 1) == "crate" && ident(f, i + 2) => {
                let name = f.tok(i + 2).to_string();
                let mut local = name.clone();
                if f.tok(i + 3) == "as" && ident(f, i + 4) {
                    local = f.tok(i + 4).to_string();
                }
                out.leaves.push(UseLeaf {
                    path: vec![name],
                    local,
                    tok: i + 2,
                    public: false,
                });
                i += 3;
            }
            "type" if ident(f, i + 1) && !matches!(f.tok(i + 1), "where") => {
                let name = f.tok(i + 1).to_string();
                let mut j = i + 2;
                if f.tok(j) == "<" {
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
                if f.tok(j) == "=" {
                    j += 1;
                    while matches!(f.tok(j), "&" | "mut" | "dyn" | "impl" | "::")
                        || f.tokens()
                            .get(j)
                            .is_some_and(|t| t.kind == TokKind::Lifetime)
                    {
                        j += 1;
                    }
                    let mut path = Vec::new();
                    while ident(f, j) {
                        path.push(f.tok(j).to_string());
                        if f.tok(j + 1) == "::" && ident(f, j + 2) {
                            j += 2;
                        } else {
                            break;
                        }
                    }
                    if !path.is_empty() {
                        out.types.push((name, path));
                    }
                }
                i += 2;
            }
            _ => i += 1,
        }
    }
    out
}

fn is_internal(path: &[String]) -> bool {
    path.first().is_some_and(|s| {
        matches!(s.as_str(), "crate" | "self" | "super") || s.starts_with("lector")
    })
}

/// Workspace-wide re-exports (`pub use`) and type aliases, by exported name.
#[derive(Debug, Clone, Default)]
pub struct Globals {
    exports: BTreeMap<String, BTreeSet<Vec<String>>>,
}

impl Globals {
    /// Build the table from every file (non-test code).
    pub fn build(files: &[SourceFile]) -> Globals {
        let mut g = Globals::default();
        let names: Vec<FileNames> = files
            .iter()
            .filter(|f| !f.test_only)
            .map(collect_names)
            .collect();
        // Three passes: each resolves the exports through the table of the previous pass, so a
        // chain of re-exports up to three hops long is followed.
        for _ in 0..3 {
            let mut next = g.clone();
            for fnames in &names {
                let r = Resolver::from_names(fnames.clone(), &g);
                for leaf in fnames.leaves.iter().filter(|l| l.public && l.local != "_") {
                    for p in r.expand_path(&leaf.path) {
                        next.exports
                            .entry(leaf.local.clone())
                            .or_default()
                            .insert(p);
                    }
                }
                for (name, rhs) in &fnames.types {
                    for p in r.expand_path(rhs) {
                        next.exports.entry(name.clone()).or_default().insert(p);
                    }
                }
            }
            g = next;
        }
        g
    }

    fn get(&self, name: &str) -> Option<&BTreeSet<Vec<String>>> {
        self.exports.get(name)
    }
}

/// The result of resolving one path.
#[derive(Debug, Clone, Default)]
pub struct Resolved {
    /// Every candidate full path (the path as written when nothing binds its first segment).
    pub paths: Vec<Vec<String>>,
    /// The first segment is bound by nothing this pass can see, and the file has a glob import
    /// (or a renamed import of an unknown re-export) that could have bound it.
    pub unresolved: bool,
    /// The path as written ends with this segment.
    pub last: String,
    /// The path as written.
    pub written: Vec<String>,
}

impl Resolved {
    /// Some candidate contains `pat` as consecutive segments.
    pub fn has_window(&self, pat: &[&str]) -> bool {
        !pat.is_empty()
            && self.paths.iter().any(|p| {
                p.windows(pat.len())
                    .any(|w| w.iter().zip(pat).all(|(a, b)| a == b))
            })
    }

    /// Some candidate starts with `root` and ends with `last`.
    pub fn is_rooted(&self, root: &str, last: &str) -> bool {
        self.paths
            .iter()
            .any(|p| p.first().is_some_and(|s| s == root) && p.last().is_some_and(|s| s == last))
    }

    /// Some candidate starts with `root` and has one of `names` as a segment.
    pub fn rooted_with_segment(&self, root: &str, names: &[&str]) -> bool {
        self.paths.iter().any(|p| {
            p.first().is_some_and(|s| s == root)
                && p.iter().skip(1).any(|s| names.contains(&s.as_str()))
        })
    }

    /// Some candidate has a segment equal to `name`.
    pub fn has_segment(&self, name: &str) -> bool {
        self.paths.iter().any(|p| p.iter().any(|s| s == name))
    }

    /// Some candidate's last segment is `last` and its second to last is `owner`.
    pub fn ends_with(&self, owner: &str, last: &str) -> bool {
        self.paths
            .iter()
            .any(|p| p.len() >= 2 && p[p.len() - 1] == last && p[p.len() - 2] == owner)
    }

    /// The path as written, or a candidate, ends with `name`.
    pub fn last_is(&self, name: &str) -> bool {
        self.last == name
            || self
                .paths
                .iter()
                .any(|p| p.last().is_some_and(|s| s == name))
    }

    /// The fail-closed case: the path could not be resolved and could be one of the targets
    /// `names` under another name: a bare `names` entry (`spawn(..)`, `Command::new(..)`), or a
    /// lowercase module-like first segment followed by one of them (`t::spawn`). A path that
    /// starts with an uppercase name (`Terminal::spawn`) is a type's own function, not an alias
    /// of a module.
    pub fn candidate_for(&self, names: &[&str]) -> bool {
        if !self.unresolved {
            return false;
        }
        let Some(first) = self.written.first() else {
            return false;
        };
        if names.contains(&first.as_str()) {
            return true;
        }
        self.written.len() > 1
            && first.chars().next().is_some_and(char::is_lowercase)
            && names.contains(&self.last.as_str())
    }
}

/// Resolves paths of one file.
#[derive(Debug, Clone)]
pub struct Resolver<'g> {
    uses: BTreeMap<String, BTreeSet<Vec<String>>>,
    types: BTreeMap<String, Vec<Vec<String>>>,
    globs: Vec<Vec<String>>,
    /// Some glob import could bind names we cannot see (an internal module or `std`).
    hiding_glob: bool,
    unknown_renames: BTreeSet<String>,
    exports: &'g Globals,
    /// Names collected from the file (use leaves, globs, aliases, use statement ranges).
    pub names: FileNames,
}

static NO_GLOBALS: Globals = Globals {
    exports: BTreeMap::new(),
};

impl<'g> Resolver<'g> {
    /// Resolver for `f`, extended by the workspace table.
    pub fn new(f: &SourceFile, globals: &'g Globals) -> Resolver<'g> {
        Resolver::from_names(collect_names(f), globals)
    }

    fn from_names(names: FileNames, globals: &'g Globals) -> Resolver<'g> {
        let mut uses: BTreeMap<String, BTreeSet<Vec<String>>> = BTreeMap::new();
        let mut unknown_renames = BTreeSet::new();
        for leaf in &names.leaves {
            if leaf.local == "_" {
                continue;
            }
            uses.entry(leaf.local.clone())
                .or_default()
                .insert(leaf.path.clone());
            if is_internal(&leaf.path) {
                let last = leaf.path.last().cloned().unwrap_or_default();
                match globals.get(&last) {
                    Some(ex) => {
                        uses.entry(leaf.local.clone())
                            .or_default()
                            .extend(ex.iter().cloned());
                    }
                    None if leaf.local != last => {
                        unknown_renames.insert(leaf.local.clone());
                    }
                    None => {}
                }
            }
        }
        let mut types: BTreeMap<String, Vec<Vec<String>>> = BTreeMap::new();
        for (name, rhs) in &names.types {
            types.entry(name.clone()).or_default().push(rhs.clone());
        }
        let hiding_glob = names.globs.iter().any(|g| {
            g.first().is_some_and(|s| {
                matches!(
                    s.as_str(),
                    "crate" | "self" | "super" | "std" | "core" | "alloc"
                ) || s.starts_with("lector")
            })
        });
        Resolver {
            uses,
            types,
            globs: names.globs.clone(),
            hiding_glob,
            unknown_renames,
            exports: globals,
            names,
        }
    }

    /// A resolver that knows nothing (tests, files without names).
    pub fn empty() -> Resolver<'static> {
        Resolver::from_names(FileNames::default(), &NO_GLOBALS)
    }

    fn expand_first(&self, first: &str, depth: usize) -> Vec<Vec<String>> {
        let mut out: Vec<Vec<String>> = Vec::new();
        if let Some(ps) = self.uses.get(first) {
            out.extend(ps.iter().cloned());
        }
        if depth < 4 {
            if let Some(ts) = self.types.get(first) {
                for rhs in ts {
                    for base in self.expand_first(&rhs[0], depth + 1) {
                        let mut p = base;
                        p.extend(rhs[1..].iter().cloned());
                        out.push(p);
                    }
                    if !self.uses.contains_key(&rhs[0]) && !self.types.contains_key(&rhs[0]) {
                        out.push(rhs.clone());
                    }
                }
            }
        }
        out
    }

    /// Candidate full paths of a path as written (see [`Resolver::resolve`]).
    fn expand_path(&self, segs: &[String]) -> Vec<Vec<String>> {
        let Some(first) = segs.first() else {
            return Vec::new();
        };
        let mut out: Vec<Vec<String>> = self
            .expand_first(first, 0)
            .into_iter()
            .map(|mut p| {
                p.extend(segs[1..].iter().cloned());
                p
            })
            .collect();
        if out.is_empty() {
            out.push(segs.to_vec());
        }
        out
    }

    /// Resolve a path expression (segments as written, leading `::` dropped).
    pub fn resolve(&self, segs: &[String]) -> Resolved {
        let last = segs.last().cloned().unwrap_or_default();
        let Some(first) = segs.first() else {
            return Resolved::default();
        };
        let bound = self.uses.contains_key(first)
            || self.types.contains_key(first)
            || self.names.defined.contains(first);
        let mut paths = self.expand_path(segs);
        let mut unresolved = false;
        if !bound {
            if !self.globs.is_empty() {
                unresolved = self.hiding_glob;
                for g in &self.globs {
                    let mut p = g.clone();
                    p.extend(segs.iter().cloned());
                    paths.push(p);
                }
                if let Some(ex) = self.exports.get(first) {
                    for e in ex {
                        let mut p = e.clone();
                        p.extend(segs[1..].iter().cloned());
                        paths.push(p);
                    }
                }
            }
        } else if self.unknown_renames.contains(first) {
            unresolved = true;
        }
        Resolved {
            paths,
            unresolved,
            last,
            written: segs.to_vec(),
        }
    }

    /// Resolve a use leaf: the full path as written, expanded through the aliases.
    pub fn resolve_leaf(&self, leaf: &UseLeaf) -> Resolved {
        let mut r = self.resolve(&leaf.path);
        // A leaf's own binding must not resolve it through itself.
        if r.paths.is_empty() {
            r.paths.push(leaf.path.clone());
        }
        r
    }
}

/// One path expression of a file: `a::b::c`, a bare name, `::std::fs::write`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathOcc {
    /// Token index of the first segment.
    pub start: usize,
    /// Token index just past the last segment (turbofish generics included).
    pub end: usize,
    /// The segments as written.
    pub segs: Vec<String>,
    /// The path is followed by `(`.
    pub call: bool,
}

/// The path expressions of the non-test code of `f` outside `use` statements. Definitions
/// (`fn name`, `struct Name`, ...) are not occurrences of the name they define.
pub fn path_occurrences(f: &SourceFile, names: &FileNames) -> Vec<PathOcc> {
    let n = f.tokens().len();
    let mut in_use = vec![false; n + 1];
    for (a, b) in &names.use_ranges {
        for slot in in_use.iter_mut().take((*b + 1).min(n)).skip(*a) {
            *slot = true;
        }
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if in_use[i] || f.is_test(i) || !ident(f, i) {
            i += 1;
            continue;
        }
        let prev = if i > 0 { f.tok(i - 1) } else { "" };
        let mid_path = prev == "::" && i >= 2 && (ident(f, i - 2) || f.tok(i - 2) == ">");
        if mid_path || prev == "." || prev == "$" {
            i += 1;
            continue;
        }
        if matches!(
            prev,
            "fn" | "struct" | "enum" | "mod" | "trait" | "union" | "type" | "macro_rules"
        ) {
            i += 1;
            continue;
        }
        let start = i;
        let mut segs = vec![f.tok(i).to_string()];
        let mut j = i + 1;
        loop {
            if f.tok(j) != "::" {
                break;
            }
            if ident(f, j + 1) {
                segs.push(f.tok(j + 1).to_string());
                j += 2;
            } else if f.tok(j + 1) == "<" {
                let mut depth = 0usize;
                let mut k = j + 1;
                while k < n {
                    match f.tok(k) {
                        "<" => depth += 1,
                        ">" => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    k += 1;
                }
                j = k + 1;
                if !(f.tok(j) == "::" && ident(f, j + 1)) {
                    break;
                }
            } else {
                break;
            }
        }
        out.push(PathOcc {
            start,
            end: j,
            segs,
            call: f.tok(j) == "(",
        });
        i = j.max(i + 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::file;

    fn resolver(src: &str) -> (SourceFile, Resolver<'static>) {
        let f = file("crates/a/src/x.rs", src);
        let r = Resolver::new(&f, &NO_GLOBALS);
        (f, r)
    }

    fn segs(s: &str) -> Vec<String> {
        s.split("::").map(String::from).collect()
    }

    fn paths(r: &Resolver<'_>, p: &str) -> Vec<String> {
        r.resolve(&segs(p))
            .paths
            .iter()
            .map(|p| p.join("::"))
            .collect()
    }

    #[test]
    fn plain_alias_and_group_alias() {
        let (_, r) = resolver(
            "use std::thread as t;\nuse std::{fs as f, process::{self, Command as C}};\nuse std::thread::spawn as go;",
        );
        assert_eq!(paths(&r, "t::spawn"), ["std::thread::spawn"]);
        assert_eq!(paths(&r, "f::write"), ["std::fs::write"]);
        assert_eq!(paths(&r, "C::new"), ["std::process::Command::new"]);
        assert_eq!(paths(&r, "process::exit"), ["std::process::exit"]);
        assert_eq!(paths(&r, "go"), ["std::thread::spawn"]);
    }

    #[test]
    fn imported_module_resolves_without_a_rename() {
        let (_, r) =
            resolver("use std::thread;\nuse std::fs::{self, File};\nextern crate foo as bar;");
        assert_eq!(paths(&r, "thread::spawn"), ["std::thread::spawn"]);
        assert_eq!(paths(&r, "fs::write"), ["std::fs::write"]);
        assert_eq!(paths(&r, "File::create"), ["std::fs::File::create"]);
        assert_eq!(paths(&r, "bar::x"), ["foo::x"]);
    }

    #[test]
    fn type_aliases_resolve_through_imports() {
        let (_, r) = resolver(
            "use lector_git::Repo;\ntype Rp = Repo;\ntype Q<'a> = &'a lector_git::ShadowRepo;",
        );
        assert_eq!(paths(&r, "Rp::stage"), ["lector_git::Repo::stage"]);
        assert_eq!(paths(&r, "Q::open"), ["lector_git::ShadowRepo::open"]);
    }

    #[test]
    fn unbound_paths_stay_as_written_and_globs_make_them_unresolved() {
        let (_, r) = resolver("fn f() {}");
        let x = r.resolve(&segs("std::fs::write"));
        assert_eq!(x.paths, [segs("std::fs::write")]);
        assert!(!x.unresolved);
        let (_, g) = resolver("use crate::compat::*;\nuse std::process::*;");
        let x = g.resolve(&segs("t::spawn"));
        assert!(x.unresolved && x.candidate_for(&["spawn"]) && !x.candidate_for(&["Command"]));
        // A real glob prefix is a candidate: `use std::process::*; Command::new`.
        assert!(g
            .resolve(&segs("Command::new"))
            .has_window(&["process", "Command"]));
    }

    #[test]
    fn globals_follow_reexports_and_type_aliases_across_files() {
        let util = file(
            "crates/a/src/util.rs",
            "pub use std::thread as t;\npub type Rp = lector_git::Repo;",
        );
        let user = file(
            "crates/a/src/x.rs",
            "use crate::util::{t, Rp};\nuse crate::util::t as u;",
        );
        let g = Globals::build(&[util, user.clone()]);
        let r = Resolver::new(&user, &g);
        assert!(r
            .resolve(&segs("t::spawn"))
            .has_window(&["thread", "spawn"]));
        assert!(r
            .resolve(&segs("u::spawn"))
            .has_window(&["thread", "spawn"]));
        assert!(r.resolve(&segs("Rp::stage")).ends_with("Repo", "stage"));
    }

    #[test]
    fn a_renamed_internal_import_that_cannot_be_followed_is_unresolved() {
        let (_, r) = resolver("use crate::macros::t as tt;");
        assert!(r.resolve(&segs("tt::spawn")).candidate_for(&["spawn"]));
        let (_, r) = resolver("use crate::macros::t;");
        assert!(!r.resolve(&segs("t::spawn")).unresolved);
    }

    #[test]
    fn test_code_does_not_bind_names() {
        let (_, r) = resolver("#[cfg(test)]\nmod t { use std::thread as w; }\nfn f() {}");
        assert_eq!(paths(&r, "w::spawn"), ["w::spawn"]);
    }

    #[test]
    fn occurrences_skip_use_statements_methods_and_definitions() {
        let src = "use std::thread::spawn;\nfn spawn_it() { let a = std::thread::spawn(f); x.spawn(g); self::go::<u8>(1); ::std::fs::write(a, b); }\nstruct Command;\nfn go() {}";
        let f = file("crates/a/src/x.rs", src);
        let names = collect_names(&f);
        let occ = path_occurrences(&f, &names);
        let seen: Vec<String> = occ.iter().map(|o| o.segs.join("::")).collect();
        assert!(seen.contains(&"std::thread::spawn".to_string()), "{seen:?}");
        assert!(seen.contains(&"self::go".to_string()), "{seen:?}");
        assert!(seen.contains(&"std::fs::write".to_string()), "{seen:?}");
        assert!(
            !seen
                .iter()
                .any(|s| s == "spawn_it" || s == "Command" || s == "go"),
            "{seen:?}"
        );
        assert!(occ
            .iter()
            .find(|o| o.segs.join("::") == "std::thread::spawn")
            .is_some_and(|o| o.call));
    }

    #[test]
    fn use_trees_with_self_nesting_and_trailing_commas() {
        let (f, r) = resolver("pub(crate) use a::b::{self, c::{d, e as g,}, h as _};");
        let leaves: Vec<(String, String, bool)> = r
            .names
            .leaves
            .iter()
            .map(|l| (l.path.join("::"), l.local.clone(), l.public))
            .collect();
        assert_eq!(
            leaves,
            [
                ("a::b".into(), "b".into(), true),
                ("a::b::c::d".into(), "d".into(), true),
                ("a::b::c::e".into(), "g".into(), true),
                ("a::b::h".into(), "_".into(), true),
            ]
        );
        let _ = f;
    }
}
