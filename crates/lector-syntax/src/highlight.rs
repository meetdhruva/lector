//! Highlighting: query captures (including injected languages) resolved into
//! sorted, non-overlapping spans.

use std::cmp::Reverse;
use std::ops::Range;
use std::sync::Arc;

use tree_sitter::{Node, QueryCursor, StreamingIterator, Tree};

use crate::input::TextInput;
use crate::language::{Language, LanguageRegistry};
use crate::names::{HighlightId, HIGHLIGHT_NAMES};
use crate::syntax::{parse_tree, Syntax};

/// A highlighted byte range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HighlightSpan {
    /// Byte range in the text.
    pub range: Range<usize>,
    /// Canonical highlight id (index into [`crate::HIGHLIGHT_NAMES`]).
    pub id: HighlightId,
}

/// Pseudo highlight id of `@none` captures: they shadow enclosing captures
/// (e.g. fenced code content inside a `markup.raw` block) without styling.
pub(crate) const CLEAR_ID: u16 = u16::MAX;

/// Maximum nesting of injected languages (markdown → html → js is depth 2).
const MAX_INJECTION_DEPTH: u8 = 3;
/// Bound on cached injected layers per `Syntax`.
const MAX_CACHED_LAYERS: usize = 1024;

#[derive(Clone, Copy, Debug)]
struct Capture {
    start: usize,
    end: usize,
    id: u16,
    layer: u8,
    pattern: u32,
    seq: u32,
}

impl Capture {
    /// Higher wins: deeper layer, then innermost (shortest), then style tier,
    /// then later pattern.
    fn priority(&self) -> (u8, Reverse<usize>, u8, u32, u32) {
        (
            self.layer,
            Reverse(self.end - self.start),
            tier(self.id),
            self.pattern,
            self.seq,
        )
    }
}

/// Rank among captures of identical range and layer, ahead of pattern order.
///
/// Upstream queries disagree on whether a catch-all `(identifier) @variable`
/// or `(field_identifier) @property` comes first or last. Ranking makes
/// `func main` a function in every grammar (variable < everything else), keeps
/// YAML keys as properties over generic strings, and lets method names beat the
/// generic property style.
fn tier(id: u16) -> u8 {
    match HIGHLIGHT_NAMES.get(id as usize) {
        Some(&"variable") => 0,
        Some(&"property") => 2,
        Some(name) if name.starts_with("function") || *name == "constructor" => 3,
        _ => 1,
    }
}

pub(crate) fn text_provider<'a, T: TextInput + ?Sized>(
    text: &'a T,
) -> impl FnMut(Node<'_>) -> Box<dyn Iterator<Item = &'a [u8]> + 'a> {
    move |node: Node<'_>| {
        let chunks = text.chunks_in_range(node.byte_range());
        Box::new(chunks.map(str::as_bytes)) as Box<dyn Iterator<Item = &'a [u8]> + 'a>
    }
}

impl Syntax {
    /// Highlight spans overlapping `range`, sorted, non-overlapping, clipped to
    /// `range`. Overlapping captures resolve to the innermost one (later
    /// pattern wins ties); injected languages override their host.
    pub fn highlights<T: TextInput + ?Sized>(
        &self,
        text: &T,
        range: Range<usize>,
    ) -> Vec<HighlightSpan> {
        let len = text.len();
        let range = range.start.min(len)..range.end.min(len);
        if range.start >= range.end {
            return Vec::new();
        }
        let mut captures = Vec::new();
        self.collect_layer(
            text,
            &self.tree,
            &self.lang,
            range.clone(),
            0,
            &mut captures,
        );
        let mut spans = resolve(captures, range);
        trim_trailing_newlines(text, &mut spans);
        spans
    }

