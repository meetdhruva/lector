//! Detectors shared by several rules, all built on alias resolution ([`crate::alias`]).
//!
//! Each detector looks at *sites*: every path expression of the non-test code of a file
//! (`std::thread::spawn`, `t::spawn`, a bare `go`) and every `use` leaf (`use std::thread::spawn
//! as go;`), resolved through the file's imports, glob imports and type aliases. A site whose
//! first segment cannot be resolved but could have come from a glob import is a *guess*: the
//! detectors report it for distinctive last segments (`spawn`, `Command`, `write_atomic`,
//! `OpenOptions`, executor type names) and mark the message, so a reviewer can tell it from a
//! certain hit. The real-tree noise of the guesses is stated in `arch/README.md`.

use crate::alias::{path_occurrences, Resolved, Resolver};
use crate::source::SourceFile;

/// One path expression or `use` leaf with its resolution.
#[derive(Debug, Clone)]
pub struct Site {
    /// Token index of the first segment.
    pub tok: usize,
    /// Segments as written.
    pub segs: Vec<String>,
    /// What it resolves to.
    pub res: Resolved,
    /// A `use` leaf rather than an expression or type path.
    pub import: bool,
    /// Followed by `(`.
    pub call: bool,
    /// Token index just past the path.
    pub end: usize,
}

/// All sites of the non-test code of `f`.
pub fn sites(f: &SourceFile, r: &Resolver<'_>) -> Vec<Site> {
    let mut out = Vec::new();
    for leaf in &r.names.leaves {
        let mut res = r.resolve(&leaf.path);
        res.unresolved = false;
        out.push(Site {
            tok: leaf.tok,
            segs: leaf.path.clone(),
            res,
            import: true,
            call: false,
            end: leaf.tok + 1,
        });
    }
    for o in path_occurrences(f, &r.names) {
        out.push(Site {
            tok: o.start,
            res: r.resolve(&o.segs),
            segs: o.segs,
            import: false,
            call: o.call,
            end: o.end,
        });
    }
    out.sort_by_key(|s| (s.tok, !s.import));
    out
}

impl Site {
    /// The fail-closed guess for `names` (see [`Resolved::candidate_for`]); a bare name must
    /// be called, so a local variable that happens to be named `spawn` is not a hit.
    pub fn guess(&self, names: &[&str]) -> bool {
        !self.import && self.res.candidate_for(names) && (self.segs.len() > 1 || self.call)
    }
}

/// A detector finding: token index, what was found, and whether it is a fail-closed guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Token index.
    pub tok: usize,
    /// What was found (`thread::spawn`, `fs::write`, ...).
    pub what: String,
    /// The path could not be resolved; reported because its last segment looks like the target.
    pub guess: bool,
    /// Found in a `use` statement.
    pub import: bool,
}

impl Hit {
    /// `what`, with the guess marker when the path could not be resolved.
    pub fn describe(&self) -> String {
        if self.guess {
            format!("`{}` (name cannot be resolved: a glob import or renamed re-export may hide it; treated as a hit)", self.what)
        } else if self.import {
            format!("`{}` imported", self.what)
        } else {
            format!("`{}`", self.what)
        }
    }
}

fn hit(s: &Site, what: impl Into<String>, guess: bool) -> Hit {
    Hit {
        tok: s.tok,
        what: what.into(),
        guess,
        import: s.import,
    }
}

/// `thread::spawn`, `thread::Builder` and `.spawn_scoped(`, through any alias.
pub fn thread_spawns(f: &SourceFile, r: &Resolver<'_>) -> Vec<Hit> {
    let mut out = Vec::new();
    for s in sites(f, r) {
        if s.res.has_window(&["thread", "spawn"]) {
            out.push(hit(&s, "thread::spawn", false));
        } else if s.res.has_window(&["thread", "Builder"]) {
            out.push(hit(&s, "thread::Builder", false));
        } else if s.guess(&["spawn", "Builder"]) {
            out.push(hit(&s, s.segs.join("::"), true));
        }
    }
    for i in 0..f.tokens().len() {
        if !f.is_test(i) && f.seq(i, &[".", "spawn_scoped", "("]) {
            out.push(Hit {
                tok: i,
                what: ".spawn_scoped(".to_string(),
                guess: false,
                import: false,
            });
        }
    }
    out.sort_by_key(|h| h.tok);
    out
}

