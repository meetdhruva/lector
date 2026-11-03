use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use gix::bstr::{BStr, ByteSlice};

use crate::error::{git_err, Error, Result, ResultExt};
use crate::{FileChange, HeadInfo, Oid, Remote, Side};

/// Object cache per thread-local repository handle; speeds up log walks.
const OBJECT_CACHE_BYTES: usize = 16 * 1024 * 1024;

/// A read-only handle to a git repository.
///
/// Cheap to clone and `Send + Sync`; every method opens a thread-local view, so
/// call them from background threads freely. Nothing here ever writes to the
/// repository: no index refresh, no lock files, no ref updates.
#[derive(Clone)]
pub struct Repo {
    inner: Arc<Inner>,
}

struct Inner {
    /// The opened repository plus the fingerprint of its config file at open time.
    sync: RwLock<(gix::ThreadSafeRepository, ConfigStamp)>,
    /// Working tree root, or the git dir for bare repositories.
    root: PathBuf,
    git_dir: PathBuf,
    config_path: PathBuf,
    bare: bool,
}

/// (mtime, size) of the repository config; gix caches config at open time, so
/// a changed stamp triggers a cheap re-open (remotes, upstreams may change).
type ConfigStamp = Option<(std::time::SystemTime, u64)>;

fn config_stamp(path: &Path) -> ConfigStamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

impl std::fmt::Debug for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repo")
            .field("root", &self.inner.root)
            .field("bare", &self.inner.bare)
            .finish()
    }
}

impl Repo {
    /// Find the repository containing `path` (searching upwards).
    pub fn discover(path: &Path) -> Result<Repo> {
        let sync = gix::ThreadSafeRepository::discover(path)
            .map_err(|_| Error::NotARepository(path.to_path_buf()))?;
        let local = sync.to_thread_local();
        let bare = local.workdir().is_none();
        let root = match local.workdir() {
            Some(dir) => dir.to_path_buf(),
            None => local.git_dir().to_path_buf(),
        };
        let root = gix::path::realpath(&root).unwrap_or(root);
        let git_dir = local.git_dir().to_path_buf();
        let config_path = local.common_dir().join("config");
        let stamp = config_stamp(&config_path);
        Ok(Repo {
            inner: Arc::new(Inner {
                sync: RwLock::new((sync, stamp)),
                root,
                git_dir,
                config_path,
                bare,
            }),
        })
    }

    /// The working tree root (for bare repositories: the git directory).
    pub fn workdir(&self) -> &Path {
        &self.inner.root
    }

    /// `true` if the repository has no working tree.
    pub fn is_bare(&self) -> bool {
        self.inner.bare
    }

    /// The `.git` directory.
    pub fn git_dir(&self) -> &Path {
        &self.inner.git_dir
    }

    /// A thread-local gix handle with an object cache. Re-opens the repository
    /// when its config file changed since it was opened.
    pub(crate) fn local(&self) -> gix::Repository {
        let stamp = config_stamp(&self.inner.config_path);
        let current = {
            let guard = self.inner.sync.read().unwrap_or_else(|e| e.into_inner());
            (guard.1 == stamp).then(|| guard.0.to_thread_local())
        };
        let mut repo = match current {
            Some(repo) => repo,
            None => {
                let mut guard = self.inner.sync.write().unwrap_or_else(|e| e.into_inner());
                if guard.1 != stamp {
                    if let Ok(fresh) = gix::ThreadSafeRepository::discover(&self.inner.root) {
                        *guard = (fresh, stamp);
                    } else {
                        guard.1 = stamp;
                    }
                }
                guard.0.to_thread_local()
            }
        };
        repo.object_cache_size_if_unset(OBJECT_CACHE_BYTES);
        repo
    }

    /// Error unless a working tree is present.
    pub(crate) fn require_workdir(&self) -> Result<&Path> {
        if self.inner.bare {
            Err(Error::Bare)
        } else {
            Ok(&self.inner.root)
        }
    }

    /// Information about `HEAD`, its upstream and ahead/behind counts.
    pub fn head(&self) -> Result<HeadInfo> {
        let repo = self.local();
        let head = repo.head().git()?;
        let detached = head.is_detached();
        let full_name = head.referent_name().map(|name| name.to_owned());
        let oid = head.id().map(Oid::from);
        let mut info = HeadInfo {
            branch: full_name
                .as_ref()
                .map(|n| n.shorten().to_str_lossy().into_owned()),
            oid,
            detached,
            ..HeadInfo::default()
        };
        if let Some(name) = &full_name {
            if let Some((upstream_name, upstream_id)) = upstream_of(&repo, name.as_ref()) {
                info.upstream = Some(upstream_name);
                if let Some(head_id) = oid {
                    let (ahead, behind) = ahead_behind(&repo, head_id.to_gix(), upstream_id)?;
                    info.ahead = ahead;
                    info.behind = behind;
                }
            }
        }
        Ok(info)
    }

