use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use lector_config::Settings;
use lector_diff::GutterKind;
use lector_search::CancelToken;
use lector_text::Point;
use lector_workspace::navigation::{Location, NavHistory};
use lector_workspace::project::{FileGitState, Project};
use lector_workspace::symbols::{self, TargetSource};
use lector_workspace::{Indentation, LineEnding};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(dir: &Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "t@t"]);
    git(dir, &["config", "user.name", "t"]);
}

fn commit_all(dir: &Path) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "c"]);
}

fn no_lsp() -> Settings {
    let mut s = Settings::default();
    s.lsp.enabled = false;
    s
}

fn paths(p: &Project) -> Vec<String> {
    p.rescan_files(&CancelToken::new())
        .paths
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[test]
fn open_non_repo_and_repo() {
    let t = tempfile::tempdir().unwrap();
    write(t.path(), "a.txt", "x");
    let p = Project::open(t.path(), no_lsp()).unwrap();
    assert!(p.repo().is_none());
    assert!(p.lsp().is_none());
    assert!(p.refresh_git_status().unwrap().is_empty());
    assert!(p.blame("a.txt").is_err());
    init_repo(t.path());
    let p = Project::open(t.path(), Settings::default()).unwrap();
    assert!(p.repo().is_some());
    assert!(p.lsp().is_some());
    assert_eq!(
        p.relative_path(&p.absolute_path("a/b.rs")).as_deref(),
        Some("a/b.rs")
    );
}

#[test]
fn file_list_gitignore_exclude_hidden() {
    let t = tempfile::tempdir().unwrap();
    init_repo(t.path());
    write(t.path(), ".gitignore", "target/\n*.log\n");
    write(t.path(), "src/main.rs", "fn main() {}");
    write(t.path(), "src/b.rs", "");
    write(t.path(), "target/x.o", "");
    write(t.path(), "a.log", "");
    write(t.path(), "node_modules/m.js", "");
    write(t.path(), ".hidden", "");
    let mut s = no_lsp();
    s.files.exclude = vec!["node_modules".into(), ".git".into()];
    let p = Project::open(t.path(), s.clone()).unwrap();
    let list = paths(&p);
    assert_eq!(
        list,
        vec![".gitignore", ".hidden", "src/b.rs", "src/main.rs"]
    );
    assert_eq!(p.files().version, 1);

    s.files.show_hidden = false;
    p.set_settings(s.clone());
    assert_eq!(paths(&p), vec!["src/b.rs", "src/main.rs"]);

    s.files.show_hidden = true;
    s.files.show_ignored = true;
    p.set_settings(s);
    let l = p.rescan_files(&CancelToken::new());
    assert!(l.contains("target/x.o") && l.contains("a.log"));
    assert!(!l.contains("node_modules/m.js"));
    assert!(l.is_ignored("a.log") && !l.is_ignored("src/b.rs"));
    assert_eq!(p.file_state("a.log"), Some(FileGitState::Ignored));
}

#[test]
fn git_status_rollups() {
    let t = tempfile::tempdir().unwrap();
    init_repo(t.path());
    write(t.path(), "a/b/mod.rs", "1\n");
    write(t.path(), "a/b/del.rs", "1\n");
    write(t.path(), "a/c/keep.rs", "1\n");
    write(t.path(), "top.rs", "1\n");
    commit_all(t.path());
    write(t.path(), "a/b/mod.rs", "2\n");
    fs::remove_file(t.path().join("a/b/del.rs")).unwrap();
    write(t.path(), "a/c/new.rs", "n\n");
    git(t.path(), &["add", "a/c/new.rs"]);
    write(t.path(), "u/deep/x.rs", "u\n");
    let p = Project::open(t.path(), no_lsp()).unwrap();
    let st = p.refresh_git_status().unwrap();
    assert_eq!(st.get("a/b/mod.rs").unwrap().state, FileGitState::Modified);
    assert_eq!(st.get("a/b/del.rs").unwrap().state, FileGitState::Deleted);
    let new = st.get("a/c/new.rs").unwrap();
    assert_eq!((new.state, new.staged), (FileGitState::Added, true));
    assert_eq!(
        st.get("u/deep/x.rs").unwrap().state,
        FileGitState::Untracked
    );
    assert_eq!(st.dir_state("a"), Some(FileGitState::Modified));
    assert_eq!(st.dir_state("a/b"), Some(FileGitState::Modified));
    assert_eq!(st.dir_state("a/c"), Some(FileGitState::Added));
    assert_eq!(st.dir_state("u"), Some(FileGitState::Untracked));
    assert_eq!(st.dir_state("u/deep"), Some(FileGitState::Untracked));
    assert_eq!(st.dir_state("zzz"), None);
    assert!(st.get("top.rs").is_none());
    assert_eq!(p.git_status().len(), st.len());
}

#[test]
fn subfolder_project_maps_paths() {
    let t = tempfile::tempdir().unwrap();
    init_repo(t.path());
    write(t.path(), "sub/f.rs", "fn a() {}\n");
    write(t.path(), "other.rs", "");
    commit_all(t.path());
    write(t.path(), "sub/f.rs", "fn a() {}\nfn b() {}\n");
    let p = Project::open(&t.path().join("sub"), no_lsp()).unwrap();
    let st = p.refresh_git_status().unwrap();
    assert_eq!(st.get("f.rs").unwrap().state, FileGitState::Modified);
    let d = p.load_document(Path::new("f.rs")).unwrap();
    assert_eq!(d.rel_path(), Some("f.rs"));
    let marks = d.diff_gutter(&p);
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].kind, GutterKind::Added);
    assert!(!p.blame("f.rs").unwrap().is_empty());
}

