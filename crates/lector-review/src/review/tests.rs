//! Unit tests of changeset loading, keys and file diffs.

use super::*;
use crate::content::NoCache;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@e.st")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@e.st")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn fixture() -> (tempfile::TempDir, Repo) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    std::fs::write(
        p.join("lib.rs"),
        "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n",
    )
    .unwrap();
    std::fs::write(p.join("old.txt"), "a\nb\nc\nd\ne\nf\ng\nh\n").unwrap();
    std::fs::write(p.join("gone.txt"), "bye\n").unwrap();
    git(p, &["add", "."]);
    git(p, &["commit", "-q", "-m", "Initial commit"]);
    // Working-tree changes.
    std::fs::write(
        p.join("lib.rs"),
        "fn main() {\n    let x = 2;\n    println!(\"{x}\");\n}\n",
    )
    .unwrap();
    git(p, &["mv", "old.txt", "new.txt"]);
    std::fs::remove_file(p.join("gone.txt")).unwrap();
    std::fs::write(p.join("added.md"), "# Title\n\ntext\n").unwrap();
    std::fs::write(p.join("blob.bin"), [0u8, 1, 2, 3, 0, 9]).unwrap();
    let repo = Repo::discover(p).unwrap();
    (dir, repo)
}

/// In-memory viewed marks (the app owns the real store).
#[derive(Default)]
struct Marks(std::cell::RefCell<std::collections::HashSet<(String, String, String)>>);

impl Marks {
    fn mark(&self, key: &str, file: &ReviewFile) {
        self.0.borrow_mut().insert((
            key.to_string(),
            file.path().to_string(),
            file.content_hash.clone(),
        ));
    }
}

impl ViewedMarks for Marks {
    fn is_viewed(&self, key: &str, path: &str, hash: &str) -> bool {
        self.0
            .borrow()
            .contains(&(key.to_string(), path.to_string(), hash.to_string()))
    }
}

#[test]
fn keys_and_titles() {
    assert_eq!(changeset_key(&Changeset::WorkingTree), "worktree");
    let cs = Changeset::BranchVsBase {
        branch: "feature".into(),
        base: "main".into(),
    };
    assert_eq!(changeset_key(&cs), "branch:main...feature");
    assert_eq!(
        parse_changeset_key("branch:main...feature"),
        Some(cs.clone())
    );
    for cs in [
        Changeset::WorkingTree,
        Changeset::Staged,
        Changeset::Range {
            from: "a".into(),
            to: "b".into(),
        },
    ] {
        assert_eq!(parse_changeset_key(&changeset_key(&cs)), Some(cs));
    }
    assert_eq!(parse_changeset_key("nonsense"), None);
    assert_eq!(changeset_title(None, &cs), "main…feature");
    assert_eq!(
        changeset_title(None, &Changeset::WorkingTree),
        "Working tree"
    );
}

#[test]
fn working_tree_changeset_has_stats_and_viewed_state() {
    let (dir, repo) = fixture();
    let store = Marks::default();
    let opts = DiffOptions::default();
    let view = load_changeset_with(&repo, &Changeset::WorkingTree, &store, &opts).unwrap();
    let paths: Vec<_> = view.files.iter().map(|f| f.display_path.clone()).collect();
    assert!(paths.contains(&"lib.rs".to_string()), "{paths:?}");
    assert!(paths.iter().any(|p| p == "old.txt → new.txt"), "{paths:?}");
    let lib = &view.files[view.index_of("lib.rs").unwrap()];
    assert_eq!(
        lib.stats,
        Some(DiffStats {
            added: 1,
            removed: 1
        })
    );
    let added = &view.files[view.index_of("added.md").unwrap()];
    assert_eq!(
        added.stats,
        Some(DiffStats {
            added: 3,
            removed: 0
        })
    );
    let gone = &view.files[view.index_of("gone.txt").unwrap()];
    assert_eq!(gone.stats.map(|s| s.removed), Some(1));
    let bin = &view.files[view.index_of("blob.bin").unwrap()];
    assert!(bin.binary);
    assert!(bin.stats.is_none());
    assert_eq!(view.viewed_count(), 0);

    // Mark viewed; reload sees it.
    store.mark(&view.key, lib);
    let view2 = load_changeset_with(&repo, &Changeset::WorkingTree, &store, &opts).unwrap();
    assert!(view2.files[view2.index_of("lib.rs").unwrap()].viewed);
    assert_eq!(view2.viewed_count(), 1);

    // Editing the file un-views it.
    std::fs::write(dir.path().join("lib.rs"), "fn main() {}\n").unwrap();
    let view3 = load_changeset_with(&repo, &Changeset::WorkingTree, &store, &opts).unwrap();
    assert!(!view3.files[view3.index_of("lib.rs").unwrap()].viewed);
}

#[test]
fn commit_changeset_title() {
    let (_dir, repo) = fixture();
    let head = repo.head().unwrap().oid.unwrap();
    let view = load_changeset_with(
        &repo,
        &Changeset::Commit(head),
        &NoMarks,
        &DiffOptions::default(),
    )
    .unwrap();
    assert_eq!(view.title, format!("{} Initial commit", head.short()));
    assert_eq!(view.key, format!("commit:{head}"));
    assert_eq!(view.files.len(), 3);
    assert!(view
        .files
        .iter()
        .all(|f| f.stats.is_some_and(|s| s.removed == 0)));
}