/// `std::fs` functions that change files or directories.
pub const FS_WRITES: &[&str] = &[
    "write",
    "remove_file",
    "remove_dir",
    "remove_dir_all",
    "rename",
    "copy",
    "create_dir",
    "create_dir_all",
    "hard_link",
    "soft_link",
    "symlink",
    "symlink_file",
    "symlink_dir",
    "set_permissions",
];

/// File writes through `fs::*`, `File::create*`, `OpenOptions`, `lector_fs::write_atomic` and the
/// confined write of `lector_fs::confine` (named by its `CreateDirs` argument), through any
/// alias. With `any_open_options` every mention of an `OpenOptions` type counts
/// (the `purity` rule); otherwise only one that resolves to a file API (`std::fs`, `tokio::fs`)
/// does, so an unrelated `OpenOptions` struct (a document-open option) is not a hit.
pub fn fs_writes(f: &SourceFile, r: &Resolver<'_>, any_open_options: bool) -> Vec<Hit> {
    let mut out = Vec::new();
    for s in sites(f, r) {
        if let Some(w) = FS_WRITES.iter().find(|w| s.res.has_window(&["fs", w])) {
            out.push(hit(&s, format!("fs::{w}"), false));
        } else if let Some(c) = ["create", "create_new", "options"]
            .iter()
            .find(|c| s.res.has_window(&["File", c]))
        {
            out.push(hit(&s, format!("File::{c}"), false));
        } else if s.res.is_rooted("lector_fs", "write_atomic") {
            out.push(hit(&s, "lector_fs::write_atomic", false));
        } else if !s.import && s.res.rooted_with_segment("lector_fs", &["CreateDirs"]) {
            // `Confined::write(bytes, CreateDirs::..)`: the confined write of `lector_fs::confine`.
            out.push(hit(&s, "lector_fs::confine write (CreateDirs)", false));
        } else if s.res.has_segment("OpenOptions")
            && (any_open_options || s.res.has_window(&["fs", "OpenOptions"]))
        {
            out.push(hit(&s, "OpenOptions", false));
        } else if s.guess(&["write_atomic", "OpenOptions"]) {
            out.push(hit(&s, s.segs.join("::"), true));
        }
    }
    out
}

/// A process-spawning construct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessUse {
    /// `ProcessCommand` or `Supervisor` (from `lector_process`).
    Process(String),
    /// `std::process::Command`, path, import, or `Command::new(` of an imported one.
    StdCommand,
}

/// Process-spawning constructs, through any alias: `ProcessCommand` / `Supervisor` in any
/// position, and `std::process::Command`. Returns the token index, the construct and the guess
/// flag.
pub fn process_uses(f: &SourceFile, r: &Resolver<'_>) -> Vec<(usize, ProcessUse, bool)> {
    let mut out = Vec::new();
    for s in sites(f, r) {
        if let Some(t) = ["ProcessCommand", "Supervisor"]
            .iter()
            .find(|t| s.res.has_segment(t))
        {
            out.push((s.tok, ProcessUse::Process((*t).to_string()), false));
        } else if s.res.has_window(&["process", "Command"]) {
            out.push((s.tok, ProcessUse::StdCommand, false));
        } else if s.guess(&["Command"]) {
            out.push((s.tok, ProcessUse::StdCommand, true));
        }
    }
    out
}

/// Crates whose job is to run futures or threads (`arch-check background`).
pub const EXECUTOR_CRATES: &[&str] = &[
    "tokio",
    "smol",
    "async-std",
    "async-executor",
    "async-global-executor",
    "blocking",
    "futures-executor",
    "threadpool",
    "scoped-threadpool",
    "scoped_threadpool",
];

