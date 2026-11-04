use std::ops::Range;
use std::path::PathBuf;

use crate::PointUtf16;

/// A range in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// Absolute file path.
    pub path: PathBuf,
    /// Range in UTF-16 coordinates.
    pub range: Range<PointUtf16>,
}

/// Hover content normalised to markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverInfo {
    /// Markdown text.
    pub markdown: String,
    /// Range the hover applies to.
    pub range: Option<Range<PointUtf16>>,
}

/// LSP symbol kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum SymbolKind {
    File,
    Module,
    Namespace,
    Package,
    Class,
    Method,
    Property,
    Field,
    Constructor,
    Enum,
    Interface,
    Function,
    Variable,
    Constant,
    String,
    Number,
    Boolean,
    Array,
    Object,
    Key,
    Null,
    EnumMember,
    Struct,
    Event,
    Operator,
    TypeParameter,
    Other,
}

/// A symbol in one document (hierarchical).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSymbol {
    /// Symbol name.
    pub name: String,
    /// Extra detail such as a signature.
    pub detail: Option<String>,
    /// Kind.
    pub kind: SymbolKind,
    /// Full extent including body.
    pub range: Range<PointUtf16>,
    /// The identifier itself.
    pub selection_range: Range<PointUtf16>,
    /// Nested symbols.
    pub children: Vec<DocumentSymbol>,
}

/// A symbol found by a workspace query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSymbol {
    /// Symbol name.
    pub name: String,
    /// Kind.
    pub kind: SymbolKind,
    /// File containing it.
    pub path: PathBuf,
    /// Range (zero-width at the start when the server gave none).
    pub range: Range<PointUtf16>,
    /// Enclosing container name.
    pub container: Option<String>,
}

/// A text replacement in UTF-16 coordinates (LSP `TextEdit`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    /// Range to replace, in the document version the edit was computed for.
    pub range: Range<PointUtf16>,
    /// Replacement text.
    pub new_text: String,
}

/// Edits to several files (LSP `WorkspaceEdit` without resource operations).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceEdit {
    /// Edits per file, in the order the server sent them. A file appears once.
    pub changes: Vec<(PathBuf, Vec<TextEdit>)>,
    /// Document versions the edits were computed for (`documentChanges` with a
    /// non-null version). Edits for a file whose version no longer matches
    /// must be rejected.
    pub versions: Vec<(PathBuf, i32)>,
}

impl WorkspaceEdit {
    /// Whether the edit changes nothing.
    pub fn is_empty(&self) -> bool {
        self.changes.iter().all(|(_, e)| e.is_empty())
    }

    /// The version the edits for `path` require, if the server gave one.
    pub fn version_for(&self, path: &std::path::Path) -> Option<i32> {
        self.versions
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, v)| *v)
    }

    /// Number of files with at least one edit.
    pub fn file_count(&self) -> usize {
        self.changes.iter().filter(|(_, e)| !e.is_empty()).count()
    }
}

/// Diagnostic severity (LSP numbering; `Error` is the most severe).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DiagnosticSeverity {
    /// 1
    Error,
    /// 2
    Warning,
    /// 3
    Information,
    /// 4
    Hint,
}

/// Extra diagnostic markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticTag {
    /// Unused or unnecessary code (rendered faded).
    Unnecessary,
    /// Deprecated code (rendered struck through).
    Deprecated,
}

/// One problem reported by a language server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Range in UTF-16 coordinates of the document version it was published for.
    pub range: Range<PointUtf16>,
    /// Severity (servers that omit it get `Error`, like most clients).
    pub severity: DiagnosticSeverity,
    /// Message (may contain newlines).
    pub message: String,
    /// Producer, e.g. `rustc`, `pyflakes`.
    pub source: Option<String>,
    /// Error code (numbers are converted to strings).
    pub code: Option<String>,
    /// Unnecessary / deprecated.
    pub tags: Vec<DiagnosticTag>,
    /// Related locations with a message each.
    pub related: Vec<(Location, String)>,
}

/// The diagnostics last published for one document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentDiagnostics {
    /// Absolute path.
    pub path: PathBuf,
    /// Server that published them.
    pub server: String,
    /// LSP document version they refer to, if the server said.
    pub version: Option<i32>,
    /// Caller tag of the synced text they refer to (see
    /// `LspManager::open_document_tagged`): the server's version matched the
    /// text synced with this tag (or the server gave no version and this is
    /// the latest synced text). `None` when they refer to an older text.
    pub tag: Option<u64>,
    /// The diagnostics, in server order.
    pub diagnostics: Vec<Diagnostic>,
}

/// A server command (LSP `Command`).
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    /// Title.
    pub title: String,
    /// Command identifier.
    pub command: String,
    /// Arguments, passed back verbatim.
    pub arguments: Vec<serde_json::Value>,
}

/// A code action (quick fix, refactoring, source action) or bare command.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeAction {
    /// Menu title.
    pub title: String,
    /// Kind, e.g. `quickfix`, `refactor.extract`.
    pub kind: Option<String>,
    /// The server marks this the best fix.
    pub is_preferred: bool,
    /// Why the action cannot be applied now (shown greyed out).
    pub disabled_reason: Option<String>,
    /// Edit to apply (may need `LspManager::resolve_code_action` first).
    pub edit: Option<WorkspaceEdit>,
    /// Command to run after the edit.
    pub command: Option<Command>,
    /// Diagnostics this action fixes.
    pub diagnostics: Vec<Diagnostic>,
    /// The server's JSON for this action (sent back for `codeAction/resolve`).
    pub raw: serde_json::Value,
}

impl CodeAction {
    /// Whether the kind is `quickfix` (or a sub-kind).
    pub fn is_quick_fix(&self) -> bool {
        self.kind
            .as_deref()
            .is_some_and(|k| k == "quickfix" || k.starts_with("quickfix."))
    }
}

/// Answer to `textDocument/prepareRename`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareRename {
    /// Range of the symbol to rename (`None`: the server accepts the position
    /// and the client picks the word there).
    pub range: Option<Range<PointUtf16>>,
    /// Suggested text for the rename box.
    pub placeholder: Option<String>,
}

/// Formatting preferences sent with formatting requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormattingOptions {
    /// Tab width.
    pub tab_size: u32,
    /// Indent with spaces.
    pub insert_spaces: bool,
}

impl Default for FormattingOptions {
    fn default() -> Self {
        FormattingOptions {
            tab_size: 4,
            insert_spaces: true,
        }
    }
}

/// What a running server supports (from its `initialize` result).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerCapabilities {
    /// `textDocument/hover`.
    pub hover: bool,
    /// `textDocument/definition`.
    pub definition: bool,
    /// `textDocument/references`.
    pub references: bool,
    /// `textDocument/formatting`.
    pub formatting: bool,
    /// `textDocument/rangeFormatting`.
    pub range_formatting: bool,
    /// `textDocument/rename`.
    pub rename: bool,
    /// `textDocument/prepareRename`.
    pub prepare_rename: bool,
    /// `textDocument/codeAction`.
    pub code_actions: bool,
    /// `codeAction/resolve`.
    pub code_action_resolve: bool,
    /// Commands for `workspace/executeCommand`.
    pub commands: Vec<String>,
}

/// Sync state of a document as the server knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentState {
    /// LSP version of the last text sent.
    pub version: i32,
    /// Caller tag of that text (`LspManager::open_document_tagged`).
    pub tag: Option<u64>,
}