const RUST_SRC: &str = "\
// comment
use std::fmt;

pub struct Foo {
    x: u32,
}

impl Foo {
    pub fn new() -> Self {
        let s = \"hello\";
        Foo { x: 1 }
    }

    fn other(&self) -> u32 {
        self.x
    }
}
";

#[test]
fn document_rust() {
    let t = tempfile::tempdir().unwrap();
    write(t.path(), "lib.rs", RUST_SRC);
    let p = Project::open(t.path(), no_lsp()).unwrap();
    let d = p.load_document(&t.path().join("lib.rs")).unwrap();
    assert_eq!(d.language().unwrap().name, "Rust");
    assert!(d.syntax().is_some());
    assert!(!d.is_large());
    assert_eq!(d.line_ending(), LineEnding::Lf);
    assert_eq!(d.indentation(), Indentation::Spaces(4));
    // highlights: line 3 is `pub struct Foo {`
    let hl = d.line_highlights(3..4);
    assert_eq!(hl.len(), 1);
    assert_eq!(hl[0].row, 3);
    let names: Vec<(&str, &str)> = hl[0]
        .spans
        .iter()
        .map(|(r, id)| (&"pub struct Foo {"[r.clone()], id.name()))
        .collect();
    assert!(
        names
            .iter()
            .any(|(s, n)| *s == "struct" && n.starts_with("keyword")),
        "{names:?}"
    );
    // spans inside the line, sorted
    for l in d.line_highlights(0..d.line_count()) {
        let len = d.text().unwrap().line_len(l.row) as usize;
        let mut last = 0;
        for (r, _) in &l.spans {
            assert!(r.start >= last && r.end <= len && r.start < r.end);
            last = r.end;
        }
    }
    // outline
    let o = d.outline();
    let n: Vec<&str> = o.iter().map(|i| i.name.as_str()).collect();
    assert!(
        n.contains(&"Foo") && n.contains(&"new") && n.contains(&"other"),
        "{n:?}"
    );
    assert!(!d.fold_ranges().is_empty());
    // scopes at row 11 (`Foo { x: 1 }` inside new): impl + new
    let sc = d.scopes_at_row(11);
    let sn: Vec<&str> = sc.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(sn.last(), Some(&"new"), "{sn:?}");
    assert_eq!(sn.len(), 2, "{sn:?}");
    // row of the fn header itself: only the impl
    assert_eq!(d.scopes_at_row(8).len(), 1);
    assert!(d.scopes_at_row(0).is_empty());
    // word_at
    let off = RUST_SRC.find("other").unwrap();
    assert_eq!(
        d.word_at(off + 2).unwrap(),
        (off..off + 5, "other".to_string())
    );
    assert_eq!(d.word_at(off + 5).unwrap().1, "other"); // just after
    let off = RUST_SRC.find("// comment").unwrap();
    assert_eq!(d.word_at(off + 4).unwrap().1, "comment");
    assert!(d.word_at(RUST_SRC.find("{\n    x").unwrap() + 1).is_none());
}