    /// Resolve a revision (`HEAD~2`, `main`, `v1.0`, a hex prefix, …) to an object id.
    /// Annotated tags are peeled to the object they point at.
    pub fn resolve(&self, rev: &str) -> Result<Oid> {
        let repo = self.local();
        resolve_object(&repo, rev).map(Oid::from)
    }

    /// The best common ancestor of two commits, `None` if the histories are unrelated.
    pub fn merge_base(&self, a: Oid, b: Oid) -> Result<Option<Oid>> {
        let repo = self.local();
        let base = repo.merge_base(a.to_gix(), b.to_gix()).git()?;
        Ok(base.map(Oid::from))
    }

    /// Load the bytes of one side of a change. `Ok(None)` for [`Side::None`] or
    /// when the path/object does not exist. Submodule entries load as
    /// `Subproject commit <oid>\n`, like `git diff` shows them.
    pub fn load(&self, side: &Side) -> Result<Option<Vec<u8>>> {
        match side {
            Side::None => Ok(None),
            Side::Blob(oid) => {
                let repo = self.local();
                load_object(&repo, oid.to_gix())
            }
            Side::Index(path) => {
                let repo = self.local();
                let index = repo.index_or_empty().git()?;
                let path: &BStr = path.as_bytes().as_bstr();
                let entry = index
                    .entry_by_path_and_stage(path, gix::index::entry::Stage::Unconflicted)
                    .or_else(|| {
                        index.entry_by_path_and_stage(path, gix::index::entry::Stage::Ours)
                    });
                match entry {
                    Some(entry) => {
                        let id = entry.id;
                        load_object(&repo, id)
                    }
                    None => Ok(None),
                }
            }
            Side::Workdir(path) => {
                let root = self.require_workdir()?;
                read_worktree_file(&root.join(path))
            }
        }
    }

    /// Fill [`FileChange::binary`] by loading both sides (NUL byte in the first
    /// 8000 bytes, like git). Returns the detected value.
    pub fn detect_binary(&self, change: &mut FileChange) -> Result<bool> {
        if let Some(binary) = change.binary {
            return Ok(binary);
        }
        let mut binary = false;
        for side in [&change.old, &change.new] {
            if let Some(bytes) = self.load(side)? {
                binary |= is_binary(&bytes);
            }
        }
        change.binary = Some(binary);
        Ok(binary)
    }

    /// The content of `path` at revision `rev`; `Ok(None)` if the path doesn't exist there.
    pub fn file_at(&self, rev: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let repo = self.local();
        let tree = resolve_tree(&repo, rev)?;
        blob_at_path(&repo, &tree, path)
    }

    /// The `HEAD` version of `path` (for the diff gutter); `Ok(None)` if the path is
    /// not in `HEAD` or `HEAD` is unborn.
    pub fn head_text(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let repo = self.local();
        let tree = match head_tree_id(&repo)? {
            Some(tree) => tree,
            None => return Ok(None),
        };
        let tree = repo.find_tree(tree).git()?;
        blob_at_path(&repo, &tree, path)
    }

    /// Configured remotes with their fetch URLs.
    pub fn remotes(&self) -> Result<Vec<Remote>> {
        let repo = self.local();
        let mut out = Vec::new();
        for name in repo.remote_names() {
            let name = name.to_str_lossy().into_owned();
            // The URL as configured, without `url.<base>.insteadOf` rewrites.
            let Some(Ok(remote)) = repo.try_find_remote_without_url_rewrite(name.as_str()) else {
                continue;
            };
            let url = remote
                .url(gix::remote::Direction::Fetch)
                .map(|url| url.to_bstring().to_str_lossy().into_owned())
                .unwrap_or_default();
            out.push(Remote { name, url });
        }
        Ok(out)
    }

    /// Name of the remote that `HEAD`'s branch tracks, if any.
    pub(crate) fn head_remote_name(&self) -> Option<String> {
        let repo = self.local();
        let head = repo.head().ok()?;
        let name = head.referent_name()?.to_owned();
        let remote = repo.branch_remote_name(name.shorten(), gix::remote::Direction::Fetch)?;
        Some(remote.as_bstr().to_str_lossy().into_owned())
    }
}

/// `true` if `bytes` look binary (a NUL byte within the first 8000 bytes, like git).
pub fn is_binary(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(8000)];
    head.contains(&0)
}

