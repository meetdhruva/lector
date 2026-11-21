//! `review.fetch_pull_request`: fetches into Lector's shadow repository, never the user's;
//! not journaled (it does not write the user's repository); sandboxed callers may only fetch
//! from a configured remote.

use std::path::Path;
use std::process::Command;

use lector_app::commands::pull_request::shadow_data_dir;
use lector_app::commands::{CommandContext, Source};
use lector_app::journal::Journal;
use lector_app::{AppCore, CoreOptions, Project};
use lector_config::Settings;
use lector_git::ShadowRepo;
use serde_json::json;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Ann")
        .env("GIT_AUTHOR_EMAIL", "ann@example.com")
        .env("GIT_COMMITTER_NAME", "Ann")
        .env("GIT_COMMITTER_EMAIL", "ann@example.com")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn fetches_into_the_shadow_only_and_sandboxes_the_url() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = dunce::canonicalize(tmp.path()).expect("canonical");
    let (user, forge) = (root.join("user"), root.join("forge"));
    let s = |p: &Path| p.to_string_lossy().into_owned();
    git(&root, &["init", "-q", "-b", "main", &s(&forge)]);
    std::fs::write(forge.join("a.txt"), "a\n").expect("write");
    git(&forge, &["add", "-A"]);
    git(&forge, &["commit", "-q", "-m", "base"]);
    git(&root, &["clone", "-q", &s(&forge), &s(&user)]);
    git(&forge, &["checkout", "-q", "-b", "pr"]);
    std::fs::write(forge.join("a.txt"), "pr\n").expect("write");
    git(&forge, &["commit", "-q", "-am", "pr"]);
    let head = git(&forge, &["rev-parse", "HEAD"]);
    let user_refs = git(&user, &["for-each-ref"]);

    let core = AppCore::open(CoreOptions::for_tests());
    let journal = Journal::install_in(&core, root.join("data"));
    let mut settings = Settings::default();
    settings.lsp.enabled = false;
    let project = Project::open(&user, settings).expect("project");
    let args = |url: &str| {
        json!({
            "url": url,
            "refspecs": ["+refs/heads/pr:refs/lector/pr/1/head"],
            "have": [head],
        })
    };

    // An MCP client may not fetch from a URL that is not a remote of the repository.
    let mcp = CommandContext::new(core.clone(), Some(project.clone()), Source::Mcp);
    let elsewhere = root.join("elsewhere");
    let err = core
        .registry()
        .invoke("review.fetch_pull_request", args(&s(&elsewhere)), &mcp)
        .expect_err("refused");
    assert!(err.to_string().contains("not a remote"), "{err}");

    // The configured remote (the clone's origin) is fine for it.
    let out = core
        .registry()
        .invoke("review.fetch_pull_request", args(&s(&forge)), &mcp)
        .expect("fetch");
    assert_eq!(out.data["fetched"], true);
    assert!(
        out.journal_id.is_none(),
        "not a write to the user's repository"
    );
    assert!(journal.list(10).is_empty());
    let repo = project.repo().expect("repo");
    let shadow = ShadowRepo::open(&shadow_data_dir(&core), repo).expect("shadow");
    assert_eq!(
        shadow
            .resolve(&head)
            .expect("resolve")
            .map(|o| o.to_string()),
        Some(head.clone())
    );
    assert_eq!(
        git(&user, &["for-each-ref"]),
        user_refs,
        "user refs untouched"
    );
    let in_user = Command::new("git")
        .arg("-C")
        .arg(&user)
        .args(["cat-file", "-e", &head])
        .status()
        .expect("git runs");
    assert!(
        !in_user.success(),
        "the pull request head is not in the user's objects"
    );

    // Already there: nothing to fetch, even for the sandboxed caller.
    let again = core
        .registry()
        .invoke("review.fetch_pull_request", args(&s(&elsewhere)), &mcp)
        .expect("nothing to fetch");
    assert_eq!(again.data["fetched"], false);
}
