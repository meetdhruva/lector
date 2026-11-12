# Lector architecture (v0.3)

Lector is a **reading-first** code viewer and git review tool with light editing.
v0.1 was strictly read-only; since v0.2 it writes to the user's files or repository
only on explicit user actions (see "Write policy"). Its own settings, session and
review state live under the platform config/data directories.

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
               ├─ lector-lsp      LSP client (navigation, diagnostics, edits as data)
               ├─ lector-search   walker, fuzzy picker (nucleo), project grep
               ├─ lector-syntax   tree-sitter registry, highlights, outline, folds, tags
               ├─ lector-diff     line diff (imara-diff), word refine, split/unified rows
               ├─ lector-fs       file loading/kind detection, debounced watcher
               ├─ lector-config   settings, keymaps, paths, session store
               ├─ lector-theme    theme model + built-in themes
               ├─ lector-icons    Material file icons + UI icons (SVG bytes)
               ├─ lector-text     SumTree, Rope, mmap text, coordinates (no internal deps)
               └─ lector-process  child processes: process trees, streaming, cancel, askpass (no internal deps)
   ```
   Allowed internal edges: `syntax→text`, `fs→text`, `diff→(none)`, `search→text?`,
   `git→process`, `lsp→process`, `agent→process` (v0.7), `workspace→all core`, `ui→all`.
   Nothing depends on `ui`.
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

## Write policy (v0.2+)

v0.1 was strictly read-only. From v0.2 Lector can stage and commit, and from v0.3 it
can edit and save files. The rules:

1. **Only explicit user actions write.** Save, Save All, stage/unstage, commit, discard,
   accept a conflict resolution, rename/format via LSP *applied to the buffer* (and then
   saved by the user). Opening, browsing, status refresh, diffing, blame and history
   never write — `crates/lector-workspace/tests/release_audit.rs` keeps enforcing this
   for every read path.
2. **Opt-in automation only.** Auto-save and format-on-save are off by default.
3. **Destructive operations are undoable, or confirm** (v0.4). Before a discard or any
   other overwrite of user files, Lector backs up what will be lost into the **shadow
   repo** (Lector's own data dir, never the user's `.git`), records the write in the
   activity journal and shows an "Undo" toast instead of asking first
   (`git.confirm_discard` now defaults to `false`; `true` brings the old confirmation
   back). A confirmation remains only when the backup cannot be taken, when the
   operation is irreversible (abort a rebase, ...), and for closing unsaved tabs /
   reloading a modified file. Undoing never throws away newer edits silently: files that
   changed since the operation make Undo ask "restore anyway?", and that is itself
   backed up and undoable. Backups are kept 30 days / 1 GiB per repository.
4. **Git writes go through the git CLI** (`lector-git::write`): hooks, index locking,
   filters (autocrlf, LFS) and signing behave exactly as in the terminal. Read paths stay
   on gix. Commands run with `-C <workdir>`, never through a shell, with arguments
   passed as separate argv entries; patches go through stdin.
5. **Saves are atomic** (temp file in the same directory + rename), keep the file's
   permissions, encoding (UTF-8, UTF-8 BOM, UTF-16 LE/BE) and line endings, and refuse
   to overwrite a file that changed on disk since it was loaded unless the user confirms.
6. Language servers keep running with the "no writes into the project" options from v0.1.

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
    pub fn apply_edits<S: AsRef<str>>(&mut self, edits: &[(Range<usize>, S)]);  // many edits at once, see below
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
*`Rope::apply_edits`* (additive, v0.3 polish): `edits` are sorted, non-overlapping `(range, text)` pairs against the current text
(offsets clamped and clipped to char boundaries; an overlapping edit starts at the previous edit's end; an empty list leaves
`version` unchanged). Equivalent to applying them back to front with `replace`, but `O(edits + touched chunks)` instead of
`O(edits x log n)` with a large constant. Two strategies, both producing a balanced tree and sharing untouched chunks with the
old rope: a forward cursor that slices the gaps between edits (`apply_edits_by_splicing`, sparse edits), and, when there are
at least 8 edits and at most one per 256 KiB of text, a rebuild of only the chunks the edits touch with their known summaries,
reusing every other leaf (`apply_edits_by_leaves`). Proptests (`rope::tests::apply_edits_matches_model`,
`tests/model.rs::rope_apply_edits_matches_sequential_replace`) compare both strategies with sequential `replace`.

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
v0.4 (additive): `pub mod moves` (moved-code detection) and `pub mod remap` (`LineMap`, `reanchor`), see "v0.4 algorithms".

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
pub fn replacements_in_text(text: &str, query: &SearchQuery, replacement: &str)
    -> Result<Vec<(Range<usize>, String)>>;   // same matches + replacement; regex: `$1`/`${name}` expand, `$$` = `$`
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
v0.4 (additive): `TrigramIndex`, `Candidates`, `search_files`, `PickerInjector::push_boosted`, `MAX_RESULTS`; the
Picker now has its own top-k scorer (see "v0.4 algorithms").

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
// Read-only (v0.1): `read_only_settings(name) -> Option<serde_json::Value>` is sent as `initializationOptions`
// and answers `workspace/configuration` for section `name` (rust-analyzer: cargo.noDeps, no build scripts /
// proc macros / checkOnSave / cache priming — `cargo metadata` would create or update Cargo.lock, builds write
// target/); clangd runs with `--background-index=false` (.cache/clangd); `WRITES_TO_PROJECT` /
// `writes_to_project(name)` (jdtls, kotlin-language-server, sourcekit-lsp, csharp-ls) are only started by
// lector-workspace when `lsp.servers.<name>.enabled = true`. `workspace/applyEdit` is never honored.
```
**Editing features (v0.3, as implemented).** The crate stays runtime-agnostic and never writes files: edits are
returned as plain data and applied to buffers by the UI (write policy). Client capabilities: positionEncoding utf-16,
`workspace.applyEdit`, `workspaceEdit { documentChanges: true, resourceOperations: [] }`, publishDiagnostics
(relatedInformation, tags, versionSupport), formatting / rangeFormatting, rename with `prepareSupport`, codeAction
literals with kinds + `isPreferred` / `disabled` / `resolveSupport { properties: ["edit"] }`, executeCommand.
```rust
pub struct TextEdit { pub range: Range<PointUtf16>, pub new_text: String }
pub struct WorkspaceEdit { pub changes: Vec<(PathBuf, Vec<TextEdit>)>,   // file order as sent; a file appears once
                           pub versions: Vec<(PathBuf, i32)> }            // documentChanges versions (non-null)
  // version_for(path), file_count(), is_empty(); file operations (create/rename/delete) are a Protocol error
pub enum DiagnosticSeverity { Error, Warning, Information, Hint }   pub enum DiagnosticTag { Unnecessary, Deprecated }
pub struct Diagnostic { pub range, pub severity, pub message: String, pub source: Option<String>, pub code: Option<String>,
                        pub tags: Vec<DiagnosticTag>, pub related: Vec<(Location, String)> }
pub struct DocumentDiagnostics { pub path, pub server: String, pub version: Option<i32>, pub tag: Option<u64>, pub diagnostics }
pub struct Command { pub title, pub command: String, pub arguments: Vec<serde_json::Value> }
pub struct CodeAction { pub title, pub kind: Option<String>, pub is_preferred: bool, pub disabled_reason: Option<String>,
                        pub edit: Option<WorkspaceEdit>, pub command: Option<Command>, pub diagnostics: Vec<Diagnostic>,
                        pub raw: serde_json::Value /* sent back for resolve */ }   // is_quick_fix()
pub struct PrepareRename { pub range: Option<Range<PointUtf16>> /* None: caller picks the word */, pub placeholder: Option<String> }
pub struct FormattingOptions { pub tab_size: u32, pub insert_spaces: bool }
pub struct ServerCapabilities { hover, definition, references, formatting, range_formatting, rename, prepare_rename,
                                code_actions, code_action_resolve: bool, commands: Vec<String> }
pub struct DocumentState { pub version: i32, pub tag: Option<u64> }
impl LspManager {
    pub fn open_document_tagged(&self, path, language, text, tag: u64);   // tag = caller's text version (UI: rope version)
    pub fn document_state(&self, path) -> Option<DocumentState>;          // version + tag of the last synced text
    pub fn capabilities(&self, path) -> Option<ServerCapabilities>;       // running server of `path`; None while starting
    pub fn server_capabilities(&self, name) -> Option<ServerCapabilities>;
    pub fn diagnostics(&self, path) -> Option<DocumentDiagnostics>;  pub fn all_diagnostics(&self) -> Vec<DocumentDiagnostics>;
    pub fn on_diagnostics(&self, f: impl Fn(&Path) + Send + Sync);        // reader thread; also on close / server exit (cleared)
    pub fn set_apply_edit_handler(&self, f: impl Fn(ApplyEditRequest) + Send + Sync);
    // futures (Send + 'static); Ok(empty / None) without a server:
    pub fn formatting(&self, path, FormattingOptions) -> Result<Vec<TextEdit>>;
    pub fn range_formatting(&self, path, Range<PointUtf16>, FormattingOptions) -> Result<Vec<TextEdit>>;
    pub fn code_actions(&self, path, Range<PointUtf16>, automatic: bool) -> Result<Vec<CodeAction>>; // overlapping diagnostics as context
    pub fn resolve_code_action(&self, path, CodeAction) -> Result<CodeAction>;   // codeAction/resolve if supported and no edit
    pub fn execute_command(&self, path, &Command) -> Result<()>;                 // edits arrive as workspace/applyEdit
    pub fn prepare_rename(&self, path, PointUtf16) -> Result<Option<PrepareRename>>;
    pub fn rename(&self, path, PointUtf16, new_name: &str) -> Result<Option<WorkspaceEdit>>;
}
pub struct ApplyEditRequest { pub server: String, pub label: Option<String>, pub edit: WorkspaceEdit, .. }
impl ApplyEditRequest { pub fn respond(self, applied: bool, failure_reason: Option<String>); }  // drop = applied:false
```
Diagnostics are stored per document with the LSP `version` they were published for and the caller's `tag` of the text
with that version (`None` when they refer to an older text); without a handler `workspace/applyEdit` is answered
`applied: false`. Tests: `tests/editing.rs` against `tests/fixtures/fake_lsp.py --editing` (diagnostics, formatting,
code actions + resolve + commands → applyEdit, prepare/rename across files) and an ignored real-`pylsp` test.

### Completion and signature help (v0.7) — lector-lsp, lector-config — as implemented

Product rules: completion comes **only from language servers** (no AI source); snippets are **off by default**; nothing is
requested or written on open or focus: a request is the result of typing, a trigger character or Ctrl+Space, and the buffer
changes only when the user accepts an item. `lector-lsp` stays UI-free and runtime-agnostic; the popup is built by `lector-ui` on
the types below. `lector-lsp` has its own small fuzzy matcher (`fuzzy.rs`, fzy-style scoring) because `lector-search` sits above it
in the layering and pulls in the walker and grep stack.

```rust
// ---- requests (lector_lsp::LspManager; futures are Send + 'static, nothing is sent until the document is synced) ----
impl LspManager {
    pub fn completion(&self, path: &Path, req: CompletionRequest, cancel: Option<&CancelToken>)
        -> impl Future<Output = Result<RequestOutcome<CompletionResponse>>>;
    pub fn resolve_completion(&self, path: &Path, item: &CompletionItem, cancel: Option<&CancelToken>)
        -> impl Future<Output = Result<RequestOutcome<ResolvedCompletion>>>;
    pub fn signature_help(&self, path: &Path, req: SignatureRequest, cancel: Option<&CancelToken>)
        -> impl Future<Output = Result<RequestOutcome<Option<SignatureHelp>>>>;   // Done(None): no signature here, close
    pub fn set_completion_snippets(&self, enabled: bool);   // advertised at `initialize`: servers started afterwards
}
pub enum RequestOutcome<T> { Done(T), Cancelled, Stale, Unavailable }   // done(), is_done()
pub struct CancelToken;   // Clone; new(), cancel(), is_cancelled(). Cancelling sends $/cancelRequest and ends the future at once
pub enum CompletionTrigger { Invoked /*typed identifier char, Ctrl+Space*/, TriggerCharacter(char), Incomplete }
pub struct CompletionRequest { pub pos: PointUtf16 /* UTF-16 caret */, pub trigger: CompletionTrigger }
pub struct CompletionResponse { pub items: Vec<CompletionItem>, pub is_incomplete: bool, pub request_pos: PointUtf16,
                                pub doc_version: i32, pub tag: Option<u64>, pub resolve_supported: bool }
pub struct CompletionItem { pub label: String, pub label_details: Option<LabelDetails { detail, description }>, pub kind: CompletionKind,
    pub detail: Option<String>, pub documentation: Option<Documentation { value, markdown }>, pub deprecated: bool /* flag or tag 1 */,
    pub preselect: bool, pub sort_text: Option<String>, pub filter_text: Option<String>, pub insert_text: Option<String>,
    pub format: InsertFormat /* PlainText | Snippet */, pub insert_mode: InsertTextMode /* AsIs | AdjustIndentation (default) */,
    pub edit: Option<ItemEdit { insert: Range<PointUtf16>, replace: Range<PointUtf16>, new_text }>,
    pub additional_edits: Vec<TextEdit>, pub commit_characters: Vec<char>, pub command: Option<Command>,
    pub resolved: bool, pub raw: serde_json::Value /* sent back for resolve */ }
pub enum CompletionKind { Text, Method, Function, Constructor, Field, Variable, Class, Interface, Module, Property, Unit, Value,
    Enum, Keyword, Snippet, Color, File, Reference, Folder, EnumMember, Constant, Struct, Event, Operator, TypeParameter, Other }
pub struct ResolvedCompletion { pub detail, pub documentation, pub additional_edits: Vec<TextEdit>, pub label_details,
                                pub doc_version: i32, pub tag: Option<u64> }
// ServerCapabilities gained `completion: Option<CompletionCapabilities { trigger_characters, all_commit_characters, resolve }>`
// (is_trigger(c)) and `signature_help: Option<SignatureHelpCapabilities { trigger_characters, retrigger_characters }>`.
```

