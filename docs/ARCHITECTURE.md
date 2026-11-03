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
pub struct Point { pub row: u32, pub column: u32 }          // Copy, Ord, Hash, Default (+ serde feature)
pub struct PointUtf16 { pub row: u32, pub column: u32 }     // same; also CharOffset(usize), OffsetUtf16(usize)
pub struct TextSummary { pub len: usize, pub chars: usize, pub len_utf16: usize,
                         pub lines: Point, pub longest_row: u32, pub longest_row_chars: u32,
                         /* + first_line_chars, last_line_chars, last_line_len_utf16 */ }
pub mod sum_tree { /* generic copy-on-write B+tree with summaries + cursors (Zed-style) */
    pub trait Item: Clone { type Summary: Summary; fn summary(&self) -> Self::Summary; }
    pub trait Summary: Clone + Default + std::fmt::Debug { fn add_summary(&mut self, other: &Self); }
    pub trait Dimension<'a, S: Summary>: Clone + Default { fn add_summary(&mut self, s: &'a S); /* + from_summary */ }
    // (D1, D2) and (D1, D2, D3) are Dimensions; a summary is a Dimension of itself; () is the unit dimension.
    pub trait SeekTarget<'a, S, D> { fn cmp(&self, cursor_location: &D) -> Ordering; }
    // impl'd for D: Dimension + Ord, and for D1 against (D1, D2[, D3]) (compares the first component).
    pub struct SumTree<T: Item>;   // Clone is O(1) (Arc); new, from_item, from_iter, push, append, extend,
                                   // update_last, summary, extent::<D>, first, last, cursor, iter, is_empty
    pub struct Cursor<'a, T, D>;   // seek(&target, Bias) -> bool, seek_forward, slice(&target, Bias) -> SumTree,
                                   // suffix, next/prev, item, item_summary, start() -> &D, end() -> D
    pub enum Bias { Left, Right }  // also re-exported at the crate root
}
pub struct Rope;                    // Clone O(1). Built on SumTree<Chunk> (<=128-byte inline chunks).
impl Rope {
    pub fn new() -> Self; pub fn from_str(s: &str) -> Self;  // also impl From<&str>, From<String>, FromStr
    pub fn len(&self) -> usize; pub fn is_empty(&self) -> bool;
    pub fn line_count(&self) -> u32;                 // "a\nb" = 2, "a\n" = 2, "" = 1
    pub fn summary(&self) -> TextSummary; pub fn max_point(&self) -> Point;
    pub fn line_range(&self, row: u32) -> std::ops::Range<usize>;   // excludes "\n" and "\r\n"; past end -> len..len
    pub fn line_len(&self, row: u32) -> u32;                        // == line_range(row).len()
    pub fn offset_to_point(&self, offset: usize) -> Point; pub fn point_to_offset(&self, p: Point) -> usize;
    pub fn offset_to_point_utf16(&self, offset: usize) -> PointUtf16;
    pub fn point_utf16_to_offset(&self, p: PointUtf16) -> usize;
    pub fn clip_offset(&self, offset: usize, bias: Bias) -> usize;  // to char boundary
    pub fn chunks_in_range(&self, range: Range<usize>) -> Chunks<'_>;   // Iterator<Item = &str>
    pub fn chunks(&self) -> Chunks<'_>;
    pub fn slice(&self, range: Range<usize>) -> Rope; pub fn text_for_range(&self, range) -> String;
    pub fn chars_at(&self, offset) -> impl Iterator<Item = char>; pub fn reversed_chars_at(&self, offset) -> ...;
    pub fn replace(&mut self, range: Range<usize>, text: &str);   // for future editing
    pub fn to_string(&self) -> String;   // via Display
    pub fn version(&self) -> u64;        // see Text::version
    // + push, append, byte_at, clip_point, max_point_utf16, text_summary_for_range,
    //   offset_to_char_offset/char_offset_to_offset, reversed_chunks_in_range, lines(rows) -> Iterator<String>
}
pub struct MappedText;              // memory-mapped, read-only, for very large files (lossy UTF-8)
// open(path), path(), len(), bytes(range) -> &[u8], line(row) -> Cow<str>, is_stale(), index_memory()
// + the same conversions as Rope. Invalid UTF-8 bytes are their own "chars" (1 UTF-16 unit each).
pub enum Text { Rope(Rope), Mapped(Arc<MappedText>) }   // Clone O(1), Send + Sync
impl Text {
    pub fn from_str(s: &str) -> Text;        // + From<&str>, From<String>, From<Rope>, FromStr, Display
    pub fn open_mapped(path: &Path) -> io::Result<Text>;
    pub fn version(&self) -> u64;            // unique per distinct content snapshot (global AtomicU64);
                                             // clones keep it, replace() assigns a new one
    pub fn is_mapped(&self) -> bool;  pub fn as_rope(&self) -> Option<&Rope>; pub fn as_mapped(&self) -> ...;
    // same read API as Rope: len, is_empty, line_count, max_point, line_range, line_len, offset_to_point,
    // point_to_offset, *_utf16, point_to_point_utf16, point_utf16_to_point, clip_offset, chars_at,
    pub fn text_for_range(&self, range: Range<usize>) -> Cow<'_, str>;   // borrows when possible
    pub fn line(&self, row: u32) -> Cow<'_, str>;
    pub fn chunks_in_range(&self, range: Range<usize>) -> Box<dyn Iterator<Item = &str> + '_>;  // + chunks()
    pub fn lines(&self, rows: Range<u32>) -> impl Iterator<Item = Cow<'_, str>>;   // rows clamped to line_count
    pub fn replace(&mut self, range: Range<usize>, text: &str);  // Mapped -> converted to Rope first
}
```
Line semantics (Rope and MappedText): rows split on `'\n'`; `line_range`/`line_len`/`line`
exclude a `'\r'` directly before the `'\n'`; `Point` columns are raw bytes (the `'\r'` is
addressable); out-of-range columns clip to the row's `'\n'`, rows past the end clip to
`len()`; offsets/points inside a multi-byte char clip to the char start (`Bias::Left`).
For `Text::Mapped`, returned strings are lossily decoded, so their byte length can differ
from the requested range when the file is not valid UTF-8.

`MappedText` must open a 1 GB file and serve the first screen in < 500 ms; the
line index may finish in the background but `line_count()` must then be exact.
(Current implementation builds the exact index eagerly in parallel at open:
~110 ms for 1 GB on warm cache; 12 bytes per 8 KiB block ≈ 0.15% overhead. A file
truncated while mapped can raise SIGBUS — callers should re-open on watcher events
or when `MappedText::is_stale()`.)

### lector-syntax
```rust
pub const HIGHLIGHT_NAMES: &[&str];          // canonical capture names, e.g. "keyword", "function.method"
pub struct HighlightId(pub u16);             // index into HIGHLIGHT_NAMES; .name() -> &'static str
pub struct HighlightSpan { pub range: Range<usize>, pub id: HighlightId }   // byte range in the text
pub struct Language { pub name: String, pub lsp_id: String, pub line_comment: Option<String>,
                      pub block_comment: Option<(String, String)>, pub extensions: Vec<String>, pub aliases: Vec<String>, .. }
