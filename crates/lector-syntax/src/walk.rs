//! A flat, owned snapshot of the syntax tree for structural algorithms
//! (structural diff, risk analysis) that must not depend on tree-sitter types.

use std::ops::Range;

use crate::syntax::Syntax;

/// One node of [`Syntax::flatten`], in pre-order (parents before children).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeNode<'a> {
    /// Grammar node kind (`function_item`, `identifier`, `{`, ...).
    pub kind: &'a str,
    /// Numeric kind id (stable within one grammar).
    pub kind_id: u16,
    /// Byte range of the node.
    pub range: Range<usize>,
    /// Named (as opposed to anonymous token) node.
    pub named: bool,
    /// Index of the parent in the returned vector (`None` for the root).
    pub parent: Option<u32>,
    /// Number of children (0 for leaves/tokens).
    pub child_count: u32,
    /// An `ERROR` node or a node containing a missing token.
    pub is_error: bool,
}

impl Syntax {
    /// Flattens the main tree (injected layers are not included) into a
    /// pre-order vector; `None` when it has more than `max_nodes` nodes.
    pub fn flatten(&self, max_nodes: usize) -> Option<Vec<TreeNode<'_>>> {
        let mut out: Vec<TreeNode<'_>> = Vec::new();
        let mut cursor = self.tree.walk();
        let mut parents: Vec<u32> = Vec::new();
        loop {
            let node = cursor.node();
            if out.len() >= max_nodes {
                return None;
            }
            out.push(TreeNode {
                kind: node.kind(),
                kind_id: node.kind_id(),
                range: node.byte_range(),
                named: node.is_named(),
                parent: parents.last().copied(),
                child_count: node.child_count(),
                is_error: node.is_error() || node.is_missing(),
            });
            if cursor.goto_first_child() {
                parents.push((out.len() - 1) as u32);
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    return Some(out);
                }
                parents.pop();
            }
        }
    }
}
