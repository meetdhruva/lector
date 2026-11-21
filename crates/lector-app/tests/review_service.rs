//! `ReviewService` (ADR 0001): every method returns what the direct `lector-review` loader
//! returns for the same inputs, the viewed marks round-trip through the same store and file
//! format as 0.7.1, and a cancelled call returns without doing the work.

use std::path::Path;
use std::process::Command;
use std::sync::Once;
use std::time::{Duration, Instant};

use lector_app::content_cache::{CacheConfig, ContentCache};
use lector_app::review_service::ReviewService;
use lector_base::CancelToken;
use lector_config::ReviewStore;
use lector_diff::DiffOptions;
use lector_git::{ChangeKind, Changeset, FileChange, Repo, Side};
use lector_review::review::{self, ChangesetView, FileDiff, NoMarks, ReviewFile};
use lector_review::NoCache;
use lector_syntax::LanguageRegistry;

fn hermetic() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
    });
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(dir: &Path, rel: &str, content: &[u8]) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, content).expect("write");
}

/// A repository with a commit, a second commit, staged and unstaged edits, a rename, a
/// deletion, a binary and an untracked file, and a `feature` branch.
fn fixture() -> (tempfile::TempDir, Repo) {
    hermetic();
    let dir = tempfile::tempdir().expect("tempdir");
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    git(p, &["config", "user.name", "T"]);
    git(p, &["config", "user.email", "t@example.com"]);
    git(p, &["config", "core.autocrlf", "false"]);
    write(
        p,
        "lib.rs",
        b"fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n",
    );
    write(p, "old.txt", b"a\nb\nc\nd\ne\nf\ng\nh\n");
    write(p, "gone.txt", b"bye\n");
    git(p, &["add", "."]);
    git(p, &["commit", "-q", "-m", "Initial commit"]);
    write(p, "second.txt", b"two\n");
    git(p, &["add", "."]);
    git(p, &["commit", "-q", "-m", "Second commit"]);
    git(p, &["branch", "feature"]);
    write(
        p,
        "lib.rs",
        b"fn main() {\n    let x = 2;\n    println!(\"{x}\");\n}\n",
    );
    git(p, &["mv", "old.txt", "new.txt"]);
    std::fs::remove_file(p.join("gone.txt")).expect("remove");
    write(p, "added.md", b"# Title\n\ntext\n");
    git(p, &["add", "added.md"]);
    write(p, "blob.bin", &[0u8, 1, 2, 3, 0, 9]);
    let repo = Repo::discover(p).expect("discover");
    (dir, repo)
}

fn service(dir: &Path) -> ReviewService {
    ReviewService::with_cache(
        ReviewStore::open(dir.join("review.json")),
        ContentCache::new(CacheConfig::default()),
    )
}

fn view_text(v: &ChangesetView) -> String {
    format!("{v:#?}")
}

fn diff_text(d: &FileDiff) -> String {
    let side = |s: &review::DiffSide| {
        format!(
            "{:?} {} {} {:?} {:?} {} {}",
            s.path,
            s.exists,
            s.size,
            s.text.as_ref().map(|t| t.to_string()),
            s.language.as_ref().map(|l| l.name.clone()),
            s.syntax.is_some(),
            s.encoding
        )
    };
    format!(
        "{:?}\n{:?}\n{:?}\n{}\n{}\n{:#?}\n{:#?}\n{:#?}",
        d.change,
        d.kind,
        d.content_id(),
        side(&d.old),
        side(&d.new),
        d.diff,
        d.split,
        d.unified
    )
}

#[test]
fn changesets_equal_the_direct_loaders() {
    let (dir, repo) = fixture();
    let st = tempfile::tempdir().expect("tempdir");
    let svc = service(st.path());
    let opts = DiffOptions::default();
    let cancel = CancelToken::new();
    let head = repo.head().expect("head").oid.expect("oid");
    let first = repo.resolve("HEAD~1").expect("first commit").to_string();
    let changesets = [
        Changeset::WorkingTree,
        Changeset::Unstaged,
        Changeset::Staged,
        Changeset::Untracked,
        Changeset::Commit(head),
        Changeset::Range {
            from: first.clone(),
            to: "HEAD".into(),
        },
        Changeset::BranchVsBase {
            branch: "feature".into(),
            base: "main".into(),
        },
    ];
    for cs in &changesets {
        let direct = review::load_changeset_with(&repo, cs, &NoMarks, &opts).expect("direct");
        let via = svc.changeset(&repo, cs, &opts, &cancel).expect("service");
        assert_eq!(view_text(&direct), view_text(&via), "{cs:?}");
    }
    let direct = review::load_vs_workdir_with(&repo, &first, &NoMarks, &opts).expect("direct");
    let via = svc
        .changeset_vs_workdir(&repo, &first, &opts, &cancel)
        .expect("service");
    assert!(!via.files.is_empty());
    assert_eq!(view_text(&direct), view_text(&via));
    assert_eq!(
        svc.default_base(&repo, "feature", &cancel),
        review::default_base(&repo, "feature")
    );
    assert_eq!(
        svc.default_base(&repo, "feature", &cancel).as_deref(),
        Some("main")
    );
    assert_eq!(
        svc.changeset_title(Some(&repo), &Changeset::Commit(head)),
        review::changeset_title(Some(&repo), &Changeset::Commit(head))
    );
    drop(dir);
}

