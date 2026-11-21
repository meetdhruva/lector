//! `state` (ADR 0004): Lector-owned state lives in the database, not in ad-hoc files.
//!
//! **Heuristic.** In non-test code outside the modules listed in `arch/state.allow` the rule
//! looks at statements (token runs between `;`, `{` and `}`) and reports:
//!
//! 1. a `.join(...)` whose argument contains a string literal ending in `.json`, `.redb`,
//!    `.db` or `.toml`, in a statement that also mentions `data_dir`, `state_dir` or
//!    `config_dir` (a call, a field, a parameter of that name) or a local variable that was
//!    bound from such an expression earlier in the same block;
//! 2. a `.join(format!("...{}.json", ..))` (a file named per key, the shape of a hand-rolled
//!    state store) with any receiver;
//! 3. otherwise, `fs::write`, `File::create`, `File::create_new` or `OpenOptions` in a
//!    statement that mentions a string literal ending in `.json`.
//!
//! In (1), a name declared in the same file as `const NAME: &str = "x.json";` (or `static`)
//! counts as its literal, so `data_dir().join(STATE_FILE)` is caught too.
//!
//! File names listed as `literal:<name>` in the allow file (`settings.toml`, `keymap.toml`:
//! user-edited files, ADR 0004 decision 2) are exempt from (1).
//!
//! What it cannot see: a file name held in a `const` of another file, or built by `format!`
//! from a variable;
//! a path received as a function parameter with another name; a path joined across two
//! statements through a field; persistence through a library that picks its own path;
//! writes of files with other extensions. Expect misses rather than false positives.

use crate::config::{AllowList, Config};
use crate::lexer::TokKind;
use crate::report::{RuleOutput, Unit, Violation};
use crate::rules::str_value;
use crate::source::SourceFile;

/// Rule name.
pub const RULE: &str = "state";

const DIR_NAMES: &[&str] = &["data_dir", "state_dir", "config_dir"];
const STATE_EXTENSIONS: &[&str] = &[".json", ".redb", ".db", ".toml"];

/// Run the rule over all files.
pub fn check(files: &[SourceFile], cfg: &Config) -> RuleOutput {
    let allow = cfg.allow("state");
    let found = files.iter().flat_map(|f| scan_file(f, &allow)).collect();
    RuleOutput::new(RULE, found, Unit::Count)
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
        if !f
            .tokens()
            .get(name)
            .is_some_and(|t| t.kind == TokKind::Ident)
            || f.tok(name + 1) != ":"
        {
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

/// Findings in one file.
pub fn scan_file(f: &SourceFile, allow: &AllowList) -> Vec<Violation> {
    if f.test_only || allow.matches_path(&f.path) {
        return Vec::new();
    }
    let exempt: Vec<&str> = allow.tagged("literal");
    let named = named_file_literals(f);
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
    // `let` bindings seen so far: (name, declaring block, holds a dir). The latest visible one
    // wins, so `let dir = <untainted>` shadows an earlier tainted `dir`.
    let mut bindings: Vec<(String, usize, bool)> = Vec::new();
    for c in chunks(f) {
        let here = block_at[c.start];
        if (c.start..c.end).all(|i| f.is_test(i)) {
            continue;
        }
        let is_dir = |t: &str, bindings: &[(String, usize, bool)]| {
            DIR_NAMES.contains(&t)
                || bindings
                    .iter()
                    .rev()
                    .find(|(n, blk, _)| n == t && visible(*blk, here))
                    .is_some_and(|(_, _, held)| *held)
        };
        // For `let name = <expr>` only the expression counts; the new name is bound afterwards.
        let mut scan_from = c.start;
        let mut binding: Option<String> = None;
        if f.tok(c.start) == "let" {
            let mut k = c.start + 1;
            if f.tok(k) == "mut" {
                k += 1;
            }
            let eq = (k..c.end).find(|j| f.tok(*j) == "=");
            if let (Some(eq), true) = (
                eq,
                f.tokens().get(k).is_some_and(|t| t.kind == TokKind::Ident),
            ) {
                scan_from = eq + 1;
                binding = Some(f.tok(k).to_string());
            }
        }
        let has_source = (scan_from..c.end).any(|i| is_dir(f.tok(i), &bindings));
        if let Some(name) = binding {
            bindings.push((name, here, has_source));
        }
        let mut found_join = false;
        for i in c.start..c.end {
            if f.is_test(i) || !f.seq(i, &[".", "join", "("]) {
                continue;
            }
            let close = f.close_of(i + 2);
            let literal = (i + 3..close).find_map(|k| {
                let v = if is_str(f, k) {
                    str_value(f.tok(k))
                } else {
                    named
                        .iter()
                        .find(|(n, _)| n == f.tok(k) && f.tok(k + 1) != "(")
                        .map(|(_, v)| v.as_str())?
                };
                (STATE_EXTENSIONS.iter().any(|e| v.ends_with(e)) && !exempt.contains(&v))
                    .then(|| v.to_string())
            });
            let Some(lit) = literal else { continue };
            let per_key = (i + 3..close).any(|k| f.seq(k, &["format", "!"])) && lit.contains('{');
            if has_source || per_key {
                found_join = true;
                let what = if has_source {
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
        if !found_join {
            let json_literal =
                (c.start..c.end).any(|i| is_str(f, i) && str_value(f.tok(i)).ends_with(".json"));
            if json_literal {
                for i in c.start..c.end {
                    if f.is_test(i) {
                        continue;
                    }
                    let sink = f.seq(i, &["fs", "::", "write"])
                        || f.seq(i, &["File", "::", "create"])
                        || f.seq(i, &["File", "::", "create_new"])
                        || (f.tok(i) == "OpenOptions" && f.tok(i + 1) == "::");
                    if sink {
                        out.push(Violation::new(
                            RULE,
                            &f.path,
                            f.line(i),
                            "file write whose path is a `*.json` literal; persist through lector_app::store (ADR 0004)",
                        ));
                        break;
                    }
                }
            }
        }
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
}