/// Spawning through something other than the scheduler or the GPUI bridge: a GPUI executor type
/// named or stored, another executor crate, a
/// `ThreadPool`, `rayon::spawn` (rayon's scoped and data-parallel calls are fine: they end
/// before they return).
pub fn executor_uses(f: &SourceFile, r: &Resolver<'_>) -> Vec<Hit> {
    let mut out = Vec::new();
    let roots: Vec<String> = EXECUTOR_CRATES
        .iter()
        .map(|c| c.replace('-', "_"))
        .collect();
    for s in sites(f, r) {
        let first_is_executor_crate = s.res.paths.iter().find_map(|p| {
            p.first()
                .filter(|r0| roots.iter().any(|c| c == *r0) && !r.names.defined.contains(*r0))
        });
        if let Some(t) = ["BackgroundExecutor", "ForegroundExecutor"]
            .iter()
            .find(|t| s.res.has_segment(t))
        {
            out.push(hit(&s, *t, false));
        } else if let Some(root) = first_is_executor_crate {
            out.push(hit(&s, format!("{root}::..."), false));
        } else if s
            .res
            .rooted_with_segment("rayon", &["spawn", "spawn_fifo", "ThreadPoolBuilder"])
            || (["ThreadPool", "LocalPool", "LocalSpawner"]
                .iter()
                .any(|t| s.res.has_segment(t))
                && s.res.paths.iter().any(|p| {
                    p.iter().any(|seg| {
                        matches!(
                            seg.as_str(),
                            "futures" | "futures_executor" | "executor" | "threadpool" | "rayon"
                        )
                    })
                }))
        {
            out.push(hit(&s, s.segs.join("::"), false));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alias::Globals;
    use crate::rules::file;

    fn run<T>(src: &str, f: impl Fn(&SourceFile, &Resolver<'_>) -> T) -> T {
        let sf = file("crates/a/src/x.rs", src);
        let g = Globals::default();
        let r = Resolver::new(&sf, &g);
        f(&sf, &r)
    }

    #[test]
    fn thread_spawn_through_aliases() {
        let src = "use std::thread as t;\nuse std::thread::spawn as go;\nuse std::thread::{self as th, Builder as B};\nfn f() { t::spawn(a); go(b); th::spawn(c); B::new(); }";
        let hits = run(src, thread_spawns);
        let lines: Vec<u32> = hits
            .iter()
            .map(|h| run(src, |f, _| f.line(h.tok)))
            .collect();
        assert_eq!(lines, [2, 3, 4, 4, 4, 4], "{hits:?}");
    }

    #[test]
    fn the_confined_write_is_found_by_its_create_dirs_argument() {
        let src = "use lector_fs::{Access, CreateDirs as Cd};\nfn g(t: Confined) { t.write(b, Cd::Yes); t.write(b, lector_fs::CreateDirs::No); }";
        let hits = run(src, |f, r| fs_writes(f, r, false));
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert!(hits.iter().all(|h| h.what.contains("confine write")));
    }

    #[test]
    fn fs_writes_through_aliases_and_the_unrelated_open_options() {
        let src = "use std::fs as f;\nuse std::process as p;\nuse lector_fs::write_atomic as wa;\nuse lector_app::OpenOptions;\nfn g() { f::write(a, b); wa(p, b); let _ = OpenOptions { x: 1 }; std::fs::OpenOptions::new(); }";
        let hits = run(src, |f, r| fs_writes(f, r, false));
        let whats: Vec<&str> = hits.iter().map(|h| h.what.as_str()).collect();
        assert_eq!(
            whats,
            [
                "lector_fs::write_atomic",
                "fs::write",
                "lector_fs::write_atomic",
                "OpenOptions"
            ],
            "{hits:?}"
        );
    }
}
