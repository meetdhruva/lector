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
//! File names listed as `literal:<name>` in the allow file (`settings.toml`, `keymap.toml`:
//! user-edited files, ADR 0004 decision 2) are exempt from (1).
//!
//! What it cannot see: a file name held in a `const` or built by `format!` from a variable;
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

/// Findings in one file.
pub fn scan_file(f: &SourceFile, allow: &AllowList) -> Vec<Violation> {
    if f.test_only || allow.matches_path(&f.path) {
        return Vec::new();
    }
    let exempt: Vec<&str> = allow.tagged("literal");
    let mut out = Vec::new();
    // Variables bound from a data/config/state dir expression: (name, brace depth at binding).
    let mut tainted: Vec<(String, usize)> = Vec::new();
    let mut depth = 0usize;
    let mut depth_at = vec![0usize; f.tokens().len() + 1];
    for (i, slot) in depth_at.iter_mut().enumerate().take(f.tokens().len()) {
        match f.tok(i) {
            "{" => depth += 1,
            "}" => depth = depth.saturating_sub(1),
            _ => {}
        }
        *slot = depth;
    }
    let mut last_depth = 0usize;
    for c in chunks(f) {
        let d = depth_at[c.start];
        if d < last_depth {
            tainted.retain(|(_, bound_at)| *bound_at <= d);
        }
        last_depth = d;
        if (c.start..c.end).all(|i| f.is_test(i)) {
            continue;
        }
        let has_source = (c.start..c.end).any(|i| {
            let t = f.tok(i);
            DIR_NAMES.contains(&t) || tainted.iter().any(|(n, _)| n == t)
        });
        // `let name = <expr mentioning a source>` taints `name`.
        if f.tok(c.start) == "let" && has_source {
            let mut k = c.start + 1;
            if f.tok(k) == "mut" {
                k += 1;
            }
            let eq = (k..c.end).find(|j| f.tok(*j) == "=");
            if let (Some(eq), true) = (
                eq,
                f.tokens().get(k).is_some_and(|t| t.kind == TokKind::Ident),
            ) {
                let rhs_has_source = (eq + 1..c.end).any(|i| {
                    let t = f.tok(i);
                    DIR_NAMES.contains(&t) || tainted.iter().any(|(n, _)| n == t)
                });
                if rhs_has_source {
                    tainted.push((f.tok(k).to_string(), d));
                }
            }
        }
        let mut found_join = false;
        for i in c.start..c.end {
            if f.is_test(i) || !f.seq(i, &[".", "join", "("]) {
                continue;
            }
            let close = f.close_of(i + 2);
            let literal = (i + 3..close).filter(|k| is_str(f, *k)).find_map(|k| {
                let v = str_value(f.tok(k));
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
    fn strings_and_comments_do_not_trigger() {
        let src = "// data_dir().join(\"a.json\")\nfn a() { let s = \"data_dir().join(\\\"a.json\\\")\"; }";
        assert!(run("crates/x/src/a.rs", src).is_empty());
    }
}
