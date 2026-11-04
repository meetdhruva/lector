//! The parsed-tree handle: parsing, shared caches and small structural helpers.

use std::collections::HashMap;
use std::ops::{ControlFlow, Range};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use tree_sitter::{ParseOptions, Parser, Tree};

use crate::error::{Result, SyntaxError};
use crate::input::{ChunkReader, Point, TextInput};
use crate::language::Language;
use crate::outline::OutlineItem;

/// Texts larger than this are refused by [`Syntax::parse`].
pub const MAX_PARSE_BYTES: usize = 16 * 1024 * 1024;

/// Cache key for an injected layer: language name and covered byte range.
pub(crate) type LayerKey = (String, usize, usize);

#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) outline: OnceLock<Vec<OutlineItem>>,
    pub(crate) layers: Mutex<HashMap<LayerKey, Arc<Tree>>>,
}

/// A parsed syntax tree for one text snapshot.
///
/// `Clone` is cheap. All query methods must be called with the same text the
/// tree was parsed from. Caches (outline, injected layers) are shared between
/// clones, which is sound because a `Syntax` is tied to one snapshot.
#[derive(Clone)]
pub struct Syntax {
    pub(crate) lang: Arc<Language>,
    pub(crate) tree: Tree,
    pub(crate) shared: Arc<Shared>,
}

impl std::fmt::Debug for Syntax {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Syntax")
            .field("language", &self.lang.name)
            .field("root", &self.tree.root_node().kind())
            .finish()
    }
}

thread_local! {
    /// One parser per language per thread: creating parsers is comparatively
    /// expensive and injected layers parse many small regions.
    static PARSERS: std::cell::RefCell<HashMap<String, Parser>> = std::cell::RefCell::new(HashMap::new());
}