pub struct LanguageRegistry;  // new() is cheap (process-global table, lazily compiled queries); Clone + Send + Sync
impl LanguageRegistry {
    pub fn language_for_path(&self, path: &Path, first_line: Option<&str>) -> Option<Arc<Language>>;
    pub fn by_name(&self, name: &str) -> Option<Arc<Language>>;   // name, lsp id or alias, case-insensitive ("rs", "ts", "sh", "yml")
    pub fn languages(&self) -> Vec<Arc<Language>>;
    pub fn by_extension(&self, ext: &str) -> Option<Arc<Language>>;   // added
}
pub trait TextInput { fn len(&self) -> usize; fn chunks_in_range(..); fn text_for_range(..); fn offset_to_point(..); }
// implemented for lector_text::Text, Rope, str and String. Every `&Text` parameter below is `&T where T: TextInput + ?Sized`,
// so `&Text` (the contract) and `&str` (tests/tools) both work.
pub struct Syntax;   // parsed tree for one Text snapshot; Clone (cheap), Send + Sync. Always pass the SAME text the tree was parsed from.
impl Syntax {
    pub fn parse(lang: &Arc<Language>, text: &Text) -> Result<Syntax>;  // reads via chunks (parse_with), no full copy; Err(TooLarge) above MAX_PARSE_BYTES (16 MB)
    pub fn parse_cancellable(lang: &Arc<Language>, text: &Text, cancel: &AtomicBool) -> Result<Syntax>;   // added
    pub fn language(&self) -> &Arc<Language>;
    pub fn highlights(&self, text: &Text, range: Range<usize>) -> Vec<HighlightSpan>; // sorted, non-overlapping, clipped to range, never ends with '\n'
    pub fn outline(&self, text: &Text) -> Vec<OutlineItem>;             // document order, depth = nesting; computed once and cached
    pub fn enclosing_scopes(&self, text: &Text, offset: usize) -> Vec<OutlineItem>;   // outermost first
    pub fn fold_ranges(&self, text: &Text) -> Vec<Range<u32>>;          // see "Fold ranges" below
    pub fn bracket_pair(&self, text: &Text, offset: usize) -> Option<(Range<usize>, Range<usize>)>;   // bracket at/just before offset -> (it, partner)
    pub fn enclosing_bracket_pair(&self, text: &Text, offset: usize) -> Option<(Range<usize>, Range<usize>)>;   // added: (open, close)
    pub fn tags(&self, text: &Text) -> Vec<Tag>;
    pub fn node_at(&self, offset: usize) -> Option<NodeInfo>;   // smallest named node covering the byte at offset
    pub fn has_errors(&self) -> bool;                           // added
}
pub struct NodeInfo { pub kind: String, pub range: Range<usize>, pub named: bool }   // .is_identifier() heuristic
pub struct OutlineItem { pub name: String, pub kind: SymbolKind, pub range: Range<usize>,
                         pub name_range: Range<usize>, pub depth: u32 }
pub enum SymbolKind { Module, Class, Struct, Enum, Interface, Trait, Function, Method, Field,
                      Variable, Constant, Type, Macro, Heading, Other }   // .label() -> "function", ...
pub struct Tag { pub name: String, pub kind: SymbolKind, pub range: Range<usize> /* the name identifier */, pub is_definition: bool }
pub struct SymbolIndex;  // workspace-wide tags index (fallback navigation without LSP); RwLock inside, Sync
impl SymbolIndex {
    pub fn new() -> Self;
    pub fn insert_file(&self, path: PathBuf, text: &Text, tags: Vec<Tag>);  // replaces earlier entries of path
    pub fn remove_file(&self, path: &Path);
    pub fn definitions(&self, name: &str) -> Vec<SymbolLocation>;       // ordered by path, position
    pub fn references(&self, name: &str) -> Vec<SymbolLocation>;
    pub fn all_definitions(&self) -> Vec<SymbolLocation>;   // for the symbol picker
    pub fn definitions_in_file(&self, path: &Path) -> Vec<SymbolLocation>;   // added
    pub fn file_count(&self) -> usize;                                       // added
}
pub struct SymbolLocation { pub path: PathBuf, pub name: String, pub kind: SymbolKind,
                            pub range: Range<usize>, pub point: Point }