#[test]
fn highlights_stable_across_chunks() {
    let mut src = String::new();
    for i in 0..400 {
        src.push_str(&format!("fn f{i}() {{ let x = \"s{i}\"; }}\n"));
    }
    // a multi-line block comment crossing the 128-row boundary
    src.push_str("/*\n");
    for _ in 0..300 {
        src.push_str("comment line\n");
    }
    src.push_str("*/\nfn tail() {}\n");
    let t = tempfile::tempdir().unwrap();
    write(t.path(), "big.rs", &src);
    let p = Project::open(t.path(), no_lsp()).unwrap();
    let d = p.load_document(&t.path().join("big.rs")).unwrap();
    let all = d.line_highlights(0..d.line_count());
    assert_eq!(all.len() as u32, d.line_count());
    // request in odd windows must equal slices of the full result
    for (a, b) in [
        (0u32, 5u32),
        (120, 135),
        (127, 129),
        (255, 257),
        (380, 520),
        (700, 705),
    ] {
        let part = d.line_highlights(a..b);
        assert_eq!(
            part,
            all[a as usize..(b as usize).min(all.len())],
            "window {a}..{b}"
        );
    }
    // comment rows inside the multi-line comment are fully covered
    let text = d.text().unwrap();
    for row in [401u32, 450, 500, 600] {
        let l = &all[row as usize];
        let len = text.line_len(row) as usize;
        assert_eq!(l.spans.len(), 1, "row {row}");
        assert_eq!(l.spans[0].0, 0..len);
    }
    assert!(d.line_highlights(10_000..10_010).is_empty());
}

#[test]
fn line_ending_and_indent_detection() {
    let t = tempfile::tempdir().unwrap();
    write(
        t.path(),
        "crlf.py",
        "def a():\r\n  x = 1\r\n  if x:\r\n    y = 2\r\n",
    );
    write(t.path(), "mixed.py", "a\r\nb\nc\n");
    write(
        t.path(),
        "tabs.go",
        "func a() {\n\tx()\n\tif y {\n\t\tz()\n\t}\n}\n",
    );
    write(
        t.path(),
        "four.py",
        "class A:\n    def f(self):\n        pass\n    def g(self):\n        pass\n",
    );
    write(t.path(), "none.txt", "plain\ntext\n");
    let p = Project::open(t.path(), no_lsp()).unwrap();
    let ld = |n: &str| p.load_document(&t.path().join(n)).unwrap();
    let d = ld("crlf.py");
    assert_eq!(d.line_ending(), LineEnding::CrLf);
    assert_eq!(d.indentation(), Indentation::Spaces(2));
    assert_eq!(ld("mixed.py").line_ending(), LineEnding::Mixed);
    assert_eq!(ld("tabs.go").indentation(), Indentation::Tabs);
    assert_eq!(ld("four.py").indentation(), Indentation::Spaces(4));
    assert_eq!(ld("none.txt").indentation(), Indentation::Spaces(4));
    assert!(ld("none.txt").language().is_none());
    assert_eq!(ld("none.txt").line_highlights(0..2).len(), 2);
}

#[test]
fn large_file_is_mapped_without_syntax() {
    let t = tempfile::tempdir().unwrap();
    let mut src = String::new();
    for i in 0..30_000 {
        src.push_str(&format!("fn f{i}() {{}}\n"));
    }
    write(t.path(), "huge.rs", &src);
    let mut s = no_lsp();
    s.files.large_file_threshold_mb = 0; // everything non-empty is "large"
    let p = Project::open(t.path(), s).unwrap();
    let d = p.load_document(&t.path().join("huge.rs")).unwrap();
    assert!(d.is_large());
    assert!(d.syntax().is_none());
    assert!(d.language().is_some());
    assert!(d.outline().is_empty());
    assert!(d.fold_ranges().is_empty());
    assert!(d.scopes_at_row(5).is_empty());
    assert_eq!(
        d.line_highlights(0..3)
            .iter()
            .map(|l| l.spans.len())
            .sum::<usize>(),
        0
    );
    assert_eq!(d.word_at(5).unwrap().1, "f0");
    assert!(d.diff_gutter(&p).is_empty());
}

#[test]
fn binary_document_has_no_text() {
    let t = tempfile::tempdir().unwrap();
    fs::write(t.path().join("b.bin"), [0u8, 1, 2, 0, 0, 9]).unwrap();
    let p = Project::open(t.path(), no_lsp()).unwrap();
    let d = p.load_document(&t.path().join("b.bin")).unwrap();
    assert!(d.text().is_none());
    assert!(d.line_highlights(0..5).is_empty());
    assert!(d.diff_gutter(&p).is_empty());
    assert!(p.load_document(&t.path().join("missing")).is_err());
}

