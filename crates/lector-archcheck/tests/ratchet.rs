//! End-to-end behaviour of the ratchet against a tiny fixture workspace in a temp directory:
//! two crates (`lector-review` below, `lector-ui-x` above) with one violation per rule.

use std::fs;
use std::path::Path;

use lector_archcheck::baseline::Baseline;

fn arch(root: &Path, args: &[&str]) -> (i32, String) {
    let mut argv: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    argv.push("--root".into());
    argv.push(root.to_string_lossy().into_owned());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = lector_archcheck::run(&argv, &mut out, &mut err);
    let mut text = String::from_utf8_lossy(&out).into_owned();
    text.push_str(&String::from_utf8_lossy(&err));
    (code, text)
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).expect("create dir");
    }
    fs::write(path, text).expect("write file");
}

const LAYERS: &str = r#"
[[layer]]
name = "low"
crates = ["lector-review"]
[[layer]]
name = "high"
crates = ["lector-ui-x"]

[[forbid]]
from = "lector-ui-x"
to = "lector-review"
adr = "0000"
reason = "fixture rule"

[closure]
gui_crates = ["lector-ui-x"]
forbidden = ["gpui*"]
"#;

const REVIEW_LIB: &str = "\
pub struct CancelToken;
static mut COUNTER: u32 = 0;
pub fn spawn_it() {
    std::thread::spawn(|| {});
}
pub fn state_path() -> std::path::PathBuf {
    data_dir().join(\"review.json\")
}
pub fn risky(x: Option<u8>) -> u8 {
    x.unwrap()
}
";

const UI_LIB: &str = "\
pub struct CancelToken;
pub fn open() {
    let _ = std::process::Command::new(\"git\");
}
pub fn bg(cx: &Cx) {
    cx.background_spawn(async {});
}
";

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let r = dir.path();
    write(
        r,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n",
    );
    write(
        r,
        "crates/lector-review/Cargo.toml",
        "[package]\nname = \"lector-review\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    );
    write(r, "crates/lector-review/src/lib.rs", REVIEW_LIB);
    write(
        r,
        "crates/lector-ui-x/Cargo.toml",
        "[package]\nname = \"lector-ui-x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nlector-review = { path = \"../lector-review\" }\n",
    );
    write(r, "crates/lector-ui-x/src/lib.rs", UI_LIB);
    write(
        r,
        "crates/lector-review/src/big.rs",
        &"pub fn f() {}\n".repeat(1501),
    );
    write(r, "arch/layers.toml", LAYERS);
    write(
        r,
        "docs/ARCHITECTURE.md",
        "# Fixture\n\n## Principles\n\n1. one\n\n## Next\n\ntext\n",
    );
    dir
}

fn baseline_of(root: &Path) -> Baseline {
    Baseline::load(root)
        .expect("load")
        .expect("baseline exists")
}

/// Create the baseline the way the initial commit does.
fn baselined() -> tempfile::TempDir {
    let dir = fixture();
    let r = dir.path();
    let (code, out) = arch(r, &["graph", "--write"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = arch(r, &["--update", "--allow-increase", "initial"]);
    assert_eq!(code, 0, "{out}");
    dir
}

#[test]
fn every_rule_finds_its_violation_and_the_baseline_records_it() {
    let dir = baselined();
    let b = baseline_of(dir.path());
    for rule in [
        "deps",
        "writes",
        "threads",
        "background",
        "state",
        "dupes",
        "size",
        "purity",
        "hygiene",
    ] {
        assert!(
            b.rules.get(rule).is_some_and(|e| !e.is_empty()),
            "{rule} has no baseline entries: {b:?}"
        );
    }
    assert_eq!(b.rules["deps"]["lector-ui-x -> lector-review"], 1);
    assert_eq!(b.rules["size"]["crates/lector-review/src/big.rs"], 1501);
    assert_eq!(b.rules["dupes"].len(), 2);
    assert_eq!(b.rules["state"]["crates/lector-review/src/lib.rs"], 1);
    assert_eq!(b.rules["hygiene"]["crates/lector-review/src/lib.rs"], 1);
    // Both lib.rs files lack forbid(unsafe_code).
    assert_eq!(b.rules["hygiene"].len(), 3);
}

#[test]
fn without_a_baseline_everything_is_new() {
    let dir = fixture();
    let (code, out) = arch(dir.path(), &["threads"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("FAIL"));
    assert!(
        out.contains("threads  crates/lector-review/src/lib.rs:4"),
        "{out}"
    );
}

#[test]
fn matching_baseline_passes_and_output_has_a_table() {
    let dir = baselined();
    let (code, out) = arch(dir.path(), &["all"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("rule"));
    assert!(out.contains("baseline"));
    for rule in [
        "deps",
        "writes",
        "threads",
        "background",
        "state",
        "dupes",
        "size",
        "purity",
        "hygiene",
        "graph",
    ] {
        assert!(
            out.lines()
                .any(|l| l.starts_with(rule) && l.contains(" ok ")),
            "{rule}\n{out}"
        );
    }
    let (code, verbose) = arch(dir.path(), &["all", "--verbose"]);
    assert_eq!(code, 0);
    assert!(
        verbose.contains("threads  crates/lector-review/src/lib.rs:4  "),
        "{verbose}"
    );
}

#[test]
fn a_new_violation_in_a_new_file_fails() {
    let dir = baselined();
    write(
        dir.path(),
        "crates/lector-ui-x/src/more.rs",
        "pub fn g() { std::thread::spawn(|| {}); }\n",
    );
    let (code, out) = arch(dir.path(), &["threads"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("crates/lector-ui-x/src/more.rs: 0 -> 1"),
        "{out}"
    );
    let (code, out) = arch(dir.path(), &["all"]);
    assert_eq!(code, 1, "{out}");
}

#[test]
fn a_higher_count_in_a_baselined_file_fails() {
    let dir = baselined();
    let lib = format!("{REVIEW_LIB}pub fn again() {{ std::thread::spawn(|| {{}}); }}\n");
    write(dir.path(), "crates/lector-review/src/lib.rs", &lib);
    let (code, out) = arch(dir.path(), &["threads"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("crates/lector-review/src/lib.rs: 1 -> 2"),
        "{out}"
    );
}

#[test]
fn a_file_that_grows_past_its_recorded_size_fails() {
    let dir = baselined();
    write(
        dir.path(),
        "crates/lector-review/src/big.rs",
        &"pub fn f() {}\n".repeat(1502),
    );
    let (code, out) = arch(dir.path(), &["size"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("1501 -> 1502"), "{out}");
}

#[test]
fn a_new_edge_against_the_layering_fails() {
    let dir = baselined();
    // Reverse the dependency: the baselined edge disappears, a new upward edge appears.
    write(
        dir.path(),
        "crates/lector-ui-x/Cargo.toml",
        "[package]\nname = \"lector-ui-x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        dir.path(),
        "crates/lector-review/Cargo.toml",
        "[package]\nname = \"lector-review\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nlector-ui-x = { path = \"../lector-ui-x\" }\n",
    );
    let (code, out) = arch(dir.path(), &["deps"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("lector-review -> lector-ui-x: 0 -> 1"),
        "{out}"
    );
    assert!(out.contains("higher layer"), "{out}");
    assert!(out.contains("FAIL"), "{out}");
}

#[test]
fn a_count_that_fell_demands_a_lower_baseline() {
    let dir = baselined();
    // Fix the `unwrap` in lector-review.
    let fixed = REVIEW_LIB.replace("x.unwrap()", "x.unwrap_or(0)");
    write(dir.path(), "crates/lector-review/src/lib.rs", &fixed);
    let (code, out) = arch(dir.path(), &["all"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("baseline is stale: run arch-check --update"),
        "{out}"
    );
    assert!(out.contains("STALE"), "{out}");
    // `--update` only lowers, so it is accepted without --allow-increase.
    let (code, out) = arch(dir.path(), &["--update"]);
    assert_eq!(code, 0, "{out}");
    assert!(!out.contains("ALLOWED INCREASE"));
    assert!(
        !baseline_of(dir.path()).rules["hygiene"].contains_key("crates/lector-review/src/lib.rs")
    );
    let (code, out) = arch(dir.path(), &["all"]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn update_refuses_to_raise_anything() {
    let dir = baselined();
    let before = fs::read_to_string(dir.path().join("arch/baseline.toml")).expect("read");
    write(
        dir.path(),
        "crates/lector-ui-x/src/more.rs",
        "pub fn g() { std::thread::spawn(|| {}); }\n",
    );
    let (code, out) = arch(dir.path(), &["--update"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("refusing to raise the baseline"), "{out}");
    assert!(out.contains("crates/lector-ui-x/src/more.rs"), "{out}");
    let after = fs::read_to_string(dir.path().join("arch/baseline.toml")).expect("read");
    assert_eq!(
        before, after,
        "the baseline must not change when --update refuses"
    );
    // With a reason it is allowed and says so.
    let (code, out) = arch(dir.path(), &["--update", "--allow-increase", "test reason"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("ALLOWED INCREASE (reason: test reason)"),
        "{out}"
    );
}

#[test]
fn update_of_a_single_rule_leaves_other_sections_alone() {
    let dir = baselined();
    let fixed = REVIEW_LIB.replace("x.unwrap()", "x.unwrap_or(0)");
    write(dir.path(), "crates/lector-review/src/lib.rs", &fixed);
    let (code, out) = arch(dir.path(), &["threads", "--update"]);
    assert_eq!(code, 0, "{out}");
    // hygiene was not part of that run: still stale.
    let (code, out) = arch(dir.path(), &["hygiene"]);
    assert_eq!(code, 1, "{out}");
}

#[test]
fn crlf_sources_give_the_same_counts() {
    let dir = baselined();
    write(
        dir.path(),
        "crates/lector-review/src/lib.rs",
        &REVIEW_LIB.replace('\n', "\r\n"),
    );
    write(
        dir.path(),
        "crates/lector-review/src/big.rs",
        &"pub fn f() {}\r\n".repeat(1501),
    );
    let (code, out) = arch(dir.path(), &["all"]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn graph_write_check_and_staleness() {
    let dir = fixture();
    let r = dir.path();
    let (code, out) = arch(r, &["graph", "--check"]);
    assert_eq!(code, 1, "{out}");
    let (code, out) = arch(r, &["graph"]);
    assert_eq!(code, 0);
    assert!(
        out.contains("L1 high\n  lector-ui-x   -> lector-review"),
        "{out}"
    );
    assert!(out.contains("L0 low\n  lector-review -> (none)"), "{out}");
    let (code, out) = arch(r, &["graph", "--write"]);
    assert_eq!(code, 0, "{out}");
    let doc = fs::read_to_string(r.join("docs/ARCHITECTURE.md")).expect("read");
    assert!(doc.contains("## Crate graph (generated)"));
    assert!(doc.contains("<!-- arch-graph:begin -->") && doc.contains("<!-- arch-graph:end -->"));
    assert!(doc.contains("## Next\n\ntext\n"));
    assert_eq!(arch(r, &["graph", "--check"]).0, 0);
    // Drop the dependency: the committed block is now stale.
    write(
        r,
        "crates/lector-ui-x/Cargo.toml",
        "[package]\nname = \"lector-ui-x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    let (code, out) = arch(r, &["graph", "--check"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("stale"), "{out}");
    assert_eq!(arch(r, &["graph", "--write"]).0, 0);
    assert_eq!(arch(r, &["graph", "--check"]).0, 0);
}

#[test]
fn usage_errors_exit_with_two() {
    let dir = fixture();
    assert_eq!(arch(dir.path(), &["bogus"]).0, 2);
    assert_eq!(arch(dir.path(), &["size", "--write"]).0, 2);
    assert_eq!(arch(dir.path(), &["--allow-increase", "x"]).0, 2);
    // Malformed configuration is an internal error, not a violation.
    write(dir.path(), "arch/threads.allow", "crates/x/**\n");
    let (code, out) = arch(dir.path(), &["threads"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("no reason"), "{out}");
}

#[test]
fn allowances_silence_a_rule_for_a_file() {
    let dir = baselined();
    write(
        dir.path(),
        "arch/threads.allow",
        "# fixture: this file is the thread owner\ncrates/lector-review/src/lib.rs\n",
    );
    let (code, out) = arch(dir.path(), &["threads"]);
    // The violation disappeared, so the baseline is stale rather than failing.
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("STALE"), "{out}");
}
