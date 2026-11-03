# Lector architecture (v0.1.0)

Lector is a **reading-first** code viewer and git review tool. v0.1.0 is strictly
**read-only**: it never writes to the user's files or repository. It may write
its own settings, session and review state under the platform data directory.

## Principles

1. **UI-free core.** Every crate except `lector-ui` and `lector` compiles and
   tests without a GPU, a window, or GPUI. Core crates are independently
   reusable and publishable (MIT OR Apache-2.0).
2. **Layering is strict.** Lower crates never depend on higher ones:

   ```
   lector (bin)
     └─ lector-ui ─────────────── GPUI shell, views, widgets
          └─ lector-workspace ─── UI-free app model (project, documents, review state, navigation)
               ├─ lector-git      gix read paths + git CLI (blame, line history)
               ├─ lector-lsp      read-only LSP client
               ├─ lector-search   walker, fuzzy picker (nucleo), project grep
               ├─ lector-syntax   tree-sitter registry, highlights, outline, folds, tags
               ├─ lector-diff     line diff (imara-diff), word refine, split/unified rows
               ├─ lector-fs       file loading/kind detection, debounced watcher
               ├─ lector-config   settings, keymaps, paths, session store
               ├─ lector-theme    theme model + built-in themes
               ├─ lector-icons    Material file icons + UI icons (SVG bytes)
               └─ lector-text     SumTree, Rope, mmap text, coordinates (no internal deps)
   ```
   Allowed internal edges: `syntax→text`, `fs→text`, `diff→(none)`, `search→text?`,
   `workspace→all core`, `ui→all`. Nothing depends on `ui`.
3. **Snapshots, not locks.** Text is immutable and cheaply clonable (`Arc`).
   Background work (parse, highlight, diff, git, search) runs on snapshots and
   results are tagged with the snapshot version; stale results are dropped.
4. **No async runtime in the core.** Core APIs are synchronous (blocking,
   meant to be called from a background thread) or return runtime-agnostic
   futures (`futures` channels). No tokio dependency in core crates.
5. **Cancellation everywhere** long work happens: `CancelToken` (an `Arc<AtomicBool>`).
6. **Never panic on user data.** Return `Result`; libraries use `thiserror`,
   the app edge uses `anyhow`.
7. **Every user command is an action** (palette, keybinding, menu, context menu
   all dispatch the same GPUI action). This is the future hook for agents/MCP.

## Coordinates

* Byte offsets are `usize`, UTF-8, into the text.
* `lector_text::Point { row: u32, column: u32 }` — 0-based, `column` in **bytes**.
* `lector_text::PointUtf16 { row, column }` — `column` in UTF-16 code units (LSP).
* Line numbers in all APIs are **0-based** `u32`; the UI adds 1 for display.

## Crate contracts

These are the public APIs other crates rely on. Implementations may add more,
but must not change these signatures without updating this doc.