#[test]
fn file_diff_rows_inline_and_highlights() {
    let (_dir, repo) = fixture();
    let langs = LanguageRegistry::new();
    let changes = repo.changes(&Changeset::WorkingTree).unwrap();
    let lib = changes.iter().find(|c| c.path() == "lib.rs").unwrap();
    let fd = load_file_diff_with(&repo, &langs, lib, &DiffOptions::default(), &NoCache).unwrap();
    assert_eq!(fd.kind, DiffKind::Text);
    assert_eq!(fd.diff.hunks.len(), 1);
    let changed: Vec<_> = fd
        .split
        .iter()
        .filter(|r| r.kind == RowKind::Changed)
        .collect();
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].old, Some(1));
    let inline = fd.inline_for_old(1).unwrap();
    // "1" → "2": a one-byte word change on each side.
    assert_eq!(inline.old.len(), 1);
    assert_eq!(&fd.old.line(1)[inline.old[0].clone()], "1");
    assert_eq!(&fd.new.line(1)[inline.new[0].clone()], "2");
    // Cached: same Arc.
    assert!(Arc::ptr_eq(&inline, &fd.inline_for_new(1).unwrap()));
    assert_eq!(fd.split_hunk_starts(), vec![1]);
    // Syntax on both sides.
    assert!(fd.old.syntax.is_some() && fd.new.syntax.is_some());
    let spans = fd.new.line_highlights(0..4);
    assert!(spans
        .iter()
        .any(|s| s.row == 0 && s.id.name().starts_with("keyword")));
    for s in &spans {
        let line = fd.new.line(s.row);
        assert!(s.range.end <= line.len(), "{s:?} in {line:?}");
    }
    // Folding with 0 context hides the unchanged rows around the change.
    let segs = fd.split_segments(0);
    assert!(segs.iter().any(|s| matches!(s, Segment::Folded { .. })));
}

#[test]
fn added_deleted_binary_and_large() {
    let (dir, repo) = fixture();
    let langs = LanguageRegistry::new();
    let changes = repo.changes(&Changeset::WorkingTree).unwrap();
    let opts = DiffOptions::default();
    let get = |p: &str| changes.iter().find(|c| c.path() == p).unwrap();

    let added = load_file_diff_with(&repo, &langs, get("added.md"), &opts, &NoCache).unwrap();
    assert!(!added.old.exists);
    assert_eq!(added.old.line_count(), 0);
    assert!(added.unified.iter().all(|r| r.kind == RowKind::Added));
    assert_eq!(added.unified.len(), 3); // the trailing newline adds no row

    let gone = load_file_diff_with(&repo, &langs, get("gone.txt"), &opts, &NoCache).unwrap();
    assert!(gone.unified.iter().all(|r| r.kind == RowKind::Removed));

    let bin = load_file_diff_with(&repo, &langs, get("blob.bin"), &opts, &NoCache).unwrap();
    assert_eq!(bin.kind, DiffKind::Binary);
    assert_eq!(bin.new.size, 6);

    let renamed = load_file_diff_with(&repo, &langs, get("new.txt"), &opts, &NoCache).unwrap();
    assert_eq!(renamed.display_path(), "old.txt → new.txt");
    assert!(renamed.is_identical());

    let big = vec![b'x'; (MAX_DIFF_BYTES + 10) as usize];
    std::fs::write(dir.path().join("added.md"), &big).unwrap();
    let large = load_file_diff_with(&repo, &langs, get("added.md"), &opts, &NoCache).unwrap();
    assert_eq!(large.kind, DiffKind::TooLarge);
    assert_eq!(large.new.size, MAX_DIFF_BYTES + 10);
}

#[test]
fn images_are_detected() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0";
    std::fs::write(p.join("a.png"), png).unwrap();
    let repo = Repo::discover(p).unwrap();
    let changes = repo.changes(&Changeset::Untracked).unwrap();
    let fd = load_file_diff_with(
        &repo,
        &LanguageRegistry::new(),
        &changes[0],
        &DiffOptions::default(),
        &NoCache,
    )
    .unwrap();
    assert_eq!(fd.kind, DiffKind::Image(ImageFormat::Png));
    assert!(fd.new.bytes.is_some());
}

#[test]
fn whitespace_option_hides_indent_changes() {
    let langs = LanguageRegistry::new();
    let change = FileChange {
        old_path: Some("a.py".into()),
        new_path: Some("a.py".into()),
        kind: lector_git::ChangeKind::Modified,
        old: Side::None,
        new: Side::None,
        binary: None,
    };
    let old = b"if x:\n  y()\n".to_vec();
    let new = b"if x:\n    y()\n".to_vec();
    let exact = diff_bytes(
        change.clone(),
        Some(old.clone()),
        Some(new.clone()),
        &langs,
        &DiffOptions::default(),
        &NoCache,
    );
    assert_eq!(exact.diff.hunks.len(), 1);
    let ws = diff_bytes(
        change,
        Some(old),
        Some(new),
        &langs,
        &diff_options(true),
        &NoCache,
    );
    assert!(ws.diff.hunks.is_empty());
}

