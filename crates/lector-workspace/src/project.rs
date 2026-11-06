//! The project: root folder, optional git repository, file list, status, symbols.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::SystemTime;

use lector_config::Settings;
use lector_git::{BlameHunk, ChangeKind, Oid, Repo, StatusEntry};
use lector_lsp::{LspManager, ServerConfig};
use lector_search::{walk_files, CancelToken, WalkOptions};
use lector_syntax::{LanguageRegistry, SymbolIndex, Syntax};
use lector_text::Text;
use parking_lot::{Mutex, RwLock};

use crate::document::Document;
use crate::{Error, Result};

/// Files larger than this are skipped by the symbol indexer.
const INDEX_MAX_FILE_BYTES: u64 = 1024 * 1024;
/// At most this many files are indexed.
const INDEX_MAX_FILES: usize = 20_000;

/// Snapshot of the project's files.
#[derive(Debug, Default)]
pub struct FileList {
    /// Increases with every rescan.
    pub version: u64,
    /// Sorted, `/`-separated paths relative to the project root.
    pub paths: Vec<Arc<str>>,
    ignored: HashSet<Arc<str>>,
}

impl FileList {
    /// Whether `rel` is ignored by git. Only populated when `settings.files.show_ignored` is on
    /// (ignored files are then part of `paths`); otherwise always `false`.
    pub fn is_ignored(&self, rel: &str) -> bool {
        self.ignored.contains(rel)
    }

    /// Whether `rel` is in the list.
    pub fn contains(&self, rel: &str) -> bool {
        self.paths.binary_search_by(|p| (**p).cmp(rel)).is_ok()
    }
}

/// Git state of one path (or directory rollup).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileGitState {
    /// Modified (also type changes).
    Modified,
    /// Added to the index.
    Added,
    /// Deleted.
    Deleted,
    /// Renamed.
    Renamed,
    /// Not tracked.
    Untracked,
    /// Merge conflict.
    Conflicted,
    /// Ignored by git.
    Ignored,
}

impl FileGitState {
    /// Deterministic tie-break inside a rank: Modified, then Renamed, then Deleted.
    fn tie(self) -> u8 {
        match self {
            FileGitState::Modified => 2,
            FileGitState::Renamed => 1,
            _ => 0,
        }
    }

    fn rank(self) -> u8 {
        match self {
            FileGitState::Conflicted => 4,
            FileGitState::Modified | FileGitState::Deleted | FileGitState::Renamed => 3,
            FileGitState::Added => 2,
            FileGitState::Untracked => 1,
            FileGitState::Ignored => 0,
        }
    }
}

/// Status of one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStatus {
    /// The state shown in the tree (working tree state wins over the staged one).
    pub state: FileGitState,
    /// The file has staged changes.
    pub staged: bool,
}

/// Snapshot of `git status`, with directory rollups. Paths are relative to the project root.
#[derive(Debug, Default)]
pub struct GitStatusMap {
    files: HashMap<String, FileStatus>,
    dirs: HashMap<String, FileGitState>,
    order: Vec<String>,
}

impl GitStatusMap {
    fn build(entries: &[StatusEntry], prefix: &str) -> GitStatusMap {
        let mut map = GitStatusMap::default();
        for e in entries {
            let Some(rel) = e.path.strip_prefix(prefix) else {
                continue;
            };
            let rel = rel.trim_end_matches('/');
            if rel.is_empty() {
                continue;
            }
            let state = if e.conflicted {
                FileGitState::Conflicted
            } else if let Some(k) = e.unstaged {
                kind_state(k)
            } else if let Some(k) = e.staged {
                kind_state(k)
            } else if e.untracked {
                FileGitState::Untracked
            } else {
                continue;
            };
            let status = FileStatus {
                state,
                staged: e.staged.is_some(),
            };
            map.files.insert(rel.to_string(), status);
            let mut dir = rel;
            while let Some(i) = dir.rfind('/') {
                dir = &dir[..i];
                let slot = map.dirs.entry(dir.to_string()).or_insert(state);
                if (state.rank(), state.tie()) > (slot.rank(), slot.tie()) {
                    *slot = state;
                }
            }
        }
        map.order = map.files.keys().cloned().collect();
        map.order.sort();
        map
    }

    /// Status of a file (relative path), if it has any.
    pub fn get(&self, rel_path: &str) -> Option<FileStatus> {
        self.files.get(rel_path).copied()
    }