#[test]
fn file_diffs_and_summaries_equal_the_direct_loaders_cold_and_warm() {
    let (dir, repo) = fixture();
    let st = tempfile::tempdir().expect("tempdir");
    let svc = service(st.path());
    let opts = DiffOptions::default();
    let cancel = CancelToken::new();
    let languages = LanguageRegistry::new();
    let view =
        review::load_changeset_with(&repo, &Changeset::WorkingTree, &NoMarks, &opts).expect("view");
    assert!(view.files.len() >= 4);
    for round in 0..2 {
        for f in &view.files {
            let direct = review::load_file_diff_with(&repo, &languages, &f.change, &opts, &NoCache)
                .expect("direct");
            let via = svc
                .file_diff(&repo, &languages, &f.change, &opts, &cancel)
                .expect("service");
            assert_eq!(
                diff_text(&direct),
                diff_text(&via),
                "{} round {round}",
                f.path()
            );
            let direct = review::file_summary(&repo, &f.change, &opts).expect("direct");
            let via = svc
                .file_summary(&repo, &f.change, &opts, &cancel)
                .expect("service");
            assert_eq!(direct, via, "{}", f.path());
        }
        for (path, rev) in [
            ("lib.rs", "HEAD"),
            ("lib.rs", "HEAD~1"),
            ("added.md", "HEAD"),
        ] {
            let direct =
                review::file_vs_revision_with(&repo, &languages, path, rev, &opts, &NoCache)
                    .expect("direct");
            let via = svc
                .file_vs_revision(&repo, &languages, path, rev, &opts, &cancel)
                .expect("service");
            assert_eq!(diff_text(&direct), diff_text(&via), "{path} vs {rev}");
        }
    }
    drop(dir);
}

fn lib_file(view: &ChangesetView) -> &ReviewFile {
    &view.files[view.index_of("lib.rs").expect("lib.rs")]
}

#[test]
fn viewed_state_round_trips_through_the_store_file() {
    let (dir, repo) = fixture();
    let st = tempfile::tempdir().expect("tempdir");
    let store_file = st.path().join("review.json");
    let opts = DiffOptions::default();
    let cancel = CancelToken::new();
    let svc = service(st.path());
    let mut view = svc
        .changeset(&repo, &Changeset::WorkingTree, &opts, &cancel)
        .expect("view");
    assert_eq!(view.viewed_count(), 0);
    let root = repo.workdir().to_path_buf();
    let key = view.key.clone();
    let ix = view.index_of("lib.rs").expect("lib.rs");
    svc.set_viewed(&root, &key, &mut view.files[ix], true)
        .expect("set");
    assert!(view.files[ix].viewed);
    assert!(svc.is_viewed(&root, &key, "lib.rs", &view.files[ix].content_hash));
    assert!(store_file.exists(), "the mark is on disk");

    // A new service over the same file (a restart) sees it.
    let again = service(st.path());
    let view2 = again
        .changeset(&repo, &Changeset::WorkingTree, &opts, &cancel)
        .expect("reload");
    assert!(lib_file(&view2).viewed);
    assert_eq!(view2.viewed_count(), 1);

    // Editing the file un-views it; viewing is per content.
    std::fs::write(dir.path().join("lib.rs"), "fn main() {}\n").expect("edit");
    let view3 = again
        .changeset(&repo, &Changeset::WorkingTree, &opts, &cancel)
        .expect("reload");
    assert!(!lib_file(&view3).viewed);

    // Unmarking persists too.
    let mut view4 = view2.clone();
    again
        .set_viewed(&root, &key, &mut view4.files[ix], false)
        .expect("unset");
    assert!(!view4.files[ix].viewed);
    let view5 = service(st.path())
        .changeset(&repo, &Changeset::WorkingTree, &opts, &cancel)
        .expect("reload");
    assert_eq!(view5.viewed_count(), 0);
}

