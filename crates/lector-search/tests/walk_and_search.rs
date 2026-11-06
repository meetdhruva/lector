use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use lector_search::*;
use tempfile::TempDir;

fn write(root: &Path, rel: &str, content: impl AsRef<[u8]>) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn fixture() -> TempDir {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    write(r, ".gitignore", "target/\n*.log\n");
    write(
        r,
        "src/main.rs",
        "fn main() {\n    println!(\"Hello World\");\n}\n",
    );
    write(r, "src/lib.rs", "pub fn hello() {}\n// TODO: world\n");
    write(r, "target/debug/out.rs", "fn hello() {}\n");
    write(r, "build.log", "hello log\n");
    write(r, ".hidden/secret.txt", "hello hidden\n");
    write(r, ".git/config", "hello git\n");
    write(r, "node_modules/pkg/index.js", "hello js\n");
    write(r, "docs/readme.md", "Hello docs\n");
    write(r, "bin.dat", b"hello\0binary");
    d
}

fn walk(root: &Path, opts: &WalkOptions) -> BTreeSet<String> {
    let out = Mutex::new(BTreeSet::new());
    walk_files(root, opts, &CancelToken::new(), |p| {
        out.lock()
            .unwrap()
            .insert(p.to_string_lossy().replace('\\', "/"));
    });
    out.into_inner().unwrap()
}

fn search(root: &Path, q: &SearchQuery, w: &WalkOptions) -> (Vec<FileMatches>, SearchEvent) {
    let files = Mutex::new(Vec::new());
    let dones = Mutex::new(Vec::new());
    let r = search_project(root, q, w, &CancelToken::new(), |e| match e {
        SearchEvent::File(f) => files.lock().unwrap().push(f),
        d @ SearchEvent::Done { .. } => dones.lock().unwrap().push(d),
        SearchEvent::Error(_) => {}
    });
    r.unwrap();
    let mut dones = dones.into_inner().unwrap();
    assert_eq!(dones.len(), 1, "exactly one Done");
    let mut files = files.into_inner().unwrap();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    (files, dones.remove(0))
}

fn q(p: &str) -> SearchQuery {
    SearchQuery {
        pattern: p.into(),
        ..Default::default()
    }
}

fn paths(f: &[FileMatches]) -> Vec<String> {
    f.iter()
        .map(|f| f.path.to_string_lossy().replace('\\', "/"))
        .collect()
}

#[test]
fn walk_respects_gitignore_and_skips_hidden_and_git() {
    let d = fixture();
    let o = WalkOptions {
        git_ignore: true,
        ..Default::default()
    };
    let got = walk(d.path(), &o);
    assert!(got.contains("src/main.rs") && got.contains("docs/readme.md"));
    assert!(!got.contains("target/debug/out.rs"));
    assert!(!got.contains("build.log"));
    assert!(!got
        .iter()
        .any(|p| p.starts_with(".hidden") || p.starts_with(".git/")));
    assert!(!got.contains(".gitignore")); // hidden
}

#[test]
fn walk_hidden_and_no_gitignore() {
    let d = fixture();
    let o = WalkOptions {
        hidden: true,
        git_ignore: false,
        ..Default::default()
    };
    let got = walk(d.path(), &o);
    assert!(got.contains(".hidden/secret.txt"));
    assert!(got.contains("target/debug/out.rs"));
    assert!(got.contains("build.log"));
    assert!(got.contains(".gitignore"));
    assert!(
        !got.iter().any(|p| p.starts_with(".git/")),
        ".git always skipped"
    );
}

#[test]
fn walk_exclude_globs_and_depth() {
    let d = fixture();
    let o = WalkOptions {
        git_ignore: true,
        exclude: vec!["node_modules".into(), "*.md".into()],
        ..Default::default()
    };
    let got = walk(d.path(), &o);
    assert!(!got.iter().any(|p| p.starts_with("node_modules")));
    assert!(!got.contains("docs/readme.md"));
    assert!(got.contains("src/lib.rs"));
    let o = WalkOptions {
        git_ignore: true,
        max_depth: Some(1),
        ..Default::default()
    };
    let got = walk(d.path(), &o);
    assert!(got.contains("bin.dat") && !got.contains("src/lib.rs"));
}