    /// Strongest state among the descendants of a directory (relative path).
    pub fn dir_state(&self, rel_dir: &str) -> Option<FileGitState> {
        self.dirs.get(rel_dir.trim_end_matches('/')).copied()
    }

    /// All entries, sorted by path.
    pub fn entries(&self) -> impl Iterator<Item = (&str, FileStatus)> + '_ {
        self.order.iter().map(|p| (p.as_str(), self.files[p]))
    }

    /// Number of changed files.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// No changes.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

fn kind_state(k: ChangeKind) -> FileGitState {
    match k {
        ChangeKind::Added | ChangeKind::Copied => FileGitState::Added,
        ChangeKind::Modified | ChangeKind::TypeChange => FileGitState::Modified,
        ChangeKind::Deleted => FileGitState::Deleted,
        ChangeKind::Renamed => FileGitState::Renamed,
    }
}

#[derive(Clone, PartialEq, Eq)]
struct BlameKey {
    head: Option<Oid>,
    mtime: Option<SystemTime>,
    size: u64,
}

type BlameEntry = (BlameKey, Arc<Vec<BlameHunk>>);

pub(crate) struct Inner {
    root: PathBuf,
    name: String,
    repo: Option<Repo>,
    /// Root relative to the repo workdir (`""` or `"sub/dir/"`).
    repo_prefix: String,
    settings: RwLock<Settings>,
    languages: Arc<LanguageRegistry>,
    symbols: Arc<SymbolIndex>,
    lsp: Option<LspManager>,
    files: RwLock<Arc<FileList>>,
    git_status: RwLock<Arc<GitStatusMap>>,
    file_version: AtomicU64,
    blame_cache: Mutex<HashMap<String, BlameEntry>>,
    lsp_opened: Mutex<HashMap<PathBuf, u64>>,
}

/// An opened project folder. Cheap to clone.
#[derive(Clone)]
pub struct Project {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for Project {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Project")
            .field("root", &self.inner.root)
            .field("repo", &self.inner.repo.is_some())
            .finish()
    }
}

fn lsp_configs(settings: &Settings) -> Vec<ServerConfig> {
    let mut out = Vec::new();
    for mut cfg in lector_lsp::builtin_servers() {
        let o = settings.lsp.servers.get(&cfg.name);
        let enabled = o.and_then(|o| o.enabled);
        // Read-only: servers that build into the project directory are opt-in.
        if enabled == Some(false) || (enabled.is_none() && lector_lsp::writes_to_project(&cfg.name))
        {
            continue;
        }
        if let Some(o) = o {
            if let Some(c) = &o.command {
                cfg.command = c.clone();
            }
            if let Some(a) = &o.args {
                cfg.args = a.clone();
            }
        }
        out.push(cfg);
    }
    out
}

fn to_slash(p: &Path) -> String {
    let s = p.to_string_lossy();
    if std::path::MAIN_SEPARATOR == '/' {
        s.into_owned()
    } else {
        s.replace(std::path::MAIN_SEPARATOR, "/")
    }
}

/// The canonical spelling of `path`, also for paths that do not exist yet
/// (their nearest existing ancestor is canonicalized and the rest re-appended).
fn canonical_spelling(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = dunce::canonicalize(path) {
        return Some(canonical);
    }
    let mut tail = Vec::new();
    let mut current = path;
    while let Some(parent) = current.parent() {
        tail.push(current.file_name()?.to_os_string());
        if let Ok(base) = dunce::canonicalize(parent) {
            return Some(tail.iter().rev().fold(base, |acc, part| acc.join(part)));
        }
        current = parent;
    }
    None
}

impl Project {
    /// Opens `root`. Discovers the git repository (if any); never fails because of git.
    pub fn open(root: &Path, settings: Settings) -> Result<Project> {
        Self::open_with(root, settings, true)
    }

    /// Opens `root` without discovering a git repository or starting language
    /// servers: a placeholder project for a window with no folder open.
    pub fn open_detached(root: &Path, mut settings: Settings) -> Result<Project> {
        settings.lsp.enabled = false;
        Self::open_with(root, settings, false)
    }

