//! Structural queries: fold ranges and bracket matching.

use std::cmp::Reverse;
use std::ops::Range;

use tree_sitter::Node;

use crate::input::TextInput;
use crate::syntax::Syntax;

#[derive(Clone, Copy, PartialEq, Eq)]
enum FoldStyle {
    /// A body (block, class body, indented block): when it starts on its own
    /// line, the header is the preceding non-blank line.
    Body,
    /// A delimited container (object, array, arguments): header is its own first line.
    Container,
}

fn classify(kind: &str, extras: &[&str]) -> Option<FoldStyle> {
    let body = kind == "block"
        || kind == "body"
        || kind.ends_with("_block")
        || kind.ends_with("block") && !kind.contains("comment") && !kind.starts_with("fenced")
        || kind.ends_with("_body")
        || kind.ends_with("declaration_list")
        || kind == "compound_statement"
        || kind == "block_mapping"
        || kind == "block_sequence"
        || kind == "enum_variant_list"
        || kind == "field_declaration_list"
        || kind == "match_block"
        || kind == "statement_block"
        || kind == "body_statement"
        || kind == "class_body";
    if body {
        return Some(FoldStyle::Body);
    }
    if extras.contains(&kind) {
        return Some(FoldStyle::Container);
    }
    let container = matches!(
        kind,
        "object"
            | "array"
            | "dictionary"
            | "list"
            | "tuple"
            | "set"
            | "table"
            | "arguments"
            | "argument_list"
            | "parameters"
            | "parameter_list"
            | "formal_parameters"
            | "token_tree"
            | "field_initializer_list"
            | "array_expression"
            | "tuple_expression"
            | "struct_expression"
            | "use_list"
            | "initializer_list"
            | "object_pattern"
            | "array_pattern"
            | "flow_mapping"
            | "flow_sequence"
            | "parenthesized_expression"
            | "list_comprehension"
            | "dictionary_comprehension"
            | "set_comprehension"
            | "template_string"
            | "composite_literal"
            | "literal_value"
            | "element_list"
            | "array_creation_expression"
            | "associative_array"
            | "comment"
            | "block_comment"
            | "multiline_comment"
            | "doc_comment"
            | "heredoc_body"
    );
    container.then_some(FoldStyle::Container)
}

/// True when `line` (the part of the last row up to the node end) only closes something.
fn is_closer_line(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty()
        && (t.chars().all(|c| "}])>;,".contains(c))
            || (t.starts_with("</") && t.ends_with('>') && t.matches('<').count() == 1)
            || matches!(
                t,
                "end"
                    | "fi"
                    | "done"
                    | "esac"
                    | "endif"
                    | "endfor"
                    | "endwhile"
                    | "end;"
                    | "end)"
                    | "end,"
            ))
}

impl Syntax {
    /// Foldable regions as row ranges. `start` is the header row (stays
    /// visible); rows `start + 1 .. end` are hidden when folded. When the region
    /// ends in a closing delimiter on its own row (`}`, `]`, `end`), `end` is
    /// that row, so the closer stays visible. Sorted by `start`; at most one
    /// range per start row (the outermost).
    pub fn fold_ranges<T: TextInput + ?Sized>(&self, text: &T) -> Vec<Range<u32>> {
        let extras = self.lang.fold_kinds();
        let mut out: Vec<Range<u32>> = Vec::new();
        let root = self.tree.root_node();
        let mut cursor = root.walk();
        // Iterative DFS pruning single-row subtrees.
        loop {
            let node = cursor.node();
            let multi_row = node.start_position().row != node.end_position().row;
            if multi_row {
                if let Some(range) = fold_for_node(&node, text, extras) {
                    out.push(range);
                }
                if cursor.goto_first_child() {
                    continue;
                }
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    return finish_folds(out);
                }
            }
        }
    }

    /// The bracket pair adjacent to `offset` (the bracket at `offset` or just
    /// before it): `(this bracket, its partner)`, in no particular order.
    pub fn bracket_pair<T: TextInput + ?Sized>(
        &self,
        _text: &T,
        offset: usize,
    ) -> Option<(Range<usize>, Range<usize>)> {
        let root = self.tree.root_node();
        for pos in [Some(offset), offset.checked_sub(1)].into_iter().flatten() {
            if pos >= root.end_byte() {
                continue;
            }
            if let Some(leaf) = root.descendant_for_byte_range(pos, pos + 1) {
                if let Some(pair) = pair_of_bracket(&leaf) {
                    return Some(pair);
                }
            }
        }
        None
    }

    /// The innermost bracket pair that encloses `offset` (opening bracket
    /// ≤ `offset` < closing bracket end), as `(open, close)`.
    pub fn enclosing_bracket_pair<T: TextInput + ?Sized>(
        &self,
        _text: &T,
        offset: usize,
    ) -> Option<(Range<usize>, Range<usize>)> {
        let root = self.tree.root_node();
        let probe = offset.min(root.end_byte().saturating_sub(1));
        let mut node =
            root.descendant_for_byte_range(probe, probe.saturating_add(1).min(root.end_byte()))?;
        loop {
            if let Some((open, close)) = delimiters_of(&node) {
                if open.start <= offset && offset < close.end {
                    return Some((open, close));
                }
            }
            node = node.parent()?;
        }
    }
}