    fn collect_layer<T: TextInput + ?Sized>(
        &self,
        text: &T,
        tree: &Tree,
        lang: &Arc<Language>,
        range: Range<usize>,
        depth: u8,
        out: &mut Vec<Capture>,
    ) {
        let first_new = out.len();
        if let Some(hq) = lang.highlight_query() {
            let mut cursor = QueryCursor::new();
            cursor.set_byte_range(range.clone());
            let mut captures = cursor.captures(&hq.query, tree.root_node(), text_provider(text));
            while let Some((m, idx)) = captures.next() {
                let cap = m.captures()[*idx];
                let Some(Some(id)) = hq.ids.get(cap.index as usize) else {
                    continue;
                };
                let start = cap.node.start_byte().max(range.start);
                let end = cap.node.end_byte().min(range.end);
                if start >= end {
                    continue;
                }
                out.push(Capture {
                    start,
                    end,
                    id: *id,
                    layer: depth,
                    pattern: m.pattern_index as u32,
                    seq: out.len() as u32,
                });
            }
        }
        let regions = if depth < MAX_INJECTION_DEPTH {
            injection_regions(lang, tree, text, &range)
        } else {
            Vec::new()
        };
        // `@none` captures only matter where an injected language replaces the
        // host's styling; elsewhere (unknown fence language) the host keeps it.
        let mut i = first_new;
        while i < out.len() {
            let c = out[i];
            let keep = c.id != CLEAR_ID
                || regions
                    .iter()
                    .any(|r| r.start() < c.end && c.start < r.end());
            if keep {
                i += 1;
            } else {
                out.remove(i);
            }
        }
        for region in regions {
            let Some(layer_tree) = self.layer_tree(text, &region) else {
                continue;
            };
            let sub = region.start().max(range.start)..region.end().min(range.end);
            if sub.start >= sub.end {
                continue;
            }
            self.collect_layer(text, &layer_tree, &region.lang, sub, depth + 1, out);
        }
    }

    /// Parse (or fetch from cache) the tree of an injected region.
    fn layer_tree<T: TextInput + ?Sized>(&self, text: &T, region: &Injection) -> Option<Arc<Tree>> {
        let key = (region.lang.name.clone(), region.start(), region.end());
        if let Some(t) = self.shared.layers.lock().get(&key) {
            return Some(t.clone());
        }
        let tree = Arc::new(parse_tree(&region.lang, text, Some(&region.ranges), None, None).ok()?);
        let mut layers = self.shared.layers.lock();
        if layers.len() >= MAX_CACHED_LAYERS {
            layers.clear();
        }
        layers.insert(key, tree.clone());
        Some(tree)
    }
}

/// One injected region: a language and the byte ranges it covers.
pub(crate) struct Injection {
    pub(crate) lang: Arc<Language>,
    pub(crate) ranges: Vec<tree_sitter::Range>,
}

impl Injection {
    fn start(&self) -> usize {
        self.ranges.first().map_or(0, |r| r.start_byte)
    }
    fn end(&self) -> usize {
        self.ranges.last().map_or(0, |r| r.end_byte)
    }
}

/// Find injections of `tree` intersecting `range`.
pub(crate) fn injection_regions<T: TextInput + ?Sized>(
    lang: &Language,
    tree: &Tree,
    text: &T,
    range: &Range<usize>,
) -> Vec<Injection> {
    let Some(iq) = lang.injection_query() else {
        return Vec::new();
    };
    let registry = LanguageRegistry::new();
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(range.clone());
    let mut matches = cursor.matches(&iq.query, tree.root_node(), text_provider(text));
    let mut regions = Vec::new();
    while let Some(m) = matches.next() {
        let content = m
            .captures()
            .iter()
            .find(|c| Some(c.index) == iq.content_capture)
            .map(|c| c.node);
        let Some(content) = content else { continue };
        let mut lang_name: Option<String> = None;
        for prop in iq.query.property_settings(m.pattern_index) {
            if &*prop.key == "injection.language" {
                lang_name = prop.value.as_ref().map(|v| v.to_string());
            }
        }
        if lang_name.is_none() {
            if let Some(c) = m
                .captures()
                .iter()
                .find(|c| Some(c.index) == iq.language_capture)
            {
                lang_name = Some(text.text_for_range(c.node.byte_range()).into_owned());
            }
        }
        let Some(name) = lang_name else { continue };
        // Info strings like "rust,ignore" or "{.python}" → first word.
        let name = name
            .trim_matches(|c: char| c == '{' || c == '}' || c == '.' || c.is_whitespace())
            .split([',', ' ', '{', '}', ':'])
            .next()
            .unwrap_or("");
        let Some(inj_lang) = registry.by_name(name) else {
            continue;
        };
        if inj_lang.name == lang.name && inj_lang.name != "Markdown Inline" {
            // Avoid trivially recursive self-injection.
            continue;
        }
        let include_children = iq
            .query
            .property_settings(m.pattern_index)
            .iter()
            .any(|p| &*p.key == "injection.include-children");
        let ranges = content_ranges(&content, include_children);
        if ranges.is_empty() {
            continue;
        }
        regions.push(Injection {
            lang: inj_lang,
            ranges,
        });
    }
    regions
}

