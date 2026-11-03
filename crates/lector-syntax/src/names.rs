//! Canonical highlight names and the mapping from grammar capture names onto them.

/// Canonical highlight capture names. A [`HighlightId`] indexes into this list.
///
/// Themes style these names; a theme falls back from `a.b.c` to `a.b` to `a`, so
/// dotted names are always safe to leave unstyled.
pub const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "boolean",
    "comment",
    "comment.doc",
    "constant",
    "constant.builtin",
    "constructor",
    "embedded",
    "function",
    "function.builtin",
    "function.macro",
    "function.method",
    "keyword",
    "keyword.control",
    "keyword.import",
    "keyword.return",
    "keyword.operator",
    "label",
    "module",
    "number",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    "string",
    "string.escape",
    "string.regex",
    "string.special",
    "string.special.symbol",
    "tag",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
    "variable.member",
    "markup.heading",
    "markup.bold",
    "markup.italic",
    "markup.strikethrough",
    "markup.link",
    "markup.link.url",
    "markup.raw",
    "markup.list",
    "markup.quote",
    "diff.plus",
    "diff.minus",
];

/// Index into [`HIGHLIGHT_NAMES`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HighlightId(pub u16);

impl HighlightId {
    /// The canonical name for this id (`"keyword"`, `"function.method"`, ...).
    pub fn name(self) -> &'static str {
        HIGHLIGHT_NAMES
            .get(self.0 as usize)
            .copied()
            .unwrap_or("variable")
    }

    /// Look up an id by exact canonical name.
    pub fn from_name(name: &str) -> Option<HighlightId> {
        HIGHLIGHT_NAMES
            .iter()
            .position(|n| *n == name)
            .map(|i| HighlightId(i as u16))
    }
}

/// Grammar capture names (as found in upstream highlight queries) that map to a
/// canonical name which is not a dotted prefix of themselves.
const ALIASES: &[(&str, &str)] = &[
    ("text.title", "markup.heading"),
    ("text.strong", "markup.bold"),
    ("text.emphasis", "markup.italic"),
    ("text.strike", "markup.strikethrough"),
    ("text.uri", "markup.link.url"),
    ("text.reference", "markup.link"),
    ("text.literal", "markup.raw"),
    ("text.quote", "markup.quote"),
    ("markup.strong", "markup.bold"),
    ("markup.emphasis", "markup.italic"),
    ("field", "property"),
    ("parameter", "variable.parameter"),
    ("include", "keyword.import"),
    ("conditional", "keyword.control"),
    ("repeat", "keyword.control"),
    ("exception", "keyword.control"),
    ("keyword.conditional", "keyword.control"),
    ("keyword.repeat", "keyword.control"),
    ("keyword.exception", "keyword.control"),
    ("keyword.coroutine", "keyword.control"),
    ("keyword.directive", "keyword"),
    ("keyword.function", "keyword"),
    ("keyword.modifier", "keyword"),
    ("keyword.type", "keyword"),
    ("storageclass", "keyword"),
    ("define", "keyword.import"),
    ("preproc", "keyword.import"),
    ("float", "number"),
    ("character", "string"),
    ("character.special", "string.special"),
    ("escape", "string.escape"),
    ("string.documentation", "comment.doc"),
    ("comment.documentation", "comment.doc"),
    ("string.regexp", "string.regex"),
    ("string.special.url", "markup.link.url"),
    ("symbol", "string.special.symbol"),
    ("namespace", "module"),
    ("method", "function.method"),
    ("method.call", "function.method"),
    ("tag.attribute", "attribute"),
    ("tag.delimiter", "punctuation.bracket"),
    ("variable.other.member", "variable.member"),
    ("constant.macro", "function.macro"),
    ("function.special", "function.macro"),
];

/// Capture names that must never produce a highlight (queries use them as
/// machinery, not for styling).
fn is_ignored(name: &str) -> bool {
    name.starts_with('_')
        || matches!(
            name,
            "none" | "spell" | "nospell" | "conceal" | "scope" | "text" | "local" | "injection"
        )
        || name.starts_with("local.")
        || name.starts_with("injection.")
        || name.starts_with("definition.")
        || name.starts_with("reference.")
}

/// Map a grammar capture name to the closest canonical highlight id.
///
/// Exact alias first, then the longest dotted prefix present in
/// [`HIGHLIGHT_NAMES`]. Unknown names yield `None` (the capture is skipped).
pub fn highlight_id_for_capture(name: &str) -> Option<HighlightId> {
    let name = name.strip_prefix('@').unwrap_or(name);
    if is_ignored(name) {
        return None;
    }
    if let Some(&(_, to)) = ALIASES.iter().find(|(from, _)| *from == name) {
        return HighlightId::from_name(to);
    }
    let mut candidate = name;
    loop {
        if let Some(id) = HighlightId::from_name(candidate) {
            return Some(id);
        }
        match candidate.rfind('.') {
            Some(i) => candidate = &candidate[..i],
            None => break,
        }
        if let Some(&(_, to)) = ALIASES.iter().find(|(from, _)| *from == candidate) {
            return HighlightId::from_name(to);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_by_longest_prefix() {
        let id = |s: &str| highlight_id_for_capture(s).map(HighlightId::name);
        assert_eq!(id("function.method.call"), Some("function.method"));
        assert_eq!(id("keyword.control.repeat"), Some("keyword.control"));
        assert_eq!(id("punctuation.bracket"), Some("punctuation.bracket"));
        assert_eq!(id("string.special.key"), Some("string.special"));
        assert_eq!(id("text.title"), Some("markup.heading"));
        assert_eq!(id("conditional"), Some("keyword.control"));
        assert_eq!(id("variable.other.member"), Some("variable.member"));
        assert_eq!(id("totally.unknown"), None);
        assert_eq!(id("_private"), None);
        assert_eq!(id("none"), None);
        assert_eq!(id("injection.content"), None);
    }

    #[test]
    fn names_are_unique() {
        for (i, a) in HIGHLIGHT_NAMES.iter().enumerate() {
            assert!(!HIGHLIGHT_NAMES[i + 1..].contains(a), "duplicate {a}");
        }
    }
}