fn fold_for_node<T: TextInput + ?Sized>(
    node: &Node<'_>,
    text: &T,
    extras: &[&str],
) -> Option<Range<u32>> {
    let style = classify(node.kind(), extras)?;
    let start = node.start_position();
    let end = node.end_position();
    let last_row = if end.column == 0 && end.row > start.row {
        end.row - 1
    } else {
        end.row
    };
    let mut header = start.row;
    if style == FoldStyle::Body {
        // YAML's root mapping/sequence is the document, not a fold.
        if node.kind().starts_with("block_") && start.column == 0 {
            return None;
        }
        let line_start = node.start_byte() - start.column;
        let prefix = text.text_for_range(line_start..node.start_byte());
        if prefix.trim().is_empty() {
            // Body starts its own line (Allman brace, indented block): the
            // header is the previous non-blank row.
            if start.row == 0 {
                return None;
            }
            header = previous_nonblank_row(text, line_start, start.row)?;
        }
    }
    if last_row <= header {
        return None;
    }
    // Is the final row just a closer?
    let last_row_start = if last_row == end.row {
        node.end_byte() - end.column
    } else {
        // Node ended at column 0 of the next row: the last row is the one before.
        let back = node.end_byte().saturating_sub(1);
        let probe = text.text_for_range(back.saturating_sub(4096)..back);
        let line_len = probe
            .rfind('\n')
            .map_or(probe.len(), |i| probe.len() - i - 1);
        back - line_len
    };
    let last_row_end = if last_row == end.row {
        node.end_byte()
    } else {
        node.end_byte().saturating_sub(1)
    };
    let tail = text.text_for_range(last_row_start..last_row_end.max(last_row_start));
    let end_row = if is_closer_line(&tail) {
        last_row
    } else {
        last_row + 1
    };
    (end_row > header + 1).then_some(header as u32..end_row as u32)
}

fn previous_nonblank_row<T: TextInput + ?Sized>(
    text: &T,
    line_start: usize,
    row: usize,
) -> Option<usize> {
    // Read backwards a bounded window and count newlines.
    let window_start = line_start.saturating_sub(8192);
    let window = text.text_for_range(window_start..line_start);
    let mut r = row;
    for line in window.rsplit_terminator('\n') {
        r = r.checked_sub(1)?;
        if !line.trim().is_empty() {
            return Some(r);
        }
    }
    None
}

fn finish_folds(mut ranges: Vec<Range<u32>>) -> Vec<Range<u32>> {
    ranges.sort_by_key(|r| (r.start, Reverse(r.end)));
    ranges.dedup_by_key(|r| r.start);
    ranges
}

fn closer_for(kind: &str) -> Option<&'static str> {
    match kind {
        "(" => Some(")"),
        "[" => Some("]"),
        "{" => Some("}"),
        _ => None,
    }
}

fn is_closer(kind: &str) -> bool {
    matches!(kind, ")" | "]" | "}")
}

/// Open/close delimiter tokens of `node` if it is delimited by a matching pair
/// (first and last child are the brackets).
fn delimiters_of(node: &Node<'_>) -> Option<(Range<usize>, Range<usize>)> {
    let count = node.child_count();
    if count < 2 {
        return None;
    }
    let first = node.child(0)?;
    let last = node.child(count - 1)?;
    let want = closer_for(first.kind())?;
    (last.kind() == want && first.child_count() == 0 && last.child_count() == 0)
        .then(|| (first.byte_range(), last.byte_range()))
}

/// If `leaf` is a bracket token, find its partner among its siblings.
fn pair_of_bracket(leaf: &Node<'_>) -> Option<(Range<usize>, Range<usize>)> {
    let kind = leaf.kind();
    if leaf.child_count() != 0 || !(closer_for(kind).is_some() || is_closer(kind)) {
        return None;
    }
    let parent = leaf.parent()?;
    let mut stack: Vec<Node<'_>> = Vec::new();
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        if child.child_count() != 0 {
            continue;
        }
        let ck = child.kind();
        if closer_for(ck).is_some() {
            stack.push(child);
        } else if is_closer(ck) {
            // Pop to the matching opener.
            while let Some(open) = stack.pop() {
                if closer_for(open.kind()) == Some(ck) {
                    if open.id() == leaf.id() {
                        return Some((open.byte_range(), child.byte_range()));
                    }
                    if child.id() == leaf.id() {
                        return Some((child.byte_range(), open.byte_range()));
                    }
                    break;
                }
            }
        }
    }
    None
}