#[test]
fn the_vs_workdir_view_marks_under_its_own_key() {
    let (_dir, repo) = fixture();
    let st = tempfile::tempdir().expect("tempdir");
    let svc = service(st.path());
    let (opts, cancel) = (DiffOptions::default(), CancelToken::new());
    let mut view = svc
        .changeset_vs_workdir(&repo, "HEAD~1", &opts, &cancel)
        .expect("view");
    assert_eq!(view.key, "workdir:HEAD~1");
    let ix = view.index_of("lib.rs").expect("lib.rs");
    let key = view.key.clone();
    svc.set_viewed(repo.workdir(), &key, &mut view.files[ix], true)
        .expect("set");
    let again = svc
        .changeset_vs_workdir(&repo, "HEAD~1", &opts, &cancel)
        .expect("reload");
    assert!(lib_file(&again).viewed);
    // The same file under the working-tree key is not marked.
    let wt = svc
        .changeset(&repo, &Changeset::WorkingTree, &opts, &cancel)
        .expect("wt");
    assert!(!lib_file(&wt).viewed);
}

// The file written by 0.7.1: `ReviewStore::set_viewed(root, key, path, hash, true)` straight
// from `lector_review::review::set_viewed_with` (this fixture was produced by that code path
// before it was removed; `lector-config`, which owns the format, is unchanged).
const FIXTURE_0_7_1: &str = include_str!("fixtures/review-0.7.1.json");
const FIXTURE_ROOT: &str = "/fixture/repo";
const FIXTURE_HASH: &str = "536e506bb90914c243a12b397b9a998f85ae2cbd9ba02dfd03a9e155ca5ca0f4";

fn fake_file(path: &str, hash: &str) -> ReviewFile {
    ReviewFile {
        change: FileChange {
            old_path: Some(path.into()),
            new_path: Some(path.into()),
            kind: ChangeKind::Modified,
            old: Side::Index(path.into()),
            new: Side::Workdir(path.into()),
            binary: None,
        },
        display_path: path.into(),
        stats: None,
        viewed: false,
        content_hash: hash.into(),
        binary: false,
    }
}

fn entries(json: &str) -> Vec<serde_json::Value> {
    let mut v: serde_json::Value = serde_json::from_str(json).expect("json");
    let mut list = v["entries"].as_array().cloned().expect("entries");
    for e in &mut list {
        // The timestamp is the only part that varies between runs.
        e.as_object_mut().expect("object").remove("at");
    }
    list.sort_by_key(|e| e.to_string());
    v["entries"] = serde_json::Value::Null;
    list
}

#[test]
fn the_file_of_0_7_1_is_read_and_the_written_file_has_the_same_shape() {
    let st = tempfile::tempdir().expect("tempdir");
    let file = st.path().join("review.json");
    std::fs::write(&file, FIXTURE_0_7_1).expect("fixture");
    let svc = ReviewService::new(ReviewStore::open(&file));
    let root = Path::new(FIXTURE_ROOT);
    assert!(svc.is_viewed(root, "worktree", "src/lib.rs", FIXTURE_HASH));
    assert!(!svc.is_viewed(root, "worktree", "src/lib.rs", "other-hash"));

    // Marking another file keeps the old entry and adds one of the 0.7.1 shape.
    let mut f = fake_file("src/main.rs", "abc123");
    svc.set_viewed(root, "worktree", &mut f, true).expect("set");
    assert!(f.viewed);
    let after = std::fs::read_to_string(&file).expect("read");
    let new_entries = entries(&after);
    let old_entries = entries(FIXTURE_0_7_1);
    assert_eq!(new_entries.len(), old_entries.len() + 1);
    for e in &old_entries {
        assert!(new_entries.contains(e), "{e} kept");
    }
    // The raw bytes: compact JSON, `{"entries":[{"repo":..,"changeset":..,"path":..,"hash":..,"at":..}]}`.
    assert!(after.starts_with("{\"entries\":[{\"repo\":\""), "{after}");
    assert!(after.contains(
        "\"changeset\":\"worktree\",\"path\":\"src/main.rs\",\"hash\":\"abc123\",\"at\":"
    ));
    assert!(!after.contains('\n'));

    // Reference: what the 0.7.1 call (`ReviewStore::set_viewed` with the same arguments) writes
    // equals what the service wrote, up to the timestamp.
    let ref_file = st.path().join("reference.json");
    std::fs::write(&ref_file, FIXTURE_0_7_1).expect("fixture");
    let reference = ReviewStore::open(&ref_file);
    reference
        .set_viewed(root, "worktree", "src/main.rs", "abc123", true)
        .expect("reference");
    let ref_text = std::fs::read_to_string(&ref_file).expect("read");
    assert_eq!(entries(&ref_text), new_entries);
    assert_eq!(
        ref_text.replace(|c: char| c.is_ascii_digit(), "0").len(),
        after.replace(|c: char| c.is_ascii_digit(), "0").len()
    );
}

