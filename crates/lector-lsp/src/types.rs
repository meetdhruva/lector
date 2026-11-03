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