pub(crate) fn read_worktree_file(path: &Path) -> Result<Option<Vec<u8>>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if meta.file_type().is_symlink() {
        // Git stores the link target as the blob content.
        let target = std::fs::read_link(path)?;
        let target = gix::path::into_bstr(target).git()?;
        return Ok(Some(target.into_owned().into()));
    }
    if !meta.is_file() {
        // Directories (e.g. submodules) and special files (FIFOs, sockets) have no
        // readable blob content; reading a FIFO would block forever.
        return Ok(None);
    }
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn load_object(repo: &gix::Repository, id: gix::ObjectId) -> Result<Option<Vec<u8>>> {
    let Some(header) = repo.try_find_header(id).git()? else {
        return Ok(None);
    };
    match header.kind() {
        gix::object::Kind::Blob => {
            let mut blob = repo.find_blob(id).git()?;
            Ok(Some(blob.take_data()))
        }
        _ => Ok(None),
    }
}

/// Bytes of the blob at `path` in `tree`, `None` if absent. Submodules render as git does.
pub(crate) fn blob_at_path(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    path: &str,
) -> Result<Option<Vec<u8>>> {
    let Some(entry) = tree.lookup_entry_by_path(path).git()? else {
        return Ok(None);
    };
    let mode = entry.mode();
    if mode.is_commit() {
        return Ok(Some(
            format!("Subproject commit {}\n", entry.object_id()).into_bytes(),
        ));
    }
    if mode.is_tree() {
        return Ok(None);
    }
    load_object(repo, entry.object_id())
}

pub(crate) fn resolve_object(repo: &gix::Repository, rev: &str) -> Result<gix::ObjectId> {
    let invalid = |err: gix::Error| Error::InvalidRevision {
        rev: rev.to_owned(),
        message: format!("{err:#}"),
    };
    let id = repo.rev_parse_single(rev).map_err(invalid)?;
    let object = id.object().git()?.peel_tags_to_end().git()?;
    Ok(object.id)
}

pub(crate) fn resolve_commit(repo: &gix::Repository, rev: &str) -> Result<gix::ObjectId> {
    let id = resolve_object(repo, rev)?;
    let object = repo.find_object(id).git()?;
    if object.kind != gix::object::Kind::Commit {
        return Err(Error::InvalidRevision {
            rev: rev.to_owned(),
            message: format!("expected a commit, found a {}", object.kind),
        });
    }
    Ok(id)
}

pub(crate) fn resolve_tree<'r>(repo: &'r gix::Repository, rev: &str) -> Result<gix::Tree<'r>> {
    let id = resolve_object(repo, rev)?;
    repo.find_object(id)
        .git()?
        .peel_to_tree()
        .map_err(|err| Error::InvalidRevision {
            rev: rev.to_owned(),
            message: format!("{err:#}"),
        })
}

/// The tree id of `HEAD`, `None` if `HEAD` is unborn.
pub(crate) fn head_tree_id(repo: &gix::Repository) -> Result<Option<gix::ObjectId>> {
    let head = repo.head().git()?;
    let Some(id) = head.id() else {
        return Ok(None);
    };
    let commit = repo.find_commit(id.detach()).git()?;
    Ok(Some(commit.tree_id().git()?.detach()))
}

/// Short name and tip of the upstream tracking branch of the local branch `name`.
pub(crate) fn upstream_of(
    repo: &gix::Repository,
    name: &gix::refs::FullNameRef,
) -> Option<(String, gix::ObjectId)> {
    let tracking = repo
        .branch_remote_tracking_ref_name(name, gix::remote::Direction::Fetch)?
        .ok()?;
    let mut reference = repo.try_find_reference(tracking.as_ref()).ok()??;
    let id = reference.peel_to_id().ok()?.detach();
    Some((tracking.shorten().to_str_lossy().into_owned(), id))
}

/// Count commits reachable from `a` but not `b`, and vice versa.
pub(crate) fn ahead_behind(
    repo: &gix::Repository,
    a: gix::ObjectId,
    b: gix::ObjectId,
) -> Result<(u32, u32)> {
    if a == b {
        return Ok((0, 0));
    }
    let count = |tip: gix::ObjectId, hidden: gix::ObjectId| -> Result<u32> {
        let walk = repo.rev_walk([tip]).with_hidden([hidden]).all().git()?;
        let mut n = 0u32;
        for info in walk {
            info.map_err(git_err)?;
            n = n.saturating_add(1);
        }
        Ok(n)
    };
    Ok((count(a, b)?, count(b, a)?))
}

#[cfg(test)]
mod tests {
    fn assert_send_sync<T: Send + Sync + Clone>() {}

    #[test]
    fn repo_is_send_sync_clone() {
        assert_send_sync::<super::Repo>();
    }

    #[test]
    fn binary_detection_looks_at_first_8000_bytes() {
        let mut bytes = vec![b'a'; 9000];
        assert!(!super::is_binary(&bytes));
        bytes[8500] = 0;
        assert!(!super::is_binary(&bytes));
        bytes[7999] = 0;
        assert!(super::is_binary(&bytes));
    }
}