#[test]
fn walk_cancel_stops() {
    let d = tempfile::tempdir().unwrap();
    for i in 0..300 {
        write(d.path(), &format!("d{}/f{i}.txt", i % 10), "x");
    }
    let cancel = CancelToken::new();
    cancel.cancel();
    let count = std::sync::atomic::AtomicUsize::new(0);
    walk_files(d.path(), &WalkOptions::default(), &cancel, |_| {
        count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
    assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[test]
fn search_literal_smart_case() {
    let d = fixture();
    let w = WalkOptions {
        git_ignore: true,
        ..Default::default()
    };
    let (files, done) = search(d.path(), &q("hello"), &w);
    // smart-case: lowercase matches "Hello" too; ignored/hidden/binary skipped
    assert_eq!(
        paths(&files),
        vec![
            "docs/readme.md",
            "node_modules/pkg/index.js",
            "src/lib.rs",
            "src/main.rs"
        ]
    );
    match done {
        SearchEvent::Done {
            matches,
            truncated,
            files_searched,
        } => {
            assert_eq!(matches, 4);
            assert!(!truncated);
            assert!(files_searched >= 4);
        }
        _ => unreachable!(),
    }
    let (files, _) = search(d.path(), &q("Hello"), &w);
    assert_eq!(paths(&files), vec!["docs/readme.md", "src/main.rs"]);
}

#[test]
fn search_case_modes_whole_word_regex_literal() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "a.txt", "foo Foo FOO\nfoobar\nfoo.bar\nfooXbar\n");
    let w = WalkOptions::default();
    let run = |mut query: SearchQuery| search(d.path(), &query_fix(&mut query), &w).0;
    fn query_fix(q: &mut SearchQuery) -> SearchQuery {
        q.clone()
    }
    let mut query = q("foo");
    query.case = CaseMode::Sensitive;
    let f = run(query.clone());
    assert_eq!(f[0].matches[0].ranges, vec![0..3]); // only lowercase "foo" in line 0
    query.case = CaseMode::Insensitive;
    let f = run(query.clone());
    assert_eq!(f[0].matches[0].ranges, vec![0..3, 4..7, 8..11]);
    query.whole_word = true;
    query.case = CaseMode::Sensitive;
    let f = run(query.clone());
    let lines: Vec<u32> = f[0].matches.iter().map(|m| m.line).collect();
    assert_eq!(lines, vec![0, 2]); // "foobar" / "fooXbar" excluded
                                   // literal: '.' is not a wildcard
    let mut lit = q("foo.bar");
    lit.case = CaseMode::Sensitive;
    let f = run(lit.clone());
    assert_eq!(
        f[0].matches.iter().map(|m| m.line).collect::<Vec<_>>(),
        vec![2]
    );
    lit.regex = true;
    let f = run(lit);
    assert_eq!(
        f[0].matches.iter().map(|m| m.line).collect::<Vec<_>>(),
        vec![2, 3]
    );
    // regex anchor
    let mut re = q("^foo\\w+$");
    re.regex = true;
    let f = run(re);
    assert_eq!(
        f[0].matches.iter().map(|m| m.line).collect::<Vec<_>>(),
        vec![1, 3]
    );
}

#[test]
fn search_invalid_regex_errors_with_done() {
    let d = tempfile::tempdir().unwrap();
    let mut query = q("(");
    query.regex = true;
    let events = Mutex::new(Vec::new());
    let r = search_project(
        d.path(),
        &query,
        &WalkOptions::default(),
        &CancelToken::new(),
        |e| events.lock().unwrap().push(e),
    );
    assert!(r.is_err());
    let ev = events.into_inner().unwrap();
    assert!(matches!(ev[0], SearchEvent::Error(_)));
    assert!(matches!(ev[1], SearchEvent::Done { .. }));
    assert!(find_in_text("x", &query).is_err());
}

#[test]
fn search_include_exclude_globs() {
    let d = fixture();
    let w = WalkOptions {
        git_ignore: true,
        ..Default::default()
    };
    let mut query = q("hello");
    query.include = vec!["*.rs".into()];
    assert_eq!(
        paths(&search(d.path(), &query, &w).0),
        vec!["src/lib.rs", "src/main.rs"]
    );
    query.include = vec!["src".into()];
    query.exclude = vec!["main.rs".into()];
    assert_eq!(paths(&search(d.path(), &query, &w).0), vec!["src/lib.rs"]);
}

#[test]
fn search_max_results_truncates() {
    let d = tempfile::tempdir().unwrap();
    for i in 0..5 {
        write(d.path(), &format!("f{i}.txt"), "x\nx\nx\nx\n");
    }
    let mut query = q("x");
    query.max_results = 7;
    let (files, done) = search(d.path(), &query, &WalkOptions::default());
    let n: usize = files.iter().map(|f| f.matches.len()).sum();
    assert_eq!(n, 7);
    assert!(matches!(
        done,
        SearchEvent::Done {
            matches: 7,
            truncated: true,
            ..
        }
    ));
}

#[test]
fn search_cancel_emits_done() {
    let d = fixture();
    let cancel = CancelToken::new();
    cancel.cancel();
    let events = Mutex::new(Vec::new());
    search_project(
        d.path(),
        &q("hello"),
        &WalkOptions::default(),
        &cancel,
        |e| events.lock().unwrap().push(e),
    )
    .unwrap();
    let ev = events.into_inner().unwrap();
    assert_eq!(ev.len(), 1);
    assert!(matches!(ev[0], SearchEvent::Done { matches: 0, .. }));
}

#[test]
fn search_skips_binary_even_late_nul() {
    let d = tempfile::tempdir().unwrap();
    let mut late = b"hello\n".repeat(3000); // > 8000 bytes of text before the NUL
    late.push(0);
    late.extend_from_slice(b"hello\n");
    write(d.path(), "late.bin", late);
    write(d.path(), "early.bin", b"\0hello");
    write(d.path(), "ok.txt", "hello\n");
    let (files, _) = search(d.path(), &q("hello"), &WalkOptions::default());
    assert_eq!(paths(&files), vec!["ok.txt"]);
}

#[test]
fn search_long_lines_are_windowed_with_valid_ranges() {
    let d = tempfile::tempdir().unwrap();
    let line = format!("{}needle{}é\n", "a".repeat(2000), "b".repeat(2000));
    write(d.path(), "min.js", &line);
    let (files, _) = search(d.path(), &q("needle"), &WalkOptions::default());
    let m = &files[0].matches[0];
    assert!(m.text.len() <= 500);
    assert_eq!(&m.text[m.ranges[0].clone()], "needle");
    assert_eq!(m.line, 0);
}

#[test]
fn search_handles_crlf_and_non_utf8() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "crlf.txt", "one\r\ntwo needle\r\nthree\r\n");
    write(d.path(), "latin1.txt", b"caf\xe9 needle\n");
    let (files, _) = search(d.path(), &q("needle"), &WalkOptions::default());
    assert_eq!(paths(&files), vec!["crlf.txt", "latin1.txt"]);
    assert_eq!(files[0].matches[0].text, "two needle");
    assert_eq!(files[0].matches[0].line, 1);
    let t = &files[1].matches[0];
    assert_eq!(&t.text[t.ranges[0].clone()], "needle");
}

