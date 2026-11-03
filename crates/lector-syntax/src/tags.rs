//! Definition/reference tags from `tags.scm`-style queries.

use std::ops::Range;

use tree_sitter::{QueryCursor, StreamingIterator};

use crate::highlight::text_provider;
use crate::input::TextInput;
use crate::outline::SymbolKind;
use crate::syntax::Syntax;

/// A definition or reference of a named symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    /// The symbol name.
    pub name: String,
    /// Symbol kind (for references: the kind of thing referenced, best effort).
    pub kind: SymbolKind,
    /// Byte range of the name identifier.
    pub range: Range<usize>,
    /// `true` for definitions, `false` for references.
    pub is_definition: bool,
}

const MAX_TAG_NAME: usize = 256;

/// Outline kinds that are worth indexing as navigable definitions.
fn is_symbol_kind(kind: SymbolKind) -> bool {
    !matches!(
        kind,
        SymbolKind::Field | SymbolKind::Other | SymbolKind::Heading
    )
}

impl Syntax {
    /// Definitions from the outline win over the tags query for the same name:
    /// their kinds are consistent with the outline (method vs function, struct
    /// vs class), and they cover constants, statics and type aliases too.
    fn merge_outline_definitions<T: TextInput + ?Sized>(&self, text: &T, tags: &mut Vec<Tag>) {
        for item in self.outline(text) {
            if !is_symbol_kind(item.kind)
                || item.name_range.is_empty()
                || item.name_range.len() > MAX_TAG_NAME
            {
                continue;
            }
            let name = text.text_for_range(item.name_range.clone()).into_owned();
            if name.contains('\n') {
                continue;
            }
            tags.retain(|t| !(t.is_definition && t.range == item.name_range));
            tags.push(Tag {
                name,
                kind: item.kind,
                range: item.name_range.clone(),
                is_definition: true,
            });
        }
    }

    /// Definitions and references found by the language's tags query, in
    /// document order. Empty for languages without a tags query.
    pub fn tags<T: TextInput + ?Sized>(&self, text: &T) -> Vec<Tag> {
        let Some(tq) = self.lang.tags_query() else {
            return Vec::new();
        };
        let query = &tq.query;
        let names = query.capture_names();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, self.tree.root_node(), text_provider(text));
        let mut tags = Vec::new();
        while let Some(m) = matches.next() {
            let mut name_node = None;
            let mut role: Option<(bool, SymbolKind)> = None;
            for cap in m.captures() {
                let cname = names[cap.index as usize];
                if cname == "name" {
                    name_node = Some(cap.node);
                } else if let Some(k) = cname.strip_prefix("definition.") {
                    role = Some((true, SymbolKind::from_suffix(k)));
                } else if let Some(k) = cname.strip_prefix("reference.") {
                    role = Some((false, SymbolKind::from_suffix(k)));
                }
            }
            let (Some(node), Some((is_definition, kind))) = (name_node, role) else {
                continue;
            };
            let range = node.byte_range();
            if range.is_empty() || range.len() > MAX_TAG_NAME {
                continue;
            }
            let name = text.text_for_range(range.clone()).into_owned();
            if name.contains('\n') {
                continue;
            }
            tags.push(Tag {
                name,
                kind,
                range,
                is_definition,
            });
        }
        if self.lang.spec.outline_symbols {
            self.merge_outline_definitions(text, &mut tags);
        }
        tags.sort_by_key(|t| (t.range.start, !t.is_definition));
        tags.dedup_by(|b, a| a.range == b.range && a.is_definition == b.is_definition);
        tags
    }
}
