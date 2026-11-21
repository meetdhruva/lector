//! The review functions run with nothing but the handles passed in (ADR 0001): a `Repo`,
//! a language registry, a `ReviewStore` and `NoCache`. No project, no app state.

use std::path::Path;
use std::process::Command;

use lector_config::ReviewStore;
use lector_diff::DiffOptions;
use lector_git::{Changeset, Repo};
use lector_review::review::{self, DiffKind};
use lector_review::NoCache;
use lector_syntax::LanguageRegistry;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@e.st")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@e.st")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn review_functions_run_with_only_a_repo() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    std::fs::write(p.join("lib.rs"), "fn a() -> u32 {\n    1\n}\n").unwrap();
    git(p, &["add", "."]);
    git(p, &["commit", "-q", "-m", "init"]);
    std::fs::write(p.join("lib.rs"), "fn a() -> u32 {\n    2\n}\n").unwrap();
    let repo = Repo::discover(p).unwrap();
    let st = tempfile::tempdir().unwrap();
    let store = ReviewStore::open(st.path().join("review-state.json"));
    let opts = DiffOptions::default();

    let view = review::load_changeset_with(&repo, &Changeset::Unstaged, &store, &opts).unwrap();
    assert_eq!(view.files.len(), 1);
    assert_eq!(view.total_stats().added, 1);
    let langs = LanguageRegistry::new();
    let fd =
        review::load_file_diff_with(&repo, &langs, &view.files[0].change, &opts, &NoCache).unwrap();
    assert_eq!(fd.kind, DiffKind::Text);
    assert!(fd.new.syntax.is_some());
    let vs =
        review::file_vs_revision_with(&repo, &langs, "lib.rs", "HEAD", &opts, &NoCache).unwrap();
    assert_eq!(vs.stats(), fd.stats());
}
