//! Commit history, references, branches, tags and stashes.

use std::collections::HashMap;

use gix::bstr::{BStr, ByteSlice};
use gix::refs::Category;

use crate::changes::tree_changes;
use crate::error::{git_err, Error, Result, ResultExt};
use crate::repo::{resolve_commit, upstream_of};
use crate::{
    Branch, CommitDetail, CommitInfo, LogQuery, Oid, RefKind, RefLabel, Repo, Signature, Stash, Tag,
};

/// Commit id → labels of references pointing at it.
pub(crate) type RefMap = HashMap<gix::ObjectId, Vec<RefLabel>>;

impl Repo {
    /// Walk history newest-first (by committer time, like `git log`).
    ///
    /// * `rev`: start revision (`None` = `HEAD`); `a..b` excludes commits reachable from `a`.
    /// * `all_refs`: start from all branches, remote branches, tags and `HEAD`.
    /// * `path`: keep commits whose tree entry at `path` (file or directory) differs
    ///   from every parent (merges that match one parent are dropped, like `git log -- path`;
    ///   all parents are walked, i.e. no history simplification).
    /// * `author` / `message`: case-insensitive substring filters.
    /// * `skip` / `limit` apply to matching commits; `limit == 0` means unlimited.
    ///
    /// An unborn `HEAD` yields an empty log.
    pub fn log(&self, q: &LogQuery) -> Result<Vec<CommitInfo>> {
        let repo = self.local();
        let mut tips = Vec::new();
        let mut hidden = Vec::new();
        if q.all_refs {
            tips.extend(all_ref_tips(&repo)?);
        }
        match q.rev.as_deref() {
            Some(rev) => {
                let (include, exclude) = parse_range(&repo, rev)?;
                tips.extend(include);
                hidden.extend(exclude);
            }
            None => {
                if let Some(id) = repo.head().git()?.id() {
                    tips.push(id.detach());
                }
            }
        }
        tips.sort();
        tips.dedup();
        if tips.is_empty() {
            return Ok(Vec::new());
        }
        let refs = ref_map(&repo)?;
        let author = q
            .author
            .as_deref()
            .map(str::to_lowercase)
            .filter(|s| !s.is_empty());
        let message = q
            .message
            .as_deref()
            .map(str::to_lowercase)
            .filter(|s| !s.is_empty());
        let path = q
            .path
            .as_deref()
            .map(|p| p.trim_matches('/'))
            .filter(|p| !p.is_empty());
        let limit = if q.limit == 0 { usize::MAX } else { q.limit };

        let walk = repo
            .rev_walk(tips)
            .with_hidden(hidden)
            .sorting(gix::revision::walk::Sorting::ByCommitTime(
                gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
            ))
            .all()
            .git()?;

        let mut path_cache: HashMap<gix::ObjectId, Option<gix::ObjectId>> = HashMap::new();
        let mut skipped = 0usize;
        let mut out = Vec::new();
        for info in walk {
            if out.len() >= limit {
                break;
            }
            let info = info.map_err(git_err)?;
            let commit = repo.find_commit(info.id).git()?;
            if let Some(path) = path {
                if !touches_path(&repo, &commit, path, &mut path_cache)? {
                    continue;
                }
            }
            if author.is_some() || message.is_some() {
                let decoded = commit.decode().git()?;
                if let Some(needle) = &author {
                    let sig = decoded.author().git()?;
                    let hay = format!("{} <{}>", sig.name, sig.email).to_lowercase();
                    if !hay.contains(needle.as_str()) {
                        continue;
                    }
                }
                if let Some(needle) = &message {
                    if !decoded
                        .message
                        .to_str_lossy()
                        .to_lowercase()
                        .contains(needle.as_str())
                    {
                        continue;
                    }
                }
            }
            if skipped < q.skip {
                skipped += 1;
                continue;
            }
            out.push(commit_info(&commit, &refs)?);
        }
        Ok(out)
    }

    /// One commit with its full message and its changes against the first parent.
    pub fn commit(&self, oid: Oid) -> Result<CommitDetail> {
        let repo = self.local();
        let commit = repo.find_commit(oid.to_gix()).git()?;
        let refs = ref_map(&repo)?;
        let info = commit_info(&commit, &refs)?;
        let message = commit.message_raw().git()?.to_str_lossy().into_owned();
        let new_tree = commit.tree().git()?;
        let old_tree = match commit.parent_ids().next() {
            Some(parent) => Some(repo.find_commit(parent.detach()).git()?.tree().git()?),
            None => None,
        };
        let mut changes = tree_changes(&repo, old_tree.as_ref(), Some(&new_tree))?;
        changes.sort_by(|a, b| a.path().cmp(b.path()));
        Ok(CommitDetail {
            info,
            message,
            changes,
        })
    }