/// The ranges of `node`, minus its children unless `include_children`.
fn content_ranges(node: &Node<'_>, include_children: bool) -> Vec<tree_sitter::Range> {
    let whole = tree_sitter::Range {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_point: node.start_position(),
        end_point: node.end_position(),
    };
    if include_children || node.child_count() == 0 {
        return if whole.start_byte < whole.end_byte {
            vec![whole]
        } else {
            Vec::new()
        };
    }
    let mut ranges = Vec::new();
    let mut start_byte = whole.start_byte;
    let mut start_point = whole.start_point;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.start_byte() > start_byte {
            ranges.push(tree_sitter::Range {
                start_byte,
                end_byte: child.start_byte(),
                start_point,
                end_point: child.start_position(),
            });
        }
        start_byte = child.end_byte();
        start_point = child.end_position();
    }
    if whole.end_byte > start_byte {
        ranges.push(tree_sitter::Range {
            start_byte,
            end_byte: whole.end_byte,
            start_point,
            end_point: whole.end_point,
        });
    }
    ranges
}

/// Grammars often include the line terminator in comment-like nodes; drop it so
/// spans never end with `\n` / `\r\n` (renderers work per line).
fn trim_trailing_newlines<T: TextInput + ?Sized>(text: &T, spans: &mut Vec<HighlightSpan>) {
    let last_is =
        |end: usize, start: usize, ch: &str| end > start && text.text_for_range(end - 1..end) == ch;
    spans.retain_mut(|span| {
        if last_is(span.range.end, span.range.start, "\n") {
            span.range.end -= 1;
        }
        if last_is(span.range.end, span.range.start, "\r") {
            span.range.end -= 1;
        }
        span.range.start < span.range.end
    });
}

/// Resolve overlapping captures into sorted non-overlapping spans.
fn resolve(mut captures: Vec<Capture>, range: Range<usize>) -> Vec<HighlightSpan> {
    if captures.is_empty() {
        return Vec::new();
    }
    captures.sort_by_key(|c| (c.start, c.end));
    // Segment boundaries.
    let mut bounds: Vec<usize> = Vec::with_capacity(captures.len() * 2);
    for c in &captures {
        bounds.push(c.start);
        bounds.push(c.end);
    }
    bounds.sort_unstable();
    bounds.dedup();

    let mut spans: Vec<HighlightSpan> = Vec::new();
    let mut active: Vec<usize> = Vec::new();
    let mut next_capture = 0;
    for w in bounds.windows(2) {
        let (seg_start, seg_end) = (w[0], w[1]);
        if seg_end <= range.start || seg_start >= range.end {
            continue;
        }
        while next_capture < captures.len() && captures[next_capture].start <= seg_start {
            active.push(next_capture);
            next_capture += 1;
        }
        active.retain(|&i| captures[i].end > seg_start);
        let Some(best) = active
            .iter()
            .map(|&i| &captures[i])
            .max_by_key(|c| c.priority())
        else {
            continue;
        };
        if best.id == CLEAR_ID {
            continue;
        }
        let id = HighlightId(best.id);
        match spans.last_mut() {
            Some(last) if last.id == id && last.range.end == seg_start => last.range.end = seg_end,
            _ => spans.push(HighlightSpan {
                range: seg_start..seg_end,
                id,
            }),
        }
    }
    spans
}
