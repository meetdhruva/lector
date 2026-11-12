//! Journey: review a branch. The queue puts the risky file first and the generated file last;
//! a hunk marked reviewed stays reviewed when a new commit lands, and a hunk whose content
//! changed starts unreviewed again.

use std::sync::Arc;

use lector_app::review_queue::{
    analyze, hunk_fingerprints, AnalysisOptions, FileProgress, HunkReviewStore, QueueGroup,
};
use lector_git::Changeset;
use lector_workspace::review::{self, FileDiff};

use crate::fixture::{Repo, Session};

/// 40 lines of API: edits far apart are separate hunks.
fn api_source() -> String {
    let mut s = String::from("pub fn load(path: &str) -> String {\n    String::new()\n}\n\n");
    for i in 1..=36 {
        s.push_str(&format!("fn helper_{i}() -> u32 {{ {i} }}\n"));
    }
    s
}

/// `main` has the base; `feature` changes the public signature at the top and one helper far
/// below, bumps `Cargo.lock` and edits the README.
fn feature_branch() -> Repo {
    let repo = Repo::init("review");
    repo.write("src/api.rs", &api_source());
    repo.write("README.md", "# review\n");
    repo.write("Cargo.lock", "# lock\nversion = 1\n");
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "feature"]);
    let edited = api_source()
        .replace(
            "pub fn load(path: &str) -> String {",
            "pub fn load(path: &str, strict: bool) -> String {",
        )
        .replace(
            "fn helper_30() -> u32 { 30 }",
            "fn helper_30() -> u32 { 300 }",
        );
    repo.write("src/api.rs", &edited);
    repo.write("README.md", "# review\nnow with notes\n");
    repo.write("Cargo.lock", "# lock\nversion = 2\n");
    repo.commit_all("feature work");
    repo
}

fn branch_changeset() -> Changeset {
    Changeset::BranchVsBase {
        branch: "feature".into(),
        base: "main".into(),
    }
}

/// Loads every file diff of the changeset, in path order (as the review tab does).
fn load_diffs(s: &Session, cs: &Changeset) -> (String, Vec<(String, Arc<FileDiff>)>) {
    let view = review::load_changeset(s.project(), cs, &s.core.review_store()).expect("changeset");
    let opts = review::diff_options(&s.project().settings().diff);
    let files = view
        .files
        .iter()
        .map(|f| {
            let diff = review::load_file_diff(s.project(), &f.change, &opts).expect("file diff");
            (f.path().to_string(), Arc::new(diff))
        })
        .collect();
    (view.key, files)
}

#[test]
fn the_queue_orders_by_risk_and_collapses_generated_files() {
    let repo = feature_branch();
    let s = Session::open(&repo);
    let (key, files) = load_diffs(&s, &branch_changeset());
    assert_eq!(key, "branch:main...feature");
    let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        paths,
        ["Cargo.lock", "README.md", "src/api.rs"],
        "path order in, risk order out"
    );

    let diffs: Vec<Arc<FileDiff>> = files.iter().map(|(_, d)| d.clone()).collect();
    let analysis = analyze(&diffs, &AnalysisOptions::default());
    let order: Vec<&str> = analysis.order.iter().map(|e| paths[e.index]).collect();
    assert_eq!(order.len(), 3);
    assert_eq!(
        order[0], "src/api.rs",
        "the public signature change comes first: {order:?}"
    );
    let api = analysis.entry(2).expect("api.rs in the queue");
    assert_eq!(api.group, QueueGroup::Attention);
    assert!(
        api.top_reason().is_some_and(|r| !r.is_empty()),
        "says why it is risky"
    );
    let lock = analysis.entry(0).expect("Cargo.lock in the queue");
    assert_eq!(
        lock.group,
        QueueGroup::LowRisk,
        "generated files are collapsed"
    );
    let groups: Vec<QueueGroup> = analysis.order.iter().map(|e| e.group).collect();
    let first_low = groups.iter().position(|g| *g == QueueGroup::LowRisk);
    assert!(
        first_low.is_some_and(|i| groups[i..].iter().all(|g| *g == QueueGroup::LowRisk)),
        "low-risk files trail the attention group: {order:?} {groups:?}"
    );
    assert_eq!(analysis.hunks[2].len(), 2, "api.rs has two hunks");
}

#[test]
fn a_reviewed_hunk_survives_a_new_commit_and_a_changed_hunk_does_not() {
    let repo = feature_branch();
    let s = Session::open(&repo);
    let cs = branch_changeset();
    let store = HunkReviewStore::new(s.core.db());
    let progress_of = |reviewed: &[bool]| FileProgress {
        hunks: reviewed.to_vec(),
        viewed: false,
    };

    // --- the reviewer reads the first hunk (the signature) and marks it reviewed
    let (key, files) = load_diffs(&s, &cs);
    let api = &files
        .iter()
        .find(|(p, _)| p == "src/api.rs")
        .expect("api.rs")
        .1;
    let fps = hunk_fingerprints(api);
    assert_eq!(fps.len(), 2);
    let text = repo.read("src/api.rs");
    assert_eq!(
        store.reviewed(&repo.root, &key, "src/api.rs", &fps, Some(&text)),
        [false, false],
        "nothing is reviewed at first"
    );
    store.store_reviewed(&repo.root, &key, "src/api.rs", &fps, &[true, false]);
    let reviewed = store.reviewed(&repo.root, &key, "src/api.rs", &fps, Some(&text));
    assert_eq!(reviewed, [true, false]);
    assert_eq!(
        lector_app::review_queue::totals(&[progress_of(&reviewed)]).hunks_done,
        1
    );

    // --- a new commit lands with an unrelated change between the two hunks
    let more = repo.read("src/api.rs").replace(
        "fn helper_15() -> u32 { 15 }",
        "fn helper_15() -> u32 { 150 }",
    );
    repo.write("src/api.rs", &more);
    repo.commit_all("one more change");

    let (key_after, files) = load_diffs(&s, &cs);
    assert_eq!(key_after, key, "same review, new revision");
    let api = &files
        .iter()
        .find(|(p, _)| p == "src/api.rs")
        .expect("api.rs")
        .1;
    let fps = hunk_fingerprints(api);
    assert_eq!(fps.len(), 3, "the new change is a third hunk");
    let text = repo.read("src/api.rs");
    assert_eq!(
        store.reviewed(&repo.root, &key, "src/api.rs", &fps, Some(&text)),
        [true, false, false],
        "the reviewed hunk is still reviewed; the new and the old unreviewed ones are not"
    );

    // --- the reviewed hunk itself changes: back to unreviewed
    let changed = repo.read("src/api.rs").replace(
        "pub fn load(path: &str, strict: bool) -> String {",
        "pub fn load(path: &str, strict: bool, retries: u8) -> String {",
    );
    repo.write("src/api.rs", &changed);
    repo.commit_all("signature changed again");
    let (_, files) = load_diffs(&s, &cs);
    let api = &files
        .iter()
        .find(|(p, _)| p == "src/api.rs")
        .expect("api.rs")
        .1;
    let fps = hunk_fingerprints(api);
    let text = repo.read("src/api.rs");
    let reviewed = store.reviewed(&repo.root, &key, "src/api.rs", &fps, Some(&text));
    assert_eq!(reviewed.len(), 3);
    assert!(
        !reviewed[0],
        "a hunk whose content changed must be looked at again: {reviewed:?}"
    );

    // --- review state lives in Lector's database, never in the repository
    assert_eq!(repo.git(&["status", "--porcelain"]).trim(), "");
}