    /// Build [`CommitInfo`]s (with ref labels) for the given commit ids, in order.
    pub fn commit_infos(&self, oids: &[Oid]) -> Result<Vec<CommitInfo>> {
        let repo = self.local();
        let refs = ref_map(&repo)?;
        oids.iter()
            .map(|oid| {
                let commit = repo.find_commit(oid.to_gix()).git()?;
                commit_info(&commit, &refs)
            })
            .collect()
    }

    /// Local and remote-tracking branches (locals first, each group sorted by name).
    /// Symbolic remote refs like `origin/HEAD` are skipped.
    pub fn branches(&self) -> Result<Vec<Branch>> {
        let repo = self.local();
        let head_name = repo.head_name().git()?;
        let platform = repo.references().git()?;
        let mut out = Vec::new();
        for (iter, remote) in [
            (platform.local_branches().git()?, false),
            (platform.remote_branches().git()?, true),
        ] {
            let mut group = Vec::new();
            for reference in iter {
                let mut reference = reference.map_err(git_err)?;
                if matches!(reference.target(), gix::refs::TargetRef::Symbolic(_)) {
                    continue;
                }
                let full = reference.name().to_owned();
                let Some((_, short)) = full.category_and_short_name() else {
                    continue;
                };
                let name = short.to_str_lossy().into_owned();
                let Ok(id) = reference.peel_to_id() else {
                    continue;
                };
                let id = id.detach();
                let time = repo
                    .find_commit(id)
                    .ok()
                    .and_then(|c| c.time().ok())
                    .map(|t| t.seconds)
                    .unwrap_or_default();
                let upstream = if remote {
                    None
                } else {
                    upstream_of(&repo, full.as_ref()).map(|(name, _)| name)
                };
                group.push(Branch {
                    name,
                    remote,
                    head: !remote && head_name.as_ref() == Some(&full),
                    oid: id.into(),
                    upstream,
                    time,
                });
            }
            group.sort_by(|a, b| a.name.cmp(&b.name));
            out.extend(group);
        }
        Ok(out)
    }