#[test]
fn diff_gutter_cases() {
    let t = tempfile::tempdir().unwrap();
    init_repo(t.path());
    write(t.path(), "m.txt", "1\n2\n3\n4\n5\n");
    write(t.path(), "d.txt", "1\n2\n3\n4\n5\n");
    write(t.path(), "same.txt", "x\n");
    commit_all(t.path());
    write(t.path(), "m.txt", "1\nTWO\n3\n4\n5\n6\n");
    write(t.path(), "d.txt", "1\n2\n4\n5\n");
    write(t.path(), "new.txt", "a\nb\n");
    write(t.path(), "ignored.log", "a\n");
    write(t.path(), ".gitignore", "*.log\n");
    let p = Project::open(t.path(), no_lsp()).unwrap();
    p.refresh_git_status().unwrap();
    let marks = |n: &str| p.load_document(&t.path().join(n)).unwrap().diff_gutter(&p);

    let m = marks("m.txt");
    assert_eq!(m.len(), 2, "{m:?}");
    assert_eq!((m[0].kind, m[0].rows.clone()), (GutterKind::Modified, 1..2));
    assert_eq!((m[1].kind, m[1].rows.clone()), (GutterKind::Added, 5..6));
    let d = marks("d.txt");
    assert_eq!(d.len(), 1);
    assert_eq!((d[0].kind, d[0].rows.clone()), (GutterKind::Deleted, 2..2));
    let n = marks("new.txt");
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].kind, GutterKind::Added);
    assert_eq!(n[0].rows, 0..2);
    assert!(marks("same.txt").is_empty());
    assert!(marks("ignored.log").is_empty());
}

#[test]
fn blame_is_cached() {
    let t = tempfile::tempdir().unwrap();
    init_repo(t.path());
    write(t.path(), "a.rs", "fn a() {}\n");
    commit_all(t.path());
    let p = Project::open(t.path(), no_lsp()).unwrap();
    let b1 = p.blame("a.rs").unwrap();
    let b2 = p.blame("a.rs").unwrap();
    assert!(std::sync::Arc::ptr_eq(&b1, &b2));
    assert_eq!(b1.len(), 1);
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(t.path(), "a.rs", "fn a() {}\nfn b() {}\n");
    let b3 = p.blame("a.rs").unwrap();
    assert!(!std::sync::Arc::ptr_eq(&b1, &b3));
    assert!(b3.iter().any(|h| h.uncommitted));
}

fn loc(p: &str, row: u32) -> Location {
    Location {
        path: p.into(),
        point: Point { row, column: 0 },
    }
}

#[test]
fn nav_history_semantics() {
    let mut h = NavHistory::new();
    assert!(!h.can_go_back() && !h.can_go_forward());
    assert!(h.back(loc("a", 0)).is_none());
    h.push(loc("a", 10));
    h.push(loc("b", 5));
    assert!(h.can_go_back());
    // back pushes current onto forward
    let b = h.back(loc("c", 1)).unwrap();
    assert_eq!(b, loc("b", 5));
    assert!(h.can_go_forward());
    let a = h.back(loc("b", 5)).unwrap();
    assert_eq!(a, loc("a", 10));
    assert!(h.back(loc("a", 10)).is_none());
    let f = h.forward(loc("a", 10)).unwrap();
    assert_eq!(f, loc("b", 5));
    let f = h.forward(loc("b", 5)).unwrap();
    assert_eq!(f, loc("c", 1));
    assert!(h.forward(loc("c", 1)).is_none());
    // push clears forward
    let _ = h.back(loc("c", 1));
    assert!(h.can_go_forward());
    h.push(loc("z", 0));
    assert!(!h.can_go_forward());
    // dedup within 3 rows, same path
    let mut h = NavHistory::new();
    h.push(loc("a", 10));
    h.push(loc("a", 12));
    h.push(loc("a", 20));
    h.push(loc("b", 20));
    assert_eq!(h.back(loc("q", 0)).unwrap(), loc("b", 20));
    assert_eq!(h.back(loc("b", 20)).unwrap(), loc("a", 20));
    assert_eq!(h.back(loc("a", 20)).unwrap(), loc("a", 12));
    assert!(h.back(loc("a", 12)).is_none());
    // cap
    let mut h = NavHistory::new();
    for i in 0..300 {
        h.push(loc("a", i * 10));
    }
    let mut n = 0;
    let mut cur = loc("zz", 0);
    while let Some(l) = h.back(cur.clone()) {
        cur = l;
        n += 1;
    }
    assert_eq!(n, 100);
}

