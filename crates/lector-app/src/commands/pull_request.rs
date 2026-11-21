//! `review.fetch_pull_request`: fetches a pull request's commits into Lector's **shadow
//! repository** (`lector_git::ShadowRepo`, under Lector's data directory), never into the
//! user's repository, so a pull request can be reviewed without touching the user's refs.
//!
//! Declared `writes: false`: [`CommandSpec::writes`] means "writes to the user's files or
//! repository", and this command writes only Lector's own storage (refs under
//! `refs/lector/pr/` of the shadow repository; destinations elsewhere are refused by
//! `ShadowRepo::fetch_with`). It is not journaled and has nothing to undo; a read-only MCP
//! server may run it. Sandboxed callers (MCP, agents) may only fetch from a URL that is
//! configured as a remote of the project's repository; the GUI passes the forge's remote URL.
//!
//! The optional `http_header` (an `Authorization` header for private repositories) is handed
//! to git through the environment, never logged; reads are not journaled, so it is not stored.

use std::path::PathBuf;

use anyhow::{anyhow, bail};
use lector_git::{Repo, ShadowRepo};
use serde_json::{json, Value};

use super::git_write::needs_repo;
use super::schema::{object, prop};
use super::{CommandRegistry, CommandSpec, FnCommand, Outcome};
use crate::core::AppCore;
use crate::journal::Journal;

/// Registers `review.fetch_pull_request`.
pub fn register(r: &CommandRegistry) {
    r.register(fetch_pull_request());
}

/// Where the shadow repositories of `core` live: the journal's data directory when one is
/// installed (tests use their own), else `lector_config::paths::data_dir()`. Every command
/// that backs up into or fetches into a shadow repository uses this directory, and readers of
/// the shadow repository (the pull request review) must use it too.
pub fn shadow_data_dir(core: &AppCore) -> PathBuf {
    Journal::of(core)
        .map(|j| j.data_dir().to_path_buf())
        .unwrap_or_else(lector_config::paths::data_dir)
}

fn strings(args: &Value, key: &str) -> anyhow::Result<Vec<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(list)) => list
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow!("`{key}` must be strings"))
            })
            .collect(),
        Some(_) => bail!("`{key}` must be an array of strings"),
    }
}

/// Whether `url` is the URL of one of `repo`'s remotes (separators and a trailing `/` aside,
/// so a local path matches however git stored it).
fn is_configured_remote(repo: &Repo, url: &str) -> anyhow::Result<bool> {
    let norm = |u: &str| u.replace('\\', "/").trim_end_matches('/').to_owned();
    let wanted = norm(url);
    Ok(repo.remotes()?.iter().any(|r| norm(&r.url) == wanted))
}

fn fetch_pull_request() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "review.fetch_pull_request",
            "Fetch Pull Request for Review",
            "Review",
            object(
                &[
                    ("url", prop("string", "Remote URL to fetch from (MCP clients and agents: a URL configured as a remote of the project's repository)")),
                    ("refspecs", json!({"type": "array", "items": {"type": "string"}, "description": "Refspecs whose destinations are under refs/lector/ (e.g. +refs/pull/12/head:refs/lector/pr/12/head)"})),
                    ("have", json!({"type": "array", "items": {"type": "string"}, "description": "Commits that make the fetch unnecessary when the shadow repository already has all of them"})),
                    ("http_header", prop("string", "Extra HTTP header for git (Authorization for private repositories); never logged or stored")),
                ],
                &["url", "refspecs"],
            ),
        ),
        |cx, args| {
            let repo = cx.require_repo()?;
            let url = args.get("url").and_then(Value::as_str).unwrap_or_default();
            let refspecs = strings(args, "refspecs")?;
            let have = strings(args, "have")?;
            let shadow = ShadowRepo::open(&shadow_data_dir(&cx.core), repo)?;
            let present =
                |sha: &String| !sha.is_empty() && matches!(shadow.resolve(sha), Ok(Some(_)));
            if !have.is_empty() && have.iter().all(present) {
                return Ok(Outcome::new("The pull request's commits are already fetched")
                    .with_data(json!({ "fetched": false })));
            }
            if cx.is_sandboxed() && !is_configured_remote(repo, url)? {
                bail!("{url} is not a remote of this repository");
            }
            let refs: Vec<&str> = refspecs.iter().map(String::as_str).collect();
            let opts = lector_git::shadow::FetchOptions {
                http_header: args
                    .get("http_header")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            };
            shadow.fetch_with(url, &refs, &opts)?;
            Ok(
                Outcome::new("Fetched the pull request into Lector's review storage")
                    .with_data(json!({ "fetched": true })),
            )
        },
    )
    .with_available(needs_repo)
}
