//! Outline (symbol tree), enclosing scopes and symbol kinds.

use std::cmp::Reverse;
use std::ops::Range;

use tree_sitter::{QueryCursor, StreamingIterator};

use crate::highlight::text_provider;
use crate::input::TextInput;
use crate::syntax::Syntax;

/// What kind of symbol an outline item or tag is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SymbolKind {
    /// Module, namespace, package.
    Module,
    /// Class (or class-like: object, record).
    Class,
    /// Struct.
    Struct,
    /// Enum.
    Enum,
    /// Interface / protocol.
    Interface,
    /// Trait.
    Trait,
    /// Free function.
    Function,
    /// Method (function inside a type or impl).
    Method,
    /// Field / property / enum variant.
    Field,
    /// Variable / static.
    Variable,
    /// Constant.
    Constant,
    /// Type alias.
    Type,
    /// Macro.
    Macro,
    /// Markdown heading.
    Heading,
    /// Anything else (impl blocks, sections, ...).
    Other,
}

impl SymbolKind {
    /// Short lowercase label ("function", "struct", ...).
    pub fn label(self) -> &'static str {
        match self {
            SymbolKind::Module => "module",
            SymbolKind::Class => "class",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Interface => "interface",
            SymbolKind::Trait => "trait",
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Field => "field",
            SymbolKind::Variable => "variable",
            SymbolKind::Constant => "constant",
            SymbolKind::Type => "type",
            SymbolKind::Macro => "macro",
            SymbolKind::Heading => "heading",
            SymbolKind::Other => "other",
        }
    }

    /// Parse a query kind suffix ("function", "method", "class", ...). Unknown → `Other`.
    pub(crate) fn from_suffix(s: &str) -> SymbolKind {
        match s {
            "module" | "namespace" | "package" => SymbolKind::Module,
            "class" | "object" | "record" => SymbolKind::Class,
            "struct" | "union" => SymbolKind::Struct,
            "enum" => SymbolKind::Enum,
            "interface" | "protocol" => SymbolKind::Interface,
            "trait" => SymbolKind::Trait,
            "function" | "call" => SymbolKind::Function,
            "method" | "send" | "constructor" => SymbolKind::Method,
            "field" | "property" | "member" => SymbolKind::Field,
            "variable" => SymbolKind::Variable,
            "constant" => SymbolKind::Constant,
            "type" => SymbolKind::Type,
            "macro" => SymbolKind::Macro,
            "heading" => SymbolKind::Heading,
            _ => SymbolKind::Other,
        }
    }

    fn is_container(self) -> bool {
        matches!(
            self,
            SymbolKind::Class
                | SymbolKind::Struct
                | SymbolKind::Enum
                | SymbolKind::Interface
                | SymbolKind::Trait
        )
    }
}

/// One entry of the outline tree (flattened, in document order).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutlineItem {
    /// Display name (for example `impl Display for Foo`, or a heading's text).
    pub name: String,
    /// Symbol kind.
    pub kind: SymbolKind,
    /// Byte range of the whole item (signature and body).
    pub range: Range<usize>,
    /// Byte range of the identifier that names the item.
    pub name_range: Range<usize>,
    /// Nesting depth (0 = top level).
    pub depth: u32,
}

const MAX_NAME_LEN: usize = 200;

/// Collapse whitespace runs and cap the length of a display name.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_NAME_LEN));
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
        if out.len() >= MAX_NAME_LEN {
            out.push('…');
            break;
        }
    }
    out
}

struct Raw {
    item: OutlineItem,
    /// Item is a container for methods (impl blocks, classes, ...).
    container: bool,
    /// Query pattern index (later, more specific patterns win duplicates).
    pattern: usize,
}

impl Syntax {
    /// The symbol outline of the file, in document order with nesting depth.
    ///
    /// Computed once per `Syntax` and cached; always call with the same text.
    pub fn outline<T: TextInput + ?Sized>(&self, text: &T) -> Vec<OutlineItem> {
        self.shared
            .outline
            .get_or_init(|| self.compute_outline(text))
            .clone()
    }