/// Parse `text` with `lang`, reading through the chunk callback. `None` when
/// cancelled; `ranges` restricts parsing to those byte ranges (injections).
pub(crate) fn parse_tree<T: TextInput + ?Sized>(
    lang: &Language,
    text: &T,
    ranges: Option<&[tree_sitter::Range]>,
    cancel: Option<&AtomicBool>,
    old_tree: Option<&Tree>,
) -> Result<Tree> {
    PARSERS.with(|cell| {
        let mut parsers = cell.borrow_mut();
        if !parsers.contains_key(&lang.name) {
            let mut parser = Parser::new();
            parser
                .set_language(&lang.grammar)
                .map_err(|e| SyntaxError::Grammar(lang.name.clone(), e.to_string()))?;
            parsers.insert(lang.name.clone(), parser);
        }
        let Some(parser) = parsers.get_mut(&lang.name) else {
            return Err(SyntaxError::ParseFailed(lang.name.clone()));
        };
        // Included ranges persist on a parser: always set them (empty = whole text).
        let bad_ranges = |e: tree_sitter::IncludedRangesError| {
            SyntaxError::Grammar(lang.name.clone(), format!("bad ranges at {}", e.0))
        };
        parser
            .set_included_ranges(ranges.unwrap_or(&[]))
            .map_err(bad_ranges)?;
        let mut reader = ChunkReader::new(text);
        let mut read = |offset: usize, _| reader.read(offset);
        let tree = match cancel {
            Some(flag) => {
                let mut progress = |_: &tree_sitter::ParseState| {
                    if flag.load(Ordering::Relaxed) {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                };
                parser.parse_with_options(
                    &mut read,
                    old_tree,
                    Some(ParseOptions::new().progress_callback(&mut progress)),
                )
            }
            None => parser.parse_with_options(&mut read, old_tree, None),
        };
        // A cancelled parse leaves the parser mid-state; always start fresh.
        parser.reset();
        tree.ok_or_else(|| SyntaxError::ParseFailed(lang.name.clone()))
    })
}

impl Syntax {
    /// Parse `text` as `lang`. Reads through the text's chunk iterator (no copy
    /// of the whole file). Fails for texts over [`MAX_PARSE_BYTES`].
    pub fn parse<T: TextInput + ?Sized>(lang: &Arc<Language>, text: &T) -> Result<Syntax> {
        Self::parse_inner(lang, text, None)
    }

    /// Like [`Syntax::parse`], but aborts with `ParseFailed` once `cancel` is set.
    pub fn parse_cancellable<T: TextInput + ?Sized>(
        lang: &Arc<Language>,
        text: &T,
        cancel: &AtomicBool,
    ) -> Result<Syntax> {
        Self::parse_inner(lang, text, Some(cancel))
    }

    fn parse_inner<T: TextInput + ?Sized>(
        lang: &Arc<Language>,
        text: &T,
        cancel: Option<&AtomicBool>,
    ) -> Result<Syntax> {
        let len = text.len();
        if len > MAX_PARSE_BYTES {
            return Err(SyntaxError::TooLarge {
                len,
                max: MAX_PARSE_BYTES,
            });
        }
        let tree = parse_tree(lang, text, None, cancel, None)?;
        Ok(Syntax {
            lang: lang.clone(),
            tree,
            shared: Arc::new(Shared::default()),
        })
    }

    /// Incrementally re-parses after `edits` were applied to the text this tree
    /// was parsed from; `text` is the text *after* all edits.
    ///
    /// `edits` must be sequentially valid, in the order given (each one
    /// expressed against the text left by the previous ones), which is what
    /// `lector_buffer::Transaction::deltas` provides. Invalid edits (inverted
    /// ranges, positions past the new text) fall back to a full parse instead
    /// of failing. `self` is left untouched; the result has fresh caches.
    pub fn edit_and_reparse<T: TextInput + ?Sized>(
        &self,
        text: &T,
        edits: &[SyntaxEdit],
    ) -> Result<Syntax> {
        let len = text.len();
        if len > MAX_PARSE_BYTES {
            return Err(SyntaxError::TooLarge {
                len,
                max: MAX_PARSE_BYTES,
            });
        }
        let valid = edits
            .iter()
            .all(|e| e.start_byte <= e.old_end_byte && e.start_byte <= e.new_end_byte);
        let old = if valid && !edits.is_empty() {
            let mut tree = self.tree.clone();
            for e in edits {
                tree.edit(&e.to_input_edit());
            }
            Some(tree)
        } else if edits.is_empty() {
            Some(self.tree.clone())
        } else {
            None
        };
        let tree = parse_tree(&self.lang, text, None, None, old.as_ref())?;
        Ok(Syntax {
            lang: self.lang.clone(),
            tree,
            shared: Arc::new(Shared::default()),
        })
    }

    /// The language this tree was parsed with.
    pub fn language(&self) -> &Arc<Language> {
        &self.lang
    }

    /// The tree as an s-expression (debugging aid).
    pub fn debug_sexp(&self) -> String {
        self.tree.root_node().to_sexp()
    }

    /// True when the tree contains syntax errors.
    pub fn has_errors(&self) -> bool {
        self.tree.root_node().has_error()
    }

    /// Smallest named node covering the byte at `offset` (for identifier lookup).
    pub fn node_at(&self, offset: usize) -> Option<NodeInfo> {
        let root = self.tree.root_node();
        let len = root.end_byte();
        let (start, end) = if offset >= len {
            (len.saturating_sub(1), len)
        } else {
            (offset, offset + 1)
        };
        let node = root.named_descendant_for_byte_range(start, end)?;
        Some(NodeInfo::from_node(&node))
    }
}

/// One text change for [`Syntax::edit_and_reparse`]. Same shape and field
/// names as `lector_buffer::EditDelta` (this crate does not depend on it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyntaxEdit {
    /// Byte offset where the replaced range starts.
    pub start_byte: usize,
    /// Byte offset where the replaced range ended, before the edit.
    pub old_end_byte: usize,
    /// Byte offset where the inserted text ends, after the edit.
    pub new_end_byte: usize,
    /// Point of `start_byte`.
    pub start: Point,
    /// Point of `old_end_byte` before the edit.
    pub old_end: Point,
    /// Point of `new_end_byte` after the edit.
    pub new_end: Point,
}

impl SyntaxEdit {
    fn to_input_edit(self) -> tree_sitter::InputEdit {
        let p = |p: Point| tree_sitter::Point {
            row: p.row as usize,
            column: p.column as usize,
        };
        tree_sitter::InputEdit {
            start_byte: self.start_byte,
            old_end_byte: self.old_end_byte,
            new_end_byte: self.new_end_byte,
            start_position: p(self.start),
            old_end_position: p(self.old_end),
            new_end_position: p(self.new_end),
        }
    }
}

/// A syntax node: kind and byte range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeInfo {
    /// Grammar node kind (`identifier`, `type_identifier`, ...).
    pub kind: String,
    /// Byte range of the node.
    pub range: Range<usize>,
    /// Whether the node is a named node.
    pub named: bool,
}

impl NodeInfo {
    pub(crate) fn from_node(node: &tree_sitter::Node<'_>) -> NodeInfo {
        NodeInfo {
            kind: node.kind().to_string(),
            range: node.byte_range(),
            named: node.is_named(),
        }
    }

    /// Heuristic: does this node name something (identifier-like kinds)?
    pub fn is_identifier(&self) -> bool {
        let k = self.kind.as_str();
        k == "identifier"
            || k == "name"
            || k == "constant"
            || k == "word"
            || k == "variable_name"
            || k == "simple_identifier"
            || k.ends_with("_identifier")
            || k.ends_with("_name")
    }
}
