//! `review.fetch_pull_request`: fetches a pull request's commits into Lector's **shadow
//! repository** (`lector_git::ShadowRepo`, under Lector's data directory), never into the
//! user's repository, so a pull request can be reviewed without touching the user's refs.
//!
//! **UI-only** ([`Sources::UiOnly`](super::Sources::UiOnly)): it is the Pull Requests panel's
//! fetch, run through the registry, not an API. The registry refuses it for the CLI, MCP
//! clients (read-only or not) and agents, and front-ends do not list it: it uses the network
//! with the user's credentials and writes Lector's storage, which no other caller could do
//! before.
//!
//! Declared `writes: false`: [`CommandSpec::writes`] means "writes to the user's files or
//! repository", and this command writes only Lector's own storage. It is not journaled and has
//! nothing to undo.
//!
//! Inputs are validated even from the UI ([`check_refspecs`], [`check_http_header`]): only the
//! refspecs the forge code produces (`refs/pull/<n>/head` or `refs/merge-requests/<n>/head` →
//! `refs/lector/pr/<n>/head`, and the base branch `refs/heads/<branch>` →
//! `refs/lector/pr/<n>/base`), at most one of each; `http_header` is one `Authorization:` line.
//! The header reaches git through the environment (`ShadowRepo::fetch_with`), never argv, and is
//! never logged; reads are not journaled, so it is not stored.

use std::path::PathBuf;

use anyhow::{anyhow, bail};
use lector_git::ShadowRepo;
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

fn bad_text(s: &str) -> bool {
    s.chars().any(char::is_control)
}

/// A `<digits>` pull request number (no sign, no leading zeros beyond "0").
fn pr_number(s: &str) -> Option<&str> {
    let ok = !s.is_empty()
        && s.len() <= 20
        && s.bytes().all(|b| b.is_ascii_digit())
        && (s == "0" || !s.starts_with('0'));
    ok.then_some(s)
}

/// A branch name git would accept in `refs/heads/<name>` (the rules of
/// `git check-ref-format` that matter here).
fn branch_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with('-')
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.ends_with('.')
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("@{")
        && name != "@"
        && !name
            .chars()
            .any(|c| c.is_control() || " ~^:?*[\\".contains(c))
        && name.split('/').all(|part| !part.starts_with('.'))
}

/// Checks the refspecs of a pull request fetch: only the shapes `lector_forge` produces, each
/// storing into `refs/lector/pr/<n>/`, all for the same pull request, at most one head and
/// one base.
pub fn check_refspecs(refspecs: &[String]) -> anyhow::Result<()> {
    if refspecs.is_empty() {
        bail!("nothing to fetch");
    }
    let (mut heads, mut bases) = (0, 0);
    let mut number: Option<&str> = None;
    for spec in refspecs {
        let bad = || anyhow!("refspec {spec:?} is not a pull request fetch");
        if bad_text(spec) {
            return Err(bad());
        }
        let body = spec.strip_prefix('+').unwrap_or(spec);
        let (src, dst) = body.split_once(':').ok_or_else(bad)?;
        let n = dst
            .strip_prefix("refs/lector/pr/")
            .and_then(|rest| rest.split_once('/'))
            .ok_or_else(bad)?;
        let (n, kind) = (pr_number(n.0).ok_or_else(bad)?, n.1);
        match kind {
            "head" => {
                let src_n = src
                    .strip_prefix("refs/pull/")
                    .or_else(|| src.strip_prefix("refs/merge-requests/"))
                    .and_then(|rest| rest.strip_suffix("/head"))
                    .ok_or_else(bad)?;
                if src_n != n {
                    return Err(bad());
                }
                heads += 1;
            }
            "base" => {
                let branch = src.strip_prefix("refs/heads/").ok_or_else(bad)?;
                if !branch_name_ok(branch) {
                    return Err(bad());
                }
                bases += 1;
            }
            _ => return Err(bad()),
        }
        if number.is_some_and(|m| m != n) {
            bail!("the refspecs name different pull requests");
        }
        number = Some(n);
    }
    if heads > 1 || bases > 1 {
        bail!("a pull request fetch has at most one head and one base refspec");
    }
    Ok(())
}

/// Checks an `http_header`: exactly one `Authorization: <credentials>` line, no CR / LF or
/// other control characters (header injection), bounded length.
pub fn check_http_header(header: &str) -> anyhow::Result<()> {
    let value = header
        .strip_prefix("Authorization: ")
        .ok_or_else(|| anyhow!("the HTTP header must be an Authorization header"))?;
    if value.trim().is_empty() || header.len() > 8192 || bad_text(header) {
        bail!("the Authorization header is malformed");
    }
    Ok(())
}

/// A commit id (hex, 4..=64 digits) or empty (unknown).
fn check_have(sha: &str) -> anyhow::Result<()> {
    if sha.is_empty()
        || (sha.len() >= 4 && sha.len() <= 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        Ok(())
    } else {
        bail!("{sha:?} is not a commit id")
    }
}