#[test]
fn find_in_text_basic() {
    let text = "Foo foo\nbar FOO\n";
    let mut query = q("foo");
    assert_eq!(
        find_in_text(text, &query).unwrap(),
        vec![0..3, 4..7, 12..15]
    );
    query.case = CaseMode::Sensitive;
    assert_eq!(find_in_text(text, &query).unwrap(), vec![4..7]);
    query.whole_word = true;
    assert_eq!(find_in_text("foo foobar", &query).unwrap(), vec![0..3]);
    let mut re = q("^b\\w+");
    re.regex = true;
    assert_eq!(find_in_text(text, &re).unwrap(), vec![8..11]);
    assert!(find_in_text(text, &q("")).unwrap().is_empty());
    assert_eq!(find_in_text("a.b axb", &q("a.b")).unwrap(), vec![0..3]);
    let _: PathBuf = PathBuf::new();
}

#[test]
fn replacements_expand_capture_groups_only_in_regex_mode() {
    let text = "let a = 1;\nlet bb = 22;\n";
    let mut re = q(r"let (\w+) = (\d+)");
    re.regex = true;
    let out = replacements_in_text(text, &re, "$2 => $1").unwrap();
    assert_eq!(
        out,
        vec![
            (0..9, "1 => a".to_string()),
            (11..22, "22 => bb".to_string())
        ]
    );
    // Named groups, `$$` and a literal `$1` when regex mode is off.
    let mut named = q(r"(?P<n>\d+)");
    named.regex = true;
    let out = replacements_in_text("x 5", &named, "${n}$$").unwrap();
    assert_eq!(out, vec![(2..3, "5$".to_string())]);
    let literal = q("a");
    assert_eq!(
        replacements_in_text("a", &literal, "$1").unwrap(),
        vec![(0..1, "$1".to_string())]
    );
    // Same matches as find_in_text, including case and word options.
    let mut word = q("foo");
    word.whole_word = true;
    word.case = CaseMode::Sensitive;
    let ranges: Vec<_> = replacements_in_text("foo Foo foobar foo", &word, "x")
        .unwrap()
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    assert_eq!(ranges, find_in_text("foo Foo foobar foo", &word).unwrap());
    assert!(replacements_in_text("a", &q(""), "b").unwrap().is_empty());
    let mut bad = q("(");
    bad.regex = true;
    assert!(replacements_in_text("a", &bad, "b").is_err());
}