### lector-text
```rust
pub struct Point { pub row: u32, pub column: u32 }          // Copy, Ord, Hash, Default
pub struct PointUtf16 { pub row: u32, pub column: u32 }
pub struct TextSummary { pub len: usize, pub chars: usize, pub len_utf16: usize,
                         pub lines: Point, pub longest_row: u32, pub longest_row_chars: u32 }
pub mod sum_tree { /* generic copy-on-write B+tree with summaries + cursors (Zed-style) */
    pub trait Item: Clone { type Summary: Summary; fn summary(&self) -> Self::Summary; }
    pub trait Summary: Clone + Default + std::fmt::Debug { fn add_summary(&mut self, other: &Self); }
    pub trait Dimension<'a, S: Summary>: Clone + Default { fn add_summary(&mut self, s: &'a S); }
    pub struct SumTree<T: Item>;   // Clone is O(1) (Arc); from_iter, push, append, summary, cursor, iter, is_empty
    pub struct Cursor<'a, T, D>;   // seek(&target, Bias), next/prev, item, start, end
    pub enum Bias { Left, Right }
}
pub struct Rope;                    // Clone O(1). Built on SumTree<Chunk>.
impl Rope {
    pub fn new() -> Self; pub fn from_str(s: &str) -> Self;  // also impl From<&str>, From<String>
    pub fn len(&self) -> usize; pub fn is_empty(&self) -> bool;
    pub fn line_count(&self) -> u32;                 // "a\nb" = 2, "a\n" = 2, "" = 1
    pub fn summary(&self) -> TextSummary; pub fn max_point(&self) -> Point;
    pub fn line_range(&self, row: u32) -> std::ops::Range<usize>;   // excludes '\n'
    pub fn line_len(&self, row: u32) -> u32;
    pub fn offset_to_point(&self, offset: usize) -> Point; pub fn point_to_offset(&self, p: Point) -> usize;
    pub fn offset_to_point_utf16(&self, offset: usize) -> PointUtf16;
    pub fn point_utf16_to_offset(&self, p: PointUtf16) -> usize;
    pub fn clip_offset(&self, offset: usize, bias: Bias) -> usize;  // to char boundary
    pub fn chunks_in_range(&self, range: Range<usize>) -> impl Iterator<Item = &str>;
    pub fn chunks(&self) -> impl Iterator<Item = &str>;
    pub fn slice(&self, range: Range<usize>) -> Rope; pub fn text_for_range(&self, range) -> String;
    pub fn chars_at(&self, offset) -> impl Iterator<Item = char>; pub fn reversed_chars_at(&self, offset) -> ...;
    pub fn replace(&mut self, range: Range<usize>, text: &str);   // for future editing
    pub fn to_string(&self) -> String;
}
pub struct MappedText;              // memory-mapped, read-only, for very large files (lossy UTF-8)
pub enum Text { Rope(Rope), Mapped(Arc<MappedText>) }   // Clone O(1), Send + Sync
impl Text {
    pub fn from_str(s: &str) -> Text;
    pub fn open_mapped(path: &Path) -> io::Result<Text>;
    pub fn version(&self) -> u64;            // unique per distinct content snapshot
    pub fn is_mapped(&self) -> bool;
    // same read API as Rope: len, line_count, line_range, line_len, offset_to_point,
    // point_to_offset, *_utf16, clip_offset, text_for_range(range) -> Cow<str> / String,
    // line(row) -> Cow<str>, chunks_in_range(range) -> Box<dyn Iterator<Item=&str> + '_>
    pub fn lines(&self, rows: Range<u32>) -> impl Iterator<Item = Cow<'_, str>>;
}
```
`MappedText` must open a 1 GB file and serve the first screen in < 500 ms; the
line index may finish in the background but `line_count()` must then be exact.