    fn open_with(root: &Path, settings: Settings, discover_git: bool) -> Result<Project> {
        let root = dunce::canonicalize(root)?;
        if !root.is_dir() {
            return Err(Error::Other(format!(
                "{} is not a directory",
                root.display()
            )));
        }
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string_lossy().into_owned());
        let discovered = if discover_git {
            Repo::discover(&root)
        } else {
            Err(lector_git::Error::NotARepository(root.clone()))
        };
        let repo = match discovered {
            Ok(r) if !r.is_bare() => Some(r),
            Ok(_) => None,
            Err(e) => {
                tracing::debug!("no git repository at {}: {e}", root.display());
                None
            }
        };
        let repo_prefix = repo
            .as_ref()
            .and_then(|r| {
                let wd = dunce::canonicalize(r.workdir()).ok()?;
                let rel = root.strip_prefix(&wd).ok()?;
                let s = to_slash(rel);
                Some(if s.is_empty() { s } else { format!("{s}/") })
            })
            .unwrap_or_default();
        let lsp = settings
            .lsp
            .enabled
            .then(|| LspManager::new(root.clone(), lsp_configs(&settings)));
        let inner = Inner {
            root,
            name,
            repo,
            repo_prefix,
            settings: RwLock::new(settings),
            languages: Arc::new(LanguageRegistry::new()),
            symbols: Arc::new(SymbolIndex::new()),
            lsp,
            files: RwLock::new(Arc::new(FileList::default())),
            git_status: RwLock::new(Arc::new(GitStatusMap::default())),
            file_version: AtomicU64::new(0),
            blame_cache: Mutex::new(HashMap::new()),
            lsp_opened: Mutex::new(HashMap::new()),
        };
        Ok(Project {
            inner: Arc::new(inner),
        })
    }

    /// Project root (canonical absolute path).
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Folder name of the root.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// The git repository containing the root, if any.
    pub fn repo(&self) -> Option<&Repo> {
        self.inner.repo.as_ref()
    }

    /// Current settings (copy).
    pub fn settings(&self) -> Settings {
        self.inner.settings.read().clone()
    }

    /// Replaces the settings. File list / LSP servers are not rebuilt automatically
    /// (call `rescan_files`); LSP overrides need a new `Project`.
    pub fn set_settings(&self, s: Settings) {
        *self.inner.settings.write() = s;
    }

    /// Shared language registry.
    pub fn languages(&self) -> &Arc<LanguageRegistry> {
        &self.inner.languages
    }

    /// Workspace-wide tags index.
    pub fn symbols(&self) -> &Arc<SymbolIndex> {
        &self.inner.symbols
    }

    /// The LSP manager; `None` when LSP is disabled in settings.
    pub fn lsp(&self) -> Option<&LspManager> {
        self.inner.lsp.as_ref()
    }

    /// Latest file list snapshot (empty until the first `rescan_files`).
    pub fn files(&self) -> Arc<FileList> {
        self.inner.files.read().clone()
    }

    /// Walks the project (gitignore, `files.exclude`, `files.show_hidden`) and stores the result.
    /// When cancelled the previous snapshot is returned and kept.
    pub fn rescan_files(&self, cancel: &CancelToken) -> Arc<FileList> {
        let settings = self.settings();
        let mut opts = WalkOptions {
            hidden: settings.files.show_hidden,
            git_ignore: true,
            exclude: settings.files.exclude.clone(),
            max_depth: None,
        };
        let collect = |opts: &WalkOptions| -> Vec<PathBuf> {
            let out = Mutex::new(Vec::new());
            walk_files(&self.inner.root, opts, cancel, |p| out.lock().push(p));
            out.into_inner()
        };
        let visible = collect(&opts);
        let mut ignored = HashSet::new();
        let all = if settings.files.show_ignored && !cancel.is_cancelled() {
            opts.git_ignore = false;
            let all = collect(&opts);
            let seen: HashSet<&PathBuf> = visible.iter().collect();
            for p in &all {
                if !seen.contains(p) {
                    ignored.insert(Arc::<str>::from(to_slash(p)));
                }
            }
            all
        } else {
            visible
        };
        if cancel.is_cancelled() {
            return self.files();
        }
        let mut paths: Vec<Arc<str>> = all.iter().map(|p| Arc::from(to_slash(p))).collect();
        paths.sort_unstable();
        paths.dedup();
        let list = Arc::new(FileList {
            version: self.inner.file_version.fetch_add(1, Ordering::Relaxed) + 1,
            paths,
            ignored,
        });
        *self.inner.files.write() = list.clone();
        list
    }

    /// Whether git ignores `rel` (needs `files.show_ignored` and a prior `rescan_files`).
    pub fn is_ignored(&self, rel: &str) -> bool {
        self.files().is_ignored(rel)
    }

    /// Git state of a file for display: status entry, else `Ignored`.
    pub fn file_state(&self, rel: &str) -> Option<FileGitState> {
        if let Some(s) = self.git_status().get(rel) {
            return Some(s.state);
        }
        self.is_ignored(rel).then_some(FileGitState::Ignored)
    }

    /// Latest status snapshot (empty until the first refresh).
    pub fn git_status(&self) -> Arc<GitStatusMap> {
        self.inner.git_status.read().clone()
    }

    /// Re-reads `git status` and stores the new snapshot. Without a repository: an empty map.
    pub fn refresh_git_status(&self) -> Result<Arc<GitStatusMap>> {
        let map = match &self.inner.repo {
            Some(repo) => Arc::new(GitStatusMap::build(
                &repo.status()?,
                &self.inner.repo_prefix,
            )),
            None => Arc::new(GitStatusMap::default()),
        };
        *self.inner.git_status.write() = map.clone();
        Ok(map)
    }

    /// Replaces the status snapshot with one built from already computed
    /// `git status` entries (v0.4: the shared `lector_app` git state computes
    /// status once per repository for every window).
    pub fn set_git_status(&self, entries: &[lector_git::StatusEntry]) -> Arc<GitStatusMap> {
        let map = match &self.inner.repo {
            Some(_) => Arc::new(GitStatusMap::build(entries, &self.inner.repo_prefix)),
            None => Arc::new(GitStatusMap::default()),
        };
        *self.inner.git_status.write() = map.clone();
        map
    }

    /// Indexes tags of all project files into [`Project::symbols`]. Skips files > 1 MB, binary
    /// or unknown-language files, indexes at most 20k files. `progress(done, total)` is called
    /// on the calling thread.
    pub fn index_symbols(&self, cancel: &CancelToken, progress: impl Fn(usize, usize)) {
        let mut files = self.files();
        if files.paths.is_empty() {
            files = self.rescan_files(cancel);
        }
        let candidates: Vec<Arc<str>> = files
            .paths
            .iter()
            .filter(|p| {
                self.inner
                    .languages
                    .language_for_path(Path::new(&***p), None)
                    .is_some()
            })
            .take(INDEX_MAX_FILES)
            .cloned()
            .collect();
        let total = candidates.len();
        progress(0, total);
        if total == 0 {
            return;
        }
        let next = AtomicUsize::new(0);
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .clamp(1, 8);
        let (tx, rx) = mpsc::channel::<()>();
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let tx = tx.clone();
                let next = &next;
                let candidates = &candidates;
                scope.spawn(move || loop {
                    if cancel.is_cancelled() {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(rel) = candidates.get(i) else { break };
                    self.index_file_inner(&self.absolute_path(rel));
                    if tx.send(()).is_err() {
                        break;
                    }
                });
            }
            drop(tx);
            let mut done = 0;
            while rx.recv().is_ok() {
                done += 1;
                if done % 25 == 0 {
                    progress(done, total);
                }
            }
            progress(done, total);
        });
    }

    /// (Re)indexes the tags of one file (absolute path); removes it from the index when it
    /// can't be indexed any more. Call after file-watcher events.
    pub fn reindex_file(&self, abs: &Path) {
        if !self.index_file_inner(abs) {
            self.inner.symbols.remove_file(abs);
        }
    }

    fn index_file_inner(&self, abs: &Path) -> bool {
        let Some(lang) = self.inner.languages.language_for_path(abs, None) else {
            return false;
        };
        let Ok(meta) = std::fs::metadata(abs) else {
            return false;
        };
        if !meta.is_file() || meta.len() > INDEX_MAX_FILE_BYTES {
            return false;
        }
        let Ok(bytes) = std::fs::read(abs) else {
            return false;
        };
        if lector_fs::is_binary(&bytes[..bytes.len().min(lector_fs::SNIFF_LEN)]) {
            return false;
        }
        let (s, _, _) = lector_fs::decode(&bytes);
        let text = Text::from_str(&s);
        let parsed = std::panic::catch_unwind(|| Syntax::parse(&lang, &text));
        let Ok(Ok(syntax)) = parsed else {
            return false;
        };
        let tags = syntax.tags(&text);
        self.inner
            .symbols
            .insert_file(abs.to_path_buf(), &text, tags);
        true
    }

    /// Loads `path` (absolute, or relative to the root) as an immutable [`Document`].
    pub fn load_document(&self, path: &Path) -> Result<Document> {
        let abs = if path.is_absolute() {
            // Use the project's spelling of paths inside it, so documents, symbols and
            // git paths compare equal whatever form the caller used.
            match self.relative_path(path) {
                Some(rel) => self.absolute_path(&rel),
                None => path.to_path_buf(),
            }
        } else {
            self.inner.root.join(path)
        };
        let settings = self.settings();
        let threshold = settings
            .files
            .large_file_threshold_mb
            .saturating_mul(1024 * 1024);
        let loaded = lector_fs::load_file(&abs, threshold)?;
        let rel = self.relative_path(&abs);
        Ok(Document::build(
            abs,
            rel,
            loaded,
            &self.inner.languages,
            settings.tab_size.max(1),
        ))
    }

    /// Blame of `rel_path` (relative to the root), cached by (path, HEAD, mtime, size).
    pub fn blame(&self, rel_path: &str) -> Result<Arc<Vec<BlameHunk>>> {
        let repo = self.inner.repo.as_ref().ok_or(Error::NotARepository)?;
        let meta = std::fs::metadata(self.absolute_path(rel_path)).ok();
        let key = BlameKey {
            head: repo.head().ok().and_then(|h| h.oid),
            mtime: meta.as_ref().and_then(|m| m.modified().ok()),
            size: meta.map(|m| m.len()).unwrap_or(0),
        };
        if let Some((k, v)) = self.inner.blame_cache.lock().get(rel_path) {
            if *k == key {
                return Ok(v.clone());
            }
        }
        let hunks = Arc::new(repo.blame(&self.repo_path(rel_path), None)?);
        self.inner
            .blame_cache
            .lock()
            .insert(rel_path.to_string(), (key, hunks.clone()));
        Ok(hunks)
    }

    /// Path relative to the root, `/`-separated; `None` when `abs` is outside the project.
    ///
    /// Accepts non-canonical spellings of paths inside the root too (a symlinked
    /// parent such as macOS `/var` → `/private/var`, or `\\?\` prefixes on Windows).
    pub fn relative_path(&self, abs: &Path) -> Option<String> {
        if let Ok(rel) = abs.strip_prefix(&self.inner.root) {
            let s = to_slash(rel);
            return (!s.is_empty()).then_some(s);
        }
        let canonical = canonical_spelling(abs)?;
        let s = to_slash(canonical.strip_prefix(&self.inner.root).ok()?);
        (!s.is_empty()).then_some(s)
    }

    /// Absolute path of a root-relative path.
    pub fn absolute_path(&self, rel: &str) -> PathBuf {
        // Join per component so Windows paths get native separators throughout.
        rel.split('/')
            .filter(|part| !part.is_empty())
            .fold(self.inner.root.clone(), |acc, part| acc.join(part))
    }

    /// Repo-relative path for a root-relative one (differs when the root is a repo subfolder).
    pub fn repo_path(&self, rel: &str) -> String {
        format!("{}{}", self.inner.repo_prefix, rel)
    }

    /// Syncs `doc` to the language server once per text version.
    pub(crate) fn lsp_open(&self, doc: &Document) -> bool {
        let (Some(lsp), Some(lang), Some(text)) = (self.lsp(), doc.language(), doc.text()) else {
            return false;
        };
        if text.is_mapped() {
            return false;
        }
        let version = text.version();
        let mut opened = self.inner.lsp_opened.lock();
        if opened.get(doc.path()) != Some(&version) {
            lsp.open_document(doc.path(), &lang.name, &text.to_string());
            opened.insert(doc.path().to_path_buf(), version);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_that_write_into_the_project_are_opt_in() {
        let names =
            |s: &Settings| -> Vec<String> { lsp_configs(s).into_iter().map(|c| c.name).collect() };
        let mut settings = Settings::default();
        let default = names(&settings);
        assert!(default.contains(&"rust-analyzer".to_string()));
        assert!(!default.contains(&"jdtls".to_string()));
        settings.lsp.servers.insert(
            "jdtls".into(),
            lector_config::ServerOverride {
                enabled: Some(true),
                ..Default::default()
            },
        );
        settings.lsp.servers.insert(
            "gopls".into(),
            lector_config::ServerOverride {
                enabled: Some(false),
                ..Default::default()
            },
        );
        let opted = names(&settings);
        assert!(opted.contains(&"jdtls".to_string()));
        assert!(!opted.contains(&"gopls".to_string()));
    }
}
