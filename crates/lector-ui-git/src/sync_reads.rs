//! The git reads behind the sync controller (blocking; run them through the background
//! bridge): the full sync status and the commits a branch deletion would lose.

use std::path::Path;
use std::time::SystemTime;

use lector_git::Repo;

use crate::sync_model::{RemoteInfo, SyncStatus};

fn fetch_head_time(git_dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(git_dir.join("FETCH_HEAD"))
        .and_then(|m| m.modified())
        .ok()
}

/// Reads the full sync status (blocking; background thread).
pub fn load_status(repo: &Repo) -> Result<SyncStatus, String> {
    let st = repo.upstream_status().map_err(|e| e.to_string())?;
    let head_short = repo.head().ok().and_then(|h| h.oid).map(|o| o.short());
    let remotes = repo
        .remotes()
        .unwrap_or_default()
        .into_iter()
        .map(|r| RemoteInfo {
            url: lector_git::sync::redact_credentials(&r.url),
            name: r.name,
        })
        .collect();
    Ok(SyncStatus::from_upstream(
        &st,
        head_short,
        remotes,
        fetch_head_time(repo.git_dir()),
    ))
}

/// Commits of `branch` that no other ref has (shown before deleting an unmerged branch):
/// those not on `HEAD` and not on the branch's upstream. At most 50, "abc1234 summary".
pub fn unreachable_commits(repo: &Repo, branch: &str) -> Vec<String> {
    let log = |rev: String| {
        repo.log(&lector_git::LogQuery {
            rev: Some(rev),
            limit: 200,
            ..Default::default()
        })
        .unwrap_or_default()
    };
    let mut lost = log(format!("HEAD..{branch}"));
    let others: Vec<lector_git::Branch> = repo
        .branches()
        .unwrap_or_default()
        .into_iter()
        .filter(|b| b.name != branch && !b.head)
        .collect();
    for other in others {
        if lost.is_empty() {
            break;
        }
        let keep: std::collections::HashSet<lector_git::Oid> =
            log(format!("{}..{branch}", other.name))
                .into_iter()
                .map(|c| c.oid)
                .collect();
        lost.retain(|c| keep.contains(&c.oid));
    }
    lost.into_iter()
        .take(50)
        .map(|c| format!("{} {}", c.oid.short(), c.summary))
        .collect()
}