    /// Outline items containing `offset`, outermost first (breadcrumbs, sticky scroll).
    pub fn enclosing_scopes<T: TextInput + ?Sized>(
        &self,
        text: &T,
        offset: usize,
    ) -> Vec<OutlineItem> {
        let outline = self
            .shared
            .outline
            .get_or_init(|| self.compute_outline(text));
        outline
            .iter()
            .take_while(|i| i.range.start <= offset)
            .filter(|i| offset < i.range.end || (i.range.start == offset && i.range.is_empty()))
            .cloned()
            .collect()
    }

    fn compute_outline<T: TextInput + ?Sized>(&self, text: &T) -> Vec<OutlineItem> {
        let Some(oq) = self.lang.outline_query() else {
            return Vec::new();
        };
        let query = &oq.query;
        let names = query.capture_names();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, self.tree.root_node(), text_provider(text));
        let mut raws: Vec<Raw> = Vec::new();
        while let Some(m) = matches.next() {
            let mut item_node = None;
            let mut kind = SymbolKind::Other;
            let mut container = false;
            let mut parts: Vec<(usize, Range<usize>)> = Vec::new();
            let mut name_range: Option<Range<usize>> = None;
            for cap in m.captures() {
                let cname = names[cap.index as usize];
                if cname == "item" || cname.starts_with("item.") {
                    item_node = Some(cap.node);
                    let suffix = cname.strip_prefix("item.").unwrap_or("other");
                    if suffix == "impl" {
                        kind = SymbolKind::Other;
                        container = true;
                    } else {
                        kind = SymbolKind::from_suffix(suffix);
                        container = kind.is_container();
                    }
                } else if cname == "name" {
                    let r = cap.node.byte_range();
                    if name_range.is_none() {
                        name_range = Some(r.clone());
                    }
                    parts.push((r.start, r));
                } else if cname == "context" {
                    let r = cap.node.byte_range();
                    parts.push((r.start, r));
                }
            }
            let Some(node) = item_node else { continue };
            parts.sort_by_key(|p| p.0);
            parts.dedup_by(|a, b| a.1 == b.1);
            let joined = parts
                .iter()
                .map(|(_, r)| text.text_for_range(r.clone()).into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            let name = tidy(&joined);
            if name.is_empty() {
                continue;
            }
            let range = node.byte_range();
            raws.push(Raw {
                item: OutlineItem {
                    name,
                    kind,
                    name_range: name_range.unwrap_or_else(|| range.clone()),
                    range,
                    depth: 0,
                },
                container,
                pattern: m.pattern_index,
            });
        }
        raws.sort_by_key(|r| (r.item.range.start, Reverse(r.item.range.end), r.pattern));
        // Same node matched by several patterns: keep the last (most specific).
        let mut deduped: Vec<Raw> = Vec::with_capacity(raws.len());
        for raw in raws {
            match deduped.last_mut() {
                Some(prev)
                    if prev.item.range == raw.item.range
                        && prev.item.name_range == raw.item.name_range =>
                {
                    *prev = raw
                }
                _ => deduped.push(raw),
            }
        }
        let raws = deduped;
        // Depth from containment; functions inside type-like containers become methods.
        let mut stack: Vec<(Range<usize>, bool, SymbolKind)> = Vec::new();
        let mut items = Vec::with_capacity(raws.len());
        for mut raw in raws {
            while let Some((top, _, _)) = stack.last() {
                let strictly_inside = top.start <= raw.item.range.start
                    && raw.item.range.end <= top.end
                    && *top != raw.item.range;
                if strictly_inside {
                    break;
                }
                stack.pop();
            }
            // Local variables/constants are noise in an outline.
            if matches!(raw.item.kind, SymbolKind::Variable | SymbolKind::Constant)
                && matches!(
                    stack.last(),
                    Some((_, _, SymbolKind::Function | SymbolKind::Method))
                )
            {
                continue;
            }
            raw.item.depth = stack.len() as u32;
            if let Some((_, true, _)) = stack.last() {
                match raw.item.kind {
                    SymbolKind::Function => raw.item.kind = SymbolKind::Method,
                    SymbolKind::Variable => raw.item.kind = SymbolKind::Field,
                    _ => {}
                }
            }
            stack.push((raw.item.range.clone(), raw.container, raw.item.kind));
            items.push(raw.item);
        }
        items
    }
}
