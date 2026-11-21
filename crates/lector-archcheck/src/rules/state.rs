//! `state` (ADR 0004): Lector-owned state lives in the database, not in ad-hoc files.
//!
//! **Heuristic.** In non-test code outside the files listed in `arch/state.allow` the rule looks
//! at statements (token runs between `;`, `{` and `}`) and reports:
//!
//! 1. **a file or directory created under a path derived from a data/config/state dir**,
//!    whatever its name or extension: a file-creating call (`fs::write`, `fs::copy`,
//!    `fs::rename`, `fs::create_dir*`, `File::create*`, `OpenOptions` with write/append/create,
//!    `lector_fs::write_atomic`, through any alias) in a statement that mentions such a path. A
//!    path is *derived* when it comes from `data_dir()` / `config_dir()` / `state_dir()` / any
//!    other `pub fn` of `lector-config`'s `paths.rs` that returns a `PathBuf` (derived from the
//!    source, so a new directory helper counts at once; the ones that only join an exempt
//!    `literal:` name, `settings_file()` and `keymap_file()`, do not), a parameter or field named
//!    like one, or a local variable that was bound or mutated from such an expression earlier in
//!    the function (`let mut p = data_dir(); p.push("x"); fs::write(&p, ..)`; `let q =
//!    p.with_extension("bin")`; `p = data_dir().join(..)`; `p.set_file_name(..)`). Scoping is by
//!    block: a binding is visible only inside the block that declared it, and a later
//!    `let dir = <untainted>` shadows an earlier tainted `dir`;
//! 2. a `.join(...)` whose argument contains a string literal ending in a state-file extension
//!    (`.json`, `.redb`, `.db`, `.toml`, `.ron`, `.bin`, `.dat`, `.yaml`, `.yml`, `.sqlite*`,
//!    `.jsonl`, `.ndjson`, `.cbor`, `.msgpack`), directly or through a local variable bound from a
//!    literal or `format!` earlier (`let name = format!("{}-state.bin", n);
//!    data_dir().join(name)`), in a statement that also mentions a derived dir;
//! 3. a `.join(format!("...{}.json", ..))` (a file named per key, the shape of a hand-rolled state
//!    store) with any receiver;
//! 4. otherwise, a file-creating call in a statement that mentions a string literal with a
//!    state-file extension.
//!
//! In (2) a name declared in the same file as `const NAME: &str = "x.json";` (or `static`) counts
//! as its literal. File names listed as `literal:<name>` in the allow file (`settings.toml`,
//! `keymap.toml`: user-edited files, ADR 0004 decision 2) are exempt.
//!
//! Allowance hygiene: a file entry that exempts nothing and a `literal:` entry that exempted
//! nothing are violations.
//!
//! What it cannot see: a path passed to a helper function that does the write (the parameter is
//! then a plain name; the helper's own file is checked only when it names a dir or a state
//! literal itself), a path built across two statements through a field or a collection, a file
//! name held in a `const` of another file, persistence through a library that picks its own path,
//! and writes in macro bodies. Expect misses rather than false positives.

use crate::alias::{Globals, Resolver};
use crate::config::{AllowList, Config};
use crate::items::fn_items;
use crate::lexer::TokKind;
use crate::report::{RuleOutput, Unit, Violation};
use crate::rules::detect::fs_writes;
use crate::rules::str_value;
use crate::source::SourceFile;

/// Rule name.
pub const RULE: &str = "state";

const DIR_NAMES: &[&str] = &["data_dir", "state_dir", "config_dir"];
const STATE_EXTENSIONS: &[&str] = &[
    ".json", ".redb", ".db", ".toml", ".ron", ".bin", ".dat", ".yaml", ".yml", ".sqlite",
    ".sqlite3", ".jsonl", ".ndjson", ".cbor", ".msgpack",
];
const PATH_PATH: &str = "crates/lector-config/src/paths.rs";
const MUTATORS: &[&str] = &["push", "extend", "set_file_name", "set_extension"];