### lector-syntax
```rust
pub const HIGHLIGHT_NAMES: &[&str];          // canonical capture names, e.g. "keyword", "function.method"
pub struct HighlightId(pub u16);             // index into HIGHLIGHT_NAMES
pub struct HighlightSpan { pub range: Range<usize>, pub id: HighlightId }   // byte range in the text
pub struct Language { pub name: String, pub lsp_id: String, pub line_comment: Option<String>, .. }
pub struct LanguageRegistry;  // new() loads ~20 built-ins; Send + Sync
impl LanguageRegistry {
    pub fn language_for_path(&self, path: &Path, first_line: Option<&str>) -> Option<Arc<Language>>;
    pub fn by_name(&self, name: &str) -> Option<Arc<Language>>;
    pub fn languages(&self) -> Vec<Arc<Language>>;
}
pub struct Syntax;   // parsed tree for one Text snapshot; Clone, Send + Sync
impl Syntax {
    pub fn parse(lang: &Arc<Language>, text: &Text) -> Result<Syntax>;  // reads via chunks, no full copy
    pub fn language(&self) -> &Arc<Language>;
    pub fn highlights(&self, text: &Text, range: Range<usize>) -> Vec<HighlightSpan>; // sorted, non-overlapping
    pub fn outline(&self, text: &Text) -> Vec<OutlineItem>;
    pub fn enclosing_scopes(&self, text: &Text, offset: usize) -> Vec<OutlineItem>;   // outermost first
    pub fn fold_ranges(&self, text: &Text) -> Vec<Range<u32>>;                       // row ranges
    pub fn bracket_pair(&self, text: &Text, offset: usize) -> Option<(Range<usize>, Range<usize>)>;
    pub fn tags(&self, text: &Text) -> Vec<Tag>;
    pub fn node_at(&self, offset: usize) -> Option<NodeInfo>;   // kind + range, for word/ident lookup
}
pub struct OutlineItem { pub name: String, pub kind: SymbolKind, pub range: Range<usize>,
                         pub name_range: Range<usize>, pub depth: u32 }
pub enum SymbolKind { Module, Class, Struct, Enum, Interface, Trait, Function, Method, Field,
                      Variable, Constant, Type, Macro, Heading, Other }
pub struct Tag { pub name: String, pub kind: SymbolKind, pub range: Range<usize>, pub is_definition: bool }
pub struct SymbolIndex;  // workspace-wide tags index (fallback navigation without LSP)
impl SymbolIndex {
    pub fn insert_file(&self, path: PathBuf, text: &Text, tags: Vec<Tag>);  // interior mutability, Sync
    pub fn remove_file(&self, path: &Path);
    pub fn definitions(&self, name: &str) -> Vec<SymbolLocation>;
    pub fn references(&self, name: &str) -> Vec<SymbolLocation>;
    pub fn all_definitions(&self) -> Vec<SymbolLocation>;   // for the symbol picker
}
pub struct SymbolLocation { pub path: PathBuf, pub name: String, pub kind: SymbolKind,
                            pub range: Range<usize>, pub point: Point }
```

### lector-diff
```rust
pub enum Algorithm { Histogram, Myers }
pub enum Whitespace { Exact, IgnoreAll, IgnoreChange, IgnoreEol }
pub struct DiffOptions { pub algorithm: Algorithm, pub whitespace: Whitespace }  // Default: Histogram, Exact
pub struct Hunk { pub old: Range<u32>, pub new: Range<u32> }      // 0-based line ranges
pub struct LineDiff { pub hunks: Vec<Hunk>, pub old_lines: u32, pub new_lines: u32 }
impl LineDiff { pub fn stats(&self) -> DiffStats; }   // DiffStats { added: u32, removed: u32 }
pub fn diff_lines(old: &str, new: &str, opts: &DiffOptions) -> LineDiff;
pub struct InlineChanges { pub old: Vec<Range<usize>>, pub new: Vec<Range<usize>> } // byte ranges
pub fn refine_inline(old: &str, new: &str) -> InlineChanges;    // word-level, char-level inside words
pub enum RowKind { Context, Added, Removed, Changed }
pub struct SplitRow { pub kind: RowKind, pub old: Option<u32>, pub new: Option<u32>, pub hunk: Option<usize> }
pub struct UnifiedRow { pub kind: RowKind, pub old: Option<u32>, pub new: Option<u32>, pub hunk: Option<usize> }
pub fn split_rows(diff: &LineDiff) -> Vec<SplitRow>;
pub fn unified_rows(diff: &LineDiff) -> Vec<UnifiedRow>;
pub enum Segment { Rows(Range<usize>), Folded { rows: Range<usize> } }   // indices into a rows vec
pub fn fold_context<R: HasKind>(rows: &[R], context: u32) -> Vec<Segment>;
pub enum GutterKind { Added, Modified, Deleted }
pub struct GutterMark { pub rows: Range<u32>, pub kind: GutterKind }   // rows in NEW text; Deleted = empty range at row
pub fn gutter_marks(diff: &LineDiff) -> Vec<GutterMark>;
pub fn is_binary(bytes: &[u8]) -> bool;
```