fn fetch_pull_request() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "review.fetch_pull_request",
            "Fetch Pull Request for Review",
            "Review",
            object(
                &[
                    ("url", prop("string", "The forge remote's URL")),
                    ("refspecs", json!({"type": "array", "items": {"type": "string"}, "description": "+refs/pull/<n>/head (or refs/merge-requests/<n>/head):refs/lector/pr/<n>/head and +refs/heads/<base>:refs/lector/pr/<n>/base"})),
                    ("have", json!({"type": "array", "items": {"type": "string"}, "description": "Commit ids that make the fetch unnecessary when the shadow repository already has all of them"})),
                    ("http_header", prop("string", "`Authorization: ...` for private repositories; never logged or stored")),
                ],
                &["url", "refspecs"],
            ),
        )
        .ui_only(),
        |cx, args| {
            let repo = cx.require_repo()?;
            let url = args.get("url").and_then(Value::as_str).unwrap_or_default();
            if url.is_empty() || url.starts_with('-') || bad_text(url) {
                bail!("invalid fetch URL {url:?}");
            }
            let refspecs = strings(args, "refspecs")?;
            check_refspecs(&refspecs)?;
            let have = strings(args, "have")?;
            for sha in &have {
                check_have(sha)?;
            }
            let http_header = args
                .get("http_header")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let Some(h) = &http_header {
                check_http_header(h)?;
            }
            let shadow = ShadowRepo::open(&shadow_data_dir(&cx.core), repo)?;
            let present =
                |sha: &String| !sha.is_empty() && matches!(shadow.resolve(sha), Ok(Some(_)));
            if !have.is_empty() && have.iter().all(present) {
                return Ok(Outcome::new("The pull request's commits are already fetched")
                    .with_data(json!({ "fetched": false })));
            }
            let refs: Vec<&str> = refspecs.iter().map(String::as_str).collect();
            let opts = lector_git::shadow::FetchOptions { http_header };
            shadow.fetch_with(url, &refs, &opts)?;
            Ok(
                Outcome::new("Fetched the pull request into Lector's review storage")
                    .with_data(json!({ "fetched": true })),
            )
        },
    )
    .with_available(needs_repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specs(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn only_forge_refspecs_into_the_shadow_namespace_pass() {
        for ok in [
            &["+refs/pull/12/head:refs/lector/pr/12/head"][..],
            &["refs/merge-requests/7/head:refs/lector/pr/7/head"],
            &[
                "+refs/pull/12/head:refs/lector/pr/12/head",
                "+refs/heads/main:refs/lector/pr/12/base",
            ],
            &["+refs/heads/release/1.2:refs/lector/pr/3/base"],
        ] {
            assert!(check_refspecs(&specs(ok)).is_ok(), "{ok:?}");
        }
        for bad in [
            &[][..],
            &["+refs/heads/main:refs/heads/main"],
            &["+refs/pull/12/head:refs/lector/snapshots/x"],
            &["+refs/pull/12/head:refs/lector/pr/13/head"],
            &["+refs/pull/12/head:refs/lector/pr/12/other"],
            &["+refs/pull/x/head:refs/lector/pr/x/head"],
            &["+refs/pull/012/head:refs/lector/pr/012/head"],
            &["-refs/pull/1/head:refs/lector/pr/1/head"],
            &["+refs/tags/v1:refs/lector/pr/1/head"],
            &["+refs/heads/../x:refs/lector/pr/1/base"],
            &["+refs/heads/-x:refs/lector/pr/1/base"],
            &["+refs/heads/a b:refs/lector/pr/1/base"],
            &["+refs/heads/a\nb:refs/lector/pr/1/base"],
            &["+refs/heads/*:refs/lector/pr/1/base"],
            &["+refs/pull/1/head:refs/lector/pr/1/head:x"],
            &["refs/pull/1/head"],
            &[
                "+refs/pull/1/head:refs/lector/pr/1/head",
                "+refs/heads/main:refs/lector/pr/2/base",
            ],
            &[
                "+refs/pull/1/head:refs/lector/pr/1/head",
                "+refs/merge-requests/1/head:refs/lector/pr/1/head",
            ],
            &[
                "+refs/heads/a:refs/lector/pr/1/base",
                "+refs/heads/b:refs/lector/pr/1/base",
            ],
        ] {
            assert!(check_refspecs(&specs(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_header_is_one_authorization_line() {
        assert!(check_http_header("Authorization: Basic eC1hY2Nlc3MtdG9rZW46dA==").is_ok());
        for bad in [
            "",
            "Authorization: ",
            "Authorization:Basic x",
            "Cookie: a=b",
            "authorization: Basic x",
            "Authorization: Basic x\r\nX-Evil: 1",
            "Authorization: Basic x\nX-Evil: 1",
            "Authorization: Basic \u{0}x",
        ] {
            assert!(check_http_header(bad).is_err(), "{bad:?}");
        }
        assert!(check_http_header(&format!("Authorization: {}", "a".repeat(9000))).is_err());
    }

    #[test]
    fn have_entries_are_commit_ids() {
        assert!(check_have("").is_ok());
        assert!(check_have("deadbeef").is_ok());
        for bad in ["HEAD", "--all", "abc", "main..x", &"a".repeat(65)] {
            assert!(check_have(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_command_is_ui_only() {
        let spec = fetch_pull_request();
        let spec = super::super::Command::spec(&spec);
        assert_eq!(spec.sources, super::super::Sources::UiOnly);
        assert!(!spec.writes);
        assert_eq!(spec.to_json()["ui_only"], true);
    }
}