#[test]
fn a_store_that_cannot_write_reports_it_and_leaves_the_file_unmarked() {
    let st = tempfile::tempdir().expect("tempdir");
    // The store's file is a directory: writing fails.
    let file = st.path().join("review.json");
    std::fs::create_dir(&file).expect("dir");
    let svc = ReviewService::new(ReviewStore::open(&file));
    let mut f = fake_file("a.rs", "h");
    assert!(svc
        .set_viewed(Path::new("/r"), "worktree", &mut f, true)
        .is_err());
    assert!(!f.viewed, "the flag changes only when the write succeeded");
    // Without a store nothing is viewed and writing fails.
    let none = ReviewService::without_marks();
    assert!(!none.is_viewed(Path::new("/r"), "worktree", "a.rs", "h"));
    assert!(none
        .set_viewed(Path::new("/r"), "worktree", &mut f, true)
        .is_err());
}

fn cancelled<T>(r: &Result<T, lector_review::Error>) -> bool {
    matches!(r, Err(lector_review::Error::Cancelled))
}

#[test]
fn a_cancelled_token_stops_every_method_at_once() {
    let (_dir, repo) = fixture();
    let st = tempfile::tempdir().expect("tempdir");
    let svc = service(st.path());
    let opts = DiffOptions::default();
    let languages = LanguageRegistry::new();
    let cancel = CancelToken::new();
    cancel.cancel();
    let change = repo
        .changes(&Changeset::Unstaged)
        .expect("changes")
        .remove(0);
    let t = Instant::now();
    assert!(cancelled(&svc.changeset(
        &repo,
        &Changeset::WorkingTree,
        &opts,
        &cancel
    )));
    assert!(cancelled(
        &svc.changeset_vs_workdir(&repo, "HEAD", &opts, &cancel)
    ));
    assert!(cancelled(
        &svc.file_diff(&repo, &languages, &change, &opts, &cancel)
    ));
    assert!(cancelled(&svc.file_vs_revision(
        &repo, &languages, "lib.rs", "HEAD", &opts, &cancel
    )));
    assert!(cancelled(&svc.file_summary(&repo, &change, &opts, &cancel)));
    assert_eq!(svc.default_base(&repo, "feature", &cancel), None);
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
}

#[test]
fn cancelling_a_large_changeset_while_it_loads_returns_promptly() {
    hermetic();
    let dir = tempfile::tempdir().expect("tempdir");
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    git(p, &["config", "user.name", "T"]);
    git(p, &["config", "user.email", "t@example.com"]);
    git(p, &["config", "core.autocrlf", "false"]);
    let body = |n: usize, v: u32| -> String {
        (0..400)
            .map(|i| format!("fn f{n}_{i}() -> u32 {{ {v} + {i} }}\n"))
            .collect()
    };
    for n in 0..300 {
        write(p, &format!("src/m{n}.rs"), body(n, 1).as_bytes());
    }
    git(p, &["add", "."]);
    git(p, &["commit", "-q", "-m", "many files"]);
    for n in 0..300 {
        write(p, &format!("src/m{n}.rs"), body(n, 2).as_bytes());
    }
    let repo = Repo::discover(p).expect("discover");
    let st = tempfile::tempdir().expect("tempdir");
    let svc = service(st.path());
    let opts = DiffOptions::default();

    let t = Instant::now();
    let full = svc
        .changeset(&repo, &Changeset::Unstaged, &opts, &CancelToken::new())
        .expect("uncancelled");
    let full_time = t.elapsed();
    assert_eq!(full.files.len(), 300);

    let cancel = CancelToken::new();
    let canceller = {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            cancel.cancel();
        })
    };
    let t = Instant::now();
    let result = svc.changeset(&repo, &Changeset::Unstaged, &opts, &cancel);
    let elapsed = t.elapsed();
    canceller.join().expect("canceller");
    eprintln!("full load {full_time:?}; cancelled call returned after {elapsed:?}");
    match result {
        Err(lector_review::Error::Cancelled) => {
            assert!(
                elapsed < full_time.max(Duration::from_millis(200)) + Duration::from_secs(1),
                "cancelled after {elapsed:?}, a full load takes {full_time:?}"
            );
        }
        // The load finished before the flag was seen (a very fast machine): still correct.
        Ok(view) => assert_eq!(view.files.len(), 300),
        Err(other) => panic!("unexpected error {other}"),
    }
}
