//! UI-free text editing model for Lector (see `docs/ARCHITECTURE.md`, "lector-buffer").
//!
//! A [`Buffer`] owns a [`Rope`](lector_text::Rope), a set of multi-cursor
//! [`Selection`]s and an undo history. Every mutating call returns a
//! [`Transaction`] listing exact [`EditDelta`]s, which feed tree-sitter
//! `InputEdit`s and LSP incremental document sync.
//!
//! * Offsets are UTF-8 byte offsets, always on grapheme cluster boundaries.
//! * Selections are sorted, non-overlapping and merged after every operation.
//! * History stores rope snapshots (clones are `O(1)` and share structure),
//!   so a step costs `O(edit size + log n)` regardless of file size.
//!
//! Nothing here touches the file system or a window system.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod buffer;
mod edit;
mod engine;
mod history;
mod lines;
mod motion;
pub mod save;
mod selection;
mod text_util;

#[cfg(test)]
mod tests;

use std::ops::Range;

pub use buffer::Buffer;
pub use lector_text::Point;
pub use selection::Selection;

/// How one indentation level is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndentStyle {
    /// One tab character per level.
    Tabs,
    /// This many spaces per level.
    Spaces(u32),
}

impl Default for IndentStyle {
    fn default() -> Self {
        IndentStyle::Spaces(4)
    }
}

/// Comment delimiters of a language.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommentTokens {
    /// Line comment prefix, e.g. `//` or `#`.
    pub line: Option<String>,
    /// Block comment delimiters, e.g. `("/*", "*/")`.
    pub block: Option<(String, String)>,
}

/// Editing behaviour switches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferOptions {
    /// Width of a tab character in columns (used for indent alignment).
    pub tab_size: u32,
    /// How indentation is inserted.
    pub indent: IndentStyle,
    /// Copy the indentation of the current line on newline.
    pub auto_indent: bool,
    /// Insert matching closers for `([{"'` and type over the ones inserted.
    pub auto_close_brackets: bool,
    /// Indent the line after one ending in `:` (Python, YAML, Nim and the like);
    /// lines ending in `{`, `(` or `[` are always indented.
    pub indent_after_colon: bool,
    /// Comment delimiters used by [`Buffer::toggle_comment`].
    pub comment: CommentTokens,
}

impl Default for BufferOptions {
    fn default() -> Self {
        BufferOptions {
            tab_size: 4,
            indent: IndentStyle::Spaces(4),
            auto_indent: true,
            auto_close_brackets: true,
            indent_after_colon: false,
            comment: CommentTokens::default(),
        }
    }
}

/// An edit expressed against the text *before* a batch of edits is applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEdit {
    /// Byte range to replace.
    pub range: Range<usize>,
    /// Replacement text.
    pub new_text: String,
}

/// One text change, in the shape of tree-sitter's `InputEdit` and an LSP
/// incremental change.
///
/// Within a [`Transaction`] deltas are listed **back to front** (descending
/// `start_byte`): `start`/`old_end` coordinates refer to the text before the
/// whole batch, which equals the text at the moment the delta is applied when
/// deltas are applied in the listed order. Feeding them in order to
/// `Tree::edit` or sending them in order as LSP `contentChanges` is therefore
/// always valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EditDelta {
    /// Byte offset where the replaced range starts.
    pub start_byte: usize,
    /// Byte offset where the replaced range ended (before the edit).
    pub old_end_byte: usize,
    /// Byte offset where the inserted text ends (after the edit).
    pub new_end_byte: usize,
    /// Point of `start_byte`.
    pub start: Point,
    /// Point of `old_end_byte` before the edit.
    pub old_end: Point,
    /// Point of `new_end_byte` after the edit.
    pub new_end: Point,
}

/// The result of a mutating call: the exact deltas, in application order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Transaction {
    /// Deltas to apply in order (see [`EditDelta`]). Empty when nothing changed.
    pub deltas: Vec<EditDelta>,
}

impl Transaction {
    /// True when the call changed no text.
    pub fn is_empty(&self) -> bool {
        self.deltas.is_empty()
    }
}

/// Caret movements for [`Buffer::move_carets`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    /// One grapheme left.
    Left,
    /// One grapheme right.
    Right,
    /// One line up, keeping the goal column.
    Up,
    /// One line down, keeping the goal column.
    Down,
    /// To the previous word start.
    WordLeft,
    /// To the next word end.
    WordRight,
    /// Smart Home: first non-blank, then column 0.
    LineStart,
    /// End of the line.
    LineEnd,
    /// This many lines up.
    PageUp(u32),
    /// This many lines down.
    PageDown(u32),
    /// Start of the buffer.
    BufferStart,
    /// End of the buffer.
    BufferEnd,
}