/// Run the rule over all files.
pub fn check(files: &[SourceFile], cfg: &Config) -> RuleOutput {
    let allow = cfg.allow("state");
    let globals = Globals::build(files);
    let sources = derive_sources(files, &allow);
    let mut found: Vec<Violation> = files
        .iter()
        .flat_map(|f| scan_file_with(f, &allow, &globals, &sources))
        .collect();
    found.extend(allow.stale_violations(RULE, &|_| false));
    RuleOutput::new(RULE, found, Unit::Count)
}

/// Names that yield a path under the data/config/state directories: the fixed ones plus every
/// `pub fn ... -> PathBuf` of `lector-config`'s `paths.rs` that does not just name an exempt file.
pub fn derive_sources(files: &[SourceFile], allow: &AllowList) -> Vec<String> {
    let mut out: Vec<String> = DIR_NAMES.iter().map(|s| (*s).to_string()).collect();
    let exempt = allow.tagged("literal");
    for f in files.iter().filter(|f| f.path == PATH_PATH) {
        for it in fn_items(f) {
            let returns_path = (it.ret.0..it.ret.1).any(|k| f.tok(k) == "PathBuf");
            if !it.is_pub || !returns_path || f.is_test(it.name_idx) {
                continue;
            }
            let names_exempt = it.body.is_some_and(|(a, b)| {
                (a..b).any(|k| {
                    f.tokens()[k].kind == TokKind::Str && exempt.contains(&str_value(f.tok(k)))
                })
            });
            if !names_exempt && !out.contains(&it.name) {
                out.push(it.name.clone());
            }
        }
    }
    out
}

struct Chunk {
    start: usize,
    end: usize,
}

fn chunks(f: &SourceFile) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut start = 0;
    for i in 0..f.tokens().len() {
        if matches!(f.tok(i), ";" | "{" | "}") {
            if i > start {
                out.push(Chunk { start, end: i });
            }
            start = i + 1;
        }
    }
    if f.tokens().len() > start {
        out.push(Chunk {
            start,
            end: f.tokens().len(),
        });
    }
    out
}

fn is_str(f: &SourceFile, i: usize) -> bool {
    f.tokens().get(i).is_some_and(|t| t.kind == TokKind::Str)
}

fn is_ident(f: &SourceFile, i: usize) -> bool {
    f.tokens().get(i).is_some_and(|t| t.kind == TokKind::Ident)
}

/// `const NAME: &str = "...";` / `static NAME: &str = "...";` items of non-test code:
/// (name, literal value).
fn named_file_literals(f: &SourceFile) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let n = f.tokens().len();
    for i in 0..n {
        if f.is_test(i) || !matches!(f.tok(i), "const" | "static") {
            continue;
        }
        let name = i + 1;
        if !is_ident(f, name) || f.tok(name + 1) != ":" {
            continue;
        }
        // The type must mention `str`, and the value must be one string literal.
        let Some(eq) = (name + 2..n.min(name + 12)).find(|j| matches!(f.tok(*j), "=" | ";")) else {
            continue;
        };
        if f.tok(eq) != "=" || !(name + 2..eq).any(|j| f.tok(j) == "str") {
            continue;
        }
        if is_str(f, eq + 1) && f.tok(eq + 2) == ";" {
            out.push((
                f.tok(name).to_string(),
                str_value(f.tok(eq + 1)).to_string(),
            ));
        }
    }
    out
}

/// A local variable seen so far.
struct Bind {
    name: String,
    block: usize,
    /// Holds a path under a data/config/state dir.
    tainted: bool,
    /// String literals of the expressions it was bound from.
    lits: Vec<String>,
}

fn has_state_ext(v: &str) -> bool {
    STATE_EXTENSIONS.iter().any(|e| v.ends_with(e))
}

/// Findings in one file (no workspace table, the fixed dir names).
pub fn scan_file(f: &SourceFile, allow: &AllowList) -> Vec<Violation> {
    let sources: Vec<String> = DIR_NAMES.iter().map(|s| (*s).to_string()).collect();
    scan_file_with(f, allow, &Globals::default(), &sources)
}