```
Languages (`Language::name`, use these in `ServerConfig::languages`): Rust, TypeScript, TSX, JavaScript (also `.jsx`),
Python, Go, Java, Kotlin, Swift, C, C++, C#, Ruby, PHP, Bash, HTML, CSS, JSON, JSONC, YAML, TOML, Markdown, SQL
(LSP ids: rust, typescript, typescriptreact, javascript, python, go, java, kotlin, swift, c, cpp, csharp, ruby, php,
shellscript, html, css, json, jsonc, yaml, toml, markdown, sql). Detection: exact file name (`Cargo.lock`, `.bashrc`,
`Gemfile`, ...), patterns (`tsconfig.*.json`, `.vscode/*.json` → JSONC), extension, then shebang / first line.
Highlights follow injections (Markdown fences by info string, Markdown inline, front matter, HTML `<script>`/`<style>`).
Grammar/query provenance and licenses: `crates/lector-syntax/GRAMMARS.md`.

**Fold ranges.** `start` is the header row (stays visible); rows `start+1 .. end` are hidden when folded. If the region ends with
a closing delimiter on its own row (`}`, `]`, `)`, `end`, `fi`, `</tag>`), `end` is that row, so the closer stays visible;
otherwise (Python blocks, Markdown sections) `end` is one past the last row. Bodies that start on their own line (Allman braces,
Python/YAML blocks) use the previous non-blank row as header. Sorted by `start`, one range per start row (outermost wins).

**Symbols.** `OutlineItem.range` covers the whole item, `name_range` the identifier. `Tag.range` is the name identifier;
definition tags merge the grammar's tags query with outline items (consistent kinds: Method vs Function, Struct vs Class).

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
Notes (as implemented): lines are split on `'\n'` only (`line_count = newlines + 1`, a trailing
newline yields an empty last line, `'\r'` stays in the line; use `Whitespace::IgnoreEol` to hide CRLF
conversion). Hunks always index original lines; sliders use imara-diff's indent heuristic.
`LineDiff::is_empty()` added. `HasKind { fn kind(&self) -> RowKind }` implemented for both row types.
`fold_context` keeps `context` rows around changes, never folds fewer than 2 rows, and folds a
change-free file entirely. `refine_inline` returns whole-line ranges when lines share <35% of
non-whitespace bytes or exceed 4000 tokens. `GutterMark` `Deleted` is an empty range at the row after the removal.

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
Notes (as implemented): `Result<T>` = `Result<T, SearchError>` (`SearchError::Pattern(String)`).
`WalkOptions::hidden = true` means *include* hidden files (`.git` is always skipped); `git_ignore` also works
outside git repos; exclude/include globs are gitignore-like (a glob without `/` matches at any depth and
excludes descendants). `Picker::new_for_paths` (path-aware scoring) and `Picker::with_config` were added;
`Picker::clear` disconnects old injectors (call `injector()` again). `PickerMatch.indices` are sorted char
indices. `search_project`: `max_results == 0` is unlimited and counts matching lines; an empty pattern
completes with `Done` and zero results; an invalid regex emits `Error` then `Done` and returns `Err`; binary
files (NUL anywhere) and files > 10 MB are skipped; lines > 500 bytes are windowed around the first match
(`ranges` always index `text`); `^`/`$` anchor per line; non-UTF-8 files are searched lossily.

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
// Additions beyond the original contract:
impl Repo {
    pub fn is_bare(&self) -> bool; pub fn git_dir(&self) -> &Path;
    pub fn detect_binary(&self, change: &mut FileChange) -> Result<bool>;   // fills FileChange::binary lazily
    pub fn commit_infos(&self, oids: &[Oid]) -> Result<Vec<CommitInfo>>;   // with ref labels
}
pub struct GraphEdge { pub from_lane: u16, pub to_lane: u16, pub kind: EdgeKind }  pub enum EdgeKind { Straight, MergeIn, BranchOut }
pub enum Error { NotARepository(PathBuf), Bare, InvalidRevision { rev, message }, NoMergeBase(String, String),
                 GitNotInstalled(&'static str), GitCommand { command, status, stderr }, Parse(String), Io(io::Error), Git(String) }
pub fn is_binary(bytes: &[u8]) -> bool;                     // NUL in first 8000 bytes
pub fn parse_remote_url(url: &str) -> Option<RemoteWeb>;    // { host, path, scheme }
pub fn build_permalink(remote_url: &str, sha: &str, path: &str, lines: Option<Range<u32>>) -> Option<String>;
// Oid: from_hex, from_bytes, as_bytes, is_zero, FromStr; serde (hex string) behind the `serde` feature,
// which also derives Serialize/Deserialize for all data types.
```
Semantics: `changes()` returns paths sorted, `binary: None`; renames detected (50%) for Staged/Commit/
Range/BranchVsBase. `Unstaged` = index→worktree (old `Side::Index`, new `Side::Workdir`); `Staged` = HEAD
blob→`Side::Index`; `WorkingTree` = HEAD blob→`Side::Workdir`, following staged renames. `Side::Index` of a
conflicted path loads stage 2 ("ours"). `LogQuery.limit == 0` = unlimited; author/message are case-insensitive
substrings; `path` keeps commits whose entry differs from every parent (no parent-rewriting simplification);
`all_refs` excludes `refs/stash`. The HEAD commit carries a `RefKind::Head` label (attached or detached). `graph` edges go from
this row's center to the next row's; lanes compact when they end. `line_history` addresses lines as in
`HEAD`. Workdir-dependent calls (status, worktree changesets, CLI-backed calls) return `Error::Bare` in bare
repos. A changed `.git/config` is picked up automatically (cheap stat per call). Requires Rust 1.88 (gix 0.90).

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
pub struct DocumentSymbol { pub name: String, pub detail: Option<String>, pub kind: SymbolKind,
                            pub range: Range<PointUtf16>, pub selection_range: Range<PointUtf16>, pub children: Vec<DocumentSymbol> }
pub struct WorkspaceSymbol { pub name: String, pub kind: SymbolKind, pub path: PathBuf,
                             pub range: Range<PointUtf16>, pub container: Option<String> }
pub enum SymbolKind { File, Module, Namespace, Package, Class, Method, Property, Field, Constructor, Enum, Interface,
                      Function, Variable, Constant, String, Number, Boolean, Array, Object, Key, Null, EnumMember,
                      Struct, Event, Operator, TypeParameter, Other }   // lector_lsp::SymbolKind (LSP kinds)
// Additions: LspManager::with_options(root, configs, LspOptions), root(); lector_lsp::which(cmd);
// LspError / Result<T>; futures are Send + 'static. Languages: "Rust","TypeScript","TSX","JavaScript","Python",...
// Config order = priority: the first config for a language whose command is on PATH is used (pyright, then pylsp).
// Per-server overrides from lector-config (`lsp.servers.<name>`) are applied by the caller to `ServerConfig`s.
```

### lector-fs
```rust
pub enum ImageFormat { Png, Jpeg, Gif, Webp, Svg, Bmp, Ico }      // from_extension, from_magic, mime_type
pub enum FileKind { Text, Binary, Image(ImageFormat), Directory }
pub struct LoadedFile { pub kind: FileKind, pub text: Option<Text>, pub size: u64, pub encoding: &'static str, pub lossy: bool }
// text: Some for Text and Image(Svg) (source view); None for other images, binary, directories.
// encoding: "UTF-8" | "UTF-16LE" | "UTF-16BE" | "windows-1252" | "binary" | "" (directory)
pub fn detect_kind(path: &Path, head: &[u8]) -> FileKind;   // head = first SNIFF_LEN (8 KiB) bytes
pub fn is_binary(head: &[u8]) -> bool;                       // NUL or >10% control bytes; UTF-16 BOM => text
pub fn load_file(path: &Path, mmap_threshold: u64) -> io::Result<LoadedFile>;  // >= threshold → Text::Mapped
                                                             // (except UTF-16-BOM files, which are decoded)
pub fn decode(bytes: &[u8]) -> (String, &'static str /*encoding*/, bool /*lossy*/);  // UTF-8, UTF-16 BOM, latin1 fallback
pub enum FsEventKind { Created, Modified, Removed }
pub struct FsEvent { pub path: PathBuf, pub kind: FsEventKind }   // absolute paths, deduped per batch
pub enum WatchEvent { Files(Vec<FsEvent>), GitStateChanged /* .git/HEAD, index, refs, packed-refs, *_HEAD */, Rescan }
pub struct Watcher;   // drop to stop
pub fn watch(root: &Path, debounce: Duration, sink: impl Fn(WatchEvent) + Send + 'static) -> lector_fs::Result<Watcher>;
pub enum Error { Notify(notify::Error), Io(io::Error) }   // thiserror; `Result<T> = Result<T, Error>`
```
Per debounced batch the sink gets either `Rescan` alone, or `Files` (if any) followed by
`GitStateChanged` (if any). The sink runs on the watcher's background thread.

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
// Additions (lector-config): `load_settings_for(Option<&Path>)`, `Settings` sub-structs (ThemeSettings, DiffSettings,
// FileSettings, LspSettings{enabled, servers: BTreeMap<String, ServerOverride{command,args,enabled}>}, GitSettings,
// SessionSettings); `buffer_font_family: Vec<String>` (accepts a string or list in TOML); `Platform::{MacOS,Linux,Windows}`
// + `Platform::current()`; `merge_keymap`; keymap.toml = `[[bindings]] keys/action/context` ("none"/"" unbinds).
// Env overrides: LECTOR_CONFIG_DIR, LECTOR_DATA_DIR. Contexts: Workspace, CodeView, DiffView, Picker.
// SessionStore::{new(), with_dir(dir), load(&root) -> Option<Session>, save(&root, &Session) -> io::Result<()>};
// Session { panes: Vec<PaneState{tabs: Vec<TabState{kind: TabKind, scroll_row, active}>}>, axis, active_pane,
//           sidebar_visible, sidebar_width, sidebar_tab, window: Option<WindowBounds> };
// TabKind::{File{path}, Diff{changeset_key, path}, Commit{oid}, Other{label}}.
// ReviewStore::{open(file), default(), is_viewed(repo, changeset, path, content_hash), set_viewed(repo, changeset, path,
//   content_hash, viewed) -> io::Result<()>}; `content_hash(bytes) -> String` (sha256 hex). Clone + Send + Sync.
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
impl Color { pub fn parse(s: &str) -> Result<Color, ParseColorError>; pub fn to_hex(self) -> String;
             pub fn with_alpha(self, a: f32) -> Color; pub fn over(self, bg: Color) -> Color;   // source-over compositing
             pub fn contrast_ratio(self, bg: Color) -> f32; }                                  // WCAG 2.x
impl Theme { pub fn with_colorblind_diff(&self) -> Theme; }   // blue (added) / orange (removed) diff + git colors
impl ThemeRegistry {
    pub fn new() -> Self; pub fn get(&self, name: &str) -> Option<&Theme>; pub fn names(&self) -> Vec<&str>;
    pub fn themes(&self) -> &[Theme]; pub fn default_for(&self, a: Appearance) -> &Theme; pub fn insert(&mut self, t: Theme);
    pub fn load_dir(&mut self, dir: &Path) -> (Vec<String>, Vec<ThemeError>);   // *.toml; missing dir is not an error
    pub fn load_str(&mut self, toml: &str) -> Result<&Theme, ThemeError>;
}
// User theme TOML: name, appearance ("light"|"dark"), optional base (theme name); [ui], [diff], [git] override
// individual colors ("#rrggbb[aa]"); [syntax] maps capture names to "#color" or { color, bold, italic }.
// Overriding a capture `a` also replaces inherited `a.*` styles.
pub const CAPTURE_NAMES: &[&str];   // canonical capture names every built-in theme styles
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
impl FileIcons { pub fn by_name(&self, name: &str) -> Option<IconRef>; }
impl UiIcon { pub const ALL: &'static [UiIcon]; pub const fn name(self) -> &'static str /* lucide stem */;
              pub const fn svg(self) -> &'static [u8]; pub fn from_name(name: &str) -> Option<UiIcon>; }
// for_file precedence: exact file name (case-insensitive) → longest compound extension ("d.ts",
// "test.tsx") → extension → default. Mapping is generated by scripts/gen_icons.py (src/generated.rs).
```

### lector-workspace (UI-free app model; all calls are blocking — run on background threads)
```rust
pub mod project {
  pub struct Project;  // Clone (Arc), Send + Sync
  impl Project {
    pub fn open(root: &Path, settings: Settings) -> Result<Project>;   // discovers repo; never fails if not a repo
    pub fn root(&self) -> &Path;  pub fn name(&self) -> &str;
    pub fn repo(&self) -> Option<&Repo>;  pub fn settings(&self) -> Settings; pub fn set_settings(&self, s: Settings);
    pub fn languages(&self) -> &Arc<LanguageRegistry>;  pub fn symbols(&self) -> &Arc<SymbolIndex>;
    pub fn lsp(&self) -> Option<&LspManager>;            // None when settings.lsp.enabled == false
    pub fn files(&self) -> Arc<FileList>;                // snapshot: sorted relative paths + version
    pub fn rescan_files(&self, cancel: &CancelToken) -> Arc<FileList>;
    pub fn git_status(&self) -> Arc<GitStatusMap>;       // snapshot
    pub fn refresh_git_status(&self) -> Result<Arc<GitStatusMap>>;
    pub fn index_symbols(&self, cancel: &CancelToken, progress: impl Fn(usize, usize));  // tags for the whole project, budgeted
    pub fn load_document(&self, path: &Path) -> Result<Document>;
    pub fn blame(&self, rel_path: &str) -> Result<Arc<Vec<BlameHunk>>>;   // cached by (path, HEAD, mtime)
    pub fn relative_path(&self, abs: &Path) -> Option<String>;  pub fn absolute_path(&self, rel: &str) -> PathBuf;
  }
  pub struct FileList { pub version: u64, pub paths: Vec<Arc<str>> }   // '/'-separated, relative
  pub enum FileGitState { Modified, Added, Deleted, Renamed, Untracked, Conflicted, Ignored }
  pub struct GitStatusMap;  // get(rel_path) -> Option<FileStatus{ state, staged: bool }>, dir_state(rel_dir) -> Option<FileGitState> (rollup), entries()
}
pub mod document {
  pub struct Document;  // Clone (Arc inside), Send + Sync; immutable snapshot of one file
  impl Document {
    pub fn path(&self) -> &Path;  pub fn rel_path(&self) -> Option<&str>;
    pub fn kind(&self) -> &FileKind;  pub fn size(&self) -> u64;  pub fn encoding(&self) -> &str;  pub fn lossy(&self) -> bool;
    pub fn text(&self) -> Option<&Text>;  pub fn language(&self) -> Option<&Arc<Language>>;  pub fn syntax(&self) -> Option<&Syntax>;
    pub fn line_ending(&self) -> LineEnding /* Lf, CrLf, Mixed */;  pub fn indentation(&self) -> Indentation /* Tabs, Spaces(n) */;
    pub fn is_large(&self) -> bool;   // mapped / syntax disabled
    pub fn line_highlights(&self, rows: Range<u32>) -> Vec<LineHighlights>;  // { row, spans: Vec<(Range<usize> /*bytes in line*/, HighlightId)> }, cached in row chunks
    pub fn outline(&self) -> Arc<Vec<OutlineItem>>;  pub fn fold_ranges(&self) -> Arc<Vec<Range<u32>>>;
    pub fn scopes_at_row(&self, row: u32) -> Vec<OutlineItem>;   // sticky scroll + breadcrumbs
    pub fn word_at(&self, offset: usize) -> Option<(Range<usize>, String)>;
    pub fn diff_gutter(&self, project: &Project) -> Vec<GutterMark>;   // vs HEAD; untracked → all Added
  }
}
pub mod navigation {
  pub struct Location { pub path: PathBuf, pub point: Point }
  pub struct NavHistory;  // push(loc) (dedups, caps at 100), back(current) -> Option<Location>, forward(current), can_go_back/forward
}
pub mod symbols {
  pub struct Target { pub path: PathBuf, pub range: Range<Point>, pub preview: String, pub source: TargetSource /* Lsp, Tags, Text */ }
  pub fn definitions(project: &Project, doc: &Document, offset: usize) -> Result<Vec<Target>>;  // LSP (3 s timeout) → tags → none
  pub fn references(project: &Project, doc: &Document, offset: usize) -> Result<Vec<Target>>;   // LSP → tags refs ∪ whole-word text search
  pub fn hover(project: &Project, doc: &Document, offset: usize) -> Result<Option<String /*markdown*/>>;
  pub fn project_symbols(project: &Project) -> Vec<SymbolLocation>;
}
pub mod review {   // owned by the git-review work
  pub struct ChangesetView { pub key: String, pub title: String, pub changeset: Changeset, pub files: Vec<ReviewFile> }
  pub struct ReviewFile { pub change: FileChange, pub display_path: String, pub stats: Option<DiffStats>, pub viewed: bool,
                          pub content_hash: String, pub binary: bool }   // + path()
  pub fn load_changeset(project: &Project, cs: &Changeset, store: &ReviewStore) -> Result<ChangesetView>;
  pub struct FileDiff;  // old/new Text + Syntax, LineDiff, split + unified rows, lazy inline refinement, binary/image/too-large flags
  pub fn load_file_diff(project: &Project, change: &FileChange, opts: &DiffOptions) -> Result<FileDiff>;
}
```

Notes (as implemented): `lector_workspace::{Error, Result}` (`Io`, `Git`, `Syntax`, `Lsp`, `NotARepository`, `Other`);
common types are re-exported at the crate root. `Project::open` canonicalizes the root and fails only if it is not a
directory; if the root is a subfolder of a repo, status/blame/gutter paths are mapped (`Project::repo_path(rel)`).
Additions: `Project::{is_ignored, file_state, reindex_file(abs), repo_path}`, `FileList::{is_ignored, contains}`
(ignored files are only listed — and flagged — when `files.show_ignored`; they come from a second walk without gitignore),
`GitStatusMap::{len, is_empty}`, `entries()` yields `(&str, FileStatus)`. `FileStatus.state` prefers the working-tree state
over the staged one; directory rollups tie-break Modified > Renamed > Deleted. `index_symbols` keys the index by
**absolute** path, reports progress on the calling thread, and uses worker threads (no rayon). LSP overrides in
`set_settings` do not rebuild the `LspManager`. `Document::line_count()`; `line_highlights` returns one entry per
requested row (empty spans when no syntax); `Indentation::Spaces(u32)`; `LineEnding::Lf` also for files without newlines.
`diff_gutter` ignores CRLF-vs-LF differences. `symbols::definitions` ranks same file, same extension, then directory
proximity, indexes the open document on demand, and falls back to tags when LSP returns nothing or times out (3 s).
`NavHistory` also has `new`, `clear`; `back`/`forward` skip entries within 3 rows of `current`.

Notes on `review` (as implemented): every function has a `*_with(repo, ..)` variant over the core services
(`load_changeset_with(repo, cs, store, opts)`, `load_file_diff_with(repo, languages, change, opts)`); the `Project`
versions use `settings.diff` (`diff_options(&DiffSettings)`: ignore_whitespace → `Whitespace::IgnoreAll`).
* Keys (`changeset_key`, inverse `parse_changeset_key`): `worktree`, `staged`, `unstaged`, `untracked`, `commit:<sha>`,
  `range:<from>..<to>`, `branch:<base>...<branch>`. Titles (`changeset_title`): "Working tree", "abc1234 Fix parser",
  "main…feature", "a..b".
* `ReviewFile.content_hash` = sha256 of both sides' identities (blob oid, or content hash for index/workdir sides), so a
  `ReviewStore` mark stops counting when either side changes; `set_viewed_with(store, root, key, &mut file, bool)`.
  Stats are computed eagerly on up to 4 threads for changesets ≤ `MAX_EAGER_STATS` (3000 files; 500 files ≈ 30 ms).
* `FileDiff { change, kind: DiffKind::{Text, Binary, Image(ImageFormat), TooLarge}, old/new: DiffSide, diff, split,
  unified, options }`; `DiffSide { path, exists, size, text, bytes (images), language, syntax, encoding }` with
  `line(row)`, `line_count()`, `line_highlights(rows) -> Vec<LineSpan{row, range (bytes in line), id}>`.
  Sides over `MAX_DIFF_BYTES` (5 MB) → `TooLarge` (sizes only, nothing parsed). Added/deleted files diff as pure
  insert/remove of their real lines (a trailing newline adds no row). `inline_for_old(line)`/`inline_for_new(line)`
  refine `Changed` pairs lazily (cached `Arc<InlineChanges>`); `split_segments(ctx)`/`unified_segments(ctx)` fold
  context; `split_hunk_starts()`, `new_line_near_split_row(ix)` (+ unified variants), `is_identical()`.
* Extras: `diff_bytes(change, old, new, languages, opts)`, `file_vs_revision_with(repo, languages, path, rev, opts)`
  (working copy vs a revision), `load_vs_workdir_with(repo, rev, store, opts)` (`rev` → working tree incl. untracked;
  changeset `Range { from: rev, to: WORKDIR_REV }`, key `workdir:<rev>`), `file_at_revision`, `side_from_bytes`,
  `default_base(repo, branch)` (upstream unless same-named, else main/master/origin/main/origin/master/develop),
  `file_summary(repo, change, opts)`.

### lector-ui-git
GPUI views for git review, registered into `lector-ui`'s `Workspace` through its
extension seams (`init(cx)` called from the `lector` binary): Changes panel,
diff pane item (split/unified), changeset review, History panel with graph,
commit detail, Branches/Tags/Stashes panel, blame gutter, file/line history.

As implemented (`init` uses `cx.observe_new::<Workspace>`; nothing is registered outside a git repository):
* `store::GitStore` (one per window): the workspace's `Project`, `HeadInfo`, `status`, `ReviewStore`, the panel
  entities (`views: Option<GitViews>`); refreshes HEAD+status in the background on `WorkspaceEvent::{GitStateChanged,
  FilesChanged}` and polls every 3 s while the window is active (coalesced); emits `GitStoreEvent::Changed`. It sets
  the status-bar branch label ("main ↑2 ↓1") via `Workspace::set_branch`.
* `changes_panel::ChangesPanel` (`SidebarPanel::Changes`): Conflicts / Staged / Unstaged / Untracked sections (collapsible,
  counts) or any changeset via the header switcher (Working tree, Staged, Unstaged, Branch vs default base,
  Branch vs Base…, Commit Range… — ref pickers in `ref_picker`). Rows: Material icon, name + dim dir, `+a −r`, colored
  status letter; click = preview diff tab, double-click = pinned; context menu: Open Diff / File / in Split, Copy
  (Relative) Path, File History, Reveal in File Tree. "Review" opens a review tab.
* `diff_view::DiffView` (pane item `DiffItem`, key context `DiffView`): split = two `CodeView`s over
  `diff_source::DiffSource` (`DiffColumn::{Old, New, Unified}`) with synchronized scrolling and mirrored cursor row;
  filler rows (`line_number` None, `DecorationColor::Filler` hatch), line/word backgrounds from `row_decorations`,
  syntax on both sides, folded context rows "⋯ N unchanged lines" (click expands; `diff::ExpandAllContext` toggles all);
  unified uses `CodeSource::line_number_label` for the "old new" number pair. Layout: `diff.layout` (auto = split at
  ≥ 1280 px), `diff::ToggleLayout`; added/deleted files always render single-column. Header: icon, path
  (renames "old → new"), kind badge, `+/−`, hunk count + prev/next, expand, ignore-whitespace, Split/Unified segmented
  control, open file (`diff::OpenInEditor`, at the new-side line), Viewed checkbox. `NextHunk/PrevHunk` propagate
  at the ends (so a review tab can move on). Binary → "Binary file changed (12 KB → 14 KB)"; images side by side;
  > 5 MB → summary. `DiffTarget::{Change(FileChange), FileVsRev{path, rev}}`; `open_diff(..)` dedups by `item_key`.
* `review_tab::ReviewView` (pane item `ReviewItem`; also the commit tab): file list with viewed checkboxes and a
  "7 / 12 viewed" progress bar, `diff::NextFile/PrevFile/ToggleViewed` (marking viewed auto-advances to the next
  unviewed file), the `DiffView` on the right; worktree-like changesets reload on git changes. Commit tabs add a
  header (avatar initial, summary, refs pills, author/committer, dates, clickable parents, copy sha). File context
  menu: View File at This Revision ("file.rs @ abc1234", read-only, highlighted — `diff_source::RevisionSource`),
  View File Before, Open Working File, Compare Working File with This Revision, Copy Path, File History.
* `history_panel::HistoryPanel` (`SidebarPanel::History`): virtualized two-line rows with a painted graph column
  (`lector_git::graph`, lanes as colored curves, merge/HEAD nodes hollow), ref pills, sha/author/relative date; pages
  of 300 loaded as you scroll; filter input (`text`, `@author`, `path:dir/`); scopes: current branch, all branches,
  any ref, file history (`file_history`, follows renames), line history (`line_history`). Context menu: Open Commit,
  Copy Hash / Short Hash / Commit Link (forge URL from the remote), Compare with Working Tree / HEAD, View File at
  Revision / Compare File (file scopes).
* `branches_panel::BranchesPanel` (`SidebarPanel::Branches`): current branch card (upstream ↑/↓), local branches
  (current marked, per-branch ↑/↓ vs upstream), remotes (collapsed), tags (newest first), stashes. Click = show its
  log in History; double-click a branch = review `BranchVsBase { branch, base: current }`; stash click opens it like a
  commit; context menu: Show Log, Compare with current, Changes of current since branch, Open Tip Commit, Copy.
* `blame`: `editor::ToggleBlame` toggles `CodeView::set_gutter_annotations` (left column: "Author · summary", age,
  age-heat bar, alternating shading per commit block, "Not committed yet" muted); clicking a block opens the commit.
* Workspace actions: `editor::{ToggleBlame, ShowFileHistory, ShowLineHistory (selection rows), CopyPermalink (toast
  "Permalink copied")}`, plus `diff::{CompareWith (pick a ref → file vs ref), ReviewWorkingTree, ReviewBranch}`.
* Session: diff / review / commit tabs save as `TabKind::{Diff{changeset_key, path}, Other{"review:<key>" |
  "vs:<rev>:<path>"}, Commit{oid}}` and are reopened by `init` (the workspace restores its own session before
  extensions register, so `restore_git_tabs` re-reads the session; tabs are deduped by `item_key`).

### lector-ui (extension seams)
```rust
// Data in through traits; integration plugs the core crates in here.
pub trait CodeSource: 'static {             // code_view::CodeSource — rows are 0-based
    fn line_count(&self) -> u32; fn line(&self, row: u32) -> SharedString;
    fn version(&self) -> u64; fn max_line_len(&self) -> Option<usize>;
    fn highlights(&self, rows: Range<u32>) -> Vec<HighlightSpan>;      // SpanStyle::Capture(&'static str) | Custom
    fn gutter_marks(&self, rows: Range<u32>) -> Vec<GutterMark>;       // Added | Modified | Deleted
    fn fold_ranges(&self) -> Vec<Range<u32>>;                          // folding r hides r.start+1..r.end
    fn sticky_rows(&self, top_row: u32) -> Vec<u32>;                   // enclosing scope headers, outermost first
    fn line_number(&self, row: u32) -> Option<u32>;                    // None = blank (diff filler rows)
    fn row_decorations(&self, rows: Range<u32>) -> Vec<RowDecoration>; // diff line/word backgrounds
    fn language_name(&self) -> Option<SharedString>;
}   // all but line_count/line have defaults; InMemorySource is the demo implementation
pub trait TreeDelegate { type Id; fn children(&mut self, parent: Option<&Id>, cx) -> Vec<TreeNode<Id>>;
                         fn context_menu(&mut self, node, window, cx) -> Option<Entity<ContextMenu>>; }
pub trait PickerDelegate { fn match_count; fn selected_index; fn set_selected_index; fn update_matches(query);
                           fn confirm; fn render_match; /* + placeholder, dismissed, empty_message, width, footer */ }
pub trait PaneItem { fn title; fn icon; fn path; fn view(&self) -> AnyView; fn focus_handle; fn code_view; fn clone_item; }
// CodeView API: scroll_to_row(row, center), set_cursor/set_selection, set_search_matches, set_decorations,
// set_annotations (blame), on_hover(cb), on_cmd_click(cb), set_soft_wrap, set_show_minimap, set_folded.
// Added for lector-ui-git: CodeSource::line_number_label(row) -> Option<SharedString> + line_number_chars() ->
// Option<usize> (custom gutter labels, e.g. "12  14" in unified diffs); CodeView::set_gutter_annotations(
// Option<GutterAnnotations{width_chars, blocks: Vec<GutterAnnotation{rows, text, detail, bar, shaded, muted}>}>)
// (a left column before the line numbers) + on_gutter_annotation_click(|block_ix, window, cx|);
// Pane::index_for_key(key, cx); Workspace::{show_toast(msg, cx), set_branch(branch, status_label, cx)}.
// Keymap: keymap::bind_keymap(cx, &[KeySpec { keys, action, context }]) (same shape as lector-config).
```
Extension seams for other UI crates (e.g. `lector-ui-git`), all on `views::Workspace`:
```rust
// Hook in from your crate's init(cx), called by the binary before windows open:
cx.observe_new(|ws: &mut Workspace, window: Option<&mut Window>, cx: &mut Context<Workspace>| {
    ws.register_action(|ws, _: &ShowChanges, window, cx| { .. });   // runs before built-ins; cx.propagate() falls through
    ws.sidebar().update(cx, |s, cx| s.set_panel_view(SidebarPanel::Changes, view.into(), cx));
    cx.subscribe(&cx.entity(), |ws, _, ev: &WorkspaceEvent, cx| { .. }).detach();
}).detach();
pub enum WorkspaceEvent { ActiveItemChanged, FilesChanged(Vec<PathBuf>), GitStateChanged, SettingsChanged }
impl Workspace {
    pub fn register_action<A: Action>(&mut self, f: impl Fn(&mut Workspace, &A, &mut Window, &mut Context<Workspace>) + 'static) -> &mut Self;
    pub fn sidebar(&self) -> &Entity<Sidebar>;            // Sidebar::set_panel_view(panel, AnyView, cx), set_active_panel
    pub fn add_item(&mut self, item: Box<dyn PaneItem>, preview: bool, window, cx);   // into the active pane
    pub fn active_pane(&self) -> &Entity<Pane>;  pub fn active_code_view(&self, cx) -> Option<Entity<CodeView>>;
    pub fn active_path(&self, cx) -> Option<PathBuf>;     pub fn root(&self) -> &Path;
    pub fn open_path(&mut self, abs: PathBuf, preview: bool, window, cx);  pub fn open_path_at(.., line_1based, col_1based, ..) -> Option<Entity<FileView>>;
    pub fn open_path_with(&mut self, abs, preview, reveal: Option<editor::Reveal>, record_nav: bool, window, cx) -> Option<Entity<FileView>>;
    pub fn project(&self) -> &lector_workspace::Project;   // opened before the window; never changes for a workspace
    pub fn file_list(&self) -> &Arc<FileList>;  pub fn git_status(&self) -> &Arc<GitStatusMap>;   // latest snapshots
    pub fn active_file_view(&self, cx) -> Option<Entity<editor::FileView>>;  pub fn file_views(&self, cx) -> Vec<Entity<FileView>>;
    pub fn refresh_git_status(&mut self, cx);  pub fn record_location(&mut self, cx);   // push current location on GoBack history
    pub fn register_tab_restorer(&mut self, f: impl Fn(&TabKind, &mut Window, &mut Context<Workspace>) -> Option<Box<dyn PaneItem>>);
    pub fn show_toast(&mut self, msg, cx);  pub fn set_branch(..);   // (added by lector-ui-git)
}
// PaneItem additions: fn file_view(&self) -> Option<Entity<FileView>> (None); fn tab_kind(&self, cx) -> Option<TabKind>
// (session; default File{path} from path()); fn item_key(&self, cx) -> Option<SharedString> (dedup non-file tabs).
// editor::FileView (a file tab): document(), code_view(), path(), reveal(Reveal{row, cols, center}), reload(), refresh_gutter(),
// cursor_offset(); events FileViewEvent::{Loaded, CursorMoved}. editor::DocumentSource: CodeSource over a Document
// (background highlight chunks, gutter vs HEAD, folds, sticky rows from scopes). CodeSource::text_snapshot() -> Option<Text>.
// CodeView additions: point_for_pos(pos) -> Option<Point<Pixels>> (popover anchor), top_visible_row(),
// CodeViewEvent::HoverMoved(Option<Pos>), HOVER_DELAY_MS = 500.
// Settings: settings::{AppSettings::get(cx) -> Arc<Settings>, apply_settings, bind_effective_keymap, UiFont}.
// Markdown: markdown::{parse(&str) -> Vec<Block> (blocking, highlights code fences), render_blocks(..)}.
// lector-ui additions to Actions: workspace::ShowOutline (outline sidebar panel; ToggleOutline is the file-symbol picker).
```
Startup (`lector` binary): settings (`load_settings_for(root)`) and `Project::open` run on a thread while GPUI starts;
`lector_ui::init(cx, themes, mode, settings)` applies settings/keymap, then `lector_ui_git::init(cx)`, then
`open_workspace_window` restores the session (`SessionStore`) and opens the CLI file. Watchers: project root
(`lector_fs::watch`, 120 ms debounce: reload open docs, rescan on create/remove, git status + gutters on
`GitStateChanged`) and Lector's config dir (live-reload settings, keymap, themes). LSP documents are opened
(warm-up) when a file loads; status polled every second for the status bar.

Notes (as implemented, lector-ui integration):
* File tabs (`editor::FileView`) load `Document`s on the background executor, pre-computing outline, folds and the
  highlight chunks around the target row, so the first paint is already colored; other chunks (128 rows) are queued
  by `DocumentSource::highlights` and computed off-thread (newest 4 requests per batch; missing rows render plain).
  `Text::Mapped` files show a "Large file" banner; binaries a panel; images an `ImageView` (fit / 100 %, checkerboard;
  SVG rasterized with resvg at the displayed size, "Source" toggles the code view); lossy decodes a banner.
* Pickers run on nucleo (`fuzzy::NucleoList` over `lector_search::Picker`): file finder (recent files first,
  `path:line[:col]`, git colors), command palette, outline (`ToggleOutline`), project symbols (`ToggleSymbolFinder`,
  from `symbols::project_symbols`), multiple definition targets. Find bar uses `find_in_text` on a background thread.
* Sidebar panels: Files (`DirIndex` built from `FileList` off-thread, git colors/badges with folder roll-ups,
  auto-reveal of the active file, context menu), Search (debounced 150 ms, `CancelToken` per query, streamed in
  30 ms batches, 20 000-line cap), Outline (`ShowOutline`). References open as a pane item; Markdown preview opens
  side by side (`ToggleMarkdownPreview` toggles; follows the source scroll; relative links open files, URLs the browser;
  remote images are not fetched).
* Navigation history: jumps (definition, pickers, search/references results, cross-file opens) and in-file moves of
  ≥ 20 rows after an 800 ms rest are recorded in `NavHistory`.
* Session: saved 1 s after tab/pane changes and on quit (`SessionStore`); restore re-creates panes along the saved
  axis, files at their scroll rows, sidebar visibility/width/panel. Pane item views and the sidebar render through
  `AnyView::cached` so a scrolling view does not re-render its siblings.
* Not yet applied from settings: `show_whitespace`, `cursor_blink`, `scroll_beyond_last_line`.

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
* lector-ui-git additions: `diff::` CompareWith (file vs a picked ref), ReviewWorkingTree, ReviewBranch
  (current branch vs its default base).
* lector-ui additions (UI plumbing, also bindable): `workspace::` CloseOtherTabs, CloseTabsToRight,
  ToggleColorblindDiff, About; `editor::` MoveToLineStart, MoveToLineEnd, MoveWordLeft, MoveWordRight,
  SelectToLineStart, SelectToLineEnd, SelectWordLeft, SelectWordRight, SelectPageUp, SelectPageDown,
  SelectToTop, SelectToBottom, ClearSelection; `workspace::ShowOutline`; `tree::` ExpandSelected, CollapseSelected;
  `text_input::` Backspace, Delete, Left, Right, SelectLeft, SelectRight, SelectAll, Home, End, Paste,
  Cut, Copy, ShowCharacterPalette. Key contexts: `Workspace`, `CodeView`, `DiffView`, `Picker`,
  `TextInput`, `ContextMenu`, `TreeView`.

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
