//! `review.fetch_pull_request`: UI-only; fetches into Lector's shadow repository, never the
//! user's; not journaled (it does not write the user's repository); every other caller (CLI,
//! MCP, agents, internal) is refused by the registry before anything runs.

use std::path::Path;
use std::process::Command;

use lector_app::commands::pull_request::shadow_data_dir;
use lector_app::commands::{CommandContext, CommandError, Source};
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
fn the_ui_fetches_into_the_shadow_only_and_every_other_source_is_refused() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = dunce::canonicalize(tmp.path()).expect("canonical");
    let (remote, user, author) = (
        root.join("remote.git"),
        root.join("user"),
        root.join("author"),
    );
    let s = |p: &Path| p.to_string_lossy().into_owned();
    git(&root, &["init", "-q", "--bare", "-b", "main", &s(&remote)]);
    git(&root, &["clone", "-q", &s(&remote), &s(&author)]);
    std::fs::write(author.join("a.txt"), "a\n").expect("write");
    git(&author, &["add", "-A"]);
    git(&author, &["commit", "-q", "-m", "base"]);
    git(&author, &["push", "-q", "origin", "main"]);
    git(&root, &["clone", "-q", &s(&remote), &s(&user)]);
    // The pull request head lives only on the remote, as `refs/pull/1/head` on GitHub.
    std::fs::write(author.join("a.txt"), "pr\n").expect("write");
    git(&author, &["commit", "-q", "-am", "pr"]);
    git(&author, &["push", "-q", "origin", "HEAD:refs/pull/1/head"]);
    let head = git(&author, &["rev-parse", "HEAD"]);
    let user_refs = git(&user, &["for-each-ref"]);

    let core = AppCore::open(CoreOptions::for_tests());
    let journal = Journal::install_in(&core, root.join("data"));
    let mut settings = Settings::default();
    settings.lsp.enabled = false;
    let project = Project::open(&user, settings).expect("project");
    let args = json!({
        "url": s(&remote),
        "refspecs": [
            "+refs/pull/1/head:refs/lector/pr/1/head",
            "+refs/heads/main:refs/lector/pr/1/base",
        ],
        "have": [head],
    });

    for source in [Source::Cli, Source::Mcp, Source::Agent, Source::Internal] {
        let cx = CommandContext::new(core.clone(), Some(project.clone()), source);
        match core
            .registry()
            .invoke("review.fetch_pull_request", args.clone(), &cx)
        {
            Err(CommandError::Unavailable(why)) => {
                assert!(why.contains("only from Lector's window"), "{why}")
            }
            other => panic!("{source:?} was not refused: {other:?}"),
        }
        assert!(!core
            .registry()
            .list_for(source)
            .iter()
            .any(|c| c.spec().id == "review.fetch_pull_request"));
    }
    let repo = project.repo().expect("repo");
    let shadow = ShadowRepo::open(&shadow_data_dir(&core), repo).expect("shadow");
    assert_eq!(
        shadow.resolve(&head).expect("resolve"),
        None,
        "refused: nothing fetched"
    );

    let ui = CommandContext::new(core.clone(), Some(project.clone()), Source::Ui);
    // Validation applies to the UI too: no other refspec shapes, no header injection.
    for bad in [
        json!({"url": s(&remote), "refspecs": ["+refs/heads/main:refs/heads/main"]}),
        json!({"url": s(&remote), "refspecs": ["+refs/pull/1/head:refs/lector/pr/1/head"],
               "http_header": "Authorization: Basic x\r\nX-Evil: 1"}),
        json!({"url": "--upload-pack=x", "refspecs": ["+refs/pull/1/head:refs/lector/pr/1/head"]}),
    ] {
        assert!(core
            .registry()
            .invoke("review.fetch_pull_request", bad.clone(), &ui)
            .is_err());
    }
    let out = core
        .registry()
        .invoke("review.fetch_pull_request", args.clone(), &ui)
        .expect("fetch");
    assert_eq!(out.data["fetched"], true);
    assert!(
        out.journal_id.is_none(),
        "not a write to the user's repository"
    );
    assert!(journal.list(10).is_empty());
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
    assert!(!in_user.success(), "the head is not in the user's objects");

    // Already there: nothing to fetch.
    let again = core
        .registry()
        .invoke("review.fetch_pull_request", args, &ui)
        .expect("nothing to fetch");
    assert_eq!(again.data["fetched"], false);
}