/// Findings in one file.
pub fn scan_file_with(
    f: &SourceFile,
    allow: &AllowList,
    globals: &Globals,
    sources: &[String],
) -> Vec<Violation> {
    if f.test_only {
        return Vec::new();
    }
    let exempt: Vec<&str> = allow.tagged("literal");
    let named = named_file_literals(f);
    let r = Resolver::new(f, globals);
    let sinks = fs_writes(f, &r, true);
    let mut out = Vec::new();
    // Block structure: every `{` opens a block with a parent; `block_at[i]` is the innermost
    // block containing token `i`. A binding is visible only inside the block that declared it.
    let mut parent: Vec<usize> = vec![0];
    let mut stack: Vec<usize> = vec![0];
    let mut block_at = vec![0usize; f.tokens().len() + 1];
    for (i, slot) in block_at.iter_mut().enumerate().take(f.tokens().len()) {
        match f.tok(i) {
            "{" => {
                parent.push(stack.last().copied().unwrap_or(0));
                stack.push(parent.len() - 1);
            }
            "}" if stack.len() > 1 => {
                stack.pop();
            }
            _ => {}
        }
        *slot = stack.last().copied().unwrap_or(0);
    }
    let visible = |declared: usize, here: usize| {
        let mut b = here;
        loop {
            if b == declared {
                return true;
            }
            if b == 0 {
                return false;
            }
            b = parent[b];
        }
    };
    let mut binds: Vec<Bind> = Vec::new();
    for c in chunks(f) {
        let here = block_at[c.start];
        if (c.start..c.end).all(|i| f.is_test(i)) {
            continue;
        }
        let latest = |t: &str, binds: &[Bind]| {
            binds
                .iter()
                .rposition(|b| b.name == t && visible(b.block, here))
        };
        let is_dir = |t: &str, binds: &[Bind]| {
            sources.iter().any(|s| s == t) || latest(t, binds).is_some_and(|k| binds[k].tainted)
        };
        // `let name = <expr>`, `name = <expr>` and `name.push(<expr>)`: only the expression
        // counts; the name is bound or updated afterwards.
        let mut scan_from = c.start;
        let mut binding: Option<(String, bool)> = None; // (name, is a new `let`)
        if f.tok(c.start) == "let" {
            let mut k = c.start + 1;
            if f.tok(k) == "mut" {
                k += 1;
            }
            let eq = (k..c.end).find(|j| f.tok(*j) == "=");
            if let (Some(eq), true) = (eq, is_ident(f, k)) {
                scan_from = eq + 1;
                binding = Some((f.tok(k).to_string(), true));
            }
        } else if is_ident(f, c.start) && f.tok(c.start + 1) == "=" && f.tok(c.start + 2) != "=" {
            scan_from = c.start + 2;
            binding = Some((f.tok(c.start).to_string(), false));
        } else if is_ident(f, c.start)
            && f.tok(c.start + 1) == "."
            && MUTATORS.contains(&f.tok(c.start + 2))
            && f.tok(c.start + 3) == "("
        {
            scan_from = c.start + 3;
            binding = Some((f.tok(c.start).to_string(), false));
        }
        let has_source = (scan_from..c.end).any(|i| is_dir(f.tok(i), &binds));
        let mut lits: Vec<String> = (scan_from..c.end)
            .filter(|i| is_str(f, *i))
            .map(|i| str_value(f.tok(i)).to_string())
            .collect();
        // Literals of tainted or literal-holding variables used in the expression.
        for i in scan_from..c.end {
            if let Some(k) = latest(f.tok(i), &binds) {
                lits.extend(binds[k].lits.iter().cloned());
            }
        }
        if let Some((name, is_let)) = binding {
            let existing = (!is_let).then(|| latest(&name, &binds)).flatten();
            match existing {
                Some(k) => {
                    binds[k].tainted |= has_source;
                    binds[k].lits.extend(lits.iter().cloned());
                }
                None => binds.push(Bind {
                    name,
                    block: here,
                    tainted: has_source,
                    lits: lits.clone(),
                }),
            }
        }
        let mentions_dir = (c.start..c.end).any(|i| is_dir(f.tok(i), &binds));
        // The literals the whole statement can see: its own and those of the variables in it.
        let mut stmt_lits: Vec<String> = (c.start..c.end)
            .filter(|i| is_str(f, *i))
            .map(|i| str_value(f.tok(i)).to_string())
            .collect();
        for i in c.start..c.end {
            if let Some(k) = latest(f.tok(i), &binds) {
                stmt_lits.extend(binds[k].lits.iter().cloned());
            }
        }
        let exempted = stmt_lits.iter().find(|l| exempt.contains(&l.as_str()));
        let mut found_join = false;
        for i in c.start..c.end {
            if f.is_test(i) || !f.seq(i, &[".", "join", "("]) {
                continue;
            }
            let close = f.close_of(i + 2);
            let mut candidates: Vec<String> = Vec::new();
            for k in i + 3..close {
                if is_str(f, k) {
                    candidates.push(str_value(f.tok(k)).to_string());
                } else if let Some((_, v)) = named
                    .iter()
                    .find(|(n, _)| n == f.tok(k) && f.tok(k + 1) != "(")
                {
                    candidates.push(v.clone());
                } else if let Some(b) = latest(f.tok(k), &binds) {
                    candidates.extend(binds[b].lits.iter().cloned());
                }
            }
            let Some(lit) = candidates.iter().find(|v| has_state_ext(v)).cloned() else {
                continue;
            };
            if exempt.contains(&lit.as_str()) {
                allow.mark_tagged("literal", &lit);
                continue;
            }
            let per_key = (i + 3..close).any(|k| f.seq(k, &["format", "!"])) && lit.contains('{');
            if mentions_dir || per_key {
                found_join = true;
                let what = if mentions_dir {
                    "path under a data/config/state dir"
                } else {
                    "per-key state file"
                };
                out.push(Violation::new(
                    RULE,
                    &f.path,
                    f.line(i + 1),
                    format!("{what} joined with \"{lit}\"; persist through lector_app::store (ADR 0004)"),
                ));
            }
        }
        // File-creating calls.
        let chunk_sinks: Vec<_> = sinks
            .iter()
            .filter(|h| h.tok >= c.start && h.tok < c.end && !f.is_test(h.tok))
            .filter(|h| {
                h.what != "OpenOptions"
                    || (c.start..c.end).any(|i| {
                        matches!(
                            f.tok(i),
                            "write" | "append" | "create" | "create_new" | "truncate"
                        )
                    })
            })
            .collect();
        if let (false, Some(h)) = (found_join, chunk_sinks.first()) {
            let state_literal = stmt_lits.iter().any(|l| has_state_ext(l));
            if mentions_dir || state_literal {
                if let Some(l) = exempted {
                    allow.mark_tagged("literal", l);
                } else {
                    let why = if mentions_dir {
                        "a path under a data/config/state dir"
                    } else {
                        "a path with a state-file name"
                    };
                    out.push(Violation::new(
                        RULE,
                        &f.path,
                        f.line(h.tok),
                        format!("`{}` creates a file at {why}; persist through lector_app::store (ADR 0004)", h.what),
                    ));
                }
            }
        }
    }
    if !out.is_empty() && allow.exempt_path(&f.path) {
        out.clear();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::file;

    fn run(path: &str, src: &str) -> Vec<Violation> {
        let allow = AllowList::from_entries([
            "crates/lector-app/src/db/**",
            "crates/lector-config/src/paths.rs",
            "literal:settings.toml",
            "literal:keymap.toml",
        ]);
        scan_file(&file(path, src), &allow)
    }

    #[test]
    fn direct_join_on_a_dir_call() {
        let src =
            "fn p() -> PathBuf {\n    lector_config::paths::data_dir().join(\"notices.json\")\n}";
        let v = run("crates/lector-ui-agent/src/notices.rs", src);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].line, 2);
    }

    #[test]
    fn multi_line_chain_and_format_literal() {
        let src = "fn p() {\n    let f = config::data_dir()\n        .join(\"terminal\")\n        .join(format!(\"{:016x}.json\", h));\n}";
        let v = run("crates/lector-ui-terminal/src/s.rs", src);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].line, 4);
    }

    #[test]
    fn variables_holding_a_dir_are_followed_within_their_block() {
        let src = "fn a() {\n    let dir = data_dir();\n    let p = dir.join(\"recent.json\");\n    let q = p.join(\"x\");\n}\nfn b(dir: &Path) {\n    let r = dir.join(\"recent.json\");\n}";
        let v = run("crates/lector-config/src/recent.rs", src);
        assert_eq!(v.iter().map(|v| v.line).collect::<Vec<_>>(), [3]);
    }

    #[test]
    fn taint_does_not_leak_into_sibling_blocks() {
        let src = "fn paths(project: &Path) {\n    { let dir = data_dir(); }\n    { let dir = project; let path = dir.join(\"manifest.toml\"); }\n}";
        assert!(run("crates/x/src/a.rs", src).is_empty());
        let same = "fn p() {\n    { let dir = data_dir(); let a = dir.join(\"a.json\"); }\n}";
        assert_eq!(run("crates/x/src/a.rs", same).len(), 1);
    }

    #[test]
    fn shadowing_clears_taint_for_its_scope_only() {
        let src = "fn p(project: &Path) {\n    let dir = data_dir();\n    {\n        let dir = project;\n        dir.join(\"a.json\");\n    }\n    dir.join(\"b.json\");\n    let dir = project;\n    dir.join(\"c.json\");\n}";
        let v = run("crates/x/src/a.rs", src);
        assert_eq!(v.iter().map(|v| v.line).collect::<Vec<_>>(), [7]);
    }

    #[test]
    fn named_params_and_fields_count_as_dirs() {
        let src = "fn a(data_dir: &Path) { std::fs::read(data_dir.join(\"review.json\")); self.config_dir.join(\"x.db\"); }";
        assert_eq!(run("crates/x/src/a.rs", src).len(), 2);
    }

    #[test]
    fn literals_without_a_dir_are_not_flagged() {
        let src = "fn a(tmp: &Path) { tmp.join(\"x.json\"); project.join(\".lector\").join(\"settings.toml\"); let n = data_dir().join(\"logs\"); }";
        assert!(run("crates/x/src/a.rs", src).is_empty());
    }

    #[test]
    fn exempt_literals_allowed_modules_and_tests() {
        let src =
            "fn a() { config_dir().join(\"settings.toml\"); config_dir().join(\"keymap.toml\"); }";
        assert!(run("crates/lector-ui/src/settings.rs", src).is_empty());
        let src = "fn a() { data_dir().join(\"lector.redb\"); }";
        assert!(run("crates/lector-app/src/db/mod.rs", src).is_empty());
        assert!(run("crates/lector-config/src/paths.rs", src).is_empty());
        assert!(run("crates/lector-app/src/core.rs", src).len() == 1);
        let tests = "#[cfg(test)]\nmod t { fn a() { data_dir().join(\"x.json\"); } }";
        assert!(run("crates/x/src/a.rs", tests).is_empty());
        assert!(run(
            "crates/x/tests/a.rs",
            "fn a() { data_dir().join(\"x.json\"); }"
        )
        .is_empty());
    }

    #[test]
    fn per_key_json_files_are_flagged_with_any_receiver() {
        let src = "fn p(&self, key: &str) -> PathBuf {\n    self.dir.join(format!(\"{}.json\", key))\n}\nfn q(d: &Path) { d.join(format!(\"{}.txt\", 1)); d.join(\"plain.json\"); }";
        let v = run("crates/lector-config/src/session.rs", src);
        assert_eq!(v.iter().map(|v| v.line).collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn json_writes_with_a_literal_path() {
        let src = "fn a() {\n    std::fs::write(\"out.json\", data).ok();\n    let f = File::create(root.join(\"report.json\"));\n    fs::write(path, bytes);\n    fs::write(data_dir().join(\"c.json\"), b);\n}";
        let v = run("crates/x/src/a.rs", src);
        assert_eq!(v.iter().map(|v| v.line).collect::<Vec<_>>(), [2, 3, 5]);
    }

    #[test]
    fn file_names_held_in_consts_and_statics_count_as_literals() {
        let src = "const STATE_FILE: &str = \"notices.json\";\nstatic DB: &'static str = \"x.redb\";\nconst SETTINGS: &str = \"settings.toml\";\nconst DIR: &str = \"logs\";\nfn p() {\n    data_dir().join(STATE_FILE);\n    config_dir().join(DB);\n    config_dir().join(SETTINGS);\n    data_dir().join(DIR);\n    project.join(STATE_FILE);\n}";
        let v = run("crates/x/src/a.rs", src);
        assert_eq!(v.iter().map(|v| v.line).collect::<Vec<_>>(), [6, 7]);
        assert!(v[0].message.contains("notices.json"), "{}", v[0].message);
        // A const of a test region, or a function call of that name, does not count.
        let tests = "#[cfg(test)]\nconst F: &str = \"a.json\";\nfn p() { data_dir().join(F); data_dir().join(G()); }\nconst G: &str = \"g.json\";";
        assert!(run("crates/x/src/a.rs", tests).is_empty());
        // Not a string const: ignored.
        let other = "const N: usize = 3;\nfn p() { data_dir().join(N); }";
        assert!(run("crates/x/src/a.rs", other).is_empty());
    }

    #[test]
    fn strings_and_comments_do_not_trigger() {
        let src = "// data_dir().join(\"a.json\")\nfn a() { let s = \"data_dir().join(\\\"a.json\\\")\"; }";
        assert!(run("crates/x/src/a.rs", src).is_empty());
    }

    // Bypasses the audit demonstrated: each of these used to pass.

    #[test]
    fn bypass_a_path_built_with_push_and_written() {
        let src = "fn save() {\n    let mut p = data_dir();\n    p.push(\"x.json\");\n    std::fs::write(&p, data).ok();\n}";
        let v = run("crates/x/src/a.rs", src);
        assert_eq!(v.iter().map(|v| v.line).collect::<Vec<_>>(), [4], "{v:?}");
    }

    #[test]
    fn bypass_per_key_file_with_a_bin_extension() {
        let src = "fn p(n: u32) -> PathBuf { data_dir().join(format!(\"{}-state.bin\", n)) }";
        assert_eq!(run("crates/x/src/a.rs", src).len(), 1);
        let ron = "fn p() -> PathBuf { config_dir().join(\"layout.ron\") }";
        assert_eq!(run("crates/x/src/a.rs", ron).len(), 1);
    }

    #[test]
    fn bypass_a_file_name_formatted_in_an_earlier_statement() {
        let src = "fn p(n: u32) {\n    let name = format!(\"{}-state.bin\", n);\n    let q = data_dir().join(name);\n    let r = data_dir().join(&name);\n}";
        let v = run("crates/x/src/a.rs", src);
        assert_eq!(
            v.iter().map(|v| v.line).collect::<Vec<_>>(),
            [3, 4],
            "{v:?}"
        );
    }

    #[test]
    fn bypass_any_extension_counts_when_a_file_is_created_under_a_dir() {
        let src = "fn a() {\n    let p = data_dir().join(\"cache\");\n    std::fs::write(&p, b);\n    let q = p.with_extension(\"anything\");\n    File::create(q);\n    std::fs::create_dir_all(config_dir().join(\"x\"));\n    let mut o = OpenOptions::new();\n    o.append(true);\n}";
        let v = run("crates/x/src/a.rs", src);
        assert_eq!(
            v.iter().map(|v| v.line).collect::<Vec<_>>(),
            [3, 5, 6],
            "{v:?}"
        );
    }

    #[test]
    fn taint_follows_assignments_and_mutations_and_aliased_sinks() {
        let src = "use std::fs as f;\nfn a(project: &Path) {\n    let mut p = project.to_path_buf();\n    p = data_dir();\n    p.set_file_name(\"z\");\n    f::write(&p, b);\n}\nfn b(project: &Path) {\n    let mut p = project.to_path_buf();\n    p.push(data_dir());\n    f::write(p, b);\n}";
        let v = run("crates/x/src/a.rs", src);
        assert_eq!(
            v.iter().map(|v| v.line).collect::<Vec<_>>(),
            [6, 11],
            "{v:?}"
        );
    }

    #[test]
    fn writes_elsewhere_are_not_state() {
        let src = "fn a(project: &Path, dir: &Path) {\n    let p = project.join(\"out\");\n    std::fs::write(p, b);\n    std::fs::write(dir.join(\"x\"), b);\n    let q = data_dir();\n    let q = project;\n    std::fs::write(q, b);\n    let t = data_dir().join(\"logs\");\n    let n = t.exists();\n}";
        assert!(
            run("crates/x/src/a.rs", src).is_empty(),
            "{:?}",
            run("crates/x/src/a.rs", src)
        );
    }

    #[test]
    fn exempt_literals_stay_exempt_for_writes_and_are_marked_used() {
        let allow = AllowList::from_entries(["literal:settings.toml"]);
        let src = "fn a() { let p = config_dir().join(\"settings.toml\"); std::fs::write(&p, b); }";
        assert!(scan_file(&file("crates/x/src/a.rs", src), &allow).is_empty());
        assert!(allow.unused().is_empty());
        let other = AllowList::from_entries(["literal:keymap.toml"]);
        assert_eq!(scan_file(&file("crates/x/src/a.rs", src), &other).len(), 2);
        assert_eq!(other.unused().len(), 1);
    }

    #[test]
    fn file_allowances_are_narrow_and_stale_ones_fail() {
        let files = [
            file(
                "crates/lector-app/src/store/db.rs",
                "fn p() { data_dir().join(\"lector.redb\"); }",
            ),
            file(
                "crates/lector-app/src/store/new_file.rs",
                "fn p() { data_dir().join(\"x.bin\"); }",
            ),
            file("crates/lector-app/src/health/gone.rs", "fn p() {}"),
        ];
        let mut cfg = Config::default();
        cfg.set_allow(
            "state",
            AllowList::from_entries([
                "crates/lector-app/src/store/db.rs",
                "crates/lector-app/src/health/gone.rs",
            ]),
        );
        let out = check(&files, &cfg);
        let msgs: Vec<String> = out
            .violations
            .iter()
            .map(|v| format!("{}: {}", v.file, v.message))
            .collect();
        assert_eq!(out.violations.len(), 2, "{msgs:?}");
        assert!(msgs.iter().any(|m| m.contains("store/new_file.rs")));
        assert!(msgs
            .iter()
            .any(|m| m.contains("stale allowance") && m.contains("gone.rs")));
    }

    #[test]
    fn sources_are_derived_from_lector_config_paths() {
        let paths = file(
            "crates/lector-config/src/paths.rs",
            "pub fn config_dir() -> PathBuf { x }\npub fn cache_dir() -> PathBuf { y }\npub fn settings_file() -> PathBuf { config_dir().join(\"settings.toml\") }\npub fn portable_root() -> Option<&'static Path> { None }\nfn private_dir() -> PathBuf { z }",
        );
        let allow = AllowList::from_entries(["literal:settings.toml"]);
        let sources = derive_sources(&[paths], &allow);
        assert!(sources.contains(&"cache_dir".to_string()));
        assert!(!sources.contains(&"settings_file".to_string()));
        assert!(!sources.contains(&"portable_root".to_string()));
        assert!(!sources.contains(&"private_dir".to_string()));
    }
}