    /// Tags sorted by name, peeled through annotated tags.
    pub fn tags(&self) -> Result<Vec<Tag>> {
        let repo = self.local();
        let platform = repo.references().git()?;
        let mut out = Vec::new();
        for reference in platform.tags().git()? {
            let mut reference = reference.map_err(git_err)?;
            let name = reference.name().shorten().to_str_lossy().into_owned();
            let Ok(id) = reference.peel_to_id() else {
                continue;
            };
            out.push(Tag {
                name,
                oid: id.detach().into(),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Stash entries from the `refs/stash` reflog, most recent (`stash@{0}`) first.
    pub fn stashes(&self) -> Result<Vec<Stash>> {
        let repo = self.local();
        let Some(reference) = repo.try_find_reference("refs/stash").git()? else {
            return Ok(Vec::new());
        };
        let mut platform = reference.log_iter();
        let Some(lines) = platform.all()? else {
            return Ok(Vec::new());
        };
        let mut entries = Vec::new();
        for line in lines {
            let line = line.map_err(git_err)?;
            entries.push((
                Oid::from(line.new_oid()),
                line.message.to_str_lossy().into_owned(),
            ));
        }
        Ok(entries
            .into_iter()
            .rev()
            .enumerate()
            .map(|(index, (oid, message))| Stash {
                index,
                message,
                oid,
            })
            .collect())
    }
}

/// Resolve `rev` into (include, exclude) tips. Supports `a..b`, `^a`, and single revisions.
fn parse_range(
    repo: &gix::Repository,
    rev: &str,
) -> Result<(Vec<gix::ObjectId>, Vec<gix::ObjectId>)> {
    let invalid = |message: String| Error::InvalidRevision {
        rev: rev.to_owned(),
        message,
    };
    if !rev.contains("..") && !rev.starts_with('^') {
        return Ok((vec![resolve_commit(repo, rev)?], Vec::new()));
    }
    let spec = repo
        .rev_parse(rev)
        .map_err(|e| invalid(format!("{e:#}")))?
        .detach();
    use gix::revision::plumbing::Spec;
    let peel = |id: gix::ObjectId| -> Result<gix::ObjectId> {
        Ok(repo.find_object(id).git()?.peel_to_commit().git()?.id)
    };
    Ok(match spec {
        Spec::Include(id) => (vec![peel(id)?], Vec::new()),
        Spec::Exclude(id) => (Vec::new(), vec![peel(id)?]),
        Spec::Range { from, to } => (vec![peel(to)?], vec![peel(from)?]),
        Spec::Merge { theirs, ours } => {
            let (a, b) = (peel(theirs)?, peel(ours)?);
            let bases: Vec<_> = repo
                .merge_base(a, b)
                .git()?
                .map(|id| id.detach())
                .into_iter()
                .collect();
            (vec![a, b], bases)
        }
        Spec::IncludeOnlyParents(id) => {
            let commit = repo.find_commit(peel(id)?).git()?;
            (
                commit.parent_ids().map(|p| p.detach()).collect(),
                Vec::new(),
            )
        }
        Spec::ExcludeParents(id) => {
            let id = peel(id)?;
            let commit = repo.find_commit(id).git()?;
            (vec![id], commit.parent_ids().map(|p| p.detach()).collect())
        }
    })
}

/// Commits pointed to by HEAD, branches, remote branches and tags (not the stash).
fn all_ref_tips(repo: &gix::Repository) -> Result<Vec<gix::ObjectId>> {
    let mut tips = Vec::new();
    if let Some(id) = repo.head().git()?.id() {
        tips.push(id.detach());
    }
    let platform = repo.references().git()?;
    for iter in [
        platform.local_branches().git()?,
        platform.remote_branches().git()?,
        platform.tags().git()?,
    ] {
        for reference in iter {
            let mut reference = reference.map_err(git_err)?;
            if let Ok(id) = reference.peel_to_id() {
                if is_commit(repo, id.detach()) {
                    tips.push(id.detach());
                }
            }
        }
    }
    Ok(tips)
}

fn is_commit(repo: &gix::Repository, id: gix::ObjectId) -> bool {
    repo.find_header(id)
        .is_ok_and(|h| h.kind() == gix::object::Kind::Commit)
}

/// Map every commit to the labels of references pointing at it.
pub(crate) fn ref_map(repo: &gix::Repository) -> Result<RefMap> {
    let mut map: RefMap = HashMap::new();
    let platform = repo.references().git()?;
    for reference in platform.all().git()? {
        let Ok(mut reference) = reference else {
            continue;
        };
        if matches!(reference.target(), gix::refs::TargetRef::Symbolic(_)) {
            continue;
        }
        let name = reference.name().to_owned();
        let Some((category, short)) = name.category_and_short_name() else {
            continue;
        };
        let kind = match category {
            Category::LocalBranch => RefKind::LocalBranch,
            Category::RemoteBranch => RefKind::RemoteBranch,
            Category::Tag => RefKind::Tag,
            _ => continue,
        };
        let Ok(id) = reference.peel_to_id() else {
            continue;
        };
        map.entry(id.detach()).or_default().push(RefLabel {
            name: short.to_str_lossy().into_owned(),
            kind,
        });
    }
    if let Some(id) = repo.head().git()?.id() {
        map.entry(id.detach()).or_default().push(RefLabel {
            name: "HEAD".into(),
            kind: RefKind::Head,
        });
    }
    for labels in map.values_mut() {
        labels.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
    }
    Ok(map)
}

fn signature(sig: gix::actor::SignatureRef<'_>) -> Signature {
    let time = sig.time().unwrap_or_default();
    Signature {
        name: sig.name.trim().to_str_lossy().into_owned(),
        email: sig.email.trim().to_str_lossy().into_owned(),
        time: time.seconds,
        offset_minutes: time.offset / 60,
    }
}

/// The first paragraph of a message, joined into a single line (like `%s`).
fn summary_of(message: &BStr) -> String {
    let text = message.to_str_lossy();
    let mut summary = String::new();
    for line in text.trim_start().lines() {
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if !summary.is_empty() {
            summary.push(' ');
        }
        summary.push_str(line);
    }
    summary
}

pub(crate) fn commit_info(commit: &gix::Commit<'_>, refs: &RefMap) -> Result<CommitInfo> {
    let decoded = commit.decode().git()?;
    let id = commit.id;
    Ok(CommitInfo {
        oid: id.into(),
        parents: decoded.parents().map(Oid::from).collect(),
        author: decoded.author().map(signature).unwrap_or_default(),
        committer: decoded.committer().map(signature).unwrap_or_default(),
        summary: summary_of(decoded.message),
        refs: refs.get(&id).cloned().unwrap_or_default(),
    })
}

/// Tree entry id at `path` in `commit`'s tree (cached by commit id).
fn path_entry(
    repo: &gix::Repository,
    commit_id: gix::ObjectId,
    path: &str,
    cache: &mut HashMap<gix::ObjectId, Option<gix::ObjectId>>,
) -> Result<Option<gix::ObjectId>> {
    if let Some(entry) = cache.get(&commit_id) {
        return Ok(*entry);
    }
    let commit = repo.find_commit(commit_id).git()?;
    let tree = commit.tree().git()?;
    let entry = tree
        .lookup_entry_by_path(path)
        .git()?
        .map(|e| e.object_id());
    cache.insert(commit_id, entry);
    Ok(entry)
}

/// Whether `commit` changed `path` relative to all of its parents (TREESAME rule).
fn touches_path(
    repo: &gix::Repository,
    commit: &gix::Commit<'_>,
    path: &str,
    cache: &mut HashMap<gix::ObjectId, Option<gix::ObjectId>>,
) -> Result<bool> {
    let own = path_entry(repo, commit.id, path, cache)?;
    let parents: Vec<_> = commit.parent_ids().map(|p| p.detach()).collect();
    if parents.is_empty() {
        return Ok(own.is_some());
    }
    for parent in parents {
        if path_entry(repo, parent, path, cache)? == own {
            return Ok(false);
        }
    }
    Ok(true)
}