### lector-search
```rust
pub struct CancelToken; // new(), cancel(), is_cancelled(); Clone
pub struct WalkOptions { pub hidden: bool, pub git_ignore: bool, pub exclude: Vec<String>, pub max_depth: Option<usize> }
pub fn walk_files(root: &Path, opts: &WalkOptions, cancel: &CancelToken,
                  sink: impl Fn(PathBuf) + Send + Sync);           // parallel, relative paths
pub struct Picker<T: Send + Sync + 'static>;   // nucleo wrapper, streaming
impl<T> Picker<T> {
    pub fn new(notify: Arc<dyn Fn() + Send + Sync>) -> Self;
    pub fn injector(&self) -> PickerInjector<T>;    // push(item, haystack: &str); Clone + Send
    pub fn set_query(&mut self, query: &str);
    pub fn tick(&mut self, timeout_ms: u64) -> bool;   // true while still matching
    pub fn matched_count(&self) -> u32; pub fn total_count(&self) -> u32;
    pub fn matches(&self, range: Range<u32>) -> Vec<PickerMatch<'_, T>>;  // { item, score, indices: Vec<u32> }
    pub fn clear(&mut self);
}
pub enum CaseMode { Smart, Sensitive, Insensitive }
pub struct SearchQuery { pub pattern: String, pub regex: bool, pub case: CaseMode, pub whole_word: bool,
                         pub include: Vec<String>, pub exclude: Vec<String>, pub max_results: usize }
pub struct LineMatch { pub line: u32, pub text: String, pub ranges: Vec<Range<usize>> } // ranges in `text`
pub struct FileMatches { pub path: PathBuf, pub matches: Vec<LineMatch> }
pub enum SearchEvent { File(FileMatches), Done { files_searched: usize, matches: usize, truncated: bool }, Error(String) }
pub fn search_project(root: &Path, query: &SearchQuery, walk: &WalkOptions, cancel: &CancelToken,
                      sink: impl Fn(SearchEvent) + Send + Sync) -> Result<()>;
pub fn find_in_text(text: &str, query: &SearchQuery) -> Result<Vec<Range<usize>>>;   // in-file find
```

