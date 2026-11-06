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

**Fuzzy finder — `lector_search::Picker`** (API unchanged; added `PickerInjector::push_boosted(item, haystack, boost)`
and `MAX_RESULTS = 1000`). nucleo is only the lock-free append-only item store; scoring (nucleo `Pattern`/`Matcher`)
runs on a worker thread using all cores. Only a top-k heap (score, shorter haystack, lower index) is kept,
`matches()` exposes the best `MAX_RESULTS`; the empty query orders by boost then insertion. Partial results are
published after the first batch (<= 2048 items) and then at most every 12 ms, `tick` returns true until settled.
Completed match sets of the last 16 queries are remembered, so typing narrows the previous set and backspace
reuses a remembered one.

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