/// The trigram index narrows candidates and `search_files` confirms them: the result must equal a
/// full `search_project` for literal and regex queries.
#[test]
fn trigram_candidates_then_search_files_equal_full_search() {
    let d = fixture();
    let root = d.path();
    let opts = WalkOptions {
        git_ignore: true,
        ..Default::default()
    };
    let files: Vec<PathBuf> = walk(root, &opts).into_iter().map(PathBuf::from).collect();
    let index = TrigramIndex::build(root, &files, &CancelToken::new());
    assert!(index.contains("bin.dat"));
    assert!(!index.searchable_paths().contains(&"bin.dat".to_string()));

    type Sink<'a> = &'a (dyn Fn(SearchEvent) + Send + Sync);
    let collect = |run: &dyn Fn(Sink<'_>)| {
        let out = Mutex::new(BTreeSet::new());
        run(&|ev| {
            if let SearchEvent::File(f) = ev {
                for m in f.matches {
                    out.lock()
                        .unwrap()
                        .insert((f.path.to_string_lossy().replace('\\', "/"), m.line));
                }
            }
        });
        out.into_inner().unwrap()
    };
    for (pattern, regex) in [
        ("hello", false),
        ("Hello World", false),
        ("pub fn hello|TODO: w", true),
        ("fn\\s+main", true),
        ("[a-z]+", true),
        ("nomatchatall", false),
    ] {
        let q = SearchQuery {
            pattern: pattern.into(),
            regex,
            ..Default::default()
        };
        let full = collect(&|sink| {
            search_project(root, &q, &opts, &CancelToken::new(), sink).unwrap();
        });
        let candidates = match index.candidates(&q).unwrap() {
            Candidates::All => index.searchable_paths(),
            Candidates::Files(f) => f,
        };
        let cands: Vec<PathBuf> = candidates.into_iter().map(PathBuf::from).collect();
        let narrowed = collect(&|sink| {
            search_files(root, &cands, &q, &CancelToken::new(), sink).unwrap();
        });
        assert_eq!(full, narrowed, "pattern {pattern:?}");
        if pattern == "nomatchatall" {
            assert!(cands.is_empty(), "trigrams must rule everything out");
        }
    }
}