*Client capabilities* (declared only for what is implemented): completion `contextSupport`, `insertTextMode: 2`, `completionItem`
`{ snippetSupport: <setting, default false>, commitCharactersSupport, documentationFormat [markdown, plaintext], deprecatedSupport,
preselectSupport, tagSupport [1], insertReplaceSupport, resolveSupport [documentation, detail, additionalTextEdits],
insertTextModeSupport [1, 2], labelDetailsSupport }`, `completionItemKind` 1..25, `completionList.itemDefaults` `[commitCharacters,
editRange, insertTextFormat, insertTextMode, data]` (applied per item when parsing; `data` is merged into the item's `raw` so resolve
works; `textEditText` becomes the edit's text); signature help `contextSupport`, `activeParameterSupport`, `labelOffsetSupport`,
markdown/plaintext documentation. `completionItem/resolve` is only sent when the server has `resolveProvider`, only for items not
yet resolved, and only `documentation`, `detail`, `labelDetails` and `additionalTextEdits` are taken from the answer (label, filter,
sort and the main edit stay as listed).

*Request lifecycle* (all three requests). The request is tied to the document version synced when it is made (`document_state`):
the document must be synced first (`DocHandle::sync_lsp_now()`, tag = rope version) with the text the position refers to.
`Stale`: the version changed before the request was sent or before the answer arrived, or the server answered `ContentModified`
(-32801); the answer is dropped, re-issue it if still wanted. `Cancelled`: a newer request of the same kind (completion, resolve,
signature help are separate kinds) for the same document superseded it, the `CancelToken` fired, or the server answered
`RequestCancelled` (-32800); `$/cancelRequest` is sent and the future ends without waiting for the server (also while the server is
still starting). `Unavailable`: no server has the document, it is not running, or it lacks the capability; nothing is sent. Other
server errors and timeouts are `Err`. Parsing a 10,000-item list happens inside the future, off the UI thread.

*Session model* (`lector_lsp::CompletionSession`, UI-free; text is a `lector_text::Rope`, offsets are byte offsets, the server's
UTF-16 positions are converted with the rope's converters and clamped to the line):
```rust
pub struct SessionOptions { pub insert_mode: InsertMode /* Insert (default) | Replace */, pub snippets: bool /* false */,
                            pub word_chars: WordChars /* WordChars::for_language(name) */, pub commit_characters: bool /* true */ }
pub enum OpenReason { Typing, TriggerCharacter(char), Manual }
pub enum Update { Continue, Requery(CompletionRequest), Close(CloseReason) }
pub enum CloseReason { CaretMoved, LeftWord, NoMatches, TextChanged, Committed, Dismissed }
impl CompletionSession {
    pub fn open(text: &Rope, caret: usize, reason: OpenReason, options: SessionOptions) -> Self;  // typed prefix = word chars before the caret
    pub fn request(&self) -> CompletionRequest;                       // send with LspManager::completion
    pub fn set_response(&mut self, text: &Rope, caret: usize, r: CompletionResponse) -> bool;     // false: ignored (closed, other row, caret left)
    pub fn update(&mut self, text: &Rope, caret: usize) -> Update;    // after every edit / caret move; re-filters locally
    pub fn close(&mut self, reason: CloseReason); pub fn is_closed(&self) -> bool; pub fn close_reason(&self) -> Option<CloseReason>;
    pub fn anchor(&self) -> usize; pub fn typed(&self) -> &str; pub fn has_response(&self) -> bool; pub fn is_incomplete(&self) -> bool;
    pub fn items(&self) -> &[CompletionItem]; pub fn item(&self, i: usize) -> Option<&CompletionItem>;
    pub fn matches(&self) -> &[Match { item: usize, score: i32 }];    // filtered and ranked rows
    pub fn label_highlights(&self, row: usize) -> Vec<Range<usize>>;  // byte ranges of the label that match the typed text (lazy)
    pub fn selected(&self) -> Option<usize>; pub fn selected_item(&self) -> Option<(usize, &CompletionItem)>;
    pub fn select(&mut self, row: usize); pub fn select_next(&mut self); pub fn select_previous(&mut self);   // wrap
    pub fn select_relative(&mut self, delta: isize); pub fn select_page(&mut self, rows: isize);              // no wrap
    pub fn is_commit_character(&self, c: char) -> bool;               // call before inserting `c`
    pub fn needs_resolve(&self, item: usize) -> bool; pub fn generation(&self) -> u64;
    pub fn apply_resolved(&mut self, generation: u64, item: usize, r: ResolvedCompletion) -> bool;   // false: older list
    pub fn commit(&self, text: &Rope, carets: &[usize], primary: usize, item: usize) -> Result<CompletionCommit, CommitError>;
}
pub struct CompletionCommit { pub edits: Vec<ByteEdit { range: Range<usize>, new_text }> /* sorted, non-overlapping, offsets in the text given;
        same shape as lector_buffer::TextEdit: Buffer::apply_edits, one undo step */, pub carets: Vec<usize> /* after the edits, same order */,
        pub tab_stops: Vec<TabStop>, pub command: Option<Command> /* triggers_signature_help() */,
        pub skipped_additional_edits: usize, pub skipped_carets: usize }
```
*Rules.* **Typed prefix / replace range:** an item's `textEdit` wins (`insert` range or `replace` range per `InsertMode`; a plain
`TextEdit` has one range for both); without one the range is the word before the caret (Insert) or the whole word around it
(Replace) and the text is `insertText` or the label. The server's UTF-16 range is **re-anchored** to what was typed since the
request: on the caret's row, range ends and positions at or after the old caret move with the caret, range starts at the old caret
stay, so an answer for `fo` still replaces `foo` after one more keystroke (and shrinks on backspace). **Filtering:** the typed text
is fuzzy-matched (case-insensitive subsequence) against `filterText` or the label; items whose edit starts before the word (for
example at a `.`) are matched against the text from their own start. Score: fzy-style boundary bonuses (start, after `_ - . / :`,
camelCase), consecutive runs, +50 per exact-case char, +1500 for a prefix match (+500 more for an exact one), no penalty for text
after the last match, deprecated items minus 3000. Rows are ordered by score, then `sortText` (or label), then `preselect`, then
server order; the default selection is the first `preselect` item among the best-scoring rows, else row 0; an explicit selection
survives narrowing. Extending the query only searches the previous survivors; there is no re-query unless the list was
`isIncomplete`, in which case any change of the typed text returns `Update::Requery` (trigger kind 3; the UI sends it, the manager
cancels the one in flight). **Closing:** `update` returns `Close` when the caret leaves the word's row or goes before its start
(`CaretMoved`), a non-word char is typed (`LeftWord`), the text before the word on the line changed (`TextChanged`), or nothing
matches a complete list (`NoMatches`); Escape and accepting are the UI's `close(Dismissed)` / `close(Committed)`. **Commit characters:**
an item's own, else `itemDefaults`, else the server's `allCommitCharacters`; `is_commit_character(c)` is true for the selected item
only when something was typed or the user moved the selection, so a fresh list does not swallow `(`. **Additional edits** (auto-imports)
come from the list or from resolve; resolved ones are placed relative to the caret of the document version they were computed for
(the session remembers the caret per `rope.version()` of the last 64 updates, so documents must be tagged with the rope version);
edits that overlap the main edit or whose version is unknown are dropped and counted in `skipped_additional_edits`. **Multi-line text:**
`AdjustIndentation` (the default) re-indents lines after the first to the indentation of the caret's line, `AsIs` does not; line
breaks follow the document's (CRLF if its first line break is CRLF). **Multi-caret:** the main edit applies at the primary caret;
every other caret gets the same inserted text when the text before it (and, in Replace mode, after it) equals the primary's text
over the main range, otherwise it is left alone (`skipped_carets`); additional edits are for the primary only; `carets` gives all
new caret offsets. **Snippets:** with `SessionOptions::snippets` off a snippet (which a server should not send) is flattened to its
plain text; on, `Snippet::parse` (`$1`, `${1:x}` nested, `${1|a,b|}`, `$0`, mirrors, variables replaced by their default or nothing,
escapes) yields tab stops as byte ranges in the final text (`CompletionCommit::tab_stops`, visiting order, `$0` last) and the primary
caret is placed at the first stop. Lector 0.7 does not run a tab-stop session itself; the UI may select the first stop or ignore them.

*Signature help* (`lector_lsp::SignatureSession`):
```rust
pub enum SignatureTrigger { Invoked, TriggerCharacter(char), ContentChange }
pub struct SignatureRequest { pub pos: PointUtf16, pub trigger: SignatureTrigger, pub active: Option<SignatureHelp> /* retrigger */ }
pub struct SignatureHelp { pub signatures: Vec<SignatureInfo { label, documentation, parameters: Vec<ParameterInfo { label, range: Option<Range<usize>>,
                           documentation }>, active_parameter }>, pub active_signature: usize, pub active_parameter: Option<usize>, pub raw: Value }
impl SignatureSession {
    pub fn new() -> Self; pub fn is_open(&self) -> bool; pub fn close(&mut self);
    pub fn request(&self, pos: PointUtf16, trigger: SignatureTrigger) -> SignatureRequest;   // retrigger (isRetrigger, activeSignatureHelp) when open
    pub fn trigger_for_typed(&self, caps: &SignatureHelpCapabilities, c: char) -> Option<SignatureTrigger>;  // trigger chars; retrigger chars only while open
    pub fn set_help(&mut self, help: Option<SignatureHelp>);    // None closes; the user's overload pick survives while still offered
    pub fn next_signature(&mut self); pub fn previous_signature(&mut self);                  // wrap
    pub fn active(&self) -> Option<ActiveSignature { signature, index, count, parameter: Option<usize>, highlight: Option<Range<usize>>, parameter_documentation }>;
}
```
Parameter labels are resolved to byte ranges of the signature label: a string label is searched after the previous parameter's end
(then anywhere), a `[start, end]` pair is UTF-16 and converted (ranges that split a surrogate pair or fall outside the label give
`None`). The active parameter is the signature's own, else the help's, and out of range means nothing is highlighted.

*Driving it from the UI* (typing a char `c` at the primary caret, completion enabled in settings, server has `completion`):
1. Session open and `session.is_commit_character(c)`: accept the selected item first (step 4), then insert `c`.
2. Insert `c` into the buffer; sync the document (`sync_lsp_now`).
3. Session open: `match session.update(rope, caret)`: `Continue` re-render rows; `Requery(req)` send it (same path as step 5);
   `Close(_)` hide. Session closed or none: if `c` is in `caps.completion.trigger_characters` open `TriggerCharacter(c)`, else if `c` is a
   word char open `Typing` (Ctrl+Space opens `Manual`, also with an empty prefix).
4. Accept (Enter/Tab/click): if `needs_resolve(item)` run `resolve_completion` and `apply_resolved(generation, ..)` first (a `Stale` answer:
   retry once or accept without the additional edits), then `commit(rope, &carets, primary, item)`, `Buffer::apply_edits(commit.edits.map(..))`,
   `set_selections(commit.carets)`, close with `Committed`, run `commit.command` if any (`triggers_signature_help()` opens signature help).
5. A new session sends `LspManager::completion(path, session.request(), None)` on a background task and feeds `Done(r)` to
   `set_response(rope_now, caret_now, r)` followed by `update(..)` (the user may have typed meanwhile); `Stale` with the session still
   open and no response yet: send `session.request()` again; `Cancelled`/`Unavailable`: ignore. When the selection changes and
   `needs_resolve`, resolve it (a newer resolve cancels the previous) to show documentation.
6. Signature help: `trigger_for_typed` on typed chars while the setting is on; while open, send a `ContentChange` request after edits or caret
   moves inside the call; `Done(None)` or a caret outside the call closes it; Up/Down (or the UI's keys) call `previous_signature` / `next_signature`.

*Settings* (`lector-config`, additive): `[editor.completion]` `enabled = true`, `snippets = false` (read when servers start; call
`LspManager::set_completion_snippets` before opening documents), `insert_mode = "insert" | "replace"` (`CompletionInsertMode`); `[editor.signature_help]`
`enabled = true`. They change how Lector talks to servers, never which program runs; project settings can set them like other editor settings
(the user-only restrictions on server commands are unchanged).

*Tests and numbers.* `tests/completion.rs` runs a scripted fake server (`tests/fixtures/fake_completion.py`: per-method steps with delays,
errors, ignoring cancellation, request log) over the real transport: capabilities and declaration, label / insertText / textEdit items with UTF-16
positions after `𝄞é`, insert vs replace, `itemDefaults`, `isIncomplete` re-query (trigger kind 3), resolve with additional edits, stale-version drop,
superseding cancel (+ `$/cancelRequest` id), `CancelToken`, `-32800` / `-32801`, unsupported servers, signature help with both label forms and
retrigger; an ignored `pylsp_smoke` runs against a real server. `tests/completion_perf.rs`: 10,000 items, per keystroke (median of 30 runs, workspace dev
profile): 0.54 ms for the first character (1,664 survivors), 0.35 / 0.29 / 0.11 ms for the next ones, 0.27 ms for a backspace (full re-filter),
worst keystroke 0.67 ms; ingesting the 10,000 items (`set_response`) 3.8 ms; the test asserts < 5 ms medians. Unit tests cover the matcher (ranking table), snippet
parser, parsing, session rules and signature help.

### Completion and signature help — editor UI (v0.7) — lector-ui `editor::{completion, signature_help}`, `code_view::popup` — as implemented

The popups are built on the session models above; the editor integration is three small pieces.

* **`CodeView` popup hosting** (`code_view/popup.rs`). `CodeView::set_popup(key, Option<EditorPopup { view, anchor: Pos, size, prefer:
  Vertical, on_dismiss }>)` shows a non-focusing popup (focus stays in the editor; typing keeps going to the document). The placement
  is the pure `place_popup(anchor_point, line_height, size, window, margin) -> Placement { origin, above }`: below the line, above
  when it does not fit below and fits above (else the side with more room), x clamped into the window, so a popup at the window edge
  is shifted, never cut. The anchor is a buffer `Pos` mapped through the last layout on every frame, so the popup **follows
  horizontal and vertical scrolling**; it is dropped (`on_dismiss`) when its row scrolls out of view, when the view loses focus, or by
  its owner. `set_context_flag(flag, on)` adds an identifier to the view's key context (`CodeView && completion`);
  `take_typed()` returns the text of the last plain keystroke (so owners can tell typing from other edits);
  `set_input_interceptor(f)` lets an owner take a keystroke over before it is inserted (commit characters);
  `insert_typed(text)` types text like a keystroke without asking the interceptor.
* **One controller per file tab** (`editor::completion::CompletionController`, found with `controller_for(&Entity<FileView>)`;
  `completion::init` is called by `lsp::init` and creates it, and the tab's `SignatureController`, for every `FileView`). Only
  file tabs have controllers: diff, review and other non-editable code views never open a popup. The controller listens to the code
  view's `Edited` / `SelectionChanged` events, drives `CompletionSession` as in "Driving it from the UI" (steps 1 to 6), and is the
  popup's view.
* **Opening.** A typed identifier character (`WordChars::for_language`) or a server trigger character opens a session; Ctrl+Space
  (`editor::TriggerSuggest`) opens one by hand, also with an empty prefix. Never opened: `editor.completion.enabled = false`, a
  read-only tab, an IME composition (a composition in progress also closes a session), while the same word was dismissed
  with Escape or has no match (until a non-word char is typed or the caret moves to another word). Nothing is requested on open or
  focus. The document is synced (`lsp::sync_now`, tag = rope version) before every request, the request runs on a background task, the manager
  cancels the one in flight, and a `Stale` answer for a session without an answer is asked again (at most three times). The popup shows only
  when the server answered and something matches; until then the keys keep their normal meaning (only Escape, which cancels the request).
  Manual trigger without a usable server never fails silently: a status-bar toast says why (servers off, no server for this kind of
  file, still starting, failed, or the "install ..." hint from `lector_lsp::install` when none is installed); automatic triggers are
  silent. Latency: the answer is filtered synchronously on the UI thread when it arrives (`set_response`, then `update` for text typed
  meanwhile); a debug log (`lector_ui::perf`: "completion list filtered", "completion reply -> first render") times reply to first paint.
  Measured at model level, 10,000 items (`editor::completion::tests::ten_thousand_items_reach_the_first_page_within_budget`, workspace dev
  profile): `set_response` + first page of rows about 5 ms, one more keystroke about 2 ms, against the 50 ms budget.
* **The popup.** A hand-virtualized list: at most `VISIBLE_ROWS` (10) rows are built, whatever the list size; mouse wheel scrolls,
  click accepts. Row = kind badge (the outline's symbol badges where a symbol kind exists, a letter badge otherwise), label with the
  matched characters accented (`label_highlights`), dimmed `labelDetails.detail` after it, right-aligned dimmed description or detail,
  deprecated items struck through. A side pane shows the selected item's detail and documentation (the existing markdown renderer);
  documentation that only `completionItem/resolve` gives is requested 120 ms after the selection settled and cancelled when it moves.
  Width adapts to the first 64 rows within 280..560 px plus the docs pane (320 px); theme tokens; follows UI scale; no animation.
* **Keys while the popup shows** (key context `CodeView && completion`, bindings defined after the plain `CodeView` ones so they win;
  all rebindable, actions in the `completion` namespace, hidden from the palette): `down` / `up` / `ctrl-n` / `ctrl-p` `SelectNext` /
  `SelectPrev` (wrap), `pagedown` / `pageup` `PageDown` / `PageUp` (10 rows, no wrap), `enter` `AcceptOnEnter`, `tab` `Accept`, `escape`
  `Dismiss`. Everything else types into the document and updates the session (typing narrows, backspace widens, a non-word char or leaving
  the word closes). The actions are handled at workspace level (`Workspace::register_action` runs before the editor's); since GPUI
  reads the key context of the last rendered frame, each handler falls back to the editor action the key normally has (`MoveDown`,
  `PageUp`, `Tab`, `Newline`, `ClearSelection`) when the popup is already gone. **Escape closes only the popup**: it does not clear
  multi-caret selections or close anything else; a second Escape is the editor's. `editor.completion.accept_on_enter` (default `true`)
  turns Enter's acceptance off (Enter then closes the list and inserts a line break); Tab always accepts. **Commit characters** go through
  the input interceptor: when `is_commit_character(c)`, the selected item is accepted first, then `c` is typed. Ctrl+I is not bound
  (the inline agent edit owns it); Ctrl+Space and Ctrl+Shift+Space are the only new global bindings.
* **Accepting** (one undo step): if `needs_resolve`, resolve first, waiting at most 300 ms (then accept without the extra edits), then
  `CompletionSession::commit(rope, carets, primary, item)` and **one** `Buffer::apply_edits` for the main edit and the additional edits
  (auto-imports), then `set_selections(commit.carets)`, so undo restores text, imports and carets together. Multi-caret follows the
  session rules (`skipped_carets`). If the document version changed so that the commit no longer fits (`CommitError`), the popup closes quietly.
  The session is closed with `Committed` and does not reopen for the same word; with `commit.triggers_signature_help()` signature help opens
  right after. **Snippets** (`editor.completion.snippets = true`; applied through `LspManager::set_completion_snippets` when the project
  opens): there is no tab-stop session in the editor, so the caret is placed at the first stop and the remaining stops are ignored.
* **Signature help** (`editor::signature_help::SignatureController`). Opens on the server's trigger characters (`(`, `,` per
  capabilities) while `editor.signature_help.enabled`, after accepting a completion that asks for it, and with Ctrl+Shift+Space
  (`editor::TriggerParameterHints`). A small popup above the line (it coexists with the completion popup below the line) shows the active
  overload with the active parameter highlighted (byte range from the session), the overload counter ("1/3") when there are several, the
  signature's and the active parameter's documentation. `alt-down` / `alt-up` (and plain `down` / `up` when there is more than one overload
  and no completion popup) switch overloads (`signature::NextSignature` / `PrevSignature`, context `signature_help` / `signature_overloads`);
  `escape` closes it (`signature::Dismiss`, only when no completion popup shows). While open, edits and caret moves inside the call send a
  `ContentChange` retrigger (coalesced; a newer request cancels the older); it closes on `Done(None)`, when the caret leaves the call
  (including typing the matching `)`), on Escape, on blur, or when the tab becomes read-only.
* **Settings and menus.** `[editor.completion] accept_on_enter = true` is new (the others are described above). Edit menu: "Trigger Suggest",
  "Trigger Parameter Hints" (palette commands `editor::TriggerSuggest`, `editor::TriggerParameterHints`); the shortcut viewer lists the new
  bindings like any other.
* *Tests.* `editor/completion/tests.rs`: row model, labels runs (match accent, strikethrough, char boundaries), badges, list scrolling,
  width, the 10,000-item budget; in a test window with the fake server (`fake_completion.py`): open on typing, filter as typing continues
  without asking again, accept with Enter and Tab as one undo step, `accept_on_enter = false`, Escape closes only the popup with two carets,
  a late answer for a closed session and a stale answer (asked again), read-only tab, IME composition, `enabled = false`, additional edits in
  the same undo step, multi-caret accept, selection navigation and paging (Up / Down / Ctrl+N / Ctrl+P / PageUp / PageDown, wrap), commit
  character, caret leaving the word, Ctrl+Space, no language server, signature help (active parameter, retrigger on `,`, overloads with Alt+Up/Down
  and Up/Down, closing on `)`, Escape, manual trigger). `code_view/popup.rs`: placement math (below, above, clamped, margins). Not covered
  by automated tests: the real paint of the popup in a window (see `scripts/screenshot.sh`) and real servers (the ignored `pylsp_smoke`
  exercises only the protocol).

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
// load_file: non-regular files (FIFOs, sockets, devices) → Err(InvalidInput) instead of blocking.
pub enum FsEventKind { Created, Modified, Removed }
pub struct FsEvent { pub path: PathBuf, pub kind: FsEventKind }   // absolute paths, deduped per batch
pub enum WatchEvent { Files(Vec<FsEvent>), GitStateChanged /* .git/HEAD, index, refs, packed-refs, *_HEAD */, Rescan }
pub struct Watcher;   // drop to stop
pub fn watch(root: &Path, debounce: Duration, sink: impl Fn(WatchEvent) + Send + 'static) -> lector_fs::Result<Watcher>;
pub enum Error { Notify(notify::Error), Io(io::Error) }   // thiserror; `Result<T> = Result<T, Error>`
```
Per debounced batch the sink gets either `Rescan` alone, or `Files` (if any) followed by
`GitStateChanged` (if any). The sink runs on the watcher's background thread. Symlinks are not
followed (like the walker), so a link to `/` or a symlink loop cannot make the watch crawl or fail.

### lector-config
```rust
pub struct Settings { /* serde(default) everywhere; see crate docs */ }
// v0.4 additions: `ui: UiSettings { scale, reduce_motion }` (`[ui]`), see "Accessibility" below.
pub fn load_settings() -> (Settings, Vec<String> /*warnings*/);   // defaults ← user settings.toml ← project .lector/settings.toml
pub fn default_settings_toml() -> &'static str;   // documented, commented default file
pub struct KeyBinding { pub keys: String /* "cmd-p" / "ctrl-shift-f" (gpui syntax) */, pub action: String, pub context: Option<String> }
pub fn default_keymap(platform: Platform) -> Vec<KeyBinding>;   pub fn load_keymap() -> (Vec<KeyBinding>, Vec<String>);
pub mod paths { pub fn portable_root() -> Option<&'static Path> /* <exe dir>/lector-data */; pub fn is_portable() -> bool; pub fn config_dir() -> PathBuf; pub fn data_dir() -> PathBuf; pub fn settings_file() -> PathBuf; pub fn keymap_file() -> PathBuf; pub fn themes_dir() -> PathBuf; pub fn log_file() -> PathBuf; }
pub struct SessionStore; // load(root) -> Option<Session>, save(root, &Session) — JSON under data_dir/sessions/<hash>.json
pub struct RecentProjects; // new(), list() -> Vec<PathBuf> (newest first), most_recent_existing(), record(root), remove(root) — data_dir/recent.json, max 20
pub struct ReviewStore;  // viewed-file state per (repo root, changeset key): is_viewed/set_viewed, persisted JSON
// v0.4: ReviewStore::with_backend(Arc<dyn ReviewBackend { get, set, len }>) (lector-app's Db implements it);
// session_key(root) -> String; SessionStore::new / RecentProjects::new are deprecated (Db-backed stores in lector-app).
// Additions (lector-config): `load_settings_for(Option<&Path>)`, `Settings` sub-structs (ThemeSettings, DiffSettings,
// FileSettings, LspSettings{enabled, servers: BTreeMap<String, ServerOverride{command,args,enabled}>}, GitSettings,
// SessionSettings); `buffer_font_family: Vec<String>` (accepts a string or list in TOML); `Platform::{MacOS,Linux,Windows}`
// + `Platform::current()`; `merge_keymap`; keymap.toml = `[[bindings]] keys/action/context` ("none"/"" unbinds).
// Env overrides: LECTOR_CONFIG_DIR, LECTOR_DATA_DIR. Contexts: Workspace, CodeView, DiffView, Picker.
// v0.7: `git.auto_fetch: AutoFetch::{Off (default), Minutes(n)}` (TOML `"off"` or a number; `interval()`), user-level
// only (dropped from the project layer with a warning): nothing fetches on a timer unless the user opts in.
// Project layer (`<project>/.lector/settings.toml`) cannot set `[forge]`, `agent.servers`, `lsp.servers.*.command` /
// `args` or `lsp.servers.*.enabled = true` (dropped with a warning): opening a project never runs a program it chose.
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
    pub fn open_detached(root: &Path, settings: Settings) -> Result<Project>;   // no git discovery, LSP off (no-folder windows)
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
    // v0.3 editing: snapshots of edited text and a cheaper gutter for live edits.
    pub fn with_text(&self, text: Text, syntax: Option<Syntax>) -> Document;  // same path/language/encoding/line ending/indentation; caches empty
    pub fn with_path(&self, path: PathBuf, rel: Option<String>, languages: &LanguageRegistry) -> Document;  // "Save As"
    pub fn gutter_base(&self, project: &Project) -> Option<String>;   // HEAD text ("" untracked), None = no gutter (blocking)
    pub fn gutter_against(&self, base: &str) -> Vec<GutterMark>;      // diff_gutter = gutter_against(gutter_base)
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
* **Staging & committing (v0.2)** — every write is an explicit user action run off the main thread through
  `staging::{GitOp, run_op}` (Stage / Unstage / StageAll / UnstageAll / Discard paths, Hunk {changeset, change, action,
  selection}); afterwards `notify_repo_changed` emits `GitStoreEvent::Changed` unconditionally (status can look the
  same after staging one more hunk) and refreshes HEAD/status, so Changes, History, diff tabs and the branch label
  reload at once. Failures show git's stderr inline (panel banner / diff banner / commit error box), never only logs.
  * Changes panel: hover buttons per row (Staged: − Unstage; Unstaged: ↶ Discard, + Stage; Untracked: 🗑 Delete,
    + Stage; Conflicts: ✓ Mark Resolved) replacing the stats while hovered; section headers: Unstage All / Discard
    All… + Stage All / Delete All Untracked + Stage All; the same items head each row's context menu. The file list
    has key context `ChangesPanel` (`space` = `git::Stage` toggles the selected row, `up`/`down`/`enter`; clicking a
    row opens the preview diff and keeps focus in the list). After an op the selection stays on the file if it is still
    in its section, else on the row now at the same position (space walks down a section); the list keeps its scroll.
  * Discards (v0.4) run at once through the `git.discard*` registry commands after a shadow backup and show the
    8 s "Discarded … · Undo" toast (`Workspace::show_action_toast`; also `workspace::UndoLastOperation`). They confirm
    via `confirm::{confirm, discard_spec}` (modal in the workspace's `ModalLayer`; Enter / Escape; naming every file,
    saying staged changes are kept and that untracked files are deleted from disk) only when `git.confirm_discard =
    true` (default `false`) or when the backup is unavailable (`BackupUnavailable` → "Discard without a backup?").
    Every Changes-panel / diff / merge-editor button goes through `staging::run_op(store, GitOp, cx)`, which invokes
    the command (`GitOp::command` maps op → id + args), refreshes the git views and shows the toast.
  * `activity::{ActivityView, ActivityItem}` — the **Activity log** tab (`workspace::ShowActivity`, View → Activity
    Log): every journaled write newest first (what, when, repository, files), an Undo button per entry (disabled with
    the reason: already undone, irreversible, backup cleaned up), "Show changes" opening a before/after `CompareItem`
    from the shadow backup; live via `Journal::subscribe`. Undo of an entry → `Journal::undo`; `Changed` → confirmation
    "These files changed since — restore anyway?" → `UndoMode::Force`; a successful undo toasts "Redo".
  * `commit_composer::CommitComposer` (top of the panel in Working Tree mode; key context `CommitEditor`, so
    `cmd/ctrl-enter` = `git::Commit`): a `TextArea` in the buffer font with the `git.commit_message_ruler` column and
    a subject-length counter (amber > 50, red > 72), Amend (`git::ToggleAmend`; loads `last_commit_message`, restores
    the draft when turned off), Sign off (default `git.commit_sign_off`), "Committing as Name <email>" from
    `Repo::identity` (missing → inline help with the exact `git config --global user.name/user.email` commands, copy
    and re-check buttons), Commit button disabled with a tooltip saying why (committing, empty message, nothing staged
    and not amending, no identity), hook/commit errors in a scrollable monospace box, success toast
    "Committed abc1234 <summary>", then refresh.
  * Diff view (`DiffView::changeset()` from the tab's changeset key): `HunkAction::available_for` decides the actions —
    floating "+ Stage / ↶ Discard" (Unstaged) or "− Unstage" (Staged) toolbars at the right edge above each visible
    hunk (the hunk at the cursor is outlined; repositioned on scroll), `git::StageHunk` / `UnstageHunk` / `DiscardHunk`
    on the hunk at the cursor (`cmd/ctrl-alt-s` / `cmd/ctrl-alt-u`; deliberately no bare-letter keys, so stray typing never stages), `git::StageSelectedLines` / `UnstageSelectedLines` (rows of the selection in
    the active column; both sides of changed rows), `git::Stage` / `Unstage` / `DiscardChanges` for the whole file, and
    a right-click menu (captured before the code view's) with Stage/Unstage/Discard Selected Lines, … Hunk, Copy,
    Open File. Discarding a hunk confirms. After an op the diff reloads and the cursor moves to the hunk now at the
    same index (the next one). Index/worktree diffs reload on every `GitStoreEvent`.
  * Workspace actions (`Workspace::register_action`, so menus and the palette find handlers): `git::{Stage, Unstage,
    DiscardChanges}` (active file), `StageAll`, `UnstageAll`, `DiscardAll` (confirmed), `Refresh`, `FocusCommitMessage`
    (shows the Changes panel), `Commit`, `ToggleAmend`. Focused list / editor / diff handlers run first.
* **Merge conflicts (v0.3)** — `GitStore::conflicts: Arc<ConflictState>` refreshes with HEAD/status.
  * `inline_conflicts`: `InlineConflicts` (in `GitViews::conflicts`) attaches a `FileConflicts` tracker to every file
    view (scanned on `ActiveItemChanged` / `FilesChanged`). It re-parses the buffer 80 ms after edits (background) and
    sets layer `"git-conflicts"`: tinted Current (success color) / base (grey) / Incoming (info color) rows, stronger on
    marker lines, and an action bar after each `<<<<<<<` line — Accept Current | Accept Incoming | Accept Both |
    Compare (`CompareItem` of the two sides) | Open Merge Editor. Resolutions are `DocumentModel::apply_edits` (one
    undo step, every tab of the file follows; nothing is written until the user saves). Read-only documents get tints only.
  * `merge_editor::MergeEditor` (pane item `MergeItem`, key `merge:<rel>`, session `TabKind::Other{"merge:<rel>"}`,
    key context `MergeEditor`): header (file, conflict kind, operation, "2 of 5 unresolved", prev/next, Base toggle,
    All Current… / All Incoming… with confirmation, **Mark Resolved**), Current | [Base] | Incoming panes on top
    (read-only `RevisionSource`s built from `side_view` of the first parse, highlighted, conflicts tinted — focused one
    stronger, resolved ones grey with a "Resolved" pill — and Accept buttons placed after the first short row of each
    conflict), Result below = a `FileView` of the working file (shared `DocumentModel`; its tracker is registered so the
    inline bar shows without "Open Merge Editor"). `MergeItem::file_view()` is the Result, so Save, dirty dots and close
    prompts work. Conflicts are followed across edits with `match_regions`; accepting advances to the next unresolved
    one. Scrolling is synchronized piecewise through the conflicts' row spans (the scrolled pane drives for 250 ms).
    The Base pane shows diff3 sections when every conflict has one, else the `:1` stage (whole file, untinted).
    Mark Resolved: markers or marker problems left → confirm "Resolve Anyway"; then `FileView::save` (normal save path,
    disk-conflict prompt included) → `GitOp::Stage` → opens the next conflicted path (sorted, wrapping) and closes
    the tab, or toasts and shows the Changes panel when none is left. Modify/delete conflicts show Keep File (stage)
    / Delete File (`git.resolve_delete`: backed up, no confirmation, Undo toast).
  * `operation_banner::OperationBanner` at the top of the Changes panel: "Merging feature → main" / "Rebasing feature
    onto main (3/7)" / "Cherry-picking abc1234 …", "2 conflicts · resolve, then Continue", **Continue** (disabled
    while conflicts remain; merge → commit composer with `MERGE_MSG` prefilled once per merge — the composer is in
    merge mode, so a merge commit needs nothing staged; rebase / cherry-pick / revert / am → `GitOp::Continue`) and
    **Abort…** (confirmation naming the branch, what is discarded and the conflicted files → `GitOp::Abort`). The
    composer refuses to commit while conflicts remain. Conflicts section rows open the merge editor (context menu:
    Open Merge Editor, Mark Resolved (Stage), ...).
  * Workspace actions (`conflict_actions`): `git::{OpenMergeEditor (active file if conflicted / has markers, else the
    first conflicted path), NextConflict, PrevConflict, AcceptOurs, AcceptTheirs, AcceptBoth (conflict at the cursor),
    AcceptAllOurs, AcceptAllTheirs (confirmed), MarkResolved (save + stage the active file), ContinueOperation,
    AbortOperation}`; the merge editor handles the same actions itself when focused.
* **Sync with the remote (v0.7)** — `sync::SyncController` (one per window, `GitStore::sync`), UI-free logic in
  `sync_model`, dialogs in `sync_dialogs`, credential prompts in `askpass_ui`. As implemented:
  * State: `SyncStatus` (branch / detached + short sha, upstream, `upstream_gone`, push remote, ahead/behind, remotes with
    redacted URLs, `FETCH_HEAD` mtime) from `Repo::upstream_status` in the background; ahead/behind follow every
    `GitStoreEvent`, the full read happens when branch / upstream / HEAD change and after every operation.
    `SyncState::{Loading, UpToDate, NeedsPush, NeedsPull, Diverged, NoUpstream, UpstreamGone, Detached, NoRemote}` →
    `control_view` = branch label, counts as text (`↑2 ↓1`), a state word ("up to date", "push", "pull", "diverged",
    "publish", "upstream gone", "detached", "no remote" — never color alone) and the click action (`PrimaryAction`:
    Fetch / Push / Pull (ff) / Pull (Rebase) for diverged / Publish / New Branch for detached / none).
  * Status bar: `SyncControl` replaces the branch button (`Workspace::set_branch_status_view`); click = `git::Sync` (the
    primary action); tooltip = what the click does + "Last fetched 12 min ago" (+ auto-fetch interval / last automatic
    failure); the chevron opens the sync menu above the bar (`git::ShowSyncMenu`): Fetch, Pull (Fast-Forward), Pull
    (Rebase), Push, Push (Force with Lease)…, Publish Branch, Set Upstream…, (Cancel), New Branch…, Switch Branch…,
    Branches. While an operation runs: spinner, "Pushing main → origin/main 45% · Writing objects", × = `git::CancelSync`.
  * Changes panel: `SyncPanelBar` at the top (state text, Fetch / Pull / Push icon buttons with tooltips and key
    bindings; disabled with "A push is running" while busy; progress bar + Cancel while running).
  * Running: `run_action(SyncAction)` → `sync_model::plan` → `Plan::{Run, Confirm, Blocked}`; `execute(OpKind, steps,
    label, quiet)` runs the registry commands in order on the app `Scheduler` (`Priority::Interactive`, journaled,
    `Source::Ui`), then `notify_repo_changed` (status, history, branches, diff tabs reload) and a status refresh. One
    operation per repository: this window's running op or a `SyncHost::running()` entry for the repo blocks new ones
    (toast). Progress comes from `SyncHost::subscribe` (filtered by repo, coalesced to the newest event). Cancel =
    `SyncHost::cancel_repo` (git, ssh and helpers killed) + pending credential prompts cancelled.
  * Force push exists only as `SyncAction::ForcePushWithLease` and is always `Plan::Confirm` first ("Force push to
    origin/main?" naming the remote branch, saying commits there may be discarded for everyone and that the lease refuses
    a moved branch); plain `--force` is never built.
  * Outcomes (`success_view`): toasts "Pulled 3 commits into main", "Pushed main → origin/main", "Published f → origin/f",
    "Already up to date"; a pull with an undo record shows the Undo toast (`git.restore_ref`); a rebase pull that stops
    with conflicts toasts and opens the Changes panel, where the operation banner / conflict flow takes over. After a
    successful commit the composer's toast offers **Push** (ahead of the upstream) or **Publish** (no upstream, a push
    remote exists) — `commit_composer::push_offer`.
  * Failures (`failure_view`, every `SyncErrorKind`): a `NoticeDialog` with what happened, the files involved, git's or
    the hook's output in the composer's output box (`commit_composer::output_box`), and next steps (`NextStep`):
    Rejected → Pull First (then the toast offers Push again; a diverged ff pull offers Pull (Rebase), which also offers
    Push again) / Pull (Rebase); NoUpstream → Publish Branch / Set Upstream…; LocalChanges / DirtyWorktree on pull →
    Stash and Pull (explicit; the toast then offers Pop Stash), on switch → Stash Changes and Switch; Diverged → Pull
    (Rebase); StaleLease → Fetch; AuthenticationFailed → explains which credential sources were tried (configured
    `credential.helper` values read with `git config --get-all`, whether Lector's prompt was answered or cancelled, ssh
    agent / key files for SSH remotes) → Retry; HostKeyUnknown / NetworkUnreachable / RemoteNotFound / HookDeclined /
    Other → Retry; HostKeyChanged → a warning and no way around it; DetachedHead → New Branch…. Cancelled → toast.
    Background (auto-fetch) failures never open a dialog: they show in the control's tooltip.
  * Credential prompts (`askpass_ui`): one `AskpassServer` per process, started lazily by the first network operation
    (runtime dir = `paths::data_dir()`), env installed with `SyncHost::set_askpass_env`; stopped on app quit. The handler
    posts to the GPUI thread and blocks ≤ 10 min (`PROMPT_TIMEOUT`); prompts queue and show one at a time as a
    `PromptDialog` in the window whose operation is running ("Operation: Pushing main → origin/main", "Remote:
    https://***@host/…"). Kinds (`prompt_spec`): username; password / token (masked `TextInput::set_masked`, never
    logged or stored by Lector, the note says git's credential helper may store it, the field is cleared on close);
    SSH key passphrase (masked, key path shown); unknown host key (host, key type, fingerprint; Cancel is the default —
    Enter answers Cancel until the user moves to "Trust and Continue" with Tab / arrows — and the answer is exactly
    `yes`); anything else verbatim, masked when it looks secret. Closing the dialog any other way, the window closing,
    the operation being cancelled or ending, all answer Cancel.
  * Branches: New Branch… (`InputDialog`: name validated as you type with git's ref rules, `validate_branch_name`; start
    point = HEAD or a branch / tag from the "Choose…" menu; "Switch to it" on by default), Switch (unsaved editor buffers
    go through the workspace's save prompt first; when git refuses because local changes would be overwritten the
    dialog lists the files and offers Stash Changes and Switch / Cancel — never a silent stash), Rename…, Delete (merged:
    at once with an Undo toast; unmerged: a confirmation listing the commits that would become unreachable, then
    `force`, still undoable), Set Upstream… (remote branch picker), Publish, Check Out as Local Branch for remote branches
    (create at the remote branch + set upstream). In the Branches panel's context menus, the Git menu, the palette and
    the sync menu.
  * Stash: Stash Changes… (message, "Include untracked files"), Apply / Pop / Drop from the stash rows' context menu
    or a stash picker; Drop and Pop are undoable from the toast (journal); conflicts on pop open the Changes panel.
  * Auto-fetch: `git.auto_fetch` (user-level, default `"off"`); when set, a 30 s tick fetches quietly once the last
    fetch (`FETCH_HEAD`) is older than the interval and nothing runs in the repository.
  * Tests: `sync_model::tests` (state → label / action for every state, action → command, force-with-lease gating,
    failure → next steps for every `SyncErrorKind`, credential source text, outcome toasts, branch-name table, prompt
    classification and masking), `sync_tests` (GPUI: push → rejected → Pull First → diverged → Pull (Rebase) → Push
    against a bare remote through the controller; dirty-tree switch → Stash Changes and Switch; force push waits for
    the confirmation and Escape cancels it; host-key default Cancel and literal `yes`, masked password answer and
    cleared field, prompt queue, cancel and close → Cancel).

### lector-buffer (v0.3, UI-free editing model; no GPUI, no tokio)
```rust
pub struct Selection { pub id: u64, pub anchor: usize, pub head: usize, pub goal: Option<u32> }
// byte offsets into the text; `head` is the caret; goal = preferred column (chars) for up/down
impl Selection { pub fn range(&self) -> Range<usize>; pub fn is_empty(&self) -> bool; pub fn reversed(&self) -> bool; }
pub enum IndentStyle { Tabs, Spaces(u32) }
pub struct CommentTokens { pub line: Option<String>, pub block: Option<(String, String)> }
pub struct BufferOptions { pub tab_size: u32, pub indent: IndentStyle, pub auto_indent: bool,
                           pub auto_close_brackets: bool, pub indent_after_colon: bool /* default false */,
                           pub comment: CommentTokens }
pub struct TextEdit { pub range: Range<usize>, pub new_text: String }   // offsets in the text *before* the edit batch
pub struct EditDelta { pub start_byte: usize, pub old_end_byte: usize, pub new_end_byte: usize,
                       pub start: Point, pub old_end: Point, pub new_end: Point }  // tree-sitter InputEdit / LSP change shape
pub struct Transaction { pub deltas: Vec<EditDelta> }  // returned by every mutating call (empty = nothing changed)
pub enum Motion { Left, Right, Up, Down, WordLeft, WordRight, LineStart /*smart: first non-blank, then col 0*/,
                  LineEnd, PageUp(u32 /*rows*/), PageDown(u32), BufferStart, BufferEnd }
pub struct Buffer;   // owns a Rope + selections + undo history; Clone is cheap-ish (Rope is O(1))
impl Buffer {
    pub fn new(text: Rope, options: BufferOptions) -> Buffer;      // one caret at 0
    pub fn text(&self) -> &Rope;  pub fn version(&self) -> u64;    // bumps on every change, undo/redo included
    pub fn is_dirty(&self) -> bool; pub fn mark_saved(&mut self);  // dirty = version != saved version (undo back to it → clean)
    pub fn options(&self) -> &BufferOptions; pub fn set_options(&mut self, o: BufferOptions);
    pub fn selections(&self) -> &[Selection];                     // sorted, non-overlapping, merged after every op
    pub fn set_selections(&mut self, sels: Vec<Selection>);       // normalizes (clip to char boundaries, sort, merge)
    pub fn set_cursor(&mut self, offset: usize);                  // single caret
    // editing — all multi-cursor aware; each call is one undo step unless coalesced (typing within 1 s,
    // same kind, adjacent) — see `begin_transaction`/`end_transaction` for explicit grouping
    pub fn insert(&mut self, text: &str) -> Transaction;          // typed text: auto-close brackets/quotes, type-over closers
    pub fn paste(&mut self, text: &str) -> Transaction;           // distributes lines across carets when counts match
    pub fn newline(&mut self) -> Transaction;                     // auto-indent (+1 level after `{ ( [`, after `:` only with `indent_after_colon`, and between pairs)
    pub fn newline_below(&mut self) -> Transaction; pub fn newline_above(&mut self) -> Transaction;
    pub fn backspace(&mut self) -> Transaction;                   // deletes a whole indent unit in leading whitespace, pairs `(|)`
    pub fn delete(&mut self) -> Transaction;
    pub fn delete_word_left(&mut self) -> Transaction; pub fn delete_word_right(&mut self) -> Transaction;
    pub fn delete_to_line_start(&mut self) -> Transaction; pub fn delete_line(&mut self) -> Transaction;
    pub fn tab(&mut self) -> Transaction;                         // indent selected lines, else insert indent unit
    pub fn indent(&mut self) -> Transaction; pub fn outdent(&mut self) -> Transaction;
    pub fn toggle_comment(&mut self) -> Transaction;              // line comments (block if no line token)
    pub fn duplicate_line(&mut self) -> Transaction;
    pub fn move_lines(&mut self, up: bool) -> Transaction;
    pub fn cut(&mut self) -> (String, Transaction);               // empty selection cuts the whole line
    pub fn copy(&self) -> String;                                 // empty selection copies the whole line
    pub fn apply_edits(&mut self, edits: Vec<TextEdit>) -> Transaction;  // external (LSP format/rename/code action, reload); one undo step; selections mapped
    pub fn replace_all(&mut self, text: &str) -> Transaction;     // keeps carets near their old line/column
    pub fn undo(&mut self) -> Transaction; pub fn redo(&mut self) -> Transaction;  // restore selections too
    pub fn begin_transaction(&mut self); pub fn end_transaction(&mut self);
    // selection & multi-cursor
    pub fn move_carets(&mut self, motion: Motion, extend: bool);  // collapses non-empty selections on plain Left/Right
    pub fn select_all(&mut self); pub fn select_line(&mut self);  // repeated: extends
    pub fn select_word_at(&mut self, offset: usize);
    pub fn select_next_occurrence(&mut self) -> bool;             // from a caret: whole words, case-sensitive; from a selection: substring; wraps
    pub fn select_all_occurrences(&mut self);
    pub fn add_caret_above(&mut self); pub fn add_caret_below(&mut self);
    pub fn add_caret(&mut self, offset: usize);                    // alt-click; on an existing caret removes it (never the last one)
    pub fn split_selection_into_lines(&mut self);
    pub fn collapse_selections(&mut self);                        // Escape: keep the newest caret only
}
pub mod save {  // encoding back to bytes for saving
    pub enum Encoding { Utf8, Utf8Bom, Utf16Le, Utf16Be }
    pub enum LineEnding { Lf, CrLf }
    pub struct SaveOptions { pub encoding: Encoding, pub line_ending: Option<LineEnding> /* None = keep text as is */,
                             pub trim_trailing_whitespace: bool, pub ensure_final_newline: bool }
    pub fn encode(text: &Rope, opts: &SaveOptions) -> Vec<u8>;
}
```
`lector-fs` adds `write_atomic(path, bytes) -> io::Result<()>` (temp file in the same dir,
fsync, rename, keep permissions; symlinks are written through to their target) and
`FileStamp { len: u64, modified: SystemTime }` + `stamp(path) -> io::Result<FileStamp>` for "changed on disk since load" checks.
`lector-syntax` adds `Syntax::edit_and_reparse<T: TextInput + ?Sized>(&self, text: &T, edits: &[SyntaxEdit]) -> Result<Syntax>`
where `SyntaxEdit { start_byte, old_end_byte, new_end_byte, start: Point, old_end: Point, new_end: Point }` has the same
field names as `EditDelta` (tree-sitter incremental parse, < 5 ms for typical edits in a 10k-line file).

**Notes (as implemented: lector-buffer, `write_atomic`, `edit_and_reparse`)**

*Deltas.* `Transaction::deltas` of a mutating call are listed **back to front** (descending `start_byte`); `start`/`old_end`
are coordinates in the text *before* the call, `new_end = start + extent(inserted text)`. Because the part of the text before
`start` is unchanged when a delta is applied in the listed order, the list is a valid sequential `Tree::edit` /
`SyntaxEdit` / LSP `contentChanges` sequence (convert `start`/`old_end` to UTF-16 against the pre-edit text). `undo()` returns the
inverse of the step's deltas (reversed order, `old_end`/`new_end` swapped) and `redo()` the recorded ones; for a step made of several
calls (typing run, explicit transaction) these are sequential, not descending-by-offset, but still valid in the listed order.
Selection-only calls change no text and return no deltas. A call that replaces every selection by identical text (paste or
`insert` of the selected text) is not an edit: no deltas, no undo step, the dirty flag is untouched; selections still move as if it
had been applied.

*Extra public API* (additive): `Buffer::{newest_selection, selected_text, can_undo, can_redo, undo_depth, set_coalesce_window,
break_undo_group}`, `Selection::caret`, `Transaction::is_empty`, `LineEnding::as_str`, `Default` for `BufferOptions` /
`IndentStyle` (`Spaces(4)`) / `SaveOptions`. `Buffer: Send + Sync`, `Clone` shares the rope and copies the history.

*Version and dirty.* `Buffer::version()` is the rope's process-unique content version: it changes on every edit and is the old
value again after `undo` restores that snapshot, so `is_dirty()` (version != saved version) is clean after undoing to the saved
state. Selection moves never change it.

*Selections.* Offsets are clipped to grapheme boundaries on every change (`\r\n` is one unit; a caret between `\r` and `\n`
is impossible). Normalization sorts by range and merges selections that overlap, or touch while at least one is empty; the merged
selection keeps id/direction/goal of the newest selection if involved, else of the first. `set_selections` reassigns ids that are 0 or
duplicated and makes the last given selection the newest. `collapse_selections`: several selections -> keep only the newest (as
is); one non-empty -> collapse to its head.

*History.* Steps keep rope snapshots (before/after) plus selections and deltas: `O(edit + log n)` memory per step, consecutive
steps share the middle snapshot. Bounds: 10 000 steps and a cost budget of 1 000 000 units (a selection counts 1, a delta 4, i.e.
roughly the rope chunk it dirtied), whichever hits first drops the oldest steps; a coalesced run stops growing at 20 000 cost units.
Coalescing (default window 1 s, `set_coalesce_window`, zero disables): consecutive `insert` (single typed text without newline),
`backspace`, or `delete` of the same kind, adjacent in time and with unchanged carets, join one step. Everything else (newline, paste,
line ops, any `move_carets`/`set_*`/`select_*` call) breaks the run; typing over a closer (a caret move without an edit) does not.
`begin_transaction`/`end_transaction` nest; `undo`/`redo` inside a transaction end it. A new edit clears the redo stack.

*Typing.* `insert("\n")` is `newline()`. Auto-close only for a single typed char and `auto_close_brackets`: `([{` close before
end of text, whitespace or one of `)]};:.,=>`; quotes `"'` and backtick also need a non-word, non-backslash char before them, an even
number of that quote earlier on the line and a different char before; a closer or quote is typed over only when Lector inserted it
and the caret has not left the `start..=end` region of that insertion since (regions are mapped through edits, dropped on undo,
redo and `replace_all`, and pruned as soon as no caret is inside), so `(|)` typed by the user and then `)` gives `()|)`; with a
non-empty selection `([{"'` and backtick surround it and keep the inner selection. `newline` copies the indent up to the caret, adds one
unit after `{ ( [` (last non-blank char before the caret; `:` too when `BufferOptions::indent_after_colon`, which the UI sets
for Python, YAML, Nim, CoffeeScript and Starlark from the language id in `editor/model.rs::buffer_options`) and expands `{|}`, `(|)`, `[|]` to three lines; it uses the document's
line ending (the first line break; `\n` if none). `backspace`: pairs `(|)` `"|"` vanish together; in leading whitespace with
`Spaces(n)` it deletes back to the previous multiple of `n` columns (tabs count `tab_size`); otherwise one grapheme.

*Words* (word motions, `delete_word_*`, double-click, occurrences): UAX #29 segments, with `a.b` / `can't` additionally split at
punctuation, runs of whitespace merged, runs of punctuation merged. `WordLeft` skips blanks then one word/punctuation run;
`WordRight` skips blanks then one run; at line edges they move to the neighbouring line. `select_next_occurrence` /
`select_all_occurrences` only start from a real word (not whitespace/punctuation). From an empty selection they match whole
words case-sensitively (`foo` does not select inside `foobar` or `Foo`); once the selection is non-empty (double-click, or
after `select_next_occurrence`) matches are plain substrings. The whole-word mode lasts while the text version and the newest
selection's text are unchanged. Skipping selected matches and wrap-around are unchanged.

*Electric outdent.* Typing `}` `]` or `)` on a line that is only whitespace (caret at its end, `auto_indent` on, no selection)
re-indents the line to the indent of the line holding the matching opener (backward scan over the same bracket kind, at most
200 000 bytes); with no opener it drops one indent unit.

*Lines.* Row groups: a non-empty multi-row selection ending at column 0 does not include that row. `indent` skips empty lines in
multi-line groups and aligns `Spaces(n)` to the next multiple; `outdent` removes a tab or up to one level of spaces. `toggle_comment`
with a line token decides over all non-blank rows (commented -> uncomment, else comment at the minimum indent, skipping blank rows);
without one it wraps/unwraps each selection's non-blank span with the block delimiters. `duplicate_line` copies below and moves the
selection into the copy; `move_lines` keeps line terminators in place (so the last line without `\n` works) and merges adjacent row
groups; `delete_line` keeps the caret column and removes the preceding break when deleting the final row.

*Clipboard.* `copy()` with only empty selections returns the carets' lines (one per row, each with a terminator) and remembers the
string; `paste` of exactly that string inserts above each caret's row (N lines onto N rows are distributed). Other pastes distribute N
lines (a single trailing newline ignored) onto N carets, else insert the whole text at every caret. Pasted line breaks are converted to
the document's style when the document has a line break.

*External edits.* `apply_edits`: clip to char boundaries, sort, drop edits overlapping an earlier one, shrink every edit to its
minimal differing part (common prefix/suffix), drop no-ops; selections map through with inserts at a caret pushing it along.
`replace_all`: one minimal edit; carets before the change stay, after it shift, inside keep (row, char column).

*Save.* `save::encode`: UTF-16 output carries a BOM; `line_ending: Some(_)` converts `\r\n`/`\n` (a lone `\r` is content);
`ensure_final_newline` never adds a break to empty text and uses the first line break's style when no `line_ending` is given.

*`write_atomic`.* Temp file `.<name>.lector-<pid>-<n>.tmp` beside the target (file watchers may briefly report it), fsync, rename,
parent-directory fsync (Unix); the existing file's permissions are copied; a read-only target is refused with `PermissionDenied`;
symlinks (also dangling ones and chains, up to 40 hops) are resolved so the target is replaced; on Windows the rename is retried on
`PermissionDenied` (sharing violations). The temp file is removed on any failure.

*`edit_and_reparse`.* Clones the tree, applies each edit with `Tree::edit` in order and re-parses with the old tree; the result has
fresh outline/injection caches (so injected layers such as Markdown fences are re-parsed). Inverted or otherwise invalid edits fall
back to a full parse. Measured: single-char edit in an 11k-line Rust file, median ~1 ms, p90 ~2 ms (fresh parse ~32 ms).
Property tests assert the incremental tree equals a fresh parse (s-expression) for valid programs.

*Performance* (criterion, `cargo bench -p lector-buffer`, bench profile, 2-core shared box, medians): insert one char in a
100k-line buffer 34 us (46 us when it starts its own undo step), newline 55 us; typing one char with 1000 carets 0.76 ms
(carets on nearby lines; was 1.0), backspace 1.9 ms (not re-measured), and 2.6 ms when the 1000 carets are spread over a 100k-line
file (was 10.4 ms before, ~14 ms in an earlier measurement, with per-edit `Rope::replace`; `Buffer` now builds the new text with one `Rope::apply_edits`, and `slice` keeps the chunk summaries
it already knows); moving 1000 carets 0.8 ms;
undo+redo with 1000 carets 9 us; `select_all_occurrences` with 10k matches 0.9 ms, `select_next_occurrence` 63 us, typing
with 10k carets 2.3 ms.

### lector-git write API (v0.2; `lector_git::write`, git CLI) — as implemented
```rust
impl Repo {
    pub fn stage_paths(&self, paths: &[&str]) -> Result<()>;     // git add -A -- <paths> (modifications, deletions, untracked)
    pub fn unstage_paths(&self, paths: &[&str]) -> Result<()>;   // git restore --staged -- <paths>; unborn HEAD: rm --cached -r
                                                                 // --ignore-unmatch. Staged rename: pass old and new path.
    pub fn stage_all(&self) -> Result<()>;                       // git add -A
    pub fn unstage_all(&self) -> Result<()>;                     // git reset -q (unborn HEAD: rm --cached -r -- .)
    pub fn discard_paths(&self, paths: &[&str]) -> Result<()>;   // tracked (git ls-files): restore --worktree (from the index,
                                                                 // staged changes survive); untracked: git clean -f -d (DELETES;
                                                                 // never touches tracked or ignored files)
    pub fn apply_patch(&self, patch: impl AsRef<[u8]>, target: PatchTarget, reverse: bool) -> Result<()>;
                                                                 // git apply [--cached] [-R] - (patch on stdin; context is checked)
    pub fn create_commit(&self, opts: &CommitOptions) -> Result<Oid>;   // = the contract's `commit` (that name is taken by the
                                                                 // reader `commit(oid)`): git commit -q -F - [--amend] [--signoff]
    pub fn last_commit_message(&self) -> Result<Option<String>>; // gix; None on an unborn branch
    pub fn identity(&self) -> Result<Option<(String, String)>>;  // git var GIT_AUTHOR_IDENT (config, env, useConfigOnly);
                                                                 // None = `git commit` would fail with "Author identity unknown"
}
pub enum PatchTarget { Index, Worktree }
pub struct CommitOptions { pub message: String, pub amend: bool, pub sign_off: bool }   // Default
```
Every write runs `git -C <workdir> --literal-pathspecs --no-pager -c core.quotepath=off <args>` (no shell; paths after `--`,
in chunks of 500; names with `*`, `[`, `:` are literal), `GIT_TERMINAL_PROMPT=0`, `GIT_EDITOR=true`, the user's config,
hooks, filters and signing (never `--no-verify`; `apply.whitespace` applies like `git add -p`). Paths that are absolute or
contain `..` are refused. Failures are `Error::GitCommand { command, status, stderr }` whose `stderr` is git's output
verbatim (stderr, plus stdout when git reports there — "nothing to commit", hook output), e.g. failing hooks, missing
identity, `index.lock: File exists`, "patch does not apply". Read paths are unchanged (gix, `GIT_OPTIONAL_LOCKS=0`).

**Patches** (`lector_diff::patch`, generic, no git): `build_patch(&PatchFile { path, mode }, old: Option<&[u8]>,
new: Option<&[u8]>, &LineSelection { old: Vec<Range<u32>>, new: Vec<Range<u32>> }) -> Option<Vec<u8>>` (+ `build_patch_with(..,
Algorithm)`) applies the selected changes of `old → new` to `old` with 3 lines of context taken from `old`, `git add -p`
edit-mode semantics (unselected removed lines become context, unselected added lines are dropped). The diff runs in the
display row model of `diff_lines` (split on `'\n'`, so hunks and rows are exactly what the view shows); output lines keep
their bytes (CRLF), `\ No newline at end of file` is emitted where needed (the trailing empty row is the "final newline"
change), `old == None` writes a new-file patch, `new == None` with everything selected a deleted-file patch, paths with
`"`, `\` or control characters are C-quoted, binary input (NUL) returns `None`. `LineSelection::{hunk(&Hunk), all(),
swapped(), is_empty()}`; `CONTEXT_LINES = 3`.

**Partial staging** (`lector_workspace::review`): `HunkAction::{Stage, Unstage, Discard}`,
`HunkAction::available_for(&Changeset)` (Unstaged → Stage + Discard, Staged → Unstage, others → none),
`FileDiff::{hunk_selection(ix), hunk_at(old, new)}`, `hunk_patch(repo, &changeset, &change, action, &selection, algorithm)
-> Result<Option<(Vec<u8>, PatchTarget)>>` and `apply_hunk_action(..) -> Result<bool>` (false = nothing to apply).
`apply_hunk_action_if_unchanged(.., expected_content_id: &str)` (used by the UI) first checks that both sides still match
`FileDiff::content_id()` of the diff the rows were selected in (blob ids / content hashes, working copy normalized as
below) and otherwise refuses with an error and writes nothing — rows of a stale diff never stage or discard other lines.
Stage applies `index → worktree` to the index; Unstage builds `index → HEAD` and Discard `worktree → index` with the
selection swapped and applies them forwards, so context always comes from the content being patched (neighbouring
unselected changes and staged+unstaged mixes never break it). Working-tree bytes are compared in git's normalized form
(CRLF → LF when the index has LF), which is what `git apply` reads after its clean filter (autocrlf round-trips).
Tests: `crates/lector-git/tests/write.rs` (paths, unborn HEAD, literal pathspecs, discard incl. untracked, apply, commit,
amend, sign-off, hooks, identity, index.lock, renames), `crates/lector-workspace/tests/staging.rs` (hunk stage/unstage/
discard round trips checked with `git diff --cached`, line selections, file edges, staged+unstaged in one file, missing
final newline, CRLF, autocrlf, deleted/added files), proptests in `lector_diff::patch`.

### Merge conflicts (v0.3) — lector-git, lector-workspace — as implemented
```rust
// lector-git (read-only: .git state files + index stages through gix)
impl Repo {
    pub fn operation(&self) -> Result<Option<RepoOperation>>;     // rebase-merge/, rebase-apply/ (+applying = am),
                                                                  // MERGE_HEAD, CHERRY_PICK_HEAD, REVERT_HEAD (in that order)
    pub fn conflicts(&self) -> Result<Vec<ConflictEntry>>;        // index entries with stages 1..3, sorted by path
    pub fn conflict_stage(&self, path: &str, stage: ConflictStage) -> Result<Option<Vec<u8>>>;  // `git show :n:path`
    pub fn merge_message(&self) -> Result<Option<String>>;        // .git/MERGE_MSG without `#` lines
}
pub enum OperationKind { Merge, Rebase, CherryPick, Revert, Am }  // command() "merge"…, verb() "Merging"…
pub struct RepoOperation { pub kind, pub interactive: bool, pub head_name: Option<String> /*branch changed / rebased*/,
    pub incoming: Option<String> /*"feature", "abc1234 Subject", rebase onto name or short sha*/, pub incoming_oid: Option<Oid>,
    pub step: Option<(u32, u32)> /*rebase/am progress*/, pub message: Option<String> /*MERGE_MSG*/ }  // describe():
    // "Merging feature into main", "Rebasing feature onto main (3/7)", "Cherry-picking abc1234 Fix parser"
pub enum ConflictStage { Base = 1, Ours = 2, Theirs = 3 }
pub struct ConflictEntry { pub path: String, pub base/ours/theirs: Option<Oid>, pub kind: ConflictKind }
pub enum ConflictKind { BothModified, BothAdded, DeletedByUs, DeletedByThem, AddedByUs, AddedByThem, BothDeleted } // label(), has_markers()
// lector-git write module (git CLI, explicit user actions only; `GIT_EDITOR=true`, never blocks on an editor)
impl Repo {
    pub fn continue_operation(&self, kind: OperationKind) -> Result<()>;   // git <op> --continue (fails while unmerged)
    pub fn abort_operation(&self, kind: OperationKind) -> Result<()>;      // git <op> --abort (DISCARDS resolutions; confirm first)
    pub fn skip_operation_step(&self, kind: OperationKind) -> Result<()>;  // git <op> --skip (not for merges)
}
// lector-workspace::conflicts (UI-free; re-exports the types above)
pub struct ConflictState { pub operation: Option<RepoOperation>, pub entries: Vec<ConflictEntry> }  // is_clean(), entry(path)
pub fn conflict_state(repo: &Repo) -> Result<ConflictState>;
pub fn stage_text(repo: &Repo, path: &str, stage: ConflictStage) -> Result<Option<String>>;  // None: absent or binary
pub fn has_conflict_markers(text: &str) -> bool;
pub fn parse_conflicts(text: &str) -> ConflictParse;   // { regions: Vec<ConflictRegion>, problems: Vec<MarkerProblem> }
pub struct ConflictRegion { pub range, pub ours, pub base: Option<_>, pub theirs: Range<usize> /*bytes*/,
    pub ours_label, pub base_label: Option<String>, pub theirs_label: String,
    pub start_row, pub base_row: Option<u32>, pub separator_row, pub end_row: u32 }  // ours/base/theirs_rows(), rows(), side(text, Side), contents(text)
pub enum Side { Ours, Theirs, Base }
pub enum Resolution { Ours, Theirs, OursThenTheirs, TheirsThenOurs, Base }
pub fn resolve(text: &str, region: &ConflictRegion, r: Resolution) -> lector_buffer::TextEdit;
pub fn resolve_all(text: &str, regions: &[ConflictRegion], r: Resolution) -> Vec<TextEdit>;
pub fn side_view(text: &str, parse: &ConflictParse, side: Side) -> SideView;  // { text, rows: Vec<Range<u32>> per conflict, complete }
pub fn match_regions(original: &[ConflictContents], current: &[ConflictContents]) -> Vec<Option<usize>>;  // None = resolved
```
Markers: exactly 7 marker chars then end of line or a space + label (`=======` takes none); `\r\n` is handled and kept
(ranges include terminators). The diff3 / zdiff3 `|||||||` base section is optional per conflict. Nothing panics:
nested `<<<<<<<` abandons the open conflict (`Unterminated` + `Nested`), `>>>>>>>` before `=======` is
`MissingSeparator`, stray `>>>>>>>` / `|||||||` are `Stray`; a lone `=======` outside a conflict is content
(Markdown/RST underlines). `resolved_text` drops the final line break when the `>>>>>>>` line had none (EOF).
`match_regions` aligns identical contents by LCS, then pairs edited-but-still-conflicted regions by position.
`ConflictParse::{region_at_row, next_after(row, wrap), prev_before(row, wrap)}`. Modern git marks plain rebases
`interactive` too (merge backend), exactly as `git status` reports them.
Tests: `crates/lector-git/tests/conflict.rs` (merge with both-modified / add-add / modify-delete stages, MERGE_MSG,
continue refused while conflicted then a 2-parent merge commit, abort, rebase progress 2/3 + continue + abort,
interactive flag, cherry-pick continue, revert abort), `crates/lector-workspace/tests/conflicts.rs` (merge / diff3 /
zdiff3 working files resolve to exactly stage :2 / :3 / :1, add/add, modify/delete, CRLF, and reading conflict
state leaves the repository byte-for-byte unchanged), unit + proptests in `conflicts.rs` (generated conflicts with
LF/CRLF round-trip through parse → resolve_all / side_view; arbitrary marker soup never panics).

### v0.4 foundations — design goals

Driven by four user jobs: *"what did the agent just do, and is it safe?"*, *"help me
understand this code"*, *"fix this small thing and ship it"*, *"tell the agent what's
wrong"*. Foundations so every later feature is cheap:

1. **Headless core (`lector-app`).** Documents, git state, review state, diagnostics,
   commands and history live in a UI-free crate. GPUI views render snapshots and dispatch
   commands. Agents, the CLI, MCP and tests drive the exact same core.
2. **Commands are the API.** Typed, parameterized commands with availability, preview,
   apply and undo. Menus, palette, keys, CLI, MCP tools and agents are front-ends.
3. **Undo instead of confirm.** Every write is recorded in an append-only **activity
   journal** with an undo record; file contents are backed up first in a **shadow repo**
   (Lector's own data dir, never the user's `.git`). Confirmation dialogs remain only for
   things that cannot be undone (e.g. aborting a rebase, pushing).
4. **One scheduler** with priorities, cancellation and keyed "latest wins" jobs.
5. **Versioned derived data**: outline, folds, gutter, blame mapping, diagnostics are
   queries over snapshot versions, cached — not ad-hoc debounced tasks.
6. **Embedded database** (redb) for review state, comments, sessions, recents, caches.
7. **Content-addressed caching** keyed by git blob id / content hash.

### lector-app (v0.4; UI-free, no GPUI, no tokio)
```rust
// scheduler.rs (Agent APP)
pub enum Priority { Interactive, Visible, Background, Idle }
pub struct JobKey(pub String);                 // jobs with the same key: newest cancels older ("latest wins")
pub struct Scheduler;                          // Clone; Scheduler::global(); fixed worker pool; priority queues
impl Scheduler {
    pub fn spawn<T: Send + 'static>(&self, prio: Priority, key: Option<JobKey>,
        f: impl FnOnce(&CancelToken) -> T + Send + 'static) -> JobHandle<T>;
    pub fn debounce(&self, key: JobKey, delay: Duration, prio: Priority, f: impl FnOnce(&CancelToken) + Send + 'static);
}
pub struct JobHandle<T>;   // cancel(), is_finished(), try_take(), into_future() -> impl Future<Output = Option<T>> (None = cancelled)

// commands/ (Agent APP)
pub struct CommandSpec { pub id: &'static str /* "git.stage" */, pub title: &'static str, pub category: &'static str,
                         pub args_schema: serde_json::Value /* JSON Schema */, pub writes: bool /* journaled */,
                         pub destructive: bool, pub undoable: bool,
                         pub ui_action: Option<&'static str> /* "git::Stage" — the GPUI action that dispatches it */ }
pub enum Availability { Available, Unavailable(String /* why, shown as tooltip */) }
pub struct CommandContext { pub core: AppCore, pub project: Option<Project>, pub focus: Focus /* active path, selections, changeset, hunk */ }
// is_sandboxed(): source is Mcp or Agent; confine_path(path) -> Result<PathBuf>: for sandboxed callers, path
// arguments of `document.*` / `workspace.open_path` / `git.*` reads must stay inside the project (after `..` and
// symlinks of the deepest existing ancestor) and outside `.git`; GUI and CLI callers are not restricted.
pub trait Command: Send + Sync {
    fn spec(&self) -> &CommandSpec;
    fn available(&self, cx: &CommandContext) -> Availability;
    fn preview(&self, cx: &CommandContext, args: &Value) -> anyhow::Result<Option<String /* human text or unified diff */>> { Ok(None) }
    fn apply(&self, cx: &CommandContext, args: &Value) -> anyhow::Result<Outcome>;
}
pub struct Outcome { pub summary: String, pub undo: Option<UndoRecord>, pub data: Value, pub ui: Vec<UiRequest>,
                     pub paths: Vec<PathBuf> /* written */, pub journal_id: Option<u64> /* set by invoke */ }
pub trait ActivitySink: Send + Sync { fn record(&self, r: ActivityRecord) -> Option<u64>; }   // the journal implements it
pub enum UiRequest { OpenPath { path: PathBuf, point: Option<Point> }, OpenChangeset(Changeset), Toast(String), Refresh }
pub struct CommandRegistry;  // register, get(id), list(), invoke(id, args, cx) -> Result<Outcome> — invoke records to the journal
pub struct AppCore;          // Clone handle: scheduler, registry, journal, db, git service, document store

// git_service.rs (Agent APP)
pub struct GitService;       // process-wide; one RepoState per repository shared by all windows
impl GitService { pub fn repo(&self, workdir: &Path) -> Option<Arc<RepoState>>; }
pub struct RepoState;        // snapshot() -> Arc<GitSnapshot { head, status, operation, conflicts, generation }>;
                             // subscribe(cb) -> Subscription; refresh() (coalesced); driven by the fs watcher, slow poll only as fallback

// db/ (Agent APP) — redb file at data_dir()/lector.redb
pub struct Db;               // open(path), tables: review (viewed marks), comments, sessions, recent, kv, cache (bytes by key, LRU-trimmed)
                             // one-time import of the v0.3 JSON stores (review.json, sessions/*.json, recent.json); keep the public
                             // ReviewStore / SessionStore / RecentProjects APIs working (backed by Db)

// journal/ (Agent SAFE) — as implemented, see "Activity journal and undo" below
pub struct Journal;          // append-only, persisted in Db; list(limit), get(id), undo(id, &CommandContext, UndoMode) -> Result<ActivityEntry, UndoError>, subscribe(cb)
pub struct ActivityEntry { pub id: u64, pub time: SystemTime, pub command: String, pub summary: String,
                           pub repo: Option<PathBuf>, pub paths: Vec<PathBuf>, pub undo: Option<UndoRecord>, pub undone: bool, .. }
pub enum UndoRecord {
    RestoreFiles { shadow: ShadowSnapshot, paths: Vec<String>, after: BTreeMap<String, String> },  // discard, accept, resolve, redo of an undo
    Invoke { command: String, args: Value },                         // stage <-> unstage (the inverse command)
    RestoreIndex { shadow: ShadowSnapshot, paths: Vec<String>, after: BTreeMap<String, String> },  // staging that has no simple inverse
    Multi { records: Vec<UndoRecord> },                              // a write that touched files and the index; undone last first
}

// documents/ (Agent DOCS)
pub struct DocumentStore;    // open(path) -> DocHandle (one per canonical path, shared by every view/window/agent)
pub struct DocHandle;        // Clone; snapshot() -> DocSnapshot { text, version, syntax?, dirty, disk: DiskState }; edit(|buffer|) -> Transaction;
                             // apply_edits(Vec<TextEdit>); save(); reload(); subscribe(cb);
                             // queries (cached per version, computed on the scheduler): outline(), folds(), gutter(), highlights(rows),
                             // diagnostics() mapped through edits, blame_map()
```

**Notes (as implemented) — scheduler, commands, db, git service, UI bridge (Agent APP).**
* **AppCore** (`lector_app::{AppCore, CoreOptions}`): `AppCore::global()` (GUI; the user's db) / `AppCore::open(CoreOptions
  { db_path, in_memory_db, workers, import_from, data_dir })` (never fails; `CoreOptions::for_tests()` = in-memory db, own
  2-thread pool). Accessors `scheduler()`, `registry()`, `db()`, `git()`, `review_store()`; `set_activity_sink(Arc<dyn
  ActivitySink>)` / `activity_sink()`; typed extension slots `set_extension::<T>(Arc<T>)`, `extension::<T>()`,
  `extension_or_init` (DocumentStore, Journal live there). `open` registers `commands::register_builtin` (read +
  documents + git_write) and installs the journal (`Journal::install`, or `install_in(data_dir)`).
* **Scheduler**: `Scheduler::new(workers)` / `global()` (`default_workers()` = num_cpus − 1, min 2); FIFO within a
  priority, highest priority first; keyed jobs cancel the previous job of the key (queued ones never run, running ones
  see the token and their result is dropped → handle resolves `None`); `debounce` keeps only the last closure per key
  and runs it as a keyed job `delay` after the last call; `cancel_key`, `pending()`, `wait_idle(timeout)`; panics in
  jobs are caught (handle → `None`). `JobHandle::{cancel, token, is_finished, try_take, into_future, wait}` (`wait` blocks;
  never from a worker). Workers stop when the last `Scheduler` clone drops. `CancelToken` = `lector_search::CancelToken`.
* **Commands**: `CommandSpec { id, title, category, args_schema, writes, destructive, undoable, ui_action }` built with
  `CommandSpec::read(..)` / `::write(..)` + `.with_ui_action(..)`, `.destructive()`, `.undoable()`; `to_json()` is the
  MCP/CLI description. `Outcome { summary, undo, data, ui, paths, journal_id }` (`Outcome::new(..).with_data/with_ui/
  with_undo/with_paths`). `CommandContext { core, project, focus: Focus { path, selections: Vec<Range<Point>>,
  changeset, hunk }, source: Source::{Ui, Cli, Mcp, Agent, Internal} }`. `CommandRegistry::{register, register_arc, get,
  for_ui_action, list (sorted by id), invoke, invoke_async}`; `invoke` = validate args (`commands::schema`: type incl.
  unions, properties, required, `additionalProperties: false`, items, enum, minimum/maximum; `null` args = `{}`) →
  availability → apply → for `writes` specs `ActivitySink::record(ActivityRecord { command, args, summary, repo, paths,
  undo, source }) -> Option<u64>` (the id lands in `Outcome::journal_id`). Errors: `CommandError::{Unknown,
  Unavailable(reason), InvalidArgs, Failed(anyhow)}`. `FnCommand::new(spec, apply).with_available(..)`.
* **Read commands** (`commands::read`, all `writes = false`, compact JSON, project-relative paths, 0-based lines,
  `[start, end)` ranges): `workspace.open_path {path, line?, column?}`, `workspace.open_location {location:
  "path:line:col" (1-based)}`, `git.open_changeset {changeset}`, `search.project {pattern, regex?, case?, whole_word?,
  include?, exclude?, max_results?=200}`, `symbols.find {name, exact?, limit?}` (indexes tags on demand),
  `git.list_changes {changeset?}` (+branch, operation, conflicts for the working tree), `git.show_diff {changeset?, path?,
  context?=3, max_files?=50}` (hunks with `@@` header and `+`/`-`/` ` lines), `git.blame {path?, start_line?, end_line?}`,
  `git.history {path?, start_line?, end_line?, rev?, author?, message?, limit?=50}`, `commands.list {category?,
  available_only?}`, `settings.get {key?}` (dotted). Changeset args use the review keys (`worktree`, `commit:<sha>`, …);
  missing args default from `Focus`.
* **Db** (`lector_app::db`): redb 4.5, tables `meta` (schema version, flags, comment seq), `review`, `comments`
  (`Comment { id, repo, changeset, path, line, anchor_text, body, created, resolved }`: `add_comment`, `update_comment`,
  `delete_comment`, `comments(repo, changeset?)`), `sessions` (key = `lector_config::session_key(root)`), `recent`, `kv`
  (`kv_get/put`, `kv_get_json/put_json`, `kv_scan(prefix)`), `cache` (`cache_get/put`, LRU by write time, trimmed to
  `CACHE_MAX_BYTES` = 64 MB every 64 puts, `trim_cache`), `journal` (`append_journal`, `journal_get/put/list/trim`).
  `SCHEMA_VERSION = 1`; migrations run in one write transaction on open; a newer file → `DbError::TooNew`. Opened in
  redb's `MultiWriter` mode (several Lector processes share the file; falls back to `ExclusiveWriter`); if opening fails
  (lock held by an incompatible process, corruption) `open_or_fallback` uses a private temp copy (deleted on drop), then
  memory — `Db::mode() -> DbMode::{File, TempCopy, Memory}`. `import_v03(data_dir)` imports review.json, sessions/*.json and
  recent.json once (flag in `meta`; files are kept). Stores: `Db::review_store()` → `lector_config::ReviewStore::with_backend`
  (new `ReviewBackend` trait in lector-config, so `lector_workspace::review` keeps taking `&ReviewStore`),
  `Db::session_store() -> DbSessionStore { load, save }`, `Db::recent_projects() -> DbRecentProjects { list,
  most_recent_existing, record, remove }`. The JSON `SessionStore::new` / `RecentProjects::new` are `#[deprecated]`;
  `ReviewStore::open` stays for tests/import.
* **GitService**: `GitService::{new(scheduler), without_watching, repo(path), repo_for(&Repo), live_repos}` keeps weak
  refs keyed by canonical workdir. `RepoState::{repo, workdir, snapshot, subscribe(cb) -> Subscription (drop =
  unsubscribe), refresh (debounced 40 ms, keyed per repo → N windows = one computation), refresh_and_notify (publish even
  if unchanged, after writes), refresh_blocking, is_watching, computations}`. `GitSnapshot { head, status, conflicts
  (operation + entries), generation, error }` + `branch_label()`; snapshots are published only when something changed
  (unchanged parts keep their `Arc`). Each state starts a `lector-fs` watcher on a background thread (`Files`,
  `GitStateChanged`, `Rescan` → refresh); if that fails, a 5 s poll thread runs while the state is alive.
* **UI** (`lector_ui::commands_bridge`): `CoreGlobal(AppCore)`, `core(cx)`, `command_context(&Workspace, cx)` (project,
  active path, selections + `register_focus_provider` hooks — lector-ui-git fills the changeset of the active diff /
  review / commit tab), `invoke(ws, id, args, toast_summary, window, cx)` (scheduler job; errors toast), `apply_outcome`
  (OpenPath → `open_path_at`; OpenChangeset → `set_changeset_opener` hook (lector-ui-git: review / commit tab); Toast;
  Refresh → `RepoState::refresh_and_notify`), `palette_entries(ctx)`, `recent_projects()`, `session_store()`. Every
  GPUI action named by a `CommandSpec::ui_action` is registered on each workspace (`Workspace::register_boxed_action`)
  and runs `invoke` (unavailable → toast with the reason). The palette (`CommandPaletteDelegate::with_registry`) greys
  action rows whose command is unavailable (reason as tooltip; confirm does nothing) and adds argument-free registry
  commands without an action ("Git: List Changes"). `Workspace` file-tree status and `GitStore` both subscribe to the
  shared `RepoState` (`Project::set_git_status(&[StatusEntry])` builds the tree's map from the shared status);
  `GitStore` keeps its public fields/API (`head`, `status`, `conflicts`, `review` (db-backed), `generation`, `refresh`)
  plus `repo_state()` / `refresh_and_notify()`, and no longer polls. `lector_ui_git::store_for(workspace_id, cx)`.
* Tests: scheduler (priority order, latest-wins, running-job cancellation, debounce coalescing / quiet period, 2000 jobs
  from 8 threads none lost or duplicated, keyed contention, panics); schema; registry invoke/journal; db (persistence,
  second open / corrupt file fallback, stores, v0.3 import once, cache LRU, journal, comments); git service (one state
  and one computation for many refreshes, subscriptions + generation, watcher-driven refresh, drop);
  `tests/read_only.rs` (every non-writing command on a repo fixture — tree and `.git` byte- and mtime-identical — plus
  structured outcome checks).

**documents (as implemented, `lector_app::documents`).**
```rust
pub trait DocExecutor: Send + Sync { fn spawn(&self, Priority, Option<JobKey>, DocJob); fn debounce(&self, JobKey, Duration, Priority, DocJob); }
// impl for Scheduler (production); InlineExecutor (runs at once; tests/CLI); lector-ui's GpuiExecutor under GPUI's test dispatcher.
impl DocumentStore {
    pub fn new(Arc<dyn DocExecutor>) -> Self;  pub fn for_core(&AppCore) -> Arc<DocumentStore>;   // AppCore extension slot, on its scheduler
    pub fn open(&self, path, &Project) -> Result<DocHandle, DocError>;   // blocking (reads + parses); existing doc if open
    pub fn open_with(&self, path, &Project, OpenOptions { buffer_options, warm_rows }) -> Result<DocHandle, DocError>;
    pub fn get(path) -> Option<DocHandle>;  documents();  dirty_documents();  forget(&DocHandle);  release(DocHandle);
    pub fn register_before_save(Arc<dyn BeforeSaveHook>) -> HookId;  unregister_before_save(HookId);
    pub fn set_delays(QueryDelays { structure: 300ms, gutter: 300ms, lsp: 150ms, blame: 120ms });   // QueryDelays::IMMEDIATE
}
impl DocHandle {   // PartialEq = same document
    // state: id, path, project, version (Rope::version; LSP tag), seq (monotonic edit counter), is_dirty, is_editable,
    // is_read_only, text() -> Option<Rope>, live_document() -> Document, snapshot() -> DocSnapshot { id, path, document, version,
    // seq, dirty, editable, read_only, disk: DiskState { stamp, encoding, deleted, conflict: Option<DiskConflict { disk, stamp }> },
    // save_count, syntax_pending }, holders()
    pub fn edit_from<R>(&self, origin: EditOrigin, since_seq: u64, f: impl FnOnce(&mut Buffer) -> (R, Transaction)) -> Result<EditResult<R>, DocError>;
    //   EditResult { value, transaction, catch_up: Vec<HistoryEntry> /* others' edits after since_seq */, gap, view: BufferView }
    pub fn edit(f) -> Result<(R, Arc<Transaction>)>;  apply_edits(Vec<TextEdit>);  apply_edits_at(version, edits) /* VersionMismatch */;
    pub fn replace_all(&str);  undo();  redo();  with_buffer_mut(|b| ..) /* no text change */;  set_options;  set_read_only(bool);
    pub fn catch_up(since_seq) -> Option<CatchUp { entries: Vec<HistoryEntry { seq, origin, version, text, transaction }>, gap, view }>;
    pub fn subscribe(cb: Fn(&DocEvent)) -> Subscription;   // in order, outside the document lock, on the thread that caused it
    // disk (blocking): save(SaveRequest { force, trim_trailing_whitespace, ensure_final_newline, run_hooks, path /* Save As */ })
    //   -> Result<SaveOutcome::{Saved, Unchanged}, DocError::ChangedOnDisk{..} | NotEditable | ReadOnly | Io>;
    //   check_disk() -> DiskChange::{Unchanged, Reloaded, Conflict, Deleted}; reload() (discard); keep_mine()
    // queries (non-blocking; QueryResult<T> { value /* newest known */, version, current }):
    pub fn structure() -> QueryResult<Structure { document, outline, folds }>;  outline();  folds();  settled_document();  scopes_at_row(row);
    pub fn gutter() -> QueryResult<Arc<Vec<GutterMark>>>;  refresh_gutter();  set_gutter_enabled(bool);
    pub fn blame_map() -> QueryResult<Arc<Vec<BlameHunk>>>;   // disk blame remapped through a line diff to the buffer
    pub fn highlights(rows) -> Option<Vec<LineHighlights>>;  request_highlights(&[chunk]) -> bool;  highlight_chunk(chunk);  cached_highlights();
    pub fn set_diagnostics(Option<lsp::DocumentDiagnostics>);  diagnostics() -> Arc<Vec<DocDiagnostic>>;   // mapped through edits
    pub fn sync_lsp_now() -> Option<u64>;   compute_counts() -> QueryCounts;
}
pub enum DocEvent { Edited { origin, seq, version, transaction }, Reparsed { version }, QueryReady { kind: QueryKind::{Structure, Gutter, Blame}, version },
                    HighlightsReady { version, chunks }, DiagnosticsChanged, DirtyChanged { dirty }, Saved { version }, DiskStateChanged,
                    Reloaded { version }, Replaced, PathChanged { old, new }, ReadOnlyChanged { read_only } }
pub struct EditOrigin(u64);  // EXTERNAL, RELOAD, SAVE, EditOrigin::new() per front-end
// policy: buffer_options(&Document, &Settings); auto_save_decision(AutoSave, delay_ms, AutoSaveTrigger::{Edited, TimerFired, FocusLost},
//   AutoSaveInputs { dirty, editable, conflict, deleted, read_only }) -> AutoSaveDecision::{Never, SaveNow, SaveAfter(d)} (never over a
//   conflict or a deleted file); cleanup_edits(rope, line_ending, trim, final_newline). derive: map_range/map_offset/map_row, remap_blame.
```
* *Locking.* One mutex per document, held only for a buffer op or installing a result. Events and jobs produced under it are collected
  and run after it is released, so callbacks may call back into the handle and `InlineExecutor` cannot deadlock. Jobs hold weak references.
* *Edits.* Every non-empty transaction gets a `seq`, a `HistoryEntry` (last 256: view catch-up, diagnostics conversion by LSP tag), a new
  live `Document` (syntax `None` until the reparse), queued `SyntaxEdit`s (one incremental reparse at a time on the executor, edits made
  meanwhile coalesced into the next; full parse on failure; a generation counter drops parses superseded by reloads / Save As), mapped
  diagnostics and a debounced LSP sync (tagged with the version). Queries are lazy: asking schedules a keyed debounce for the current
  version ("latest wins"); structure waits for the reparse of that version; results are installed only for their version and announced
  with `QueryReady`. Highlights: per-chunk cache for the current version; stale results are still announced (requesters forget pending marks).
* *Registry.* Keyed by `dunce::canonicalize(path)`; weak entries, so dropping the last handle forgets the document; `forget` detaches it
  when the last view closes without saving (the next open reloads from disk). LSP `close_document` when a document is dropped (unless a
  newer document of the path exists). Save As re-keys and re-detects the language.
* *Saving.* Serialized per document: store hooks → cleanup edits as buffer edits (origin `SAVE`) → encode with the file's encoding / line
  ending → stamp check (`ChangedOnDisk` unless `force`) → `write_atomic` → new stamp, `mark_saved` if unchanged meanwhile. A clean,
  deleted document is written again.
* *External changes.* Same stamp → nothing; gone → `deleted`; clean → `replace_all` (origin `RELOAD`, carets kept) adopting the disk
  parse (`Reloaded`); dirty → `DiskConflict`. Non-editable (or changed editability) documents are replaced wholesale (`Replaced`).
* *Commands* (`commands/documents.rs`, registered by `register_builtin`; JSON, project-relative paths, 0-based lines): `document.open
  {path}`, `document.read {path, start_line?, end_line?}` (≤ 2000 lines by default; returns `version`), `document.apply_edits {path,
  edits: [{range: [start, end) bytes | lines: [start, end), new_text}], expected_version?}` (unsaved until saved), `document.save {path,
  force?}`, `document.save_all`, `document.revert {path}` (destructive), `document.set_read_only {path, read_only}`, `document.list
  {dirty_only?}`. They use `DocumentStore::for_core`, the same store as the GUI.
* Tests: `documents::tests` (shared handle across two views with catch-up, encodings/CRLF/BOM round trip, stamp conflicts, cleanup edits,
  reload / conflict / keep mine / delete / recreate / revert, binary and read-only refusals, queries cached per version, gutter and
  blame in a git fixture, diagnostics mapping, before-save hooks, Save As, scheduler cancellation of superseded work, re-entrant
  subscribers), `policy::tests` (auto-save), `commands::documents::tests` (agent round trip).

### lector-git shadow repo (v0.4; Agent SAFE) — as implemented
```rust
pub struct ShadowRepo;       // bare repo at data_dir()/shadow/<hash of workdir>.git; objects/info/alternates -> the user repo's object dir
                             // (reads only). Lector writes only into the shadow, never into the user's .git.
impl ShadowRepo {
    pub fn path_for(data_dir: &Path, workdir: &Path) -> PathBuf;
    pub fn open(data_dir: &Path, repo: &Repo) -> Result<ShadowRepo>;                       // creates on first use; hermetic git config
    pub fn snapshot_paths(&self, paths: &[&str], note: &str) -> Result<ShadowSnapshot>;   // working-tree contents (missing = recorded as absent)
    pub fn snapshot_index(&self, paths: &[&str], note: &str) -> Result<ShadowSnapshot>;   // index entries incl. conflict stages 1-3 and modes
    pub fn snapshot_worktree(&self, note: &str) -> Result<ShadowSnapshot>;                // whole tree incl. untracked, honours .gitignore (agent checkpoints)
    pub fn is_ignored(&self, rel: &str) -> bool;                                           // `git check-ignore`: left out of snapshot_worktree
    pub fn snapshots(&self) -> Result<Vec<ShadowSnapshot>>;  contains(&snap) -> bool;  kind(&snap) -> Result<SnapshotKind>;
    pub fn entries(&self, &snap) -> Result<Vec<SnapshotEntry>>;  index_entries(&snap) -> Result<Vec<IndexRecord>>;  file_at(&snap, path) -> Result<Option<Vec<u8>>>;
    pub fn changes_between(&self, a: &ShadowSnapshot, b: &ShadowSnapshot) -> Result<Vec<FileChange>>;
    pub fn changes_to_worktree(&self, snap, ..) -> Result<Vec<FileChange>>;  load(&self, &Side) -> Result<Option<Vec<u8>>>;   // before/after text for the UI
    pub fn fingerprints(&self, paths) -> Result<BTreeMap<String, String>>;  fingerprint_worktree(..);  index_fingerprints(..);   // "changed since" checks
    pub fn restore_paths(&self, snap: &ShadowSnapshot, paths: &[&str]) -> Result<()>;     // writes files — only from an explicit undo
    pub fn restore_index(&self, snap: &ShadowSnapshot, paths: &[&str]) -> Result<()>;     // via `git update-index --index-info` on the user repo
    pub fn prune(&self, keep: Duration) -> Result<()>;  prune_with(PruneOptions { keep, max_bytes }) -> Result<PruneReport>;   // DEFAULT_MAX_BYTES = 1 GiB
}
pub struct ShadowSnapshot { pub commit: Oid, pub time: i64, pub note: String }
```
Design notes. Objects are written only through `git fast-import` into the shadow (never `hash-object -w`, which would
"freshen" alternates' object files in the user's repo); a snapshot commit has the tree `files/` (contents), `blobs/`
(index entries) and `manifest`, and is referenced from `refs/lector/snapshots/<time>-<n>` so `gc` keeps it. A lock file
serializes writers across processes. `prune` deletes refs and runs `git gc --prune=now` in the shadow only. Tests
(`crates/lector-git/tests/shadow.rs`) compare every file in the user's `.git` (content and mtime) before and after each
operation, plus round trips (binary, empty, symlink, exec bit, deleted, nested, unicode names), conflict stages and prune.

### Activity journal and undo (v0.4; Agent SAFE; `lector_app::journal`, `commands::git_write`)
* `Journal` implements `ActivitySink`; `CommandRegistry::invoke` records every `writes` command (summary, repo, paths,
  `UndoRecord`, source) and fills `Outcome::journal_id`. Entries persist in the redb journal table
  (`Db::{append_journal, journal_get, journal_put, journal_list, journal_trim}`), are bounded (`Journal::MAX_ENTRIES` =
  2000, oldest dropped) and survive restarts. `Journal::{list, list_before, get, append, subscribe, last_undoable(repo),
  undo_status(&entry) -> UndoStatus, undo_last, install(&AppCore), install_in(&AppCore, data_dir), of(&AppCore)}`;
  `JournalStore` = `DbStore` / `MemoryStore` (tests).
* `Journal::undo(id, &CommandContext, UndoMode::{Check, Force})`: `RestoreFiles` / `RestoreIndex` first compare the
  recorded `after` fingerprints with the present state and refuse with `UndoError::Changed { id, paths }` (nothing
  written); `Force` ("Restore anyway") proceeds. Before overwriting, the current state is backed up again and stored in
  the undo's own journal entry, so an undo is undoable (redo) and so is "Restore anyway". `Invoke` runs the inverse
  command through the registry (its own entry is the undo entry); `Multi` undoes last first. Errors: `UndoError::
  {NotFound, AlreadyUndone, NotUndoable(reason), Changed, BackupGone, Failed}` — every refusal writes nothing.
* **Git write commands** (`commands::git_write`, JSON schemas, `Outcome { summary, undo }`, none sets `ui_action`):
  `git.stage {paths}`, `git.unstage {paths}`, `git.discard {paths, allow_without_backup?}` (restores tracked paths from
  the index, deletes untracked ones; backed up first), `git.stage_hunk` / `git.unstage_hunk` / `git.discard_hunk {path,
  changeset?, hunk? | old_lines/new_lines?, content_id?, allow_without_backup?}` (a stale `content_id` is refused),
  `git.stage_all`, `git.unstage_all`, `git.discard_all {scope?: unstaged|untracked|all}`, `git.commit {message, amend?,
  sign_off?}` (not undoable: the journal records it), `git.accept_file {path?, side: ours|theirs}`, `git.mark_resolved {path?}`,
  `git.resolve_delete {path}` (undo = `Multi`), `git.continue_operation {kind}`, `git.abort_operation {kind}`
  (irreversible, `undo: None`). Destructive commands that cannot take their backup fail with
  `BackupUnavailable` unless `allow_without_backup` is set; `backup_available(core, repo)` lets the UI ask beforehand.
  Backups older than 30 days / above 1 GiB are trimmed the first time a repository's shadow is opened in a process.
* Tests: `crates/lector-app/tests/git_write.rs` (stage/unstage inverse, partial index restore, discard + undo exact
  bytes, `Changed` refusal + `Force` + redo, hunks, stale `content_id`, commit, conflicts, abort, `resolve_delete`,
  `undo_last`, `BackupUnavailable`, pruned backup, persistence/bound/subscribe, schema flags); hermetic git config
  (`GIT_CONFIG_GLOBAL/SYSTEM=/dev/null`, `gc.auto=0`, `maintenance.auto=false`); lector-ui-git `staging::tests` (op →
  command mapping; discard → Undo toast → `UndoLastOperation` restores the files).

### lector-process (v0.7) — as implemented
One component supervises every child process (git network/branch operations and git CLI reads, language
servers, later agents, terminals). `lector-lsp`'s client spawns its server through `Supervisor::global()` (kind
`LanguageServer`; the child is a `ChildHandle`, its stdio is taken as before, and `Client::kill` kills the whole tree). No internal dependencies, no tokio, `#![forbid(unsafe_code)]`: process trees come from
`process-wrap` 8.2 (std feature): a new **session** (`setsid`; or only a new process group with `new_session(false)`)
on Unix, a **Job Object** plus `CREATE_NO_WINDOW` on Windows.
```rust
pub struct ProcessCommand;   // new(program).arg/args/cwd/env/envs/env_remove/env_clear/stdin(StdinMode)/stdout(OutputMode)/stderr(..)
                             // .label(..).kind(ProcessKind).new_session(bool=true).kill_tree_on_exit(bool=true).console_window(bool=false)
                             // Debug shows env *names* only and the length of stdin bytes.
pub enum StdinMode { Null /* default */, Piped, Bytes(Vec<u8>) /* written on a thread, then closed */ }
pub enum OutputMode { Piped /* default */, Null, Inherit }
pub enum ProcessKind { Git, LanguageServer, Agent, Terminal, Helper, Other }
pub struct CancelToken;      // Arc<AtomicBool>, same shape as lector_search::CancelToken; from_flag(Arc<AtomicBool>) / flag()
pub struct Supervisor;       // Clone; Supervisor::global() / new()
impl Supervisor {
    pub fn spawn(&self, &ProcessCommand) -> Result<ChildHandle>;                         // long-lived (LSP, agents)
    pub fn run(&self, &ProcessCommand, &RunOptions) -> Result<RunOutput>;                // to completion, captured
    pub fn run_streaming(&self, &ProcessCommand, &RunOptions, on_line: &mut dyn FnMut(&Line)) -> Result<RunOutput>;
    pub fn list(&self) -> Vec<ProcessInfo { id, pid, label, kind, program, started, running }>;
    pub fn kill(&self, id: u64) -> bool;  pub fn shutdown(&self, grace: Duration);  pub fn is_shutting_down(&self) -> bool;
}
pub fn run(..); pub fn run_streaming(..);   // on the global supervisor
pub struct RunOptions { cancel: Option<CancelToken>, timeout: Option<Duration>, grace /* 2 s */, capture_limit /* 32 MiB per stream */,
                        drain_timeout /* 2 s */ }
pub struct RunOutput { status: ExitStatus, stdout: Vec<u8>, stderr: Vec<u8>, stdout_truncated, stderr_truncated }  // success(), code(), *_text()
pub struct Line { stream: Stream::{Stdout, Stderr}, bytes: Vec<u8>, end: LineEnd::{Newline, CarriageReturn, None} }
pub struct ChildHandle;      // id, pid, label, kind, info, killer() -> Killer (weak, cloneable), take_stdin/stdout/stderr (raw pipes),
                             // lines(capacity) -> Receiver<Line>, try_wait, wait, wait_timeout, terminate(grace), kill()
pub enum ProcessError { NotFound{program}, Spawn{program, source}, Cancelled, TimedOut{after}, ShuttingDown, Io }
```
* **Streaming.** Both pipes are read on their own threads (no deadlock on large output either side, stdin bytes are
  written concurrently); lines split on `\n`, `\r\n` and lone `\r` (git progress updates, delivered at once; lines over
  64 KiB are split) and delivered in arrival order through a channel bounded to 256 lines on the calling thread; raw
  bytes are captured exactly up to `capture_limit`.
* **Stopping.** Cancel / timeout / `terminate(grace)` / `shutdown(grace)`: close stdin, `SIGTERM` to the group (Unix),
  then after `grace` `SIGKILL` to the group / `TerminateJobObject`, and reap. Dropping a `ChildHandle` whose process still
  runs hard-kills the tree. When the main process exits, the rest of its group is killed immediately (unless
  `kill_tree_on_exit(false)`, used for git network commands so credential-cache daemons and ssh `ControlPersist`
  masters survive, as in a terminal); after that no signal is ever sent to that group id again (no pid-reuse races).
  Windows has no console-less SIGTERM: closing stdin is the polite step. A crash of Lector itself does not kill
  children (Unix sessions are detached; the Job Object is created without kill-on-close): children see EOF on stdin.
  **The app must call `Supervisor::global().shutdown(grace)` when it quits.**
* **Askpass** (`lector_process::askpass`). `AskpassServer::start(runtime_dir, handler)`: Unix domain socket `s` in a fresh
  `0700` directory `lector-askpass-<random>` under `runtime_dir` (temp dir if the path would exceed `sun_path`), socket
  `0600`; loopback TCP on Windows (`start_tcp`, also used in tests). A 256-bit random token per server. Protocol v1:
  length-prefixed frames (u32 BE, ≤ 64 KiB): `"LECTOR-ASKPASS/1"`, token, prompt → `b'A'`+answer | `b'C'`; wrong token =
  closed without reply (constant-time compare); ≤ 8 prompts at once; a panicking handler = cancel. `handler:
  Fn(PromptRequest { text, kind: PromptKind::{Username, Password, Passphrase, HostKeyConfirm, Other} }) -> PromptReply::
  {Answer(Secret), Cancel}` runs on a connection thread and may block while the user answers. `classify_prompt(text)`.
  `server.helper_program(current_exe)` creates the helper program (once) in the server's private directory (also created
  on Windows); `server.env(helper)` = `GIT_ASKPASS`/`SSH_ASKPASS` = helper, `SSH_ASKPASS_REQUIRE=force`,
  `GIT_TERMINAL_PROMPT=0`, `LECTOR_ASKPASS_ENDPOINT`, `LECTOR_ASKPASS_TOKEN` (`credential.helper` untouched: helpers
  answer first). `run_askpass_client(prompt) -> i32` (prints the answer + `\n`; 0 answered, 1 cancelled, 2 error; the
  token must be 64 hex digits, `is_valid_token`). `askpass_helper_path(current_exe)` prefers `lector-cli.exe` next to
  the GUI `lector.exe` on Windows. Both binaries call `lector_cli::run_askpass_helper(&args)` first thing in `main`.
  `Secret` zeroes itself and prints `Secret(***)`; frames holding tokens or answers are zeroed; prompts are logged by
  kind only.
* **Helper-mode trigger (0.7, hardening).** The environment alone never selects the helper mode: a git hook that runs
  `lector <path>` during a sync inherits git's environment (askpass variables included) and must open a window as usual.
  `askpass_prompt_from_args(&args)` selects it only on an explicit marker an ordinary invocation cannot carry:
  * **Unix**: the helper program is a generated `#!/bin/sh` shim `lector-askpass` (mode `0700`, `create_new`) in the
    server's `0700` directory: `exec '<lector exe>' --lector-askpass-helper "$@"` (exe path single-quoted). The hidden
    first argument `HELPER_FLAG` selects the mode; the prompt follows.
  * **Windows**: OpenSSH runs `SSH_ASKPASS` as an executable without extra arguments, so the helper is a hard link (a
    copy when linking fails, e.g. across volumes) of the console binary named `lector-askpass.exe` in the private
    directory; the mode is selected iff the executable's own file name (`current_exe` or `argv[0]`) is `HELPER_NAME`
    (`lector-askpass`). git runs `GIT_ASKPASS` through Git for Windows' `sh`, OpenSSH directly; both work.
  * Trust model: anyone able to run the helper still needs the per-server 256-bit token and the endpoint (a `0700`
    directory + `0600` socket on Unix; loopback TCP + token on Windows); the helper, socket and directory are removed
    when the server stops (`lector-ui-git` stops it on app quit). The token and endpoint only ever go into the
    environment of git network commands (`SyncOptions::env` via `SyncHost`), never into Lector's own environment.
  * **Scrubbing**: `askpass::SCRUB_ENV` = `GIT_ASKPASS`, `SSH_ASKPASS`, `SSH_ASKPASS_REQUIRE`, `GIT_TERMINAL_PROMPT`,
    `LECTOR_ASKPASS` (pre-0.7.1 flag), `LECTOR_ASKPASS_ENDPOINT`, `LECTOR_ASKPASS_TOKEN`. `ProcessCommand` removes them
    from the inherited environment of `ProcessKind::{LanguageServer, Agent, Terminal}` children
    (`ProcessKind::scrubs_askpass_env`; variables set explicitly with `.env()` are kept). Terminals spawned outside the
    supervisor must add the same names to their own scrub list (`SpawnOptions.env_scrub`).
* Tests: `tests/process.rs` (env/cwd, arrival order with `\r` updates, 3 MB on stderr then stdout, 2 MB stdin echo,
  bounded capture, cancel and timeout kill grandchildren (`sh -c 'sleep 1000 & wait'`), graceful TERM trap, drop kills
  the tree, leftovers killed unless `kill_tree_on_exit(false)`, shutdown, killer), `tests/askpass.rs` (TCP and Unix round
  trips, token rejection, 0700/0600 permissions and cleanup, concurrent prompts, panicking handler),
  `tests/askpass_git.rs` (`harness = false`: the test binary is the helper; real `git credential fill` gets username and
  password through the server; cancel makes git fail). Unix-only tests are `cfg(unix)`; Windows runs `cmd /C` basics.

### git sync (v0.7) — lector-git `sync` + branch API, lector-app `commands::git_sync` — as implemented
All through the git CLI (`git -C <workdir> --no-pager -c core.quotepath=off`, `LC_ALL=C`, `GIT_TERMINAL_PROMPT=0`,
`GIT_EDITOR=true`) under `lector-process` (new session, cancellable). `lector-git` depends on `lector-process`; its
blame / `log -L` / `--follow` runner (`cli.rs`) also runs under the supervisor now (`write.rs` and the shadow repo keep
plain `std::process` so tty-based signing pinentries behave as before).
```rust
pub struct SyncOptions { env: Vec<(OsString, OsString)> /* askpass env */, cancel: Option<CancelToken>, timeout: Option<Duration>,
                         supervisor: Option<Supervisor> }                       // Debug prints env names only
pub enum SyncEvent { Progress(Progress { phase, remote, percent, current, total, bytes, throughput, done }), Message(String) }
impl Repo {
    pub fn fetch(&self, &FetchOptions { remote: Option<String> /* None = --all */, prune }, &SyncOptions, on: &mut dyn FnMut(SyncEvent))
        -> Result<FetchOutcome { updated: Vec<RefUpdate { name, old, new }> }>;          // refs/remotes + refs/tags diff
    pub fn pull(&self, PullMode::{FastForwardOnly /* --no-rebase --ff-only */, Rebase}, &SyncOptions, on)
        -> Result<PullOutcome { status: PullStatus::{UpToDate, FastForwarded, Rebased, Conflicts}, branch, upstream, before, after,
                                incoming, rebased, conflicts }>;                         // never an implicit merge commit
    pub fn push(&self, &PushOptions { remote, branch, set_upstream /* true */, force_with_lease }, &SyncOptions, on)
        -> Result<PushOutcome { remote, branch, remote_branch, set_upstream, refs: Vec<PushedRef>, up_to_date }>;
    pub fn upstream_status(&self) -> Result<UpstreamStatus { branch, detached, upstream, remote, push_remote, upstream_gone, ahead, behind }>;
    pub fn push_remote_for(&self, branch) -> Result<Option<String>>;   // pushRemote > remote.pushDefault > branch remote > only remote / origin
    pub fn create_branch(&self, name, start: Option<&str>) -> Result<Oid>;
    pub fn switch_branch(&self, name, &SyncOptions) -> Result<BranchSwitch { from, from_oid, to, oid }>;
    pub fn rename_branch(&self, old, new) -> Result<()>;
    pub fn delete_branch(&self, name, force: bool) -> Result<DeletedBranch { name, oid, upstream }>;
    pub fn set_upstream(&self, branch, upstream: Option<&str>) -> Result<Option<String> /* previous */>;
    pub fn branch_tip(&self, name) -> Result<Option<Oid>>;  pub fn branch_upstream(&self, name) -> Result<Option<String>>;
    pub fn restore_ref(&self, refname, expected: Option<Oid>, target: Option<Oid>) -> Result<()>;   // undo; checked-out branch: reset --keep
    pub fn stash_push(&self, &StashPushOptions { message, include_untracked }) -> Result<Option<Stash>>;   // None = nothing to stash
    pub fn stash_apply(&self, index, pop: bool) -> Result<StashApplied { stash, conflicts, dropped }>;
    pub fn stash_drop(&self, index) -> Result<Stash>;  stash_store(oid, message);  stash_at(index);  stash_index_of(oid);  stash_paths(index);
}
pub fn parse_progress(&str) -> Option<Progress>;  parse_push_porcelain(&str) -> Vec<PushedRef>;  classify_failure(&str) -> Option<SyncErrorKind>;
pub fn redact_credentials(&str) -> String;        // https://user:token@host → https://***@host (all surfaced messages)
// Error::Sync(Box<SyncError { kind, message, hint, paths, hook_output, alternatives }>), Error::Cancelled, Error::TimedOut
pub enum SyncErrorKind { NoUpstream, NoRemote, DetachedHead, Rejected /* hint "Pull first" */, StaleLease, AuthenticationFailed,
    HostKeyUnknown, HostKeyChanged, NetworkUnreachable, RemoteNotFound, HookDeclined /* hook_output */, Diverged,
    LocalChanges /* paths; alternatives ["stash_and_switch"] on switch */, DirtyWorktree, NotMerged, InvalidName, AlreadyExists,
    NotFound, RefChanged, Other }
```
* Push: explicit refspec `refs/heads/<b>:refs/heads/<remote branch>` (upstream's branch when pushing to the upstream
  remote, else the same name), `--set-upstream` only when none is configured, `--force-with-lease=refs/heads/<rb>:<last
  fetched oid>` (empty = must not exist) only on request; plain force is never offered. `--porcelain` decides the outcome
  (`!` + `stale info` → `StaleLease`, `remote rejected`/`hook declined` → `HookDeclined`, other `!` → `Rejected`); a local
  `pre-push` failure (no ref lines) → `HookDeclined` with its output. Network commands use `kill_tree_on_exit(false)`.
* Conflicts: a rebase pull that stops is `PullStatus::Conflicts` (the repo is in the rebase state the existing conflict
  flow / `git.continue_operation` / `git.abort_operation` handle); a stash pop/apply that conflicts reports `conflicts`
  and (pop) keeps the stash.
* **Commands** (`lector_app::commands::git_sync`, JSON schemas; reads: `git.upstream_status {}`, `git.list_remotes {}`
  (URLs redacted), `git.stash_list {}`; writes, journaled): `git.fetch {remote?, prune?}` (not undoable), `git.pull {mode?:
  ff_only|rebase}` (undo → `git.restore_ref` = branch back, `reset --keep`), `git.push {remote?, branch?, set_upstream?,
  force_with_lease?}` (not undoable), `git.create_branch {name, start_point?, switch?}`, `git.switch_branch {name,
  stash_changes?}` (the explicit "stash and switch"; on failure the stash is popped back), `git.rename_branch {name?,
  new_name}`, `git.delete_branch {name, force?}` (destructive, undo restores tip + upstream), `git.set_upstream {branch?,
  upstream: string|null}`, `git.stash_push {message?, include_untracked?}` (undo = pop), `git.stash_apply` /
  `git.stash_pop {index?|oid?, allow_without_backup?}` (files + index entries of the stash's paths backed up in the
  shadow repo first; undo = `Multi[RestoreIndex, RestoreFiles(, git.stash_store)]`), `git.stash_drop {index?|oid?}`
  (destructive, undo = `git.stash_store`), `git.stash_store {oid, message?}`, `git.restore_ref {ref, expected, target,
  upstream?}` (refuses unless the ref is at `expected`; itself undoable). All are `writes` (MCP lists them only with
  `--allow-writes`). Errors keep their type: downcast `CommandError::Failed(anyhow)` to `lector_git::Error::Sync`.
* **`SyncHost`** (`SyncHost::of(&core)`, an `AppCore` extension): `set_askpass_env(Vec<(OsString, OsString)>)` — the UI
  starts one `AskpassServer` per process (handler = its prompt dialog) and installs
  `server.env(&server.helper_program(&current_exe()?)?)` here; `subscribe(cb: Fn(&SyncProgress { op, repo, command, event }))` → `SyncSubscription` (drop =
  unsubscribe; called on worker threads); `running() -> Vec<RunningSync { id, repo, command }>`; `cancel(id)`,
  `cancel_repo(workdir)` (kills git, ssh and helpers).
* Tests: `crates/lector-git/tests/sync.rs` (bare remote + two clones: fetch incl. tags and prune, ff pull + restore_ref,
  diverged → rebase without merge commit, rebase conflicts → abort, no upstream / dirty tree, push ff / up to date /
  rejected, set upstream, force-with-lease ok + stale, remote and local hooks, ssh auth / host key / DNS failures via
  `GIT_SSH_COMMAND` injected through `SyncOptions::env`, deterministic cancel mid-fetch kills the fake ssh, branches,
  stash round trip + conflicting pop), unit tests on captured progress (`\r` updates) / porcelain / classification /
  redaction; `crates/lector-app/tests/git_sync.rs` (spec flags, fetch + pull + journal undo, typed errors, rebase + push,
  branch create/switch/stash-and-switch/rename/delete/upstream undo, stash push/drop/pop undo, env injection via `SyncHost`).

### lector-terminal (v0.7) — as implemented
UI-free core of the integrated multi-tab terminal. No internal dependencies, no GPUI, no tokio,
`#![forbid(unsafe_code)]`. Building blocks: `portable-pty` 0.9 (openpty / ConPTY, sync) for the PTY and
`alacritty_terminal` 0.26 (default features off) for the VT parser, grid, scrollback and reflow. alacritty's own
`tty`/`event_loop` modules are not used (they own the PTY and read loop; portable-pty does that). **No alacritty or
portable-pty type appears in the public API**: snapshots, colors, cursor, modes, points and events are crate types, so
`lector-ui-terminal` depends on `lector-terminal` only.
```rust
// Session (one per tab). Dropping it kills the child's process tree.
Terminal::spawn(SpawnOptions, wake: Option<WakeFn /* Arc<dyn Fn() + Send + Sync> */>) -> Result<Terminal, TerminalError>
term.snapshot() -> Snapshot                // one per frame; re-arms Wakeup
term.try_event() / drain_events() / recv_event_timeout(d) -> TerminalEvent
term.lock() -> EmulatorGuard               // Deref<Target = Emulator>: selection, search, links, marks, scroll
term.send_key(Key, Modifiers) -> bool; send_text(&str); paste(&str); write(bytes)   // never block
term.send_mouse(MouseEvent) -> bool        // false: the app does not want it, the UI handles it (select/scroll)
term.set_focused(bool); scroll(ScrollAmount); resize(TermSize) -> Result<..>; kill()
term.cwd() -> PathBuf; resolve_link(&LinkTarget, &AtomicBool) -> Option<LinkTarget>
term.exit_info() -> Option<ExitInfo { code, signal, success }>; is_running(); pid()
SpawnOptions { profile: ShellProfile { name, program, args, env }, cwd, env, size, scrollback /*10_000*/,
               shell_integration: Option<PathBuf /*dir under data_dir()*/>, env_scrub, scroll_to_bottom_on_input }
ShellProfile::default_for_platform()       // Unix $SHELL → /bin/bash → /bin/sh (login on macOS);
                                           // Windows pwsh.exe → powershell.exe → cmd.exe on PATH
enum TerminalEvent { Wakeup, TitleChanged(Option<String>), Bell, CwdChanged(PathBuf), PromptStarted, CommandStarted,
  CommandOutputStarted, CommandFinished { exit_code }, ClipboardCopy { text, primary_selection },
  ClipboardReadDenied, Exited(ExitInfo) }
// Headless core (also usable directly; tests feed bytes without a PTY)
Emulator::new(EmulatorConfig { size, scrollback, word_separators, initial_cwd }); feed(&[u8]);
  snapshot() -> Snapshot; take_events(); take_responses() -> Vec<u8>; resize(TermSize); scroll(ScrollAmount);
  modes() -> TerminalModes; start_selection(SelectionKind{Simple,Word,Line,Block}, GridPoint, Side);
  update_selection(..); select_all(); clear_selection(); selection_span(); selection_text() -> Option<String>;
  set_search(&SearchQuery { pattern, regex, case_sensitive }) -> Result; search_next(Direction) -> Option<SearchMatch>;
  visible_matches(); link_at(GridPoint) -> Option<LinkCandidate>; visible_links(); command_blocks() -> Vec<CommandBlock>;
  command_text(&CommandBlock); scroll_to_command(Direction); encode_key/encode_paste/encode_mouse/encode_focus;
  viewport_point(row, col) -> GridPoint; set_query_colors(QueryColors); cwd(); title(); line_text(i32); screen_text()
Snapshot { seq, size, rows: Vec<Arc<SnapshotRow { cells: Vec<Cell>, wrapped }>>, damage: Damage::{Full, Rows(Vec<u32>)},
           cursor: CursorState { row, col, shape, visible, blinking, wide }, display_offset, history_size,
           selection: Option<SelectionSpan>, modes: TerminalModes }
Cell { ch, zerowidth, width: CellWidth::{Narrow, Wide, WideSpacer, LeadingSpacer},
       style: CellStyle { fg, bg: TermColor, attrs: Attrs(BOLD|ITALIC|DIM|INVERSE|HIDDEN|STRIKETHROUGH),
                          underline: UnderlineStyle, underline_color, hyperlink: Option<Arc<Hyperlink { id, uri }>> } }
SnapshotRow::runs() -> Vec<TextRun { col, cols, text, style, has_wide }>   // equal styles coalesced, all columns covered
TermColor::{Foreground, Background, Cursor, BrightForeground, DimForeground, Indexed(u8), DimIndexed(u8), Rgb(r,g,b)}
LinkCandidate { start, end: GridPoint, text, target: LinkTarget::{Url(String), File { path, line: Option<u32>, col }},
                source: LinkSource::{Hyperlink, Detected} }
CommandBlock { id, prompt_line: i32, command_start: Option<GridPoint>, output_line, end_line, exit_code, truncated }
```
- **Coordinates.** `GridPoint { line: i32, col: u32 }`: line 0 is the top of the live screen, negative lines are
  scrollback (alacritty's convention; scroll position does not change them, output scrolling does). Snapshot rows are
  viewport rows `0..rows`; `viewport_to_grid` / `grid_to_viewport` convert. File-link `line`/`col` are 0-based `u32`.
- **Threading.** Reader thread (blocking 64 KiB reads) → bounded channel (32 chunks) → parser thread (feeds the
  `Emulator` under a mutex, ≤ 512 KiB per lock hold, also wakes for expired `?2026` synchronized updates and the exit
  drain) → events channel (bounded, lossy) + wake callback. Waiter thread reports the exit status (`Exited` after the
  PTY drained or 250 ms). Writer thread drains an unbounded input queue (pastes of MBs never block the UI). Emulator
  answers to DA/CPR/OSC 10-12/4 color/size queries are queued to the writer. Under a flood the bounded channel blocks
  the *child*, so memory is bounded by the channel plus the scrollback; `Wakeup` is sent once until the next
  `snapshot()`.
- **Damage.** alacritty's line damage; full damage on resize, display scroll, selection change, screen switch, erase,
  reset. Undamaged rows are the previous snapshot's `Arc`s.
- **Shell integration.** OSC 7 (`file://host/path`, percent-decoded; other hosts ignored) and OSC 133 A/B/C/D (also
  VS Code's 633 A-D) are read by a small scanner running beside the parser, correct across chunk boundaries; it also
  tracks modifyOtherKeys (`CSI > 4 ; n m`) and X10 mouse (`?9`), which alacritty does not. Marks are stored as absolute
  line numbers: a probe selection parked far above the grid is rotated by alacritty on every scroll, and its movement
  is accumulated into a monotonic scroll counter (erase-display/erase-scrollback/screen switches/reset are split out
  so the probe is read just before them). Blocks above the oldest scrollback line are dropped; on resize marks are
  re-anchored by counting logical (soft-wrapped) lines from the cursor. Opt-in snippets for bash, zsh, fish and
  PowerShell are embedded (`shell::*_INTEGRATION`); injection writes only to the given directory: zsh via a `ZDOTDIR`
  shim, fish via an `XDG_DATA_DIRS` `vendor_conf.d` entry (both restored inside the shell), bash via `--rcfile` (a
  file sourcing the system bashrc, `~/.bashrc`, then the snippet; only for interactive non-login bash without
  arguments). PowerShell/cmd: no clean mechanism, no injection (users dot-source the snippet).
- **Environment.** `TERM=xterm-256color`, `COLORTERM=truecolor`, `TERM_PROGRAM=Lector`, `TERM_PROGRAM_VERSION`;
  `DEFAULT_ENV_SCRUB` removes `LECTOR_*`, `COLUMNS`/`LINES` and other terminals' identity variables.
- **Process lifetime.** Drop/kill: Unix collects every process of the shell's session (`/proc` on Linux, `ps`
  descendants elsewhere) while the shell is alive, sends SIGHUP+SIGCONT to them and their groups, and SIGKILLs
  survivors after 500 ms on a reaper thread (drop never blocks). Windows: `taskkill /T /F` then `TerminateProcess`.
- **Security.** Nothing received from the program is executed. OSC 52: copy reported as `ClipboardCopy` (UI decides),
  read denied (`ClipboardReadDenied`; alacritty `Osc52::OnlyCopy`). Titles: control and bidi-override characters
  stripped, 256 chars max. Link targets are untrusted data (the UI asks before opening); file links are resolved
  (exists check, `a/`/`b/` diff prefixes) only on demand for the hovered/clicked candidate, cancellable. Bracketed
  paste strips embedded `ESC[200~`/`ESC[201~` repeatedly; newlines become CR.
- **Measured.** Parse throughput (release, 2-core sandbox, `cargo bench -p lector-terminal`): 115 MiB/s plain text,
  69 MiB/s mixed (SGR, CJK, OSC 133/7 every few lines). 20 MB `yes | head` through the PTY with bounded RSS.

### lector-ui-terminal (v0.7) — as implemented
GPUI front end of the integrated terminal, on top of `lector-terminal` (depends on `lector-ui`, `lector-terminal`,
`lector-config`, `lector-theme`; nothing depends on it but the binary). `lector_ui_terminal::init(cx)` (after
`lector_ui::init`) registers the keymap extension, the "Open in Terminal" hook, quit handling and, for every
`Workspace`, a `TerminalPanel` installed with `Workspace::set_bottom_panel`.
```rust
TerminalView::new(id, SpawnOptions, WeakEntity<Workspace>, window, cx)   // spawns on the background executor
  display_title(); cwd(); has_exited(); is_command_running(); last_command_failed(); kill()
  copy(cx) -> bool; paste(window, cx); select_all; clear; scroll(ScrollAmount); scroll_to_command(Direction)
  open_find / find_next(Direction) / close_find               // events: TerminalViewEvent::{TitleChanged, Activity,
                                                              //   Bell, Exited, CloseRequested, Focused}
TerminalPanel: new_terminal(CwdRequest::{Explicit(path), Default}, profile: Option<&str>, split: Option<SplitAxis>, ..)
  toggle; kill(view) (asks when a command runs); activate_relative; focus_split_relative; start_rename;
  choose_profile; saved_tabs() / restore(Vec<SavedTab>); active_terminal(); terminals()
TerminalItem::new(view)                // a terminal as an editor-area PaneItem (item_key "terminal:<id>", no session)
panel_for(workspace_id, cx); kill_all(cx)
// Pure, tested modules: config (profile/cwd/spawn options), keys (translation + pass-through bindings),
// colors (TerminalPalette per theme), grid (grid_size, cell_at, RowCache, merge_backgrounds), model (PanelModel),
// routing (route_url, route_file, paste_needs_confirmation), session (SavedTab load/store).
```
- **Frame scheduling.** The core's wake callback (parser thread) only flips an `AtomicBool` and, once, sends on an
  unbounded channel; a GPUI task drains it, drains the session's events (title, cwd, OSC 133, exit; bounded) and
  calls `cx.notify()`. The element takes **one** `snapshot()` per frame and only when woken (or resized). The core
  sends no further `Wakeup` until the next snapshot, so a terminal that is not painted (inactive tab, collapsed
  panel, background editor tab) never snapshots and costs nothing however much it prints; it raises
  `has_new_output` (activity dot) once. Output volume never turns into UI-thread work beyond one frame.
- **Rendering.** Cell size = the font's em advance × `round(font_size × line_height)` (terminal font settings, else
  the code font, ligatures off; the code font is not UI-scaled). `grid_size(bounds)` → `Terminal::resize`, at most
  one resize per 50 ms while dragging (the last size always lands). `RowCache<Rc<ShapedRow>>` keeps one entry per
  viewport row keyed by `Arc::ptr_eq` on the snapshot row + a style generation (theme, font): only rows whose `Arc`
  changed are re-shaped (the emulator also re-reports the cursor's row). A row is shaped per styled run
  (`SnapshotRow::runs`); runs with wide characters or whose advance would drift from the grid (fallback glyphs) are
  shaped per cell with `force_width`. Backgrounds are merged spans. Order: background, cell backgrounds,
  selection / search matches, failed-command bars (OSC 133 `D` ≠ 0, left edge), text, cursor (block with the glyph
  re-drawn in `cursor_text`, beam, underline, hollow when unfocused; blink off under reduced motion), hovered-link
  underline (Ctrl/Cmd), IME preedit, scrollbar thumb. Colors: `TerminalPalette::for_theme` — hand-picked 16-color
  palettes for the four built-in themes (contrast-tested, 7:1 in high contrast), derived from the editor colors for
  other themes; 256-color cube/greys and truecolor as is; inverse/dim/hidden applied; the palette is also given to
  the emulator for OSC 10/11/12/4 queries.
- **Key routing.** The terminal surface has key context `Terminal` (find bar: `TerminalFind`). A keymap extension
  (`lector_ui::keymap::add_keymap_extension`) adds, for every binding active in `Workspace`/globally, a
  `zed::NoAction` binding with the same keys in `Terminal` — so Ctrl+W/R/P/B/T, Tab, Alt+arrows... reach the shell —
  except `keys::RESERVED_ACTIONS` (command palette, F6 region cycling, Shift+F10, next/prev editor tab, zoom, UI zoom,
  the app menu), every `terminal::*` action, keys using the platform key (cmd/super) and keys the user bound in
  `Terminal` themselves. Unbound keys: plain text goes through the IME input handler (`EntityInputHandler`: dead keys,
  compose, preedit drawn at the cursor); the rest is translated by `keys::route_keystroke` (Ctrl/Alt combos → the
  core's encoder; Windows AltGr = Ctrl+Alt producing a character is typed; macOS Option types characters unless
  `terminal.option_as_meta`; Shift+PageUp/Down scroll locally off the alternate screen).
- **Mouse.** Drag selects (double-click word, triple-click line, Alt-drag block, Shift-click extends), wheel scrolls
  the scrollback or goes to the program when it asked for mouse reporting / alternate scroll; Shift forces local
  handling; right-click menu (Open Link, Copy, Paste, Select All, Find, Clear, Split, Rename, Move, Kill).
- **Security.** Lector never writes to a terminal on its own: no auto-run; "Open in Terminal" only sets the folder;
  Clear sends Ctrl+L to the shell only as the user's explicit command. Links: only `http`, `https`, `mailto` open
  (`routing::route_url`), anything else is ignored with a toast; file links are resolved by the core against the
  terminal's cwd on the background executor (cancellable) and open in Lector at the line (outside the project:
  read-only). Multi-line pastes into a program without bracketed paste ask first (`terminal.confirm_multiline_paste`,
  "don't ask again" for the session). OSC 52 copies land only while that terminal is focused; reads are denied by
  the core. The shell program/args, profiles and env are user-only settings (project layers are stripped, like
  LSP/agent commands). Agents get no terminal access.
- **Lifecycle.** On exit: "[Process exited with code N]" banner, the tab stays until a key or close
  (`terminal.close_on_exit`). Kill asks first only when shell integration reports a running command. App quit:
  sessions saved and every terminal killed (`kill_all`); dropping a view kills its process tree. Session restore
  (`terminal.restore`): panel tabs/splits and custom names per project root in `<data dir>/terminal/sessions.json`
  (≤ 12 tabs × 4 splits), fresh shells in the saved folders; editor-area terminals are not restored.
- **Settings** (`[terminal]`, `lector_config::TerminalSettings`): `shell`, `profiles`, `default_profile`, `env` (user-only),
  `font_family`, `font_size`, `line_height` (unset: code font, 1.2), `scrollback` (10000), `cursor_style`,
  `cursor_blink`, `copy_on_select`, `ctrl_c_copies_selection`, `option_as_meta`, `confirm_multiline_paste`,
  `new_terminal_cwd` ("project" | "active_file"), `shell_integration` (true), `restore` (true), `close_on_exit`.

### v0.4 algorithms (Agent ALGO; core crates) — as implemented
All UI-free, no new dependencies on higher layers (`regex-syntax` in lector-search, `regex` in lector-workspace).
Benchmarks: `cargo bench -p lector-diff --bench moves_bench`, `-p lector-search --bench search_bench|picker_gate`,
`-p lector-workspace --bench algo_bench`.

**Moved code — `lector_diff::moves`.**
`detect_moved(old, new, &LineDiff, min_lines: u32) -> Vec<MovedBlock { old: Range<u32>, new: Range<u32>, reindented: bool }>`,
`detect_moved_with(.., &MoveOptions { min_lines: 3, min_chars: 12, max_seed_occurrences: 64 })`,
`detect_moved_across(&[(&old, &new, &LineDiff)]) -> Vec<CrossFileMove { old_file, new_file, old, new, reindented }>`
(+ `_with`). Lines are compared by whitespace-insensitive hash; only lines inside hunks take part (removed vs added), a
block never crosses a hunk boundary, matching is one-to-one, greedy longest-first (split remainders are re-queued).
Blank and lone-brace lines are never anchors (blocks are trimmed to non-trivial edges); blocks below `min_lines` or
`min_chars` alphanumerics are dropped; seeds with > 64 occurrences are skipped. `reindented` is true when the exact
text differs only in leading whitespace. 13.5k lines / 113 hunks / 30 moves: 0.93 ms.

**Remapping — `lector_diff::remap`.**
`LineMap::{new(old, new, &diff) (incl. moves), from_diff, with_moves(&diff, &[MovedBlock]), map(old_line) -> Mapped,
map_back(new_line) -> Option<u32>, new_line_count}`; `Mapped { Exact(u32) | Moved(u32) | Deleted { near: u32 } }`
with `.line() -> Option<u32>`. Unchanged lines always map `Exact` (property test); lines inside a replaced hunk map to
`Deleted { near }` (the corresponding replacement line, clamped). `Anchor { line, text, context_before, context_after }`
(`Anchor::capture(text, line, context)`), `reanchor(&Anchor, new_text) -> Option<u32>` and
`reanchor_with(.., &ReanchorOptions { window: 120, min_confidence: 0.6, hint }) -> Option<Reanchored { line,
confidence, fuzzy }>`. Exact text + matching context wins first; otherwise fuzzy score
`0.6·text + 0.3·context + 0.1·proximity` (0.85/0.15 without context). `None` when below `min_confidence`, when two
candidates tie within 0.01, or when a trivial anchor (blank/brace) lacks context ≥ 0.5.

**Risk — `lector_workspace::risk`.**
`assess(&FileDiff, &LanguageRegistry) -> RiskReport { score: f32 /*0..1, 3 decimals*/, reasons: Vec<RiskReason { kind:
RiskKind, weight, message, line: Option<u32>, evidence: Option<Evidence { path, side: EvidenceSide::{Old|New},
lines: Range<u32> /*0-based, end exclusive*/ }> }> }` (+ `level() -> RiskLevel`, `top_reason()`, `summary()`,
`is_low_value()`), `rank_files(&[FileDiff]) -> Vec<(index, RiskReport)>` (descending; ties by churn, then index) and
`rank_files_with(.., langs)`. 0.7 additions: `RiskWeights` (explicit base weights of every signal and dampening
class, `Default` = fitted, `baseline()` = hand-set start, `NAMES`/`get`/`set` for fitting), `assess_with`,
`rank_files_weighted`, and the weight-independent split `collect_facts(&FileDiff, &LanguageRegistry, tests_changed) ->
RiskFacts` + `RiskFacts::score(&RiskWeights)` + `rank_facts(&[RiskFacts], &RiskWeights)` + `tests_changed_in(files)`
(cheap re-scoring for fitting). New `RiskKind`s: `AuthCheckRemoved`, `VerificationDisabled`, `WeakCrypto`,
`HardcodedSecret`, `SecretCompare`, `DestructiveMigration`, `ConcurrencyRemoved|Weakened|Added`, `DependencyRisk`,
`CiWeakened`, `ContainerWeakened`, `TestsDisabled`, `ConfigDefaultFlipped`. Score = noisy-or of raising reasons
(`weight = base weight x magnitude`, magnitude grows with counts) x product of (1 − damping) for generated/lock/
test-only/docs files. Public-API and signature changes come from the outline (tree-sitter, per-language export rules for
Rust, TS/JS, Python, Go, Java, Kotlin, Swift, C#, C/C++ (non-`static`), PHP, Ruby; data types compare whole definitions,
others their header; added exports are one reason). Other signals are regex tables on changed lines, each pointing at
its evidence: deleted conditionals / error handling (Rust `?`, `catch`/`.catch`, `except`, Go `err != nil`), added
`unwrap`/`expect`/`panic`, `unsafe`, eval/exec/shell (also shell strings built from interpolation, `sh -c`), SQL built
from strings; and, in `risk/signals.rs`: removed authorization checks, disabled TLS/signature verification, weak hashes
or removed hardening primitives, hard-coded credentials, secrets compared with `==`, destructive migrations
(`DROP`/`TRUNCATE`/`DELETE` without `WHERE`, `op.drop_column`), removed locks/`await`/memory ordering and weakened
thread safety, dependency manifests (new dependency, install-time script, git/URL/path/wildcard source, loosened
pin, `replace`), CI (`pull_request_target`, `write-all`, removed test/lint step), containers (root, remote `ADD`),
skipped tests, flipped boolean defaults (weighted by key polarity), plus security-sensitive path words, tests-vs-source,
churn and config. Deterministic; every point of the score is attributable to a reason.
Evaluation (`crates/lector-app/tests/risk_eval`, see its README): 60 hand-written, synthetic changesets (Rust, TS,
Python, Go, config), a fixed 70/30 tune/holdout split, top-1 / top-3 / MRR through `review_queue::analyze`, a
deterministic coordinate search for the weights (`examples/risk_fit.rs`) and a floor test on the holdout top-1.
Numbers and limits are in `docs/PERFORMANCE.md`: the set was written together with the signals, so it measures
regressions and coverage, not accuracy on real pull requests (`scripts/risk-eval-import.sh` imports those locally).

**Caches — `lector_workspace::cache`.**
`ContentCache::{new(CacheConfig), global(), syntax(id, ..), outline(id, &syntax, &text), tags(..), line_diff(old_id,
new_id, ..), stats() -> CacheStats { per-kind KindStats { hits, misses, evictions, entries, bytes, budget } }, reset_stats, clear}`.
Keys: `ContentId::{Blob(Oid), Hash(u128)}` (`of_bytes`, `of_text`; 128-bit non-cryptographic hash, 60 KB in 8 µs).
Thread-safe LRU per kind with byte budgets; values are `Arc`. `review::build_file_diff` / `prepare_side` use
`ContentCache::global()` for parsing and line diffs (public APIs unchanged). Syntax parse 3.3 ms vs hit 178 ns.

**Structural diff — `lector_workspace::structural`.**
`diff(old_syntax, old_text, new_syntax, new_text, &Budget) -> Option<StructuralDiff { old: Vec<Marked>, new: Vec<Marked>,
states, skipped: Vec<SkippedRegion { old: Range<usize>, new: Range<usize>, reason: SkipReason::{RegionBudget|TotalBudget|Time} }> }>`
and `diff_cancellable(.., &Budget, &lector_search::CancelToken)` (a cancelled token returns `None`);
`Budget { max_states: 400_000 (all regions), max_region_states: 60_000, max_time: 750 ms, max_nodes: 1M }`;
`Marked { range: Range<usize> /*bytes*/, mark: Mark::{Changed|Added|Removed|Moved} }`. Trees are lists (bracketed
nodes) and atoms (leaves, strings, comments) built from `Syntax::flatten`, each with a structural hash. The two trees
are first reduced level by level: identical prefixes and suffixes are trimmed, then the child sequences are aligned by
*anchors* (hashes that occur once on each side, found with a sibling-hash diff), so unchanged functions are skipped in
O(1) each and the expensive search only runs inside the regions between anchors (and inside paired lists of the same
kind). Each region runs Dijkstra over (left cursor, right cursor, entered-list stack) with difftastic-like costs
(unchanged 1 + depth penalty, enter both lists 1, replace string/comment 150, novel node 300) under its own
`max_region_states`; small same-shape runs are paired positionally when the search runs out. A region that exceeds its
budget (or the total state/time budget) is *not* an error: it is reported in `skipped` and marked from the line diff
refined with the word diff (`lector_diff::refine_inline`), so the result never hides a change and is never worse than
the line diff, while every other region stays structural. `None` only when the trees exceed `max_nodes`, the token is
cancelled or no region could be solved: callers show the line diff. `Moved` = fully novel subtrees with identical
structure on both sides (>= 3 leaves, >= 12 bytes). Adjacent marks merge across whitespace. Invariants (property
tests): reformatting marks nothing; what is not marked is identical on both sides, in order; marks are sorted,
disjoint and in bounds, also when regions fall back. 3 000 lines with ~50 changed lines: 11-15 ms (line diff 0.2 ms), 10 000
lines 36-42 ms, 500 lines 2-3 ms (`cargo bench -p lector-workspace --bench structural_bench`); before 0.7 files of
a few hundred lines with a small edit exceeded the budget after 150-250 ms. UI note: `review_layer.rs` still treats
`None` as "too complex" and can read `skipped` (non-empty = part of the diff is line/word based) to say so.
`lector-syntax` gained `Syntax::flatten(max_nodes) -> Option<Vec<TreeNode>>` (additive).

**Risk — `lector_workspace::risk`.**
`assess(&FileDiff, &LanguageRegistry) -> RiskReport { score: f32 /*0..1, 3 decimals*/, reasons: Vec<RiskReason { kind:
RiskKind, weight, message, line: Option<u32> }> }` (+ `level() -> RiskLevel`, `top_reason()`, `summary()`),
`rank_files(&[FileDiff]) -> Vec<(index, RiskReport)>` (descending; ties by index) and `rank_files_with(.., langs)`.
Score = noisy-or of raising reasons × product of (1 − damping) for generated/lock/test-only/docs files. Public-API and
signature changes come from the outline (tree-sitter, per-language export rules; data types compare whole
definitions, others their header). Other signals are regex tables on changed lines (deleted conditionals / error
handling, added `unwrap`/`expect`/`panic`, `unsafe`, eval/exec/shell, SQL built from strings), security-sensitive path
words, tests-vs-source, churn, config. Deterministic; every point of the score is attributable to a reason.
Heuristic: validated on synthetic snippets, not on real changesets.

**Caches — `lector_workspace::cache`.**
`ContentCache::{new(CacheConfig), global(), syntax(id, ..), outline(id, &syntax, &text), tags(..), line_diff(old_id,
new_id, ..), stats() -> CacheStats { per-kind KindStats { hits, misses, evictions, entries, bytes, budget } }, reset_stats, clear}`.
Keys: `ContentId::{Blob(Oid), Hash(u128)}` (`of_bytes`, `of_text`; 128-bit non-cryptographic hash, 60 KB in 8 µs).
Thread-safe LRU per kind with byte budgets; values are `Arc`. `review::build_file_diff` / `prepare_side` use
`ContentCache::global()` for parsing and line diffs (public APIs unchanged). Syntax parse 3.3 ms vs hit 178 ns.

**Structural diff — `lector_workspace::structural`.**
`diff(old_syntax, old_text, new_syntax, new_text, &Budget { max_states: 400_000, max_time: 750 ms, max_nodes: 1M })
-> Option<StructuralDiff { old: Vec<Marked>, new: Vec<Marked>, states }>`; `Marked { range: Range<usize> /*bytes*/,
mark: Mark::{Changed|Added|Removed|Moved} }`. Trees are lists (bracketed nodes) and atoms (leaves, strings, comments)
built from `Syntax::flatten`. Identical prefixes/suffixes (and single matching list pairs) are trimmed, then Dijkstra
over (left cursor, right cursor, entered-list stack) with difftastic-like costs (unchanged 1 + depth penalty, enter both
lists 1, replace string/comment 150, novel node 300). `Moved` = fully novel subtrees with identical structure on both
sides (>= 3 leaves, >= 12 bytes). Adjacent marks merge across whitespace. `None` over budget: callers fall back to the
line diff. 99 lines: 0.4 ms; 374 / 699 lines with a small edit exceed the default budget (150 / 240 ms to give up).
`lector-syntax` gained `Syntax::flatten(max_nodes) -> Option<Vec<TreeNode>>` (additive).

**Trigram index — `lector_search::TrigramIndex`.**
`build(root, &[PathBuf], &CancelToken)` (parallel), `from_contents`, `update_file/update_bytes` (replaces postings),
`remove_file`, `refresh(&files) -> RefreshStats` (size/mtime sync), `compact`, `contains`, `file_count`,
`searchable_count/paths`, `trigram_count`, `candidates_literal`, `candidates_regex` (plan from regex-syntax HIR: AND for
concatenation, OR for alternation, no constraint for classes/optional/`(?i)`), `candidates(&SearchQuery) ->
Candidates::{All | Files(Vec<String>)}`, `to_bytes`/`from_bytes` (magic `LTRI`, `FORMAT_VERSION = 1`, varint
delta-coded postings, checksum, every read validated -> `TrigramError`). Case-folded (ASCII) trigrams; binary files and
files > `MAX_INDEXED_FILE_SIZE` are not searchable (never candidates). Candidates are a superset: run
`search_files(root, &[PathBuf], &SearchQuery, &CancelToken, sink)` (new, same events as `search_project`, no walk) on
them; an integration test checks equality with a full search. 3000 files / 16 MB: build 213 ms, index 1.6 MB,
rare literal 2 µs, regex 8-12 µs, update one file 70 µs.

**Fuzzy finder — `lector_search::Picker`** (API unchanged; added `PickerInjector::push_boosted(item, haystack, boost)`,
`MAX_RESULTS = 1000` and, in 0.7, `Picker::progress() -> PickerProgress { scanned, total, complete, ranked, current }`,
`Picker::drop_caches()` and `Picker::heap_bytes()`). nucleo (`Pattern`/`Matcher`) only does the scoring. Items live in
`picker_store`: chunks of 4096 haystacks in one byte arena each, with dense side arrays (end offsets, a 128-bit
character mask pair "occurs anywhere" / "occurs at a word boundary", boost) and a lock-free table of sealed chunks, so
the scan reads sequentially and `matches()` hands out `&T` without `unsafe`. Memory: 22 bytes per path on top of the
path text and the item (62 MB for 1 M paths of 36 bytes). `picker_plan` classifies the query: single-atom fuzzy
queries run a *fast* mode (mask rejection, then an upper bound of the nucleo score from the match length, the
boundary bonus table of the config and the masks; candidates that cannot beat the current k-th best are not scored),
other syntax (`^`, `$`, `'`, `!`, several atoms, non-ASCII needles) a *masked* or *slow* mode that is exactly nucleo.
Results are identical to a brute-force nucleo pass (property tests). A pass has two phases: *rank* (top-k heap keyed
(score, shorter haystack, lower index); `progress().ranked` means the visible page is final) and *count* (exact
`matched_count`, no scoring for items whose mask proves they match). The empty query orders by boost then insertion.
Completed passes are remembered (last 16 queries, with the end position of each match): typing narrows the previous
match set and extends stored match ends, backspace replays a remembered result without scanning, and every chunk
checks for cancellation, so a new keystroke abandons the old pass within one chunk. `tick` returns true until settled.
1 M paths on the 2-core VM, p50 / p95 in ms (settled = exact count and final ranking): cold 1-3 characters 8-41 / 10-49,
extend by one 8-38 / 10-45, backspace 0.1-3.3 / 0.2-5.5; 6 characters cold 17-30 / 20-40 on the uniform corpus and
15-59 / 16-69 on the varied one (misses: `parser` 55 and `src/ma` 69 ms p95; their first page is exact after 25 and 49 ms
p95), 6-character extension 6-30 / 8-36. See `docs/PERFORMANCE.md`.

### v0.5 / v0.6 — agents, CLI/MCP, code hosts, review UX (contracts)

**lector-agent (v0.5; UI-free).** Drives coding agents over the Agent Client Protocol
(ACP: JSON-RPC over the agent's stdio; agents: Gemini CLI `--experimental-acp`,
Claude Code via `claude-code-acp`, Codex via `codex-acp`, any ACP agent from settings).
```rust
pub struct AgentConfig { pub name: String, pub command: String, pub args: Vec<String>, pub env: Vec<(String, String)> }  // settings [[agent.servers]]
pub struct AgentSession;   // one conversation with one agent in one project; Clone handle
impl AgentSession {
    pub fn start(core: &AppCore, project: &Project, cfg: &AgentConfig) -> Result<AgentSession>;
    pub fn prompt(&self, text: String, context: Vec<PromptContext /* file, range, diff, comments */>) -> Result<TurnId>;
    pub fn cancel(&self); pub fn turns(&self) -> Vec<Turn>; pub fn subscribe(&self, cb) -> Subscription;
    pub fn answer_permission(&self, req: PermissionRequestId, choice: PermissionChoice);
}
pub struct Turn { pub id: TurnId, pub prompt: String, pub before: Option<ShadowSnapshot> /* None: checkpoints off / no repo */,
                  pub after: Option<ShadowSnapshot>, pub messages: Vec<AgentMessage>, pub tool_calls: Vec<ToolCall>,
                  pub plan: Vec<PlanEntry>, pub state: TurnState, /* + context, permissions, timeline, changed_files,
                  written, journal_id, undone, started, finished */ }
```
* Every turn is bracketed by `ShadowRepo::snapshot_worktree` checkpoints; the turn's changes are a
  `Changeset::Snapshots { from, to }` reviewable in the existing diff/review views; "Undo turn" restores
  the `before` snapshot through the journal (undoable itself).
* ACP file-system requests (`fs/read_text_file`, `fs/write_text_file`) go through `DocumentStore` so
  agent edits appear live in open tabs; writes are journaled with the turn id. Permission requests
  are shown to the user (allow once / always for this session / reject).
* Feedback loop: review comments (below) on a turn's changes → "Send to agent" builds a prompt with
  file/line anchors and the comment text.

**lector-agent as implemented (Agent AGENT).**
* *Protocol* (`rpc`, `session`): ACP v1 implemented directly on `serde_json` — newline-delimited JSON-RPC 2.0, one
  reader thread per agent process (spawned since 0.7 through `lector_process::Supervisor::global().spawn` with
  `ProcessKind::Agent`: its own process tree, killed whole on stop / restart / failed start and on app exit; askpass
  variables scrubbed from its environment), requests block their caller (`Connection::request(method, params, timeout)` /
  `start_request` → `PendingReply::wait`), agent→client requests each run on their own thread (a permission prompt
  blocks only itself), notifications are delivered in order. Malformed lines are counted and skipped, answers to
  unknown ids ignored, unknown methods answered `-32601`. No async runtime (the `agent-client-protocol` crate needs an
  executor). `initialize` advertises `fs.readTextFile/writeTextFile = true`, `terminal = false`; `session/new { cwd:
  project root, mcpServers: [] }` (an `-32000` with `authMethods` → `authenticate` with the first method, retried
  once); `session/prompt` sends the text block first, then the context (`PromptContext::{File, Range}` as embedded
  `resource` blocks when the agent reports `promptCapabilities.embeddedContext`, else fenced text; files over 256 KiB
  as `resource_link`; `Diff`/`Comments` as text). `session/update`: `agent_message_chunk` / `agent_thought_chunk` /
  `user_message_chunk` (merged per run), `tool_call` / `tool_call_update` (title, kind, status, locations (1-based →
  0-based), `diff` / `content` / `terminal` content), `plan` (replaces the plan); others ignored. `Turn::timeline`
  keeps the arrival order of messages, tool calls and permission requests.
* *Session* (`AgentSession::{start, start_with(.., SessionOptions { checkpoints, auto_allow_reads, request_timeout:
  60 s, turn_idle_timeout: 15 min, cancel_grace: 10 s }), prompt, cancel, turns, turn, last_turn,
  pending_permissions, answer_permission, wait_turn, restart, stop, undo_turn(id, force, Source), turn_changeset,
  subscribe → AgentEvent::{StatusChanged, TurnStarted, TurnUpdated, TurnFinished, PermissionRequested,
  PermissionResolved}, status → SessionStatus::{Starting, Ready, Busy(turn), Crashed(msg), Stopped}}`). One turn at a
  time (`AgentError::Busy`). The turn thread takes the "before" checkpoint, sends the prompt and waits in 100 ms slices:
  a `stopReason` ends it (`cancelled` or a pending cancel → `Cancelled`), an exited process → `Failed` with the last
  stderr lines (40 kept) and the session `Crashed` (`restart()` spawns a new process and ACP session, history kept),
  a cancel not confirmed within `cancel_grace` or no traffic for `turn_idle_timeout` ends the turn too. Cancel sends
  `session/cancel` and answers pending permission requests `cancelled`.
* *Files*: `fs/read_text_file` serves the open document's text (unsaved edits included) or the file on disk
  (`line`/`limit` honoured); `fs/write_text_file` goes through the open `DocHandle` (`replace_all` + `save`, no hooks /
  cleanup; refused when the tab is locked read-only or the file changed on disk) or `lector_fs::write_atomic` for files
  that are not open (parents created). Both only inside the project root (`paths::resolve_in_root`: `..`, absolute
  paths elsewhere, symlinks leading out — including dangling ones, whose target a write would create — and `.git/`
  compared case-insensitively, ignoring trailing dots/spaces and HFS+-ignored code points, are refused with an error to
  the agent). Each write is journaled as `agent.write_file` (`Source::Agent`, args `{session, turn, path}`); when the turn
  has no checkpoint, or the file is git-ignored (checkpoints leave those out), the file is backed up first
  (`snapshot_paths`) and the entry is undoable on its own.
* *Permissions*: `session/request_permission` becomes a `PermissionRequest` in the turn (and `pending_permissions()`);
  `AllowAlways` remembers the tool kind for the session; `auto_allow_reads` answers `read` requests whose locations are
  inside the project. `PermissionChoice` maps to the agent's option of kind `allow_once` / `allow_always` /
  `reject_once` (fallbacks: the other allow kind; reject without a reject option = `cancelled`).
* *Checkpoints and undo*: `snapshot_worktree` before and after each turn (also after failed / cancelled turns);
  `changed_files` = `ShadowRepo::changes_between`. A turn that changed files appends one journal entry `agent.turn`
  (`UndoRecord::RestoreFiles { shadow: before, paths: changed, after: fingerprints }`), so `undo_turn` is
  `Journal::undo(entry, Check|Force)`: `UndoError::Changed` → `AgentError::UndoConflict { paths }` (the UI offers
  "Restore anyway"); the undo is itself undoable (redo from the Activity Log), `Turn::undone` follows the journal, and
  "Undo last operation" picks the turn entry (newest).
* *`Changeset::Snapshots { from, to }`* (lector-git, as implemented): `Repo::changes` finds the shadow repository
  holding `from` among the shadows opened in this process (`ShadowRepo::open` registers them; `ShadowRepo::known_for
  (repo)`), sides are shadow blobs; `Repo::load(Side::Blob)` falls back to the known shadows when the user's object
  store lacks the blob, so `review::load_changeset*` / `load_file_diff*` and the review/diff tabs work unchanged
  (read-only: no hunk actions). Review key `snapshots:<from>..<to>` (`changeset_key` / `parse_changeset_key`); title =
  the "after" snapshot's note ("Agent turn 3: …", `ShadowRepo::known_note`).
* *Manager and provenance*: `AgentManager::of(&core)` (AppCore extension; `sessions`, `session_for(root)` = newest of
  the project, `start` / `start_with` (stops the project's previous session), `stop_all`, `subscribe`). It registers a
  `ProvenanceProvider` ("agent", `PRIORITY_AGENT`): for working-tree queries, the newest not-undone turn whose diff
  (before → after, `lector_diff`) added or changed the line and whose line is still as the turn left it (after →
  current text through `remap::LineMap`, Exact/Moved) explains it: title "<agent> turn N: <prompt>", detail = prompt.
* *Config*: `available_configs(&AgentSettings)` = the user's `[[agent.servers]]`, then `gemini --experimental-acp`,
  `claude-code-acp`, `codex-acp` when found on `PATH` (`PATHEXT` on Windows) and not configured already. Servers are
  read from the **user's** settings only (`load_settings()`): a project's `.lector/settings.toml` cannot add commands.
  Settings `[agent] checkpoints = true, auto_allow_reads = true, servers = [{ name, command, args, env }]`.
* *Commands* (`lector_agent::commands::register(&core)`, called by `lector_ui_agent::init`; CLI/MCP front-ends call it
  too): `agent.list_configs {}`, `agent.start {name?}`, `agent.prompt {text, files?, wait?, timeout_s?}` (with `wait`:
  the turn as JSON + `UiRequest::OpenChangeset`), `agent.cancel {}` (`ui_action` `agent::Cancel`), `agent.turns
  {limit?, turn?}`, `agent.undo_turn {turn?, force?}` (destructive), `agent.answer_permission {id, choice:
  allow_once|allow_always|reject}`, `agent.send_comments {text?, changeset?, wait?, timeout_s?}` (open
  `CommentStore` threads of the review key — default: focused changeset, else the last turn — as `Thread::to_prompt`,
  plus the typed text). Start/prompt/undo/answer/send are `writes = true` (MCP needs `--allow-writes`).
* Tests: unit (rpc framing and malformed input, timeline building, PATH discovery, path containment, option picking)
  and `tests/acp.rs` against `tests/fake_acp_agent.py` (a scriptable ACP agent: `say`, `think`, `plan`, `write`,
  `read`, `ask`, `slow`, `crash`, `garbage`, `stop`): streaming, writes through open documents, refusals outside the
  project / in `.git`, permissions (once / always / reject / automatic reads), cancel (streaming and during a
  permission prompt), crash + restart, start failures, context blocks (embedded / inline), checkpoints + changeset +
  review load + undo + redo, undo conflicts + force, commands, provenance.

**lector-ui-agent (v0.5; Agent panel).** `lector_ui_agent::init(cx)` (after `lector_ui_git::init`) registers the
commands and, per workspace with a folder, an `AgentPanel` in `SidebarPanel::Agent` (activity-bar icon `Sparkles`).
Header: agent picker (menu when several agents), status dot, restart (after a crash), new session. Conversation per
turn: "TURN n" + state, the prompt with its context labels, the timeline (Markdown answers via `markdown::parse` cached
per message length, collapsible thoughts, tool-call cards with kind icon / status / clickable locations / a mini diff of
the first 8 changed lines / text output, inline permission prompts — Allow once / Always / Reject — or their answer),
the plan checklist, "Working… Stop", errors (Restart agent), and a footer "N files changed · Review · Undo turn" with the
changed files (click opens); an undo conflict shows the edited files with "Restore anyway" / "Keep my edits". Prompt
box (`TextArea`, key context `AgentComposer`) with context chips: active file (`@file`), selection (`Range`), current
diff (`git.show_diff` of the active file, else the working tree), feedback. Session events are coalesced (30 ms) into
one refresh; the view sticks to the bottom while the user has not scrolled up. Actions (`lector_ui::actions::agent`):
`TogglePanel` (`cmd/ctrl-alt-i`), `NewSession`, `Cancel` (`escape` in `AgentPanel`), `ReviewLastTurn`
(`cmd/ctrl-alt-r`; `lector_ui_git::review_tab::open_review` with the turn's `Changeset::Snapshots`), `UndoLastTurn`
(`cmd/ctrl-shift-alt-z`), `SendFeedback` (`cmd/ctrl-alt-f`: the selection of the active file / diff / review code view
with its line numbers, plus the open review comments of the focused changeset, as a feedback chip; the panel opens with
the prompt box focused), `SendPrompt` (`cmd/ctrl-enter` in `AgentComposer`). Menu: View → Agent. Agents are stopped
on quit.
Seams added for it: `SidebarPanel::Agent` (lector-ui), `Workspace::ensure_sidebar_width(px, cx)` (the panel widens
the sidebar to 400 px when shown).

**lector-app review comments (v0.6, Agent FORGE).** `lector_app::comments::{CommentStore, Comment,
Thread, Anchor}` in the Db: threads anchored to (path, line, line text + context, changeset/blob id),
re-anchored on new revisions with `lector_diff::remap`; states open/resolved/outdated; origin
local | github | gitlab; drafts. Used by local review, PR review and the agent feedback loop.

**Provenance (v0.5, Agent REVIEWUX defines; AGENT implements a provider).**
`lector_app::provenance::{ProvenanceProvider, LineProvenance { kind: Commit | AgentTurn | Uncommitted, title,
detail, id }}` — "why is this line like this?" shown on hover over changed lines (commit message, or
the agent prompt + turn).

**lector-forge (v0.6; UI-free, blocking HTTP via ureq, no tokio).**
```rust
pub trait Forge: Send + Sync { fn kind(&self) -> ForgeKind; fn list_pull_requests(&self, state) -> Result<Vec<PullRequest>>;
    fn pull_request(&self, number) -> Result<PullRequest>; fn review_threads(&self, number) -> Result<Vec<ReviewThread>>;
    fn submit_review(&self, number, review: &ReviewSubmission /* event: Approve|RequestChanges|Comment, body, comments */) -> Result<()>;
    fn reply(&self, number, thread, body) -> Result<()>; }
pub fn detect(remote_url: &str) -> Option<ForgeRemote>;   // github.com, GitHub Enterprise, gitlab.com, self-hosted GitLab
pub fn auth_token(remote: &ForgeRemote) -> Option<Token>;   // env (GITHUB_TOKEN/GH_TOKEN/GITLAB_TOKEN), `gh auth token`, OS keychain
```
PR head/base commits are fetched into the **shadow repo** (never the user's repo) and reviewed as a
`Changeset::Range` there; inline comments come from `review_threads` and drafts from `CommentStore`.
Submitting a review is an explicit user action, irreversible → confirmation dialog.

**v0.6 review comments and pull requests — as implemented (Agent FORGE).**
* **`lector_app::comments`** (`CommentStore::{new(Db), for_core(&AppCore) /* extension slot */, threads(repo,
  &ThreadFilter { changeset, path, states, drafts_only, origin }), thread, add(NewThread), reply, add_remote_comment,
  edit_comment, set_resolved, mark_published, delete, delete_comment, find_remote, remember_text, reanchor_path,
  sync_changeset(&Repo, key, &Changeset), subscribe(cb) -> Subscription}`). `Thread { id, repo, anchor, original,
  state: Open|Resolved|Outdated, origin: Local|Github|Gitlab, remote_id, comments: Vec<Comment { id, author, body,
  created, updated, draft, remote_id }>, matched: AnchorMatch::{Original, Exact, Moved, Fuzzy, Lost}, confidence }`
  (+ `location()`, `summary()`, `to_prompt()`, `draft_count()`); `Anchor { path, side: Old|New, line, end_line,
  text, context_before/after (3 lines), changeset /* review key */, blob, content /* FNV hash of the revision */ }`.
  Review keys are changeset keys (`worktree`, `branch:main...feature`, …) or `pr:<host>/<path>#<n>` (stable across
  force-pushes and rebases). Stored as JSON in the Db `comments` table under `t\0<repo>\0<id>`
  (`Db::{next_comment_id, put_comment_thread, comment_thread, comment_threads}`); v0.4 single comments are migrated to
  threads on first read. Anchored file texts are remembered in the Db cache (`comments/text/<hash>`) so later
  revisions can be diffed against them.
* **Re-anchoring** (`reanchor_anchor(&Anchor, old_text?, new_text, min_confidence)`): with the old text, a line diff +
  move detection (`lector_diff::remap::LineMap`) gives `Exact` / `Moved`; rewritten or unknown → `remap::reanchor_with`
  (hint = mapped position) → `Exact` (identical line + context) or `Fuzzy` with its confidence; below 0.6 → `Lost`: the
  thread becomes `Outdated`, keeps its `original` anchor/snippet and retries from it on later revisions (re-attaches
  when the code comes back). Threads already on a revision (same content hash) are skipped.
* **Commands** (registered by `register_builtin`, `writes = false`: they only write Lector's db): `comments.list
  {changeset?, path?, state?, drafts_only?, all_changesets?}` (re-anchors the focused changeset first), `comments.add
  {path, line, end_line?, side?, body, changeset?, draft?, author?}`, `comments.reply {thread, body, draft?}`,
  `comments.resolve {thread, resolved?}`, `comments.delete {thread, comment?}`, `comments.export {changeset?, format?:
  markdown|json, include_resolved?}`. "Send to agent" in the review tab invokes `agent.send_comments {changeset,
  threads: [ids]}` when lector-agent registers it (otherwise the button is disabled with the reason).
* **`lector-forge`** (`ureq` 3 with rustls + the platform verifier, proxies from `HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY`,
  no proxy for loopback; `keyring` 3 with native backends): `detect` / `detect_with(url, &ForgeHosts { github, gitlab,
  api_urls })` → `ForgeRemote { kind, host, path, owner, name, api_base, web_base, remote_url }` (https, ssh, scp-like,
  git://; github.com → `api.github.com`, GHE → `/api/v3` + `/api/graphql`, GitLab → `/api/v4`; `github.*` /
  `gitlab.*` hosts are recognised too). `auth_token` = env (`GITHUB_TOKEN`, `GH_TOKEN`; GHE also
  `GH_ENTERPRISE_TOKEN`; `GITLAB_TOKEN`) → `gh auth token --hostname` / `glab auth status -t` → keychain (service
  `lector-forge`, account = host; `store_token` / `delete_token`); `Token`'s `Debug` is redacted and tokens are never
  logged. `connect(&remote, token) -> Box<dyn Forge>`; trait as above plus `remote()`, `viewer()`,
  `resolve_thread(n, thread, resolved)`, `fetch_refspecs(&pr)` (`+refs/pull/N/head` or `+refs/merge-requests/N/head`
  and the base branch into `refs/lector/pr/N/{head,base}`). GitHub: REST for pulls (filters: `Mine` = author is
  `/user`, `ReviewRequested` = in `requested_reviewers`), files, combined status (checks), `POST …/reviews`
  (event, body, inline comments with `line`/`side`/`start_line`, `commit_id`); GraphQL only for review threads
  (resolution state), replies and (un)resolving. GitLab: merge requests (`author_username` / `reviewer_username`
  filters), `/diffs` (fallback `/changes`), diff discussions (system / general notes skipped), review = one
  discussion per inline comment (text position with base/start/head sha) + summary note + `approve`; "request
  changes" is reported as unsupported in `SubmittedReview::notes`. Lines are 0-based in the API. `ForgeError::
  {Unauthorized { help }, RateLimited { reset }, NotFound, Http { status, message }, Network, Decode, Unsupported}`;
  pagination follows `Link: rel="next"` (≤ 10 pages). `git_auth_header(kind, &token)` (Basic, for git fetch),
  `review_key`, `parse_review_key`. `lector_forge::mock::MockServer` (std `TcpListener`, routes from
  `routes.json` + recorded JSON) backs the tests (`crates/lector-forge/tests/forge.rs`) and `cargo run -p
  lector-forge --example mock_forge -- <dir> <port>` for offline demos.
* **Shadow fetch** (`lector_git::ShadowRepo::{fetch(url, refspecs), fetch_with(.., &FetchOptions { http_header }),
  resolve(rev), merge_base(a, b)}`): `git fetch --no-tags --no-write-fetch-head --no-auto-gc` into the shadow with
  hermetic config, `fetch.unpackLimit=1` (objects stay packed, never freshening the alternates), `BatchMode` ssh,
  the auth header passed through `GIT_CONFIG_*` env (not argv) and redacted from errors; destinations must be under
  `refs/lector/` outside the snapshot namespace; URLs/refspecs starting with `-` are refused. A bare shadow can be
  opened with `Repo::discover(shadow.git_dir())` (alternates make the user's objects visible). Test: the user's `.git`
  is byte- and mtime-identical after fetches and refusals, and the PR head is not in the user's object store.
* **UI** (lector-ui-git): `diff_view/comments.rs` — a "+" gutter icon on the hovered line (`CodeView` gutter icons +
  `HoverMoved`), `review::AddComment` (selection → range comment), a modal `CommentComposer` (TextArea, markdown,
  Ctrl/Cmd+Enter), threads rendered between rows: `DisplayRow::Gap` rows reserve the height (estimated from the text)
  and a `RowWidget` draws the card (author, time, markdown body, Reply / Resolve / Delete, Pending / Outdated /
  Resolved / Re-anchored n% / GitHub badges, collapse; folds containing a thread open), scrollbar marks,
  `review::NextComment` / `PrevComment`. `DiffView::set_review_context(ReviewContext { key, repo /* shadow */, drafts,
  pr })`. `review_tab/comments.rs` — Files | Comments switch, the comments list (jump, "Copy as Markdown", "Send to
  agent"), pull-request header with "Submit review (n)". `pull_requests.rs` — `PrSession::open` (details, fetch into
  the shadow unless head and base are already reachable there, merge base, `Changeset::Range` read from the shadow
  `Repo`), `sync_threads` (remote threads → `CommentStore`, origin github/gitlab, anchored at head / merge base),
  `submit` (draft replies to remote threads are posted as replies; draft threads become one review's inline
  comments; published drafts are replaced by the host's threads), the **Pull Requests** sidebar panel
  (`SidebarPanel::PullRequests`; filters review requested / mine / all), `SubmitReviewDialog` (Comment / Approve /
  Request changes, summary, draft count) followed by a confirmation, and `review::SignIn` help. Actions
  `review::{AddComment (c in diffs), NextComment (alt-shift-n), PrevComment (alt-shift-p), ShowPullRequests
  (ctrl/cmd-alt-p), SubmitReview (ctrl/cmd-alt-enter), SignIn}`; menu Git › Review. `CodeView::text_width()` (new)
  sizes the cards.
* **Settings `[forge]`**: `github_hosts`, `gitlab_hosts`, `api_urls` (host → API base), `default_filter`
  (`review_requested`). Ignored (with a warning) in a project's `.lector/settings.toml`, so a repository cannot send
  your token to another server.
* **Update check (v0.7)**: `lector_forge::releases` (same ureq setup, no token) — see "Updates and signing (v0.7)".

**CLI and MCP (v0.5, Agent CLI).** Subcommands of the `lector` binary drive `AppCore` headlessly:
`lector status`, `lector changes [--json]`, `lector diff <changeset> [--json]`, `lector commands`,
`lector run <command-id> [--args JSON]`, `lector mcp` (Model Context Protocol server over stdio:
registry commands are MCP tools with their JSON schemas; read commands always available, write
commands only with `--allow-writes`, each journaled). No tokio: JSON-RPC handled directly.

### CLI and MCP (v0.5) as implemented

Two UI-free crates, layered `lector (bin) → lector-cli → lector-mcp → lector-app`; neither depends on GPUI or tokio.

* **`lector-mcp`** — `McpServer::new(AppCore, Option<Project>, ServerOptions { allow_writes, name, version })`,
  `serve(BufRead, Write + Send)` (the stdio loop; also what the tests drive with in-memory pipes), `handle(&Value) ->
  Option<Value>` (one message), `tool_name(command_id)` (`git.list_changes` → `git_list_changes`; any character outside
  `[A-Za-z0-9_-]` becomes `_`), `document_uri(rel_path)`, `LATEST_PROTOCOL_VERSION` / `SUPPORTED_PROTOCOL_VERSIONS`.
* **`lector-cli`** — library (`run`, `run_args`, `is_cli_invocation`, `launch_gui`, `parse_changeset`, `SUBCOMMANDS`) plus the
  console binary `lector-cli`. The `lector` binary calls `lector_cli::is_cli_invocation(args)` as the very first thing in
  `main` (first argument, after an optional leading `--color <when>`, is a subcommand name) and runs the CLI before any GUI
  code is touched; everything else is the GUI exactly as before (`lector ./status` opens a folder named `status`).
  The extra binary exists for Windows, where release builds of `lector.exe` have no console: see "Command line on Windows" in
  `docs/RELEASING.md` (installed as `lector-cli.exe` and `lector.com`; the latter, given a path instead of a subcommand, starts
  the GUI next to it). It avoids `AttachConsole`, so the workspace has no new `unsafe`.
  `lector_cli::session::open_core()` is where the headless registry is assembled (built-in commands come with `AppCore::open`;
  commands of other crates that should exist without a window are registered there). Language servers are disabled.
* **Subcommands**: `status [PATH]` (gix only, no core or database: ~10 ms), `changes [--staged | --unstaged | --branch BASE |
  --range A..B] [--no-risk] [-C DIR]`, `diff <changeset> [--file P] [--moved] [-U N]`, `commands [--category C]`,
  `run <id> [--args JSON | @file | -] [--allow-writes]`, `activity [-n N] [--all]`, `undo <id> [--force]`,
  `mcp [PATH] [--allow-writes]`, `health [-n N]` (local crash log and session health, see "Measurement (0.7)"); global `--color auto|always|never` (auto = stdout is a TTY, `NO_COLOR` empty/unset, `TERM`
  not `dumb`); every command except `mcp` has `--json`. Exit codes 0 ok, 1 error (message `lector: error: ...` on stderr),
  2 usage (clap). A closed stdout (`| head`) exits 0. Diagnostics go to stderr only when `RUST_LOG` is set.
  Changesets: review keys (`worktree`, `staged`, `unstaged`, `untracked`, `commit:<rev>`, `range:<a>..<b>`,
  `branch:<base>...<branch>`), `A...B` (merge base), `A..B`, or one revision (that commit against its first parent).
  `run` refuses a command with `CommandSpec::writes` unless `--allow-writes` (nothing runs, nothing is journaled); `undo`
  is itself the explicit action and needs no flag. `Source::Cli` / `Source::Mcp` is what the journal records; `Journal::undo`
  now records the caller's `Source` (it used to say `ui`).
* **`--json` formats (schema 1).** Every document is an object with `"schema": 1` (bumped only for a breaking change; fields
  are added, never renamed or removed). Paths are repository-relative, lines 0-based, ranges `[start, end)`; floats are
  rounded to 3 decimals.
  ```text
  status    {root, branch|null, detached, head|null, head_short|null, upstream|null, ahead, behind,
             operation: null | {kind: merge|rebase|cherry-pick|revert|am, description, interactive, step: [n, total]|null},
             counts: {staged, unstaged, untracked, conflicted}, clean}
  changes   {changeset, title, files: [{path, old_path?, kind: added|modified|deleted|renamed|copied|typechange,
              added?, removed?, binary?, viewed?, risk?: {score, level: low|medium|high,
              reasons: [{kind, weight, message, line?, dampening?}]}}],      // riskiest first; by path with --no-risk
             totals: {files, added, removed, high, medium, low}, risk_skipped?}  // > 500 files or --no-risk
  diff      {changeset, title, files: [<git.show_diff file object: path, kind, added, removed, hunks: [{header, old, new,
              lines: [" ctx" | "-old" | "+new"]}] | binary | image | too_large>, moved?: [{old, new, reindented}]],
             moves?: [{from: {path, lines}, to: {path, lines}, reindented}]}  // moved/moves only with --moved
  commands  {commands: [CommandSpec::to_json(): id, title, category, args (JSON Schema), writes?, destructive?, undoable?, ui_action?]}
  run       {ok, command, summary, data, paths, journal_id?, undoable?}      // with --json; else summary + pretty data
  activity  {entries: [{id, time (RFC 3339 UTC), command, source: ui|cli|mcp|agent|internal, summary, repo, paths, args,
              undoable, undone, undo_of?, undone_by?}]}                       // newest first, current repository unless --all
  undo      {undone: <id>, entry: <the entry the undo recorded>}
  health    {sessions, running, clean, abnormal, panics, crash_free_rate: 0..1|null, by_version: [{version, sessions, clean,
             abnormal, panics}], crash_reports, crash_dir, last_crashes: [{file, path, time, version, os, thread, location,
             message}]}                                                       // newest first, at most `-n` (default 5)
  ```
  `risk.reasons[].kind` is the snake_case `lector_workspace::risk::RiskKind` (`public_api_changed`, `panic_added`, ...).
  Moved blocks come from `lector_diff::moves` (within a file: `detect_moved`; between files of the changeset:
  `detect_moved_across`, computed over the whole changeset even with `--file`). In human diffs moved removed/added lines
  start with `<` / `>` instead of `-` / `+` and each file lists "moved within file / to / from".
* **MCP server.** stdio transport (UTF-8 JSON-RPC 2.0, one message per line; JSON-RPC batches are accepted), implemented
  directly with `serde_json`. Only protocol messages go to stdout. Methods: `initialize` (echoes a supported
  `protocolVersion`, else the latest; `serverInfo`, `instructions`, capabilities `tools` + `resources`, no `listChanged` /
  `subscribe`), `ping`, `tools/list`, `tools/call`, `resources/list`, `resources/templates/list`, `resources/read`,
  empty `prompts/list`, `logging/setLevel`; notifications `notifications/initialized` and `notifications/cancelled`
  (others ignored). Errors: `-32700` parse, `-32600` invalid request, `-32601` unknown method, `-32602` invalid params /
  unknown tool, `-32002` resource not found.
  *Tools* are rebuilt from the registry on every `tools/list` (commands registered later appear): `name`, `title`,
  `description`, `inputSchema` = `CommandSpec::args_schema`, `annotations { title, readOnlyHint, openWorldHint: false }` plus
  `destructiveHint` / `idempotentHint: false` for writes. Commands with `writes` are omitted and refused unless
  `ServerOptions::allow_writes`; a call to one anyway returns `isError: true` naming `--allow-writes` (nothing runs, nothing is
  journaled). `tools/call` result: `content: [{type: text, text: "<summary>\n<compact JSON data>"}]` (the data is repeated
  in the text for clients that ignore `structuredContent`), `structuredContent` = `Outcome::data` (non-objects wrapped as
  `{result}`), `isError`, and for writes `_meta.lector { command, undoable, journalId }`. Invalid arguments, unavailable
  commands and failing commands are tool errors (`isError: true`, the message), not protocol errors; a panic inside a command
  is caught and reported the same way. Calls run on one worker thread in arrival order (writes never race, `ping` and
  `resources/*` answer meanwhile); `notifications/cancelled` skips a queued call or drops the response of a running one (it
  cannot interrupt a running command). All writes are invoked through `CommandRegistry::invoke` with `Source::Mcp`, so the
  journal records origin `mcp`.
  *Resources*: `lector://changeset/<key>` (JSON of `git.show_diff`; `worktree`, `staged`, `unstaged` are listed) and
  `lector://document/<percent-encoded project-relative path>` (text of the live buffer, so unsaved edits of documents opened
  by tools are visible; listed for the documents open in the store). Documents are only served from inside the project root
  after resolving symlinks, never from `.git`. Tool path arguments are confined the same way (`CommandContext::confine_path`
  for `Source::Mcp` / `Source::Agent`), and `settings.get` redacts `agent.servers[].env` values.
* Tests: `crates/lector-mcp/tests/protocol.rs` (handshake and version negotiation, JSON-RPC errors, dynamic and
  write-filtered `tools/list`, structured results, write refusal without `--allow-writes` and journaling with it, resources
  incl. symlink and `..` escapes, batches, cancellation, ordering); `crates/lector-cli/tests/suite` (JSON formats, human
  output, exit codes, colors, risk ordering, moved blocks, `run` / `activity` / `undo` round trips, a real `lector mcp`
  process over pipes) runs against both `lector-cli` and the real `lector` binary (`crates/lector/tests/cli.rs`; no display
  is set, so a GUI start would fail), all on hermetic git fixtures with `LECTOR_DATA_DIR` / `LECTOR_CONFIG_DIR` in temp dirs.
  Startup (debug build, 20 runs): `lector status` 16 ms median through the GUI binary, 9 ms through `lector-cli`; `changes` ~55 ms.

### Review UX (v0.4; Agent REVIEWUX) — as implemented
Makes reviewing faster and calmer on top of the v0.4 algorithms. Everything here is read-only; the only state is
the user's "reviewed" marks and "last looked" fingerprint in the Db (`kv`), never the repository.
* **`[review]` settings** (`lector-config`): `risk_order` (true), `detect_moved` (true), `structural` (false);
  actions `diff::ToggleMovedCode` (`{m}-alt-m`), `diff::ToggleStructural` (`{m}-alt-t`), `review::NextUnreviewed`
  (`]` and `alt-shift-j`), `review::ToggleHunkReviewed` (`r`). Plain-letter keys (`n p v r ]`) are bound under
  `DiffView && !TextInput`, so they never fire in text inputs (keymap test `plain_keys_are_review_only_…`).
* **`lector_app::review_queue`** (pure, UI-free): `analyze(&[Arc<FileDiff>], &AnalysisOptions) -> ChangesetAnalysis
  { order: Vec<QueueEntry { index, report: RiskReport, group: Attention | LowRisk }>, moves: Vec<FileMoves>,
  hunks: Vec<Vec<HunkFingerprint>> }` (risk from `lector_workspace::risk::rank_files`; generated / lock files and
  score <= 0.05 form the low-risk group, placed last; `RiskReport::is_low_value()`). Moves: `intra_file_moves(&FileDiff)` (what a single file can
  know) and `analyze_moves(&[files])` (adds cross-file moves via `lector_diff::detect_moved_across`); a
  `MoveSpan { rows, other_path, other_lines, same_file, reindented }` per block on each end (`FileMoves { removed,
  added }`). Hunk marks: `hunk_fingerprints` (sha256 of the hunk's removed + added lines, plus an `Anchor`),
  `match_marks(current, stored, new_text)` (hash match, identical hunks disambiguated with `remap::reanchor_with`),
  `HunkReviewStore::{load, save, reviewed, store_reviewed}` (Db key `review.hunks/<repo>/<changeset>/<path>`), so only
  hunks whose content changed lose their mark across revisions and rebases. Progress: `FileProgress`, `totals` ->
  "7 of 12 files · 18 of 40 hunks reviewed", `next_unreviewed(progress, order, from) -> Position` (wraps, current file
  last). A file is done when viewed or all hunks reviewed (two-way sync with the "viewed" checkbox).
* **`lector_app::provenance`** (contract above, implemented): `LineQuery { repo, path, line, rev }`,
  `ProvenanceProvider` (`name`, `explain`), `ProvenanceRegistry::{with_builtin, register(priority, provider),
  unregister, names, explain_all, explain}` on the core (`AppCore::provenance()`), built-ins `commit` (blame +
  commit message, priority 10) and `uncommitted` (50; lines not yet committed); an agent provider registers with
  priority 100. Blame results are cached per (repo, path, rev, HEAD, mtime). `explanation_markdown(&[LineProvenance])`
  renders the hover card. Hover on a changed line shows it in code views (`FileView::show_hover`, saved buffers only)
  and in diffs (new-side changed rows; `DiffView` popover).
* **`lector-ui-git`**:
  * `review_model::ReviewModel` (one per window, `GitStore::review_model`): the risk analysis of the whole working
    tree (debounced 600 ms after the last git change, up to 400 files; `risk_of(path)`, `working()`), and the "new
    changes since you last looked" state (`hint() -> (uncommitted, new)`: a fingerprint of status + mtimes + sizes
    against the fingerprint stored when a working-tree review tab was last open; `mark_seen`). Events:
    `ReviewModelEvent::{Analysis, Hint}` (separate entity, so `GitStoreEvent` subscribers do not reload).
    `analyze_changes(repo, languages, changes, diff_options, detect_moved) -> ChangesAnalysis` (keyed by path).
  * Review tab (`review_tab/queue.rs`): list in risk order with a High / Med badge and the top reason instead of the
    directory, a collapsed "Low-risk & generated" group, per-file "2/5" hunk counters, the progress sentence and bar,
    `]` / `alt-shift-j` across files (the diff handles "next unreviewed hunk in this file" first and emits
    `DiffViewEvent::NextUnreviewed` when the file is done), the first file of the queue selected when it loads, the
    changeset's moves (with cross-file moves) handed to the diff. The Changes panel orders each section by risk
    and has the same badge, reason and collapsed group (working-tree mode only, `review.risk_order`).
  * `DiffView` review layer (`diff_view/review_layer.rs`): moved lines are painted with a "moved" tint and dimmed text
    (`DiffSource::with_moved`, `moved_blocks`), a stripe in the row-background layer `review.moved` and a clickable
    "Moved from parser.rs:120" / "Moved to …" label (row widget, `GoToMove` event for other files; reindent-only moves
    say "(reindented)"); `diff::ToggleMovedCode` / `ToggleStructural`; the structural diff
    (`lector_workspace::structural`, `DiffSource::with_structural`) hides lines whose syntax did not change and marks
    changed tokens, falling back to the line diff with a subtle notice under the header when the language, size or
    complexity does not allow it; per-hunk "Reviewed" checkbox in the hunk toolbar (`r` also moves on to the next
    unreviewed hunk); events `HunksReviewed(path, flags)`, `NextUnreviewed`, `GoToMove`.
* **`lector-ui`** seams: `Workspace::set_review_hint(Option<ReviewHint { uncommitted, new_since_last, open_review }>)`
  drives the welcome view row "Review your N uncommitted changes" (opens the review tab in risk order) and the
  status-bar chip "New changes since you last looked: Review". `lector_lsp::install::{install_hint, Os}` is a data
  table of install commands per server and operating system; the LSP status popover lists them (with a copy button)
  for servers not on `PATH`, and a one-time subtle banner (once per server, Db keys `ui/lsp-install-banner/…`, with
  "Copy" and "Don't show again") offers the command for the active file's language. Lector never runs the commands.

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
// Pane::index_for_key(key, cx); Workspace::{show_toast(msg, cx), set_branch(branch, status_label, cx), is_sidebar_visible()}.
// v0.7: Workspace::set_branch_status_view(Option<AnyView>, cx) (a view drawn in place of the status-bar branch button:
// lector-ui-git's sync control); TextInput::{set_masked(bool, cx), is_masked(), clear_secret()} (secrets drawn as •,
// no copy / cut).
// components::text_area::TextArea (v0.2): multi-line input — IME (EntityInputHandler), soft wrap, grows between
// set_line_limits(min, max) rows then scrolls, mouse/keyboard selection (double/triple click), clipboard, undo/redo
// (typing coalesced within 1 s), placeholder, set_ruler(Option<col>), set_font_family; text()/set_text/is_empty/
// selection/focus; emits TextAreaEvent::Changed. Key context "TextArea TextInput": text_input::* bindings apply,
// text_area::{Newline, MoveUp/Down, SelectUp/Down, PageUp/Down, Word*, DeleteWordLeft, *ToStart/End, Undo, Redo}.
// Keymap: keymap::bind_keymap(cx, &[KeySpec { keys, action, context }]) (same shape as lector-config).
```
Owner-keyed overlay layers on `CodeView` (v0.3, `code_view::layers`; used by git conflicts, open to LSP & co.):
```rust
pub struct RowBackground { pub rows: Range<u32>, pub color: Hsla, pub edge: Option<Hsla> /*2 px left marker*/ }
pub enum WidgetAnchor { AfterText /*2 cols after the row's text, clamped to the right edge*/, Right }
pub struct RowWidget { pub row: u32, pub anchor: WidgetAnchor, pub render: Rc<dyn Fn(&mut Window, &mut App) -> AnyElement> }
impl CodeView {
    pub fn set_row_backgrounds(&mut self, owner: &'static str, v: Vec<RowBackground>, cx);  // empty = remove the layer
    pub fn set_row_widgets(&mut self, owner: &'static str, v: Vec<RowWidget>, cx);
    pub fn has_layer(&self, owner: &'static str) -> bool;
    pub fn row_top(&self, row: u32) -> Option<Pixels>;   // y of a row in the last frame, relative to the view
}
```
Backgrounds paint under the text (above the source's own row decorations), full text width. Widgets are real GPUI
elements, laid out in the code element's prepaint *after* its hitboxes and painted last, clipped to the text area, so
they get clicks first (make them `.occlude()`); widgets on rows under the sticky header or folded away are skipped.

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
// v0.3: fn is_dirty(&self, cx) -> bool (tab dot, close prompts); fn is_read_only(&self, cx) -> bool (tab lock).
// FileView: model() -> Entity<DocumentModel>, document(cx) (settled), live_document(cx), meta(cx), is_dirty(cx),
// save(window, cx) -> Task<Result<bool>>, save_as, revert; events FileViewEvent::{StateChanged, OutlineChanged, PathChanged}.
// editor::FileView (a file tab): document(), code_view(), path(), reveal(Reveal{row, cols, center}), reload(), refresh_gutter(),
// cursor_offset(); events FileViewEvent::{Loaded, CursorMoved}. editor::DocumentSource: CodeSource over a Document
// (background highlight chunks, gutter vs HEAD, folds, sticky rows from scopes). CodeSource::text_snapshot() -> Option<Text>.
// CodeView additions: point_for_pos(pos) -> Option<Point<Pixels>> (popover anchor), top_visible_row(),
// CodeViewEvent::HoverMoved(Option<Pos>), HOVER_DELAY_MS = 500.
// Settings: settings::{AppSettings::get(cx) -> Arc<Settings>, apply_settings, bind_effective_keymap, UiFont}.
// Markdown: markdown::{parse(&str) -> Vec<Block> (blocking, highlights code fences), render_blocks(..)}.
// lector-ui additions to Actions: workspace::ShowOutline (outline sidebar panel; ToggleOutline is the file-symbol picker).
```
Seams added for the terminal (v0.7, all additive):
```rust
Workspace::set_bottom_panel(view: AnyView, focus: FocusHandle, cx); set_bottom_panel_visible(bool, window, cx);
Workspace::is_bottom_panel_visible(); bottom_panel_focus()    // panel below the editor area, resizable (top edge),
                                                             // Region::BottomPanel in F6 order (before the status bar)
keymap::add_keymap_extension(cx, fn(&[KeySpec]) -> Vec<KeySpec>); keymap::extension_bindings(cx, &specs)
                                                             // applied by settings::bind_effective_keymap
terminal_hook::{set_open_in_terminal(cx, f), has_terminal(cx), open_in_terminal(path, window, cx)}
                                                             // file tree (folders, files, empty area) and tab menus
TreeDelegate::background_context_menu(window, cx) -> Option<Entity<ContextMenu>>   // right-click below the nodes
components::split::panel_resize_handle_top(id, cx); keymap::contexts::{TERMINAL, TERMINAL_FIND}
```
Menus, window chrome and the welcome view (v0.2 "ease of access"):
```rust
// One menu model, two renderers. `menus::menu_model() -> Vec<MenuSpec{name, entries}>` (File, Edit, Selection, View,
// Go, Git, Window, Help); entries are MenuEntrySpec::{Action{label, action: "ns::Name"}, Separator,
// Submenu{label, entries}, RecentProjects}. Actions are named by string, so extension crates' actions can be listed
// (unregistered names are skipped). macOS: `menus::app_menus(cx)` → cx.set_menus (+ app menu: About, Settings,
// Services, Quit). Linux/Windows: `views::app_menu::AppMenuBar` in the title bar (Workspace::app_menu()): live key
// bindings, items disabled unless `window.is_action_available` for the element focused when the menu opened,
// submenus, arrows/enter/escape/home/end/tab, hover switching, `workspace::ShowAppMenu` (alt-f), "☰" when narrow.
// Add a command: one `a("Label", "ns::Name")` line in menu_model(); the `every_palette_action_is_in_a_menu` test
// fails for palette commands in no menu (exceptions: menus::MENU_EXEMPT_ACTIONS, with reasons).
// Title bar (views/workspace/chrome.rs): logo + menu bar + project/branch, palette trigger, toolbar (sidebar,
// split, theme, settings gear → app_menu::build_settings_menu), caption buttons (views::window_controls:
// Linux CSD drawn when Decorations::Client or LECTOR_WINDOW_CONTROLS=1; Windows uses WindowControlArea::{Drag,
// Min,Max,Close} so the OS handles them, Snap Layouts included; macOS keeps traffic lights). Linux: drag after
// press+move, double-click toggles maximize, right-click = window menu.
// No-folder windows: Workspace::new_empty / lector_ui::open_empty_window(cx, size): a placeholder Project
// (`Project::open_detached` on data_dir/no-folder; no scan, git, LSP, watcher or session). Workspace::has_folder().
// Workspace::open_folder_in_window(root, file, ..) replaces the window's workspace; open_file_anywhere(path) opens
// a file in this project or a window at its repo (lector_ui::root_for_file, open_folder_window(cx, root, file)).
// Welcome: views::welcome::WelcomeView is drawn in the center whenever the window has no tabs (and focused, so
// key bindings work), and as a tab (`ShowWelcome`, item key "lector:welcome"). Other new panes:
// views::keybindings::KeybindingsView (`ShowKeybindings`; filter text or recorded keystrokes via
// cx.intercept_keystrokes), views::about::AboutDialog (modal; version_info(), issue_url()), views::recent
// (OpenRecent picker; secondary-enter = new window), views::lsp_status (status bar popover).
// Status bar items are buttons in Workspace::render_status_bar (Button::compact + tooltip + on_click
// dispatching an action); add one there. Sidebar emits SidebarEvent::RefreshFiles; Sidebar::set_has_folder.
// Logging (lector binary): stderr + paths::log_file(), rotated to lector.log.old at 4 MiB (`OpenLogFile`).
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
* Not yet applied from settings: `show_whitespace`, `scroll_beyond_last_line`.

### Accessibility (v0.4, `lector-ui`, `lector-theme`, `lector-config`) — as implemented
User-facing description and known gaps: `docs/ACCESSIBILITY.md`. GPUI 0.2.2 has no AccessKit, so this is keyboard,
focus, scale, motion and contrast work only.
* **Settings** (`lector-config`): `Settings.ui: UiSettings { scale: f32 (default 1.0), reduce_motion: Option<bool> }`
  (`[ui]`); `UiSettings::clamped_scale()` clamps to 0.75..=2.0 (NaN → 1.0). `reduce_motion = None` follows the OS.
* **Theme** (`lector-theme`): `UiColors.focus_ring` in every built-in theme; tests assert >= 3:1 against the surfaces a
  control sits on (7:1 in the high-contrast themes), and the status colors are tested as text (4.5:1) and as tints under
  text (agent panel, comments, activity log, merge editor, risk badges).
* **Scale** (`lector_ui::scale`): the scale is process-wide (`scale()`, `set_scale`, `apply_from_settings`,
  `set_runtime_scale(scale, cx)` which also updates the `AppSettings` global). Views call `scale::px(n)` (every view file
  shadows `gpui::px` with it) and `metrics::*()` functions; `Workspace` sets the window rem size per frame
  (`scale::sync_window`). `apply_settings` re-applies it on every settings reload. Code font size is separate.
* **Reduced motion** (`lector_ui::a11y`): `reduce_motion(cx)` = `ui.reduce_motion`, else `os_reduce_motion()` (read once on
  a background thread by `start_os_detection`: `LECTOR_REDUCE_MOTION`, `defaults` on macOS, `gsettings` on GNOME), else
  false. Asked by the caret blink, smooth scrolling, toast slide-in and spinner.
* **Focus ring** (`lector_ui::a11y`): `ring_width()` (2 px, 3 px at scale >= 1.6), `focus_ring_style`, `ring_focus(cx)` for
  `.focus(..)`, `focusable(el, cx)`, `region_ring`, `ring_overlay`, `outset_ring`. Controls with their own handle show an
  inset border; containers draw an overlay while `contains_focused`.
* **Keyboard model** (`lector_ui::views::workspace::regions`): `workspace::` actions `FocusNextRegion` / `FocusPrevRegion`
  (F6 / Shift-F6: title bar, activity bar, sidebar panel, each pane, status bar; hidden regions skipped; `Region`,
  `Workspace::regions`, `focused_region`, `focus_region`), `FocusNextControl` / `FocusPrevControl` (Tab / Shift-Tab; they
  wrap inside a modal via `ModalLayer::modal_container`), `ShowContextMenu` (Shift-F10, Menu), `UiZoomIn` / `UiZoomOut`
  / `UiZoomReset` (`cmd/ctrl-alt-=`, `-`, `0`), `ToggleReduceMotion` (View menu). Key context `ListPanel` (up / down /
  enter → `picker::SelectPrev` / `SelectNext` / `Confirm`) is used by History, Branches and References; `TreeView`,
  `ChangesPanel` and `Picker` keep theirs. Panel focus handles are tab stops (`FocusHandle::tab_stop(true)`); clickable
  divs get `.tab_index(0).focus(a11y::ring_focus(cx))`.
* **Not color-only**: unified and split diff line numbers end in `+` / `-` (`DiffSource::line_number_label`); file
  status has a letter, risk badges a word, diagnostics an icon, toggles a check/square glyph.
* Keybindings viewer lists every command, bound or not ("Unbound").

### Editing (v0.3, `lector-ui`; v0.4: over the headless document store)

```
FileView (tab) ──► CodeView (carets, IME, actions) ──► EditBuffer (Entity; mirror of a shared document, or a private Buffer)
      │                                                     │ EditBufferEvent::Edited { origin, Arc<Transaction> }
      └──► DocumentModel (GPUI adapter, one per open path) ◄┘
             │ owns: EditBuffer, live/settled Document copies, SourceState (stale highlight layers), DocumentSource
             └──► lector_app::documents::DocHandle (buffer, disk state, syntax, queries, LSP sync, saving) — shared with
                  agents, commands and other windows; events bridged to the main thread through a channel
```
* **v0.4 seams.** `editor::document_store(cx)` is the app's `DocumentStore` (`DocumentStore::for_core(AppCore::global())`
  on the shared scheduler; under GPUI's test dispatcher a store on `editor::doc_store::GpuiExecutor`, so `run_until_parked`
  and fake clocks drive reparses, queries and debounces); `set_document_store` installs another. `DocumentModel::handle()`
  exposes the `DocHandle`; `is_locked()` mirrors `DocHandle::set_read_only` (the tab locks its code view). `EditBuffer::shared(handle)`
  mirrors the document's `BufferView` at the last processed `seq`: own edits go through `DocHandle::edit_from` (which also returns
  edits other front-ends made meanwhile, emitted first as `Edited { origin: None }`), `catch_up(cx)` follows foreign edits on
  `DocEvent::Edited`, `update_buffer(|b| .., cx)` runs non-text operations (selections, copy, undo grouping); `buffer()` returns a
  `BufferRef` (`version`, `text`, `selections`, `options`, `is_dirty`, `can_undo/redo`). `EditBuffer::edit` returns `None` when the
  document refused (locked). Typing latency: model sync per keystroke builds the `Document`/`DocumentSource` synchronously and
  only *requests* the structure and gutter queries (a lock + keyed debounce); reparse/highlights/outline/gutter run on the executor.
* **How a pane item becomes editable.** `CodeView::set_buffer(Some(Entity<EditBuffer>), manage_source, cx)`.
  With `manage_source = true` the view rebuilds a `RopeSource` itself after each edit (scratch buffers,
  `CodeView::editable(text, file_name, options, cx)`); otherwise the owner swaps the source (file tabs:
  `DocumentModel` emits `DocumentEvent::SourceChanged`, the `FileView` calls `CodeView::set_source`).
  `CodeView::set_read_only` / `editor::ToggleReadOnly` block editing per view (lock in the tab and status bar).
  Views without a buffer (diff, commit, file @ revision, references, Markdown preview, binary/image/mmap files,
  lossy or windows-1252 decodes) are read-only. Default: `settings.editor.read_only` (false).
* **Carets.** `CodeView` keeps its own sorted, merged `Vec<Selection>` (`selections()`, `primary_index()`,
  `set_selections`, `selection()` = primary/newest) — movement, mouse (click, alt-click adds/removes a caret,
  shift-click extends, double/triple click word/line, drag) and rendering are multi-caret. Mutating actions
  call `CodeView::edit(cx, |buffer| -> (R, Transaction))`: the view's carets are pushed into the shared buffer,
  the op runs, the resulting carets are pulled back (the primary is the buffer's newest selection). Other
  views of the same buffer map their carets, folds, search matches and soft-wrap counts through the
  transaction (`code_view::edit::map_pos_through`). Text input goes through GPUI's `EntityInputHandler` on
  `CodeView` (whole-document UTF-16 offsets via the rope's `OffsetUtf16` dimension): plain typing →
  `Buffer::insert` on every caret; IME composition → `apply_edits` inside one undo transaction with the marked
  range underlined. Caret blink follows `settings.cursor_blink` (530 ms).
* **After each edit** (`DocumentModel`, all non-blocking): new `Document::with_text(rope, None)` + `DocumentSource`
  synchronously; `SourceState::begin_edit` keeps fresh highlight chunks above the first edited row and turns the
  rest into a `StaleHighlights` layer (old spans mapped through the edits; rows inside an edit render plain);
  the store reparses incrementally (`DocEvent::Reparsed` → the model adopts the store's live document), then only the
  missing chunks are requested (`DocHandle::request_highlights`, newest 4 first; `HighlightsReady`); outline / folds /
  breadcrumbs / sticky headers from the *settled* snapshot = the store's `structure()` query, 300 ms after typing stops
  (`FileView::document()` = settled, `FileView::live_document()` = exact text; use the latter for offsets); diff gutter =
  the `gutter()` query vs a cached HEAD text after 300 ms (marks shift with edits meanwhile); the store syncs the LSP
  (`open_document_tagged`) after 150 ms. Measured on a 10k-line Rust file: `CodeView::edit` ≈ 60 µs, model sync ≈ 50 µs (p95 < 0.3 ms),
  whole keystroke incl. a window redraw ≈ 2.5 ms p50 / 3.5 ms p95 (soft wrap on or off; wrap counts are
  re-measured only for edited rows). Traced under `lector_ui::perf` ("code view edit", "document edit sync",
  "incremental reparse"); `cargo test -p lector-ui typing_latency -- --ignored --nocapture`.
* **LSP edits** (format, rename, code actions): `DocumentModel::apply_edits(Vec<lector_buffer::TextEdit>, cx)`
  (offsets into the current text; one undo step; carets of every view mapped). Open models:
  `editor::document_for_path(path, cx)`, `editor::open_documents(cx)`. Context-menu enablement of Format / Rename /
  Code Actions: `code_view::LanguageFeatureProvider::set(cx, |path, cx| LanguageFeatures { format, rename, code_actions })`
  (all disabled by default).
* **Editing polish (v0.3).**
  * *Markdown preview* renders the live buffer: `MarkdownPreview::follow_document(model, cx)` re-parses on
    `DocumentEvent::SourceChanged` 250 ms after typing stops (unsaved text included, nothing written).
  * *File deleted on disk*: `DocumentModel::is_deleted()` (set by `reload_from_disk`, announced with
    `ConflictChanged`). A clean tab stays open with a struck-through, faint title (`PaneItem::is_deleted`,
    `Tab::deleted`); a dirty tab also gets a "This file was deleted on disk" banner with Save (recreates the file, no
    conflict prompt) and Close (`FileViewEvent::CloseRequested`, asks about unsaved changes). Save on a deleted
    document writes it again, clean or dirty; the file coming back reloads a clean buffer and clears the mark.
  * *Save As onto a path open in another tab*: asks "Replace it?" (Replace / Cancel); on Replace the other tab(s) close
    (`FileViewEvent::CloseTabsOf(path)`, handled in `workspace/integration.rs`) and the file is overwritten. If that
    document has unsaved changes, the user is told so and nothing happens.
  * *Blame on a dirty buffer* (`lector_ui_git::blame`): a `BlameSession` per blamed view keeps blame of the file on disk and
    the text it was computed for; on every edit (120 ms debounce) it line-diffs (`lector_diff`) that text against the buffer
    and `remap_hunks` shifts unchanged lines with their commit and marks added or edited lines "Not committed yet". A save
    (or reload) re-runs blame. Toggling blame off drops the session.
  * *`workspace::ToggleMaximize` on Windows*: GPUI 0.2.2 only offers `Window::zoom_window` (`ShowWindow(SW_MAXIMIZE)`,
    never restores) and `Window::resize` does not leave the maximized state, so an already maximized window is not
    toggled; a toast points at the title bar's restore button / double-click (native). macOS and Linux toggle.
  * *Find and replace* (`views/find_bar.rs`): the find modal has a chevron that shows a replace row (also
    `editor::Find` again while the bar is open, `editor::FindReplace` = `cmd/ctrl-h`, which also opens the bar). Replace
    (`editor::ReplaceNext`, `cmd/ctrl-shift-1`, Enter in the replace field) swaps the selected match, else the next one
    after the caret, and selects the following match; Replace All (`editor::ReplaceAll`, `cmd/ctrl-alt-enter`) swaps every
    match as one undo step. Both compute `lector_search::replacements_in_text` over the buffer text on a background
    thread, then call `DocumentModel::apply_edits` if the buffer version is unchanged (else recompute). Regex mode expands
    `$1` / `${name}`; otherwise the replacement is literal. Match highlighting searches the live buffer, not the possibly
    lagging view source. Read-only tabs (no buffer, `set_read_only`, binary...) only search: no replace row, the actions do
    nothing. Menu: Edit > Replace…, Replace Next, Replace All. `Workspace::find_bar(cx)` exposes the open bar.
* **LSP editing features** (`lector_ui::lsp`, installed by `lector_ui::init` → `lsp::init`; see lector-lsp above):
  * *Sync*: `DocumentModel` sends text with `open_document_tagged(.., rope.version())`; `lsp::sync_now(model, cx)` syncs
    immediately before a request so the server sees the buffer text. Coordinates: `lsp::{offset_for_point, point_for_offset,
    range_for_lsp, buffer_edits}` (UTF-16 ↔ bytes, clamped).
  * *Diagnostics*: `lsp::DiagnosticsStore` (global entity; `global(cx)`, `entries(path)`, `all()`, `counts()`,
    `published(path, Option<DocumentDiagnostics>, cx)`) keeps `DiagnosticEntry { range: bytes in the current text, severity,
    message, source, code, tags }` for open documents. A publish is converted against the text whose version equals its tag
    (history of the last 256 `(version, rope, transaction)` per document) and mapped forward through every later
    `Transaction` (`diagnostics::map_range`: starts move with text inserted at them, ends don't grow); entries keep moving with
    edits until the server republishes. Rendered through `CodeView::set_inline_marks(InlineMarks { underlines: Vec<Underline
    { row, range, color, wavy }>, gutter_icons: Vec<GutterIcon { row, icon, color, clickable }>, scrollbar: Vec<(row, Hsla)> })`
    (wavy error/warning, faint straight hints; theme `ui.error/warning/info/hint`; severity icons in the gutter's left margin;
    `CodeView::on_gutter_icon_click(row)`), shown above the hover docs (`diagnostics::hover_markdown`), counted in the status
    bar ("⊗ n ⚠ m", click → `workspace::ShowProblems`: the Problems tab, `lsp::problems::ProblemsItem`, item key
    `lector:problems`, grouped by file, click jumps), navigated with `editor::NextDiagnostic` / `PrevDiagnostic` (F8 /
    shift-F8, wrap, then shows the hover).
  * *Format*: `editor::Format` → `formatting` (or `range_formatting` when there is a selection and the server supports it),
    applied with `DocumentModel::apply_edits` (one undo step) only if the buffer did not change meanwhile.
    `editor.format_on_save`: a before-save hook with `lsp::format::FORMAT_ON_SAVE_TIMEOUT` (2 s); on timeout/error the file is
    saved unformatted and a toast says so.
  * *Code actions*: `editor::CodeActions` → menu at the caret (`CodeView::show_overlay(view, position, Corner, ..)`, a generic
    focused floating view closed on `DismissEvent`; it also dismisses the pointer hover): quick fixes first (preferred first and
    selected — `ContextMenu::select_first`), other actions, then disabled ones with their reason (`ContextMenu::disabled_entry`).
    Running one resolves it (`codeAction/resolve`), applies its edit, then `execute_command`. Lightbulb: 400 ms after the caret
    rests (`FileViewEvent::CursorMoved`), an `automatic` request for the caret selection; shown on the caret row when an enabled
    action exists and neither the row nor the text changed; clicking it opens the menu.
  * *Rename*: `editor::Rename` (F2) → `prepare_rename` → `lsp::rename::RenameBox` over the symbol (placeholder selected; key
    context `Picker`: Enter confirms, Escape cancels) → `rename` → workspace edit → toast "Renamed in N files".
  * *Workspace edits* (`lsp::apply_workspace_edit(ws, WorkspaceEdit, window, cx) -> Task<Result<usize, String>>`, also for
    server `workspace/applyEdit`): validated first, all or nothing — an open document whose synced tag is not its buffer
    version, or whose versioned edit does not match the synced LSP version, rejects the whole edit. Open documents:
    `DocumentModel::apply_edits` (one undo step each); other files: `Workspace::open_path_in_background(path)` (new tab via
    `Pane::add_item_inactive`, not activated) and edited once loaded — dirty and unsaved for review (Save All saves them).
  * *Enablement*: `LanguageFeatureProvider` reports format / rename / code actions from `LspManager::capabilities`; the LSP
    status popover lists each running server's capabilities (`LspStatusPopover::set_capabilities`).
  * Tests: `lsp::ui_tests` (edits to open + unopened files with one undo step each, atomic version rejection, diagnostics
    mapped through edits + squiggles + hover + F8, and against the fake server: format undo, quick fix from server
    diagnostics, rename across files). Known caveat: third-party pylsp plugins such as `pylsp-rope` create `.ropeproject/` in
    the project when code actions are requested (outside Lector's control).
* **Before-save hooks** (format on save): `editor::register_before_save(cx, |model: &Entity<DocumentModel>, cx| ->
  Option<Task<anyhow::Result<Vec<TextEdit>>>>)` (also `Workspace::register_before_save`). Hooks run in registration
  order; their edits apply only if the buffer did not change while the hook ran.
* **Saving** (`DocumentModel::save(force)` → `SaveOutcome::{Saved, Unchanged, Conflict}`; `FileView::save(window)`
  prompts on conflicts; v0.4: GPUI hooks, then `DocHandle::save` on the background executor, `DocError::ChangedOnDisk` →
  `Conflict`): hooks → `trim_trailing_whitespace` / `ensure_final_newline` applied *as buffer edits*
  (buffer == disk afterwards) → `lector_buffer::save::encode` with the file's encoding (UTF-8, UTF-8 BOM — sniffed at
  load —, UTF-16 LE/BE) and line ending (`Lf`/`CrLf` normalized, `Mixed` kept) → stamp check (`lector_fs::stamp`
  taken *before* loading) → `lector_fs::write_atomic` off the main thread → new stamp, `mark_saved` (only if the
  buffer did not change meanwhile). `editor::Save`, `SaveAs` (`cx.prompt_for_new_path`; the tab follows the new
  path), `workspace::SaveAll`, `RevertFile` (confirm when dirty). Auto-save: `after_delay` (model timer, skipped
  while a disk conflict is pending), `on_focus_change` (window deactivation, switching tabs) — both decided by
  `lector_app::documents::auto_save_decision` (never over a conflict or a deleted file; `DocumentModel::focus_lost`).
* **Dirty state**: tab dot instead of × (× on hover), edited preview tabs are pinned, `● ` window-title prefix,
  status bar "Modified" + lock. Closing tabs / Close Others / Close All / the window / `workspace::Quit` prompts
  Save (All) / Don't Save / Cancel when the last tab of a dirty document would close (`Pane::close*`,
  `Workspace::confirm_close`, `views::workspace::quit_with_prompts`). Panes count tabs per document
  (`DocumentModel::view_count`); when the last tab closes the model leaves the registry and, unless another window or an
  agent holds the shared document, `DocumentStore::forget`s it, so reopening reloads from disk. No hot exit in 0.3.
* **External changes** (`FileView::reload` from the watcher → `DocumentModel::file_changed_on_disk` → `DocHandle::check_disk`
  on the background executor; Revert / Reload → `DocHandle::reload`): same stamp →
  ignored (our own save); clean buffer → `Buffer::replace_all` (carets kept) adopting the freshly parsed disk
  snapshot; dirty → `DiskConflict` banner: Reload (discard mine) / Keep Mine (next save overwrites) / Compare
  (`editor::compare::CompareItem`, unified disk ↔ buffer diff from `lector_diff` rows).

### Inline agent edit and turn summaries (v0.7) — as implemented

**Inline agent edit** (`Ctrl/Cmd+I`, "Edit with Agent…" in the editor context menu, View → Agent and the palette). Select
code (or leave the caret on a line), type an instruction in the small box under the selection, and the agent's change
appears as a diff inside the file's own tab.

* *Layers.* `lector_agent::inline_edit` (UI-free, unit-tested; no GPUI): `InlineRequest::from_selection`, the state machine
  `InlineEdit` (`Prompting → Running{turn} → Reviewing → Done(Resolved | NoChanges | FellBack(reason))`, plus
  `Cancelled` and `Failed`), `compute_hunks(old, new)` (line diff through `lector-diff`, lossless: a mismatch falls back
  to one whole-file hunk), `splice`, `focused_prompt`, `availability(&AvailabilityInput) -> Result<(), Unavailable>`
  and `status_line(&Turn)`. `lector_ui::editor::inline_diff` (UI only; knows nothing about agents): `FileView::set_inline_edit(
  Option<InlineEditUi>)`, `InlineEditAction { AcceptHunk, RejectHunk, AcceptAll, RejectAll, Cancel, Dismiss, FollowUp,
  OpenReview }` delivered to the owner's handler, `InlineDiffSource` (the merged view, below). `lector_ui_agent::inline_edit`
  (`InlineEdits`, one per workspace window) connects the two to the Agent panel's session, the buffer and the workspace.
* *Disabled states* (each with its reason in a toast, and a menu/context-menu entry that is never silently dead): the tab is
  not an editable text file; the tab is read-only; the file already has an unresolved inline edit; no agent is configured
  (the toast offers "Set up an agent", which runs `agent::NewSession`, the existing setup path); the session is busy.
* *The turn.* Submitting calls `AgentSession::prompt(text, context)`: a normal turn, so the checkpoint, the journal, undo
  of the whole turn, permission prompts (the Agent panel opens when one is pending) and confinement all apply. The prompt
  puts the instruction alone on its first line, then the path, the line range, the selected text, six lines of context
  either side, and the constraint to change only that file near the range. While the turn runs the range carries an
  accent tint and a "Working…" marker, the bar above the code shows the agent's name and the newest status line with a
  Cancel button (it cancels the turn), and the tab stays editable. Escape in the prompt box cancels before anything ran.
* *Apply or overlay? The agent's write lands on disk first; the editor shows a diff against the baseline.* The alternative
  (hold the proposal back and show it as an overlay with Accept/Reject) needs the agent's `fs/write_text_file` to be
  intercepted and deferred, which breaks three guarantees that already hold for every agent turn: the checkpoint after the
  turn is the truth about what the agent did (review tab, summary, "Undo turn"), the journal records every write when it
  happens, and an agent that reads a file back after writing it sees its own write. Holding writes back also changes what
  the agent can observe (tests it runs would see old files). So the write lands, and the in-place diff is a *view* of the
  turn against the text the file had when the box was submitted. Nothing is lost by this choice: Undo restores the exact old
  text, and the turn's review tab and "Undo turn" are still there.
* *The in-place diff.* While hunks are pending the tab shows the file with each hunk's old lines inserted above its new
  lines (`InlineDiffSource`: a read-only `CodeSource` wrapper, because the code view has no virtual rows). Removed lines use
  the diff's red, added lines its green, and every old row has `-` and every new row `+` in the gutter, so no meaning rests on
  color. Per hunk: Keep and Undo buttons at the first row. In the bar: "Keep all", "Undo all", "Refine…" and the count.
  Keys (context `CodeView && inline_edit`): `Alt+Y` keep the hunk under the cursor, `Alt+N` undo it, `Alt+Shift+Y` keep all,
  `Alt+Shift+N` undo all; `Alt+]` / `Alt+[` (the existing next/previous change keys) walk the hunks because the wrapper
  reports one gutter mark per hunk. Hunks far from the selection are flagged in a note ("3 changes outside the selected
  lines"). The tab is read-only while the merged view is up (rows no longer match the buffer); it returns to its previous
  read-only state, with the cursor near where it was, when the last hunk is decided.
* *Undo semantics.* **Keep** writes nothing: the text is already in the buffer and on disk. **Undo** restores the hunk's
  exact old text with one buffer edit (`DocumentModel::apply_edits`: one undo step, "Undo all" is one step for all hunks), so
  the file becomes *unsaved* and Ctrl+Z brings the agent's text back; the user saves it like any edit (Lector never saves
  on its own, see "Write policy"). The end state is always consistent: every hunk is Kept or Undone and the file is the
  splice of the two (accept all = the agent's text, undo all = the baseline, any mix = baseline hunks where undone; this is
  property-tested for every combination). "Undo turn" in the Agent panel still restores the whole turn from its checkpoint.
* *Other files.* When the turn changed other files the bar says "The agent also changed N other files" with a button that
  opens the turn's review tab.
* *Conflicts.* While the agent works, an edit by the user inside the selected range ends the inline review when the turn
  finishes (the review tab opens instead, with a toast saying why); other edits only get a note ("check that your edits are
  still there": the agent's whole-file write may have replaced them). While reviewing, an edit that touches a pending hunk
  (including a disk reload) falls back to the review tab at once. The agent's own write is recognised by its edit having no
  origin view.
* *Follow-up.* "Refine…" opens the prompt box again; the instruction becomes a new turn in the same session, measured
  against what the file would be with the pending hunks undone, and its result replaces the pending hunks (decided hunks
  stay decided).
* *Known gaps.* Word-level highlights inside a changed line are not drawn; the merged view hides folds and sticky headers;
  diagnostics squiggles can sit on the wrong row while it is up; a failed or cancelled turn that already wrote the file is
  shown as a review with a note (the write cannot be taken back except by Undo). Not exercised with a real agent in a real
  window: the controller is tested with a scripted fake ACP agent (`crates/lector-agent/tests/inline_edit.rs`).

**Turn summaries.** `lector_agent::summary::TurnSummary::compute(repo, languages, &changeset, &DiffOptions)` (blocking) and
the pure `from_diffs(&[Arc<FileDiff>])`: files in the review queue's order (`lector_app::review_queue::analyze`: attention
group by risk, then generated / lock / negligible files as a low-value group), per file the added and removed lines, the top
risk reason and its evidence lines, whether it is a test file, and a `NavTarget { path, old_side, line }` (the evidence
line, else the first changed line; a deleted file opens on the old side). `headline()` reads like "Changed 4 files (+62 −17).
Riskiest: src/auth/session.rs — removes error handling (lines 40–52). Tests touched: 2 files." with variants for one file,
only tests ("only test files"), and only generated or lock files. **No language model is involved:** the sentence is built
from the diff, the risk analysis and a path heuristic, so it is deterministic and works offline. The Agent panel computes it
on a background thread when a turn that changed files finishes and caches it per turn (turns that changed nothing get
nothing); it shows under the turn with clickable files (`AgentPanel::review_turn_at` opens the review tab at the evidence
line), "Review changes" and "Undo turn". The same card is the header of the turn's review tab (`ReviewView::set_header_view`,
`ReviewView::show_file_at`, `review_tab::open_review_view`). When `StructuralDiff.skipped` is not empty the diff shows
"Syntax-aware diff skipped for part of this file (too large); showing line diff there" (`StructuralOutcome::PartialMarks`).

## Updates and signing (v0.7)

Goal: installs and updates can be trusted without Lector ever updating itself behind the user's back.
v0.7 is **notify only**: Lector can tell you that a newer release exists, which file fits your machine and
what its checksum should be, and open the release page; it never downloads and runs anything.

### Trust model

Three layers, each useful on its own and none depending on the others:

1. **Transport**: HTTPS to `api.github.com` / `github.com` (rustls + the platform verifier, proxies from the
   environment).
2. **Operating-system signatures** (what the OS shows the user; the project cannot control how it is judged):
   macOS Developer ID + notarization (hardened runtime, no entitlements: `packaging/macos/entitlements.plist`),
   Windows Authenticode (Azure Artifact Signing or a certificate) for `lector.exe`, `lector-cli.exe`, the
   installer and the MSI, GPG detached `.asc` files (and an embedded `.rpm` signature) on Linux. All optional:
   the release workflow signs only when the matching secrets exist and states in the release notes what was
   actually signed.
3. **The project's own signature**: `SHA256SUMS.txt` of every release carries a minisign (Ed25519, prehashed
   `ED`) signature `SHA256SUMS.txt.minisig` whose trusted comment is exactly `lector <tag>`. The public key is
   committed as `packaging/update-key.pub` and compiled into `lector-forge` (`UPDATE_PUBLIC_KEY`). This is what
   `UpdateStatus::Available::verified` means, independently of OS vendors and of GitHub's account security.

`verified` is **true only when** a real key is compiled in, the `.minisig` parses, its key id matches, the Ed25519
signature over the exact `SHA256SUMS.txt` bytes and the trusted-comment signature both verify, and the trusted
comment names this release's tag (so an older, validly signed file cannot be replayed under a newer tag). Legacy
non-prehashed signatures are refused. **While `packaging/update-key.pub` is the placeholder (no key line),
nothing is ever verified**: there is no "verified: unknown" middle state and the UI must not imply verification;
it may still show the published SHA-256 as "unsigned checksum". Key rotation = ship a Lector with the new public
key first. Not covered: a stolen signing key, or a compromised build pipeline that holds it.

### Contract (`lector_forge::releases`, UI-free, blocking, no tokio)

```rust
pub fn check_for_update(current_version: &str, channel: Channel) -> UpdateStatus;      // production defaults, no state
pub fn check_for_update_with(current_version: &str, channel: Channel,
                             opts: &UpdateOptions, state: &mut UpdateState) -> UpdateStatus;
pub enum Channel { Stable, Preview }
pub enum UpdateStatus {
    UpToDate,
    Available { version, notes_url, published_at: Option<i64>, prerelease: bool, assets: Vec<ReleaseAsset>,
                verified: bool, install_kind: InstallKind, action: UpdateAction, hint: Option<String> },
    CouldNotCheck { reason: CheckFailure },        // offline, rate limited, HTTP error, odd response: stay quiet
}
pub struct ReleaseAsset { name, url, size, sha256: Option<String> }   // sha256 from SHA256SUMS.txt; trust it only if `verified`
pub enum UpdateAction { Download { asset }, PackageManager { command }, ReleasePage }
pub enum CheckFailure { RateLimited { retry_at: Option<i64> }, Network(String), Http(u16), BadResponse(String), InvalidVersion(String) }
pub struct UpdateState { last_checked: Option<i64>, etag: Option<String>, releases: Vec<Release> }   // load(path) / save(path)
pub struct UpdateOptions { api_base, repo, public_key: Option<String>, platform: Platform, install_kind: InstallKind }
// pure helpers (all unit-tested): compare_versions, pick_release, parse_releases, parse_checksums,
// verify_checksums(public_key, sums, signature, tag), select_asset, plan_action, detect_install_kind(&InstallProbe),
// os_release_ids, embedded_public_key_text, state_file_in(data_dir)
```

* **Request**: `GET {api}/repos/meetdhruva/lector/releases?per_page=30` with `Accept: application/vnd.github+json`,
  `User-Agent: lector/<version>`, no credentials, no cookies, no identifiers; `If-None-Match` when a cached list
  exists (a `304` is answered from `UpdateState::releases` and does not count against GitHub's rate limit).
  Timeout 20 s, response ≤ 8 MiB. `403`/`429` with `x-ratelimit-remaining: 0` or `Retry-After` is
  `RateLimited { retry_at }` (from `Retry-After` or `x-ratelimit-reset`); other statuses are `Http(n)`.
  A connection failure leaves `last_checked` untouched; rate limits and HTTP errors count as a check.
* **Selection**: drafts ignored; tags that are not semver ignored; build metadata ignored; `Stable` takes only
  non-prerelease versions, `Preview` also pre-releases; the newest candidate must be strictly greater than the
  running version, so a downgrade is never offered (a pre-release build is not "updated" to an older stable).
* **Verification data** (only when an update exists): `SHA256SUMS.txt` (≤ 256 KiB) and `SHA256SUMS.txt.minisig`
  (≤ 8 KiB) are fetched from the release's assets (URLs must be `https://github.com/...` or the configured API
  origin); every asset's `sha256` is filled from the sums file; `verified` as above. Nothing else is downloaded.
* **Install kind** (`detect_install_kind(&InstallProbe)`, pure; `InstallProbe::from_environment()` reads the
  executable path, `$APPIMAGE`, `$FLATPAK_ID` / `/.flatpak-info`, `/etc/os-release`, the Homebrew Caskroom,
  `lector-data` and `unins000.exe` next to the executable): `MacApp`, `Homebrew`, `WindowsSetup` (Inno uninstaller
  present), `WindowsMsi` (Program Files without it), `WindowsPortable` (`lector-data`), `Deb` / `Rpm` (`/usr/bin`
  + distro family), `AppImage`, `Tarball` (`/usr/local/bin`, `~/.local/bin`), `Flatpak`, `Unknown`. Package-manager
  installs (`Homebrew`: `brew upgrade --cask lector`, `Flatpak`: `flatpak update io.github.meetdhruva.Lector`) get
  `UpdateAction::PackageManager`; known kinds get the matching file via `select_asset`
  (`Lector-V-macos-universal.dmg`, `-windows-{x64,arm64}-setup.exe` / `.msi` / `-portable.zip`, `lector_V_{amd64,arm64}.deb`,
  `lector-V-1.{x86_64,aarch64}.rpm`, `Lector-V-{x86_64,aarch64}.AppImage`, `lector-V-linux-ARCH.tar.gz`); anything
  else gets `ReleasePage`. winget cannot be told apart from a direct installer, so Windows installers carry a
  `hint` ("Installed with winget? Run `winget upgrade meetdhruva.Lector`").

### Policy and state (`lector_config`)

* `[updates]` in `settings.toml`: `check = "off" | "on"` (**default `off`**: nothing happens on a timer until the
  user opts in; the UI asks once on first run) and `channel = "stable" | "preview"`. User settings only: a
  project's `.lector/settings.toml` cannot turn network requests on (ignored with a warning, like `[forge]`).
* `lector_config::should_check_now(last_checked, now, UpdateCheck) -> bool`: false when off; true when never
  checked, at least 24 h (`CHECK_INTERVAL_SECS`) after the last request, or when the stored time is more than a day
  in the future (clock changes). A manual "Check for updates now" ignores it and works with `check = "off"`.
* State (`last_checked`, `ETag`, cached release list) lives in `lector_config::update_state_file()`
  (`<data dir>/update-state.json`); the caller loads it, passes it to `check_for_update_with`, and saves it. A
  corrupt or missing file is an empty state. The one-time "ask the user" flag is the UI's concern.
* UI contract: show "Lector X is available" and one button that opens `notes_url` or the asset URL in the browser
  (or the package-manager command); show the SHA-256 and a verified / "not verified" label. No download-and-run.

### Update and health UI (lector-ui-agent: `update_model`, `notices`, `health_model`, `update_ui`) — as implemented

* *Ask once, default off.* On the first run of a build with this feature, a dismissible dialog asks "Check for updates
  automatically? Lector would ask GitHub once a day whether a new version exists. Nothing about you or your code is sent."
  with [Turn on] / [Not now] (Escape counts as "Not now"). Whether it was asked, and the answer, live in Lector's own state
  (`<data dir>/notices.json`, `lector_ui_agent::notices::Notices`), never in the user's settings or project. The dialog is
  skipped when `[updates] check = "on"` is already set in settings.toml, in headless screenshot runs (`LECTOR_SCREENSHOT`)
  and with `LECTOR_NO_NOTICES`. A choice made in the dialog or with the toggle wins over `[updates] check`
  (`Notices::effective_check`); there is no settings window with toggles, so the toggle is the Help menu command below.
* *Schedule.* When on, one background check at startup (`update_model::run_check` → `check_for_update_with` with the persisted
  `UpdateState`) if `should_check_now(last_checked, now, On)`: at most one request per 24 h. Its result is kept in
  `notices.json` so the quiet **status bar item** "Update X" can show on later starts without a request (dropped when the
  running version caught up). Nothing runs on a timer after startup.
* *Help menu.* "Check for Updates…" (`workspace::CheckForUpdates`) always works, also with checks off: it shows a dialog with
  the answer: up to date, could not check (with the reason, e.g. offline or rate limited), or available. "Automatic Update
  Checks" (`workspace::ToggleAutoUpdateCheck`) flips the choice. Both are in the palette; neither has a default key.
* *The dialog* (`update_model::dialog_model(&UpdateStatus, current) -> UpdateDialogModel`, UI-free): version, published date,
  release-notes link, and by `UpdateAction`: `Download` opens the asset URL in the browser and shows the expected SHA-256
  (with a copy button), `PackageManager` shows a copyable command, `ReleasePage` opens the notes. The word "Verified" appears
  **only** when `verified` is true; otherwise the dialog says "Signature not available for this build." Lector never downloads,
  installs or runs anything.
* *Health.* Help → "Show Crash Log" (`workspace::ShowCrashLog`) opens a dialog from `health_summary()`: sessions (clean,
  abnormal, running), the crash-free rate, per-version counts and the recent crash reports, each with "Open" (the report in a
  tab) and "Copy details"; "Show folder" reveals the crash directory. After an abnormal previous exit
  (`lector_app::health::previous_session_abnormal`) one quiet toast says "Lector didn't shut down cleanly last time" with
  "Show details"; it is shown once per abnormal session (`Notices::acknowledged_crash`). Nothing is sent anywhere.
* *GUI sessions only.* `crates/lector/src/main.rs` dispatches askpass and CLI/MCP invocations before anything else; the panic
  hook is installed after that and `SessionHealth::begin()` runs only for a GUI start, so `lector status`, `lector mcp` and the
  askpass helper never count as sessions (the headless paths install the hook only, in `lector_cli::run_args`).

### Release pipeline (see docs/RELEASING.md for the checklist)

`.github/workflows/release.yml` signs only with secrets present (`scripts/sign-macos.sh`, `sign-windows.ps1`,
`sign-linux.sh`, `sign-checksums.sh`), never in `release/**` dry runs unless asked, and records per platform what
happened (`signing-status/*.txt`) so `scripts/release-notes.sh` reports facts. `scripts/gen-package-manifests.sh`
fills `packaging/{homebrew,winget,flatpak}` (cask, winget 1.6.0 manifests, Flatpak manifest) from the built files.
`scripts/gen-update-key.sh` creates the minisign key pair once.

## Threading model (UI)

* GPUI main thread owns entities (`Workspace`, `Pane`, `CodeView`, ...).
* Expensive work goes through `cx.background_spawn` on snapshots; results are
  applied with `this.update(cx, ..)` and `cx.notify()`.
* Highlights are computed per visible range (+ margin) and cached per
  `(text.version(), row-chunk)`.

### Measurement (0.7): crash log, instruction gates, journeys

The 0.7 targets ("sessions without a crash", hot-path budgets, whole journeys) rest on three measurements that run
without a network and send nothing anywhere.

* **Crash log and session health (`lector_app::health`, UI-free).** `install_panic_hook()` chains a panic hook that writes
  `<data dir>/crashes/crash-<UTC time>-<pid>-<n>.txt` (time, version, OS/arch, thread, location, first line of the panic
  message, backtrace; the home directory and any `/home/<n>`, `/Users/<n>`, `C:\Users\<n>` show as `~`; no file contents:
  only the first message line is kept, cut to 300 chars, and the text a failed string slice quotes is dropped; at most 20
  reports kept, 48 KiB each, 5 written per process). It never panics itself and works on an unwritable data directory.
  `SessionHealth::begin()` / `end_clean()` (GUI only; Clone, no `Drop` so unwinding is not a clean exit) keep one record per
  process, `<data dir>/health/sessions/<started ms>-<pid>-<n>.json` (`running` -> `clean`, version, OS, panics); a process
  writes only its own record, so windows and processes never contend. A `running` record whose process is gone (pid + start
  token on Linux via `/proc`, `ps` elsewhere, `tasklist` on Windows; treated as alive when the check fails) is an abnormal
  end: the next `begin` freezes the verdict with an exclusive `<id>.json.abnormal` marker so a reused pid cannot revive it,
  and a `clean` record always wins over a late marker. The log keeps the newest 500 sessions (live ones are never deleted).
  `health_summary()` / `health_summary_in(&HealthPaths)` (read-only) -> `HealthSummary { sessions, running, clean, abnormal,
  panics, crash_free_rate = clean / (clean + abnormal) | None, by_version, last_crashes: Vec<CrashFile>, crash_reports,
  crash_dir }`. The GUI installs the hook first thing in `main` and begins the session after the start request is built;
  `end_clean` runs from `App::on_app_quit` (macOS exits inside `[NSApp terminate:]`) and again after `Application::run`;
  `lector_cli::run_args` installs the hook for the CLI and `lector mcp`, which never count as sessions. Killing the app
  counts as abnormal (indistinguishable from a crash).
* **Instruction-count gates (`crates/lector-perf`, `scripts/perf-gate.sh`, `perf/baseline.json`).** Not shipped. One gate
  binary per crate (`gate_text`, `gate_buffer`, `gate_diff`, `gate_search`, `gate_syntax`) runs named workloads on seeded
  generated inputs; the `perf-gate` runner executes them under `valgrind --tool=callgrind` and compares the instruction
  counts with the baseline (default threshold 5 %). The contract of a gate: `--list` (`name<TAB>window|process`), `<name>`,
  `<name> --setup-only`, `--native`. See `docs/PERFORMANCE.md`, "Instruction-count gates".
* **Journeys (`crates/lector-cli/tests/journeys`).** Scripted end-to-end walks through the headless core on `git` CLI
  fixtures (open -> status -> changes -> diff -> stage a hunk -> commit -> activity -> undo; edit -> save -> discard -> undo;
  review queue and reviewed hunks across a new commit; comments across unrelated edits and new commits; the CLI with its
  stable `--json` schemas; a `sync` placeholder with a bare remote and two clones). They run on Linux, macOS and Windows.

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
* `editor::` GoToLine, Find, FindNext, FindPrev, FindReplace, ReplaceNext, ReplaceAll, GoToDefinition, FindReferences, Hover,
  ToggleFold, FoldAll, UnfoldAll, Copy, SelectAll, CopyPath, CopyRelativePath, CopyPermalink,
  ToggleBlame, ToggleSoftWrap, ToggleMinimap, ToggleMarkdownPreview, ShowFileHistory,
  ShowLineHistory, NextChange, PrevChange, MoveUp, MoveDown, MoveLeft, MoveRight, PageUp,
  PageDown, ScrollToTop, ScrollToBottom, SelectUp, SelectDown, SelectLeft, SelectRight
* `diff::` ToggleLayout, NextHunk, PrevHunk, NextFile, PrevFile, ToggleViewed,
  ToggleIgnoreWhitespace, ExpandAllContext, OpenInEditor
* `picker::` Confirm, Cancel, SelectNext, SelectPrev
* lector-ui-git additions: `diff::` CompareWith (file vs a picked ref), ReviewWorkingTree, ReviewBranch
  (current branch vs its default base).
* v0.2 ease of access (`workspace::`): OpenFile, OpenRecent, NewWindow, CloseFolder (window returns to the
  welcome state), CloseAllTabs, ShowWelcome, ShowKeybindings, OpenLogFile, OpenDocumentation, ReportIssue,
  Minimize, ToggleMaximize, ToggleFullScreen, ShowAppMenu (Linux/Windows menu bar), SaveAll (v0.3),
  ShowProblems (v0.3, `cmd-shift-m`). v0.3 LSP `editor::`: Format, CodeActions, Rename, NextDiagnostic (`f8`),
  PrevDiagnostic (`shift-f8`).
* lector-ui additions (UI plumbing, also bindable): `workspace::` CloseOtherTabs, CloseTabsToRight,
  ToggleColorblindDiff, About; `editor::` MoveToLineStart, MoveToLineEnd, MoveWordLeft, MoveWordRight,
  SelectToLineStart, SelectToLineEnd, SelectWordLeft, SelectWordRight, SelectPageUp, SelectPageDown,
  SelectToTop, SelectToBottom, ClearSelection; `workspace::ShowOutline`; `tree::` ExpandSelected, CollapseSelected;
  `text_input::` Backspace, Delete, Left, Right, SelectLeft, SelectRight, SelectAll, Home, End, Paste,
  Cut, Copy, ShowCharacterPalette. Key contexts: `Workspace`, `CodeView`, `DiffView`, `Picker`,
  `TextInput`, `ContextMenu`, `TreeView`, `TextArea` (+ `TextInput`), `ChangesPanel` (file list),
  `CommitEditor` (commit composer), `ConfirmDialog`.
* v0.5 agent sessions (`agent::`, handled by `lector-ui-agent`): TogglePanel (`cmd-alt-i`), NewSession, Cancel
  (`escape` in `AgentPanel`), ReviewLastTurn (`cmd-alt-r`), UndoLastTurn (`cmd-shift-alt-z`), SendFeedback
  (`cmd-alt-f`), SendPrompt (`cmd-enter` in `AgentComposer`; menu-exempt). Menu: View → Agent.
* v0.7 terminal (`terminal::`, handled by `lector-ui-terminal`): TogglePanel (``ctrl-` ``, all platforms), NewTerminal
  (``ctrl-shift-` ``), NewTerminalWithProfile, OpenInTerminal, SplitTerminal, SplitTerminalDown, FocusNextSplit,
  FocusPrevSplit, KillTerminal, Clear, Rename, NextTerminal, PrevTerminal, ScrollToPrevCommand, ScrollToNextCommand,
  Find, FindNext / FindPrev / CloseFind (`enter` / `shift-enter` / `escape` in `TerminalFind`; menu-exempt), Copy,
  Paste, SelectAll, MoveToEditor, MoveToPanel. Key context `Terminal` (Linux/Windows: `ctrl-shift-c`/`ctrl-insert`
  copy, `ctrl-shift-v`/`shift-insert` paste, `ctrl-shift-a`, `ctrl-shift-f`, `ctrl-shift-k` clear, `ctrl-shift-w`
  kill, `ctrl-shift-5` split, `ctrl-pageup/pagedown`, `ctrl-shift-up/down` commands, `alt-shift-left/right` splits;
  macOS: `cmd-c/v/a/f/k/w`, `cmd-\`, `cmd-shift-[/]`, `cmd-up/down`, `cmd-alt-left/right`). Menu: View → Terminal,
  View → Terminal Commands; status-bar "Terminal" button.
* v0.7 inline agent edit (`agent::`, `lector-ui-agent`; the namespace is now a palette namespace): EditWithAgent (`cmd/ctrl-i`,
  editor context menu, View → Agent), AcceptInlineHunk (`alt-y`), RejectInlineHunk (`alt-n`), AcceptAllInlineEdits
  (`alt-shift-y`), RejectAllInlineEdits (`alt-shift-n`), the last four in context `CodeView && inline_edit`. v0.7 Help
  (`workspace::`, `lector-ui-agent`): CheckForUpdates, ToggleAutoUpdateCheck, ShowCrashLog.
* v0.4 undo (`workspace::`): ShowActivity (View → Activity Log; the journal of every write Lector made, with Undo per
  entry), UndoLastOperation (`cmd/ctrl-alt-z`; Git → Undo Last Operation; undoes the newest undoable operation in the
  repository of the active window, backed up first and itself undoable).
* v0.3 merge conflicts `git::` OpenMergeEditor, NextConflict, PrevConflict, AcceptOurs, AcceptTheirs, AcceptBoth,
  AcceptAllOurs, AcceptAllTheirs, MarkResolved, ContinueOperation, AbortOperation (Git → Merge Conflicts menu).
  Key context `MergeEditor`: `alt-1/2/3` accept current / incoming / both, `alt-n` / `alt-p` next / previous conflict.
* v0.2 `text_area::` Newline, MoveUp, MoveDown, SelectUp, SelectDown, MoveToStart, MoveToEnd, SelectToStart,
  SelectToEnd, SelectToLineStart, SelectToLineEnd, WordLeft, WordRight, SelectWordLeft, SelectWordRight,
  DeleteWordLeft, Undo, Redo, PageUp, PageDown (bound in `TextArea`: enter, arrows, cmd-z / cmd-shift-z, ...).

Default keys (macOS uses `cmd`, Linux/Windows use `ctrl` in its place):
file finder `cmd-p`, command palette `cmd-shift-p`, project symbols `cmd-t`, outline
`cmd-shift-o`, project search `cmd-shift-f`, sidebar `cmd-b`, files `cmd-shift-e`,
changes `ctrl-shift-g`, history `ctrl-shift-h`, close tab `cmd-w`, reopen `cmd-shift-t`,
next/prev tab `ctrl-tab`/`ctrl-shift-tab`, back/forward `ctrl--`/`ctrl-shift--`
(Linux/Windows: `alt-left`/`alt-right`), go to line `ctrl-g`, find `cmd-f`, replace `cmd-h`, next/prev
`cmd-g`/`cmd-shift-g` (Linux/Windows: `f3`/`shift-f3`), definition `f12`, references
`shift-f12`, hover `cmd-k cmd-i`, fold/unfold `cmd-alt-[`/`cmd-alt-]`, soft wrap `alt-z`,
blame `cmd-alt-b`, markdown preview `cmd-shift-v`, zoom `cmd-=`/`cmd--`/`cmd-0`,
diff next/prev hunk `alt-n`/`alt-p` (also `]c`-free plain keys `n`/`p` in diff context),
next/prev file `alt-j`/`alt-k`, toggle viewed `v` (diff context), toggle layout `cmd-alt-d`,
undo last operation `cmd-alt-z`.
* v0.7 sync, branches and stash `git::` (handled by `lector-ui-git`, Git menu → Sync … / Branches / Stash): Sync (the
  obvious next step), Fetch, Pull (fast-forward only), PullRebase, Push, ForcePushWithLease (always confirmed),
  PublishBranch, SetUpstream, CancelSync, ShowSyncMenu (menu-exempt: opens the status bar dropdown), NewBranch,
  SwitchBranch, RenameBranch, DeleteBranch, CheckoutRemoteBranch, StashChanges, ApplyStash, PopStash, DropStash.
  Default keys (Workspace; `cmd` = `ctrl` off macOS; chords with a plain second key like `cmd-k f`): Sync `cmd-k y`,
  Fetch `cmd-k t`, Pull `cmd-k l`, Push `cmd-k p`, New Branch `cmd-k n`, Switch Branch `cmd-k b`, Stash Changes
  `cmd-k z`.