### lector-git
```rust
pub struct Oid;  // 20/32-byte object id; Display (full hex), short() -> String (7), Copy, Eq, Hash, Ord
pub struct Repo; // Clone, Send + Sync. Uses gix for reads; spawns `git` CLI only where noted.
impl Repo {
    pub fn discover(path: &Path) -> Result<Repo>;
    pub fn workdir(&self) -> &Path;
    pub fn head(&self) -> Result<HeadInfo>;   // { branch: Option<String>, oid: Option<Oid>, detached: bool, upstream: Option<String>, ahead: u32, behind: u32 }
    pub fn status(&self) -> Result<Vec<StatusEntry>>;
    pub fn changes(&self, cs: &Changeset) -> Result<Vec<FileChange>>;
    pub fn load(&self, side: &Side) -> Result<Option<Vec<u8>>>;    // None for Side::None / missing
    pub fn log(&self, q: &LogQuery) -> Result<Vec<CommitInfo>>;
    pub fn commit(&self, oid: Oid) -> Result<CommitDetail>;
    pub fn resolve(&self, rev: &str) -> Result<Oid>;
    pub fn merge_base(&self, a: Oid, b: Oid) -> Result<Option<Oid>>;
    pub fn branches(&self) -> Result<Vec<Branch>>; pub fn tags(&self) -> Result<Vec<Tag>>;
    pub fn stashes(&self) -> Result<Vec<Stash>>;
    pub fn file_at(&self, rev: &str, path: &str) -> Result<Option<Vec<u8>>>;
    pub fn head_text(&self, path: &str) -> Result<Option<Vec<u8>>>;   // HEAD version, for diff gutter
    pub fn blame(&self, path: &str, rev: Option<&str>) -> Result<Vec<BlameHunk>>;   // git CLI --porcelain, includes uncommitted
    pub fn line_history(&self, path: &str, lines: Range<u32>, limit: usize) -> Result<Vec<CommitInfo>>; // git log -L
    pub fn file_history(&self, path: &str, limit: usize) -> Result<Vec<CommitInfo>>;   // follows renames
    pub fn remotes(&self) -> Result<Vec<Remote>>;   // { name, url }
    pub fn permalink(&self, path: &str, lines: Option<Range<u32>>, rev: Option<&str>) -> Result<Option<String>>;
}
pub struct StatusEntry { pub path: String, pub staged: Option<ChangeKind>, pub unstaged: Option<ChangeKind>,
                         pub untracked: bool, pub conflicted: bool, pub renamed_from: Option<String> }
pub enum ChangeKind { Added, Modified, Deleted, Renamed, Copied, TypeChange }
pub enum Changeset { Unstaged, Staged, Untracked, WorkingTree /*HEAD→workdir incl. untracked*/,
                     Commit(Oid) /*vs first parent*/, Range { from: String, to: String },
                     BranchVsBase { branch: String, base: String } /*merge-base(base,branch)→branch*/ }
pub enum Side { None, Blob(Oid), Index(String), Workdir(String) }
pub struct FileChange { pub old_path: Option<String>, pub new_path: Option<String>, pub kind: ChangeKind,
                        pub old: Side, pub new: Side, pub binary: Option<bool> }
pub struct LogQuery { pub rev: Option<String> /*None = HEAD*/, pub all_refs: bool, pub path: Option<String>,
                      pub author: Option<String>, pub message: Option<String>, pub skip: usize, pub limit: usize }
pub struct Signature { pub name: String, pub email: String, pub time: i64 /*unix secs*/, pub offset_minutes: i32 }
pub struct CommitInfo { pub oid: Oid, pub parents: Vec<Oid>, pub author: Signature, pub committer: Signature,
                        pub summary: String, pub refs: Vec<RefLabel> }
pub struct RefLabel { pub name: String, pub kind: RefKind /*LocalBranch, RemoteBranch, Tag, Head*/ }
pub struct CommitDetail { pub info: CommitInfo, pub message: String, pub changes: Vec<FileChange> }
pub struct GraphRow { pub lane: u16, pub lanes: u16, pub edges: Vec<GraphEdge> }  // edges: { from_lane, to_lane, kind: Straight|MergeIn|BranchOut }
pub fn graph(commits: &[CommitInfo]) -> Vec<GraphRow>;
pub struct Branch { pub name: String, pub remote: bool, pub head: bool, pub oid: Oid, pub upstream: Option<String>, pub time: i64 }
pub struct Tag { pub name: String, pub oid: Oid }   pub struct Stash { pub index: usize, pub message: String, pub oid: Oid }
pub struct BlameHunk { pub oid: Oid, pub lines: Range<u32>, pub orig_start: u32, pub orig_path: String,
                       pub author: String, pub time: i64, pub summary: String, pub uncommitted: bool }
```

### lector-lsp
```rust
pub struct ServerConfig { pub name: String, pub command: String, pub args: Vec<String>,
                          pub languages: Vec<String> /* Language::name */, pub root_markers: Vec<String> }
pub fn builtin_servers() -> Vec<ServerConfig>;     // rust-analyzer, typescript-language-server, pyright, gopls, clangd, ...
pub struct LspManager; // Clone + Send + Sync; one server per (root, server); lazy start; restart w/ backoff
impl LspManager {
    pub fn new(root: PathBuf, configs: Vec<ServerConfig>) -> Self;
    pub fn status(&self) -> Vec<ServerStatus>;        // { name, state: Starting|Running|Failed(String)|NotInstalled }
    pub fn open_document(&self, path: &Path, language: &str, text: &str);
    pub fn close_document(&self, path: &Path);
    // All return runtime-agnostic futures; resolve to Ok(None/empty) when no server is available.
    pub fn hover(&self, path: &Path, pos: PointUtf16) -> impl Future<Output = Result<Option<HoverInfo>>>;
    pub fn definition(&self, path: &Path, pos: PointUtf16) -> impl Future<Output = Result<Vec<Location>>>;
    pub fn references(&self, path: &Path, pos: PointUtf16) -> impl Future<Output = Result<Vec<Location>>>;
    pub fn document_symbols(&self, path: &Path) -> impl Future<Output = Result<Vec<DocumentSymbol>>>;
    pub fn workspace_symbols(&self, query: &str) -> impl Future<Output = Result<Vec<WorkspaceSymbol>>>;
    pub fn shutdown(&self);
}
pub struct Location { pub path: PathBuf, pub range: Range<PointUtf16> }
pub struct HoverInfo { pub markdown: String, pub range: Option<Range<PointUtf16>> }
```