#[test]
fn checkout_eol_is_normalized_like_git() {
    // Stored LF, working CRLF (core.autocrlf=true checkout): compared as LF.
    assert_eq!(
        normalize_checkout_eol(Some(b"a\nb\n"), b"a\r\nb\r\n".to_vec()),
        b"a\nb\n"
    );
    // A stored file that already has CR keeps the working bytes untouched.
    assert_eq!(
        normalize_checkout_eol(Some(b"a\r\n"), b"a\r\nb\r\n".to_vec()),
        b"a\r\nb\r\n"
    );
    // Lone CRs are content, not line endings.
    assert_eq!(
        normalize_checkout_eol(Some(b"a\n"), b"x\ry\r\n".to_vec()),
        b"x\ry\n"
    );
    assert_eq!(normalize_checkout_eol(None, b"a\r\n".to_vec()), b"a\r\n");
}

#[test]
fn branch_vs_base_and_default_base() {
    let (_dir, repo) = fixture();
    let p = repo.workdir().to_path_buf();
    git(&p, &["stash", "-u", "-q"]);
    git(&p, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(p.join("feature.rs"), "pub fn f() {}\n").unwrap();
    git(&p, &["add", "."]);
    git(&p, &["commit", "-q", "-m", "Add feature"]);
    assert_eq!(default_base(&repo, "feature").as_deref(), Some("main"));
    let cs = Changeset::BranchVsBase {
        branch: "feature".into(),
        base: "main".into(),
    };
    let view = load_changeset_with(&repo, &cs, &NoMarks, &DiffOptions::default()).unwrap();
    assert_eq!(view.files.len(), 1);
    assert_eq!(view.files[0].path(), "feature.rs");
    let text = file_at_revision(&repo, "main", "lib.rs").unwrap().unwrap();
    assert!(text.contains("let x = 1"));
    let fd = file_vs_revision_with(
        &repo,
        &LanguageRegistry::new(),
        "lib.rs",
        "main",
        &DiffOptions::default(),
        &NoCache,
    )
    .unwrap();
    assert!(fd.is_identical());
}

#[test]
fn vs_workdir_combines_commits_and_worktree() {
    let (dir, repo) = fixture();
    let p = dir.path();
    let first = repo.head().unwrap().oid.unwrap().to_string();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "Second"]);
    std::fs::write(p.join("later.txt"), "new\n").unwrap();
    std::fs::write(p.join("lib.rs"), "fn main() { /* edited */ }\n").unwrap();
    let view = load_vs_workdir_with(&repo, &first, &NoMarks, &DiffOptions::default()).unwrap();
    let paths: Vec<_> = view.files.iter().map(|f| f.path().to_string()).collect();
    assert!(paths.contains(&"lib.rs".to_string()), "{paths:?}");
    assert!(paths.contains(&"later.txt".to_string()), "{paths:?}");
    assert!(paths.contains(&"added.md".to_string()), "{paths:?}");
    let lib = &view.files[view.index_of("lib.rs").unwrap()];
    assert!(matches!(lib.change.new, Side::Workdir(_)));
    assert!(matches!(lib.change.old, Side::Blob(_)));
    assert!(view.title.ends_with("working tree"));
}

#[test]
#[ignore = "perf smoke test: cargo test -p lector-review review_perf -- --ignored --nocapture"]
fn review_perf_500_files() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    let body: String = (0..5000)
        .map(|i| format!("fn f{i}() {{ let x = {i}; }}\n"))
        .collect();
    std::fs::write(p.join("big.rs"), &body).unwrap();
    for i in 0..500 {
        std::fs::write(p.join(format!("f{i}.rs")), format!("fn a() {{}}\n// {i}\n")).unwrap();
    }
    git(p, &["add", "."]);
    git(p, &["commit", "-q", "-m", "init"]);
    std::fs::write(p.join("big.rs"), body.replace("let x = 1", "let y = 1")).unwrap();
    for i in 0..500 {
        std::fs::write(
            p.join(format!("f{i}.rs")),
            format!("fn a() {{ }}\n// {i}!\n"),
        )
        .unwrap();
    }
    let repo = Repo::discover(p).unwrap();
    let t = std::time::Instant::now();
    let view = load_changeset_with(
        &repo,
        &Changeset::WorkingTree,
        &NoMarks,
        &DiffOptions::default(),
    )
    .unwrap();
    eprintln!("changeset of {} files: {:?}", view.files.len(), t.elapsed());
    let big = view.files.iter().find(|f| f.path() == "big.rs").unwrap();
    let t = std::time::Instant::now();
    let fd = load_file_diff_with(
        &repo,
        &LanguageRegistry::new(),
        &big.change,
        &DiffOptions::default(),
        &NoCache,
    )
    .unwrap();
    eprintln!(
        "5k-line file diff (parse both sides): {:?}, {} hunks",
        t.elapsed(),
        fd.diff.hunks.len()
    );
}