fn two_file_project() -> (tempfile::TempDir, Project) {
    let t = tempfile::tempdir().unwrap();
    write(t.path(), "src/lib.rs", "pub fn compute(a: u32) -> u32 {\n    a + 1\n}\n\npub fn run() {\n    let v = compute(3);\n}\n");
    write(
        t.path(),
        "src/other.rs",
        "use crate::compute;\n\nfn go() -> u32 {\n    compute(5)\n}\n\nfn compute_helper() {}\n",
    );
    write(t.path(), "far/deep/x.rs", "fn compute() {}\n");
    write(t.path(), "notes.txt", "compute is mentioned in a note\n");
    let p = Project::open(t.path(), no_lsp()).unwrap();
    (t, p)
}

#[test]
fn tag_based_definitions_and_references() {
    let (t, p) = two_file_project();
    let calls = std::sync::atomic::AtomicUsize::new(0);
    p.index_symbols(&CancelToken::new(), |done, total| {
        assert!(done <= total);
        calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
    assert!(calls.load(std::sync::atomic::Ordering::Relaxed) >= 2);
    assert_eq!(p.symbols().file_count(), 3);
    assert!(symbols::project_symbols(&p).iter().any(|s| s.name == "go"));

    let other = p.load_document(&t.path().join("src/other.rs")).unwrap();
    let text = other.text().unwrap().to_string();
    let off = text.find("compute(5)").unwrap() + 2;
    let defs = symbols::definitions(&p, &other, off).unwrap();
    assert_eq!(defs.len(), 2, "{defs:?}");
    // same directory ranks before far/deep
    assert!(defs[0].path.ends_with("src/lib.rs"), "{defs:?}");
    assert!(defs[1].path.ends_with("far/deep/x.rs"));
    assert_eq!(defs[0].source, TargetSource::Tags);
    assert_eq!(defs[0].range.start, Point { row: 0, column: 7 });
    assert_eq!(defs[0].range.end, Point { row: 0, column: 14 });
    assert_eq!(defs[0].preview, "pub fn compute(a: u32) -> u32 {");

    let refs = symbols::references(&p, &other, off).unwrap();
    let show: Vec<String> = refs
        .iter()
        .map(|r| {
            format!(
                "{}:{}:{:?}",
                p.relative_path(&r.path).unwrap(),
                r.range.start.row,
                r.source
            )
        })
        .collect();
    // definitions + calls + the text mention; not compute_helper (whole word)
    assert!(show.contains(&"src/lib.rs:0:Tags".to_string()), "{show:?}");
    assert!(show.contains(&"src/lib.rs:5:Tags".to_string()), "{show:?}");
    assert!(
        show.contains(&"src/other.rs:3:Tags".to_string()),
        "{show:?}"
    );
    assert!(show.contains(&"notes.txt:0:Text".to_string()), "{show:?}");
    assert!(
        !show.iter().any(|s| s.starts_with("src/other.rs:6")),
        "{show:?}"
    );
    // no duplicate (path,row)
    let mut keys: Vec<_> = refs
        .iter()
        .map(|r| (r.path.clone(), r.range.start.row))
        .collect();
    keys.sort();
    let n = keys.len();
    keys.dedup();
    assert_eq!(n, keys.len());

    // nothing under a non-identifier
    assert!(
        symbols::definitions(&p, &other, text.find("::").unwrap() + 1)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn definitions_work_without_index_for_open_file() {
    let (t, p) = two_file_project();
    let lib = p.load_document(&t.path().join("src/lib.rs")).unwrap();
    let off = lib.text().unwrap().to_string().find("compute(3)").unwrap();
    let defs = symbols::definitions(&p, &lib, off).unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].range.start.row, 0);
}

#[test]
fn hover_fallback_without_lsp() {
    let (t, p) = two_file_project();
    p.index_symbols(&CancelToken::new(), |_, _| {});
    let lib = p.load_document(&t.path().join("src/lib.rs")).unwrap();
    let text = lib.text().unwrap().to_string();
    // `run` has a unique definition
    let h = symbols::hover(&p, &lib, text.find("run()").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(h, "```rust\npub fn run() {\n```");
    // `compute` has two definitions -> no hover
    assert!(symbols::hover(&p, &lib, text.find("compute(3)").unwrap())
        .unwrap()
        .is_none());
    // unknown word
    assert!(symbols::hover(&p, &lib, text.find("let").unwrap())
        .unwrap()
        .is_none());
}

#[test]
fn index_symbols_cancel_and_skips() {
    let t = tempfile::tempdir().unwrap();
    write(t.path(), "a.rs", "fn a() {}\n");
    fs::write(t.path().join("bin.rs"), [0u8; 100]).unwrap();
    write(t.path(), "big.rs", &"fn x() {}\n".repeat(200_000)); // > 1 MB
    write(t.path(), "x.unknown", "fn q() {}");
    let p = Project::open(t.path(), no_lsp()).unwrap();
    p.index_symbols(&CancelToken::new(), |_, _| {});
    assert_eq!(p.symbols().file_count(), 1);
    let c = CancelToken::new();
    c.cancel();
    let p2 = Project::open(t.path(), no_lsp()).unwrap();
    p2.index_symbols(&c, |_, _| {});
    assert_eq!(p2.symbols().file_count(), 0);
    // reindex a single file after change
    write(t.path(), "a.rs", "fn a() {}\nfn b() {}\n");
    p.reindex_file(&p.absolute_path("a.rs"));
    assert_eq!(p.symbols().definitions("b").len(), 1);
    fs::remove_file(t.path().join("a.rs")).unwrap();
    p.reindex_file(&p.absolute_path("a.rs"));
    assert_eq!(p.symbols().file_count(), 0);
}

#[test]
fn bench_10k_line_file() {
    let mut src = String::new();
    for i in 0..2500 {
        src.push_str(&format!(
            "/// doc {i}\npub fn function_{i}(a: u32, b: &str) -> Option<u32> {{\n    let x = a + {i}; // c\n    if b.is_empty() {{ None }} else {{ Some(x) }}\n}}\n\n"
        ));
    }
    let lines = src.lines().count();
    let t = tempfile::tempdir().unwrap();
    write(t.path(), "big.rs", &src);
    let p = Project::open(t.path(), no_lsp()).unwrap();
    let t0 = Instant::now();
    let d = p.load_document(&t.path().join("big.rs")).unwrap();
    let load = t0.elapsed();
    let t1 = Instant::now();
    let first = d.line_highlights(0..60);
    let first_screen = t1.elapsed();
    let t2 = Instant::now();
    let all = d.line_highlights(0..d.line_count());
    let full = t2.elapsed();
    let t3 = Instant::now();
    let _ = d.outline();
    let _ = d.fold_ranges();
    let outline = t3.elapsed();
    let t4 = Instant::now();
    let again = d.line_highlights(5000..5060);
    let cached = t4.elapsed();
    println!(
        "bench: {lines} lines: load+parse {load:?}, first screen {first_screen:?}, all {full:?}, outline+folds {outline:?}, cached screen {cached:?}"
    );
    assert_eq!(first.len(), 60);
    assert_eq!(all.len() as u32, d.line_count());
    assert_eq!(again.len(), 60);
}

/// A project reached through a symlinked path (like macOS `/var` → `/private/var`):
/// documents loaded through either spelling resolve to the same project paths.
#[cfg(unix)]
#[test]
fn symlinked_root_spellings_resolve_to_project_paths() {
    let t = tempfile::tempdir().unwrap();
    let real = t.path().join("real");
    fs::create_dir(&real).unwrap();
    init_repo(&real);
    write(&real, "src/lib.rs", "pub fn compute() -> u32 {\n    1\n}\n");
    commit_all(&real);
    write(&real, "src/lib.rs", "pub fn compute() -> u32 {\n    2\n}\n");
    let alias = t.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();

    let project = Project::open(&alias, no_lsp()).unwrap();
    project.refresh_git_status().unwrap();
    for spelling in [&alias, &real] {
        let path = spelling.join("src/lib.rs");
        assert_eq!(project.relative_path(&path).as_deref(), Some("src/lib.rs"));
        let doc = project.load_document(&path).unwrap();
        assert_eq!(doc.rel_path(), Some("src/lib.rs"));
        let marks = doc.diff_gutter(&project);
        assert_eq!(marks.len(), 1, "{marks:?}");
        assert_eq!(marks[0].kind, GutterKind::Modified);
    }
    // A path that does not exist yet still maps through the symlinked parent.
    assert_eq!(
        project.relative_path(&alias.join("src/new.rs")).as_deref(),
        Some("src/new.rs")
    );
    assert_eq!(project.relative_path(Path::new("/elsewhere/x.rs")), None);
}