### lector-fs
```rust
pub enum FileKind { Text, Binary, Image(ImageFormat) /*png, jpg, gif, webp, svg, bmp, ico*/, Directory }
pub struct LoadedFile { pub kind: FileKind, pub text: Option<Text>, pub size: u64, pub encoding: &'static str, pub lossy: bool }
pub fn detect_kind(path: &Path, head: &[u8]) -> FileKind;
pub fn load_file(path: &Path, mmap_threshold: u64) -> io::Result<LoadedFile>;  // >= threshold → Text::Mapped
pub fn decode(bytes: &[u8]) -> (String, &'static str /*encoding*/, bool /*lossy*/);  // UTF-8, UTF-16 BOM, latin1 fallback
pub enum FsEventKind { Created, Modified, Removed }
pub struct FsEvent { pub path: PathBuf, pub kind: FsEventKind }
pub enum WatchEvent { Files(Vec<FsEvent>), GitStateChanged /* .git/HEAD, index, refs */, Rescan }
pub struct Watcher;   // drop to stop
pub fn watch(root: &Path, debounce: Duration, sink: impl Fn(WatchEvent) + Send + 'static) -> Result<Watcher>;
```

### lector-config
```rust
pub struct Settings { /* serde(default) everywhere; see crate docs */ }
pub fn load_settings() -> (Settings, Vec<String> /*warnings*/);   // defaults ← user settings.toml ← project .lector/settings.toml
pub fn default_settings_toml() -> &'static str;   // documented, commented default file
pub struct KeyBinding { pub keys: String /* "cmd-p" / "ctrl-shift-f" (gpui syntax) */, pub action: String, pub context: Option<String> }
pub fn default_keymap(platform: Platform) -> Vec<KeyBinding>;   pub fn load_keymap() -> (Vec<KeyBinding>, Vec<String>);
pub mod paths { pub fn config_dir() -> PathBuf; pub fn data_dir() -> PathBuf; pub fn settings_file() -> PathBuf; pub fn keymap_file() -> PathBuf; pub fn themes_dir() -> PathBuf; pub fn log_file() -> PathBuf; }
pub struct SessionStore; // load(root) -> Option<Session>, save(root, &Session) — JSON under data_dir/sessions/<hash>.json
pub struct ReviewStore;  // viewed-file state per (repo root, changeset key): is_viewed/set_viewed, persisted JSON
```

### lector-theme
```rust
pub struct Color { pub r: u8, pub g: u8, pub b: u8, pub a: u8 }  // parse "#rrggbb[aa]"
pub enum Appearance { Light, Dark }
pub struct Theme { pub name: String, pub appearance: Appearance, pub ui: UiColors, pub syntax: SyntaxTheme,
                   pub diff: DiffColors, pub git: GitColors }
impl SyntaxTheme { pub fn style_for(&self, capture: &str) -> Option<TextStyle>; } // falls back "a.b.c" → "a.b" → "a"
pub struct TextStyle { pub color: Color, pub bold: bool, pub italic: bool }
pub struct ThemeRegistry; // builtins: "Lector Dark", "Lector Light", "Lector High Contrast Dark", "Lector High Contrast Light"; + user TOML themes
```

### lector-icons
```rust
pub struct FileIcons;   // Material Icon Theme (MIT) mapping + embedded SVGs
impl FileIcons {
    pub fn get() -> &'static FileIcons;
    pub fn for_file(&self, file_name: &str, light: bool) -> IconRef;
    pub fn for_folder(&self, folder_name: &str, expanded: bool, root: bool, light: bool) -> IconRef;
}
pub struct IconRef { pub name: &'static str, pub svg: &'static [u8] }
pub enum UiIcon { ChevronRight, ChevronDown, Search, Files, GitBranch, GitCommit, History, ... }
pub fn ui_icon(icon: UiIcon) -> &'static [u8];   // monochrome SVG (Lucide, ISC), tinted by the UI
```

## Threading model (UI)

* GPUI main thread owns entities (`Workspace`, `Pane`, `CodeView`, ...).
* Expensive work goes through `cx.background_spawn` on snapshots; results are
  applied with `this.update(cx, ..)` and `cx.notify()`.
* Highlights are computed per visible range (+ margin) and cached per
  `(text.version(), row-chunk)`.

## Performance budgets (v0.1.0 release gates)

| What | Target |
|---|---|
| Cold start → first paint | < 150 ms |
| Scroll | 120 fps, p99 frame < 8 ms |
| Open 1 GB file → first paint | < 500 ms |
| git status, Linux-kernel-sized repo | < 300 ms |
| Fuzzy file finder, 1M paths, first results | < 50 ms |

## Actions (shared by keymap, palette, menus, context menus)

GPUI action names are `namespace::Name`. `lector-config` default keymaps and
`lector-ui` action definitions must use exactly these names.

* `workspace::` OpenFolder, CloseWindow, Quit, ToggleCommandPalette, ToggleFileFinder,
  ToggleSymbolFinder, ToggleOutline, ToggleProjectSearch, ToggleSidebar, ShowFiles,
  ShowChanges, ShowHistory, ShowBranches, SplitRight, SplitDown, CloseTab, ReopenClosedTab,
  NextTab, PrevTab, GoBack, GoForward, RevealActiveFile, OpenSettings, OpenKeymap,
  ReloadSettings, ZoomIn, ZoomOut, ZoomReset, ToggleAppearance, SelectTheme
* `editor::` GoToLine, Find, FindNext, FindPrev, GoToDefinition, FindReferences, Hover,
  ToggleFold, FoldAll, UnfoldAll, Copy, SelectAll, CopyPath, CopyRelativePath, CopyPermalink,
  ToggleBlame, ToggleSoftWrap, ToggleMinimap, ToggleMarkdownPreview, ShowFileHistory,
  ShowLineHistory, NextChange, PrevChange, MoveUp, MoveDown, MoveLeft, MoveRight, PageUp,
  PageDown, ScrollToTop, ScrollToBottom, SelectUp, SelectDown, SelectLeft, SelectRight
* `diff::` ToggleLayout, NextHunk, PrevHunk, NextFile, PrevFile, ToggleViewed,
  ToggleIgnoreWhitespace, ExpandAllContext, OpenInEditor
* `picker::` Confirm, Cancel, SelectNext, SelectPrev

Default keys (macOS uses `cmd`, Linux/Windows use `ctrl` in its place):
file finder `cmd-p`, command palette `cmd-shift-p`, project symbols `cmd-t`, outline
`cmd-shift-o`, project search `cmd-shift-f`, sidebar `cmd-b`, files `cmd-shift-e`,
changes `ctrl-shift-g`, history `ctrl-shift-h`, close tab `cmd-w`, reopen `cmd-shift-t`,
next/prev tab `ctrl-tab`/`ctrl-shift-tab`, back/forward `ctrl--`/`ctrl-shift--`
(Linux/Windows: `alt-left`/`alt-right`), go to line `ctrl-g`, find `cmd-f`, next/prev
`cmd-g`/`cmd-shift-g` (Linux/Windows: `f3`/`shift-f3`), definition `f12`, references
`shift-f12`, hover `cmd-k cmd-i`, fold/unfold `cmd-alt-[`/`cmd-alt-]`, soft wrap `alt-z`,
blame `cmd-alt-b`, markdown preview `cmd-shift-v`, zoom `cmd-=`/`cmd--`/`cmd-0`,
diff next/prev hunk `alt-n`/`alt-p` (also `]c`-free plain keys `n`/`p` in diff context),
next/prev file `alt-j`/`alt-k`, toggle viewed `v` (diff context), toggle layout `cmd-alt-d`.
