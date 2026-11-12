//! Completion protocol data: requests, items, `CompletionList` parsing
//! (including `itemDefaults`), resolve results and the client capabilities
//! Lector declares. The interactive model lives in [`crate::session`].

use std::ops::Range;

use serde_json::{json, Value};

use crate::convert;
use crate::error::{LspError, Result};
use crate::types::{Command, TextEdit};
use crate::PointUtf16;

/// What a completion item stands for (LSP `CompletionItemKind`), for icons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum CompletionKind {
    Text,
    Method,
    Function,
    Constructor,
    Field,
    Variable,
    Class,
    Interface,
    Module,
    Property,
    Unit,
    Value,
    Enum,
    Keyword,
    Snippet,
    Color,
    File,
    Reference,
    Folder,
    EnumMember,
    Constant,
    Struct,
    Event,
    Operator,
    TypeParameter,
    Other,
}

impl CompletionKind {
    fn from_lsp(n: i64) -> CompletionKind {
        use CompletionKind::*;
        match n {
            1 => Text,
            2 => Method,
            3 => Function,
            4 => Constructor,
            5 => Field,
            6 => Variable,
            7 => Class,
            8 => Interface,
            9 => Module,
            10 => Property,
            11 => Unit,
            12 => Value,
            13 => Enum,
            14 => Keyword,
            15 => Snippet,
            16 => Color,
            17 => File,
            18 => Reference,
            19 => Folder,
            20 => EnumMember,
            21 => Constant,
            22 => Struct,
            23 => Event,
            24 => Operator,
            25 => TypeParameter,
            _ => Other,
        }
    }
}

/// Documentation text with its format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Documentation {
    /// The text.
    pub value: String,
    /// Markdown (otherwise plain text).
    pub markdown: bool,
}

impl Documentation {
    /// Parses a `string | MarkupContent` value.
    pub(crate) fn from_json(v: &Value) -> Option<Documentation> {
        let doc = match v {
            Value::String(s) => Documentation {
                value: s.clone(),
                markdown: false,
            },
            Value::Object(o) => Documentation {
                value: o.get("value")?.as_str()?.to_string(),
                markdown: o.get("kind").and_then(Value::as_str) == Some("markdown"),
            },
            _ => return None,
        };
        (!doc.value.trim().is_empty()).then_some(doc)
    }
}

/// Extra text shown after the label (LSP `CompletionItemLabelDetails`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LabelDetails {
    /// Right after the label, e.g. a signature `(x: i32)`.
    pub detail: Option<String>,
    /// Further right, e.g. the defining module.
    pub description: Option<String>,
}

/// Whether an item's insert text is plain text or LSP snippet syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertFormat {
    /// Insert as is.
    PlainText,
    /// Snippet syntax (`$1`, `${2:x}`); only requested from servers when the
    /// `editor.completion.snippets` setting is on.
    Snippet,
}

/// How whitespace in multi-line insert text is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertTextMode {
    /// Insert exactly as given.
    AsIs,
    /// Re-indent the lines after the first one to the current line's
    /// indentation (Lector's default for items that do not say).
    AdjustIndentation,
}

/// The edit of an item: replace a range with `new_text`. Servers that send a
/// plain `TextEdit` get `insert == replace`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemEdit {
    /// Range used in "insert" mode (ends at the cursor).
    pub insert: Range<PointUtf16>,
    /// Range used in "replace" mode (ends after the word under the cursor).
    pub replace: Range<PointUtf16>,
    /// Replacement text (may be a snippet, see [`CompletionItem::format`]).
    pub new_text: String,
}

/// One suggestion, normalized from LSP `CompletionItem` (with `itemDefaults`
/// applied). Positions are UTF-16 in the document version of the request.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionItem {
    /// Text shown in the list.
    pub label: String,
    /// Extra text after the label.
    pub label_details: Option<LabelDetails>,
    /// Kind (icon).
    pub kind: CompletionKind,
    /// Short description, e.g. a type or signature.
    pub detail: Option<String>,
    /// Documentation (often only after resolve).
    pub documentation: Option<Documentation>,
    /// Deprecated (`deprecated: true` or tag 1): shown struck through, ranked lower.
    pub deprecated: bool,
    /// The server recommends selecting this item.
    pub preselect: bool,
    /// Text used to order items (default: the label).
    pub sort_text: Option<String>,
    /// Text used for filtering (default: the label).
    pub filter_text: Option<String>,
    /// Text to insert when there is no [`CompletionItem::edit`] (default: the label).
    pub insert_text: Option<String>,
    /// Format of `insert_text` / `edit.new_text`.
    pub format: InsertFormat,
    /// Whitespace handling of multi-line text.
    pub insert_mode: InsertTextMode,
    /// The edit the server wants, if any.
    pub edit: Option<ItemEdit>,
    /// Further edits, e.g. an auto-import at the top of the file. Often only
    /// present after resolve.
    pub additional_edits: Vec<TextEdit>,
    /// Typing one of these accepts the item first.
    pub commit_characters: Vec<char>,
    /// Command to run after inserting (e.g. `editor.action.triggerParameterHints`).
    pub command: Option<Command>,
    /// Whether `completionItem/resolve` already ran for this item.
    pub resolved: bool,
    /// The server's JSON, sent back for `completionItem/resolve`.
    pub raw: Value,
}

/// Why a completion request is made (LSP `CompletionTriggerKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionTrigger {
    /// Typing an identifier character or the manual command (kind 1).
    Invoked,
    /// A server trigger character was typed (kind 2).
    TriggerCharacter(char),
    /// Re-query because the previous list was `isIncomplete` (kind 3).
    Incomplete,
}

/// A `textDocument/completion` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletionRequest {
    /// The caret, UTF-16. The document must have been synced with the text
    /// that has the caret there.
    pub pos: PointUtf16,
    /// Why.
    pub trigger: CompletionTrigger,
}

/// The answer to a [`CompletionRequest`].
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionResponse {
    /// The items, in server order.
    pub items: Vec<CompletionItem>,
    /// More items match than were sent: ask again after further typing.
    pub is_incomplete: bool,
    /// The position the request was made at (the coordinates of the items' edits).
    pub request_pos: PointUtf16,
    /// LSP document version the answer is for.
    pub doc_version: i32,
    /// Caller tag of the text with that version (`open_document_tagged`).
    pub tag: Option<u64>,
    /// The server supports `completionItem/resolve`.
    pub resolve_supported: bool,
}

/// What `completionItem/resolve` filled in. Only the properties Lector
/// declared as resolvable (`documentation`, `detail`, `additionalTextEdits`)
/// are taken from the answer; the label, filter, sort and edit stay as listed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedCompletion {
    /// Detail, if the answer has one.
    pub detail: Option<String>,
    /// Documentation, if the answer has one.
    pub documentation: Option<Documentation>,
    /// `additionalTextEdits` of the answer (coordinates of the document
    /// version `tag` refers to).
    pub additional_edits: Vec<TextEdit>,
    /// Label details, if the answer has them.
    pub label_details: Option<LabelDetails>,
    /// LSP document version at the time of the request.
    pub doc_version: i32,
    /// Caller tag of that text.
    pub tag: Option<u64>,
}

/// A server's completion options (from `initialize`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletionCapabilities {
    /// Typing one of these requests completion.
    pub trigger_characters: Vec<char>,
    /// Commit characters that apply to every item without its own.
    pub all_commit_characters: Vec<char>,
    /// `completionItem/resolve` is supported.
    pub resolve: bool,
}

impl CompletionCapabilities {
    /// Whether typing `c` should request completion.
    pub fn is_trigger(&self, c: char) -> bool {
        self.trigger_characters.contains(&c)
    }
}

/// A server's signature help options (from `initialize`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignatureHelpCapabilities {
    /// Typing one of these requests signature help.
    pub trigger_characters: Vec<char>,
    /// Typing one of these while signature help is open requests it again.
    pub retrigger_characters: Vec<char>,
}

fn chars_of(v: Option<&Value>) -> Vec<char> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().and_then(|s| s.chars().next()))
                .collect()
        })
        .unwrap_or_default()
}

/// `completionProvider` of an `initialize` result's capabilities.
pub(crate) fn completion_capabilities(caps: &Value) -> Option<CompletionCapabilities> {
    let p = caps.get("completionProvider")?;
    if !p.is_object() && p.as_bool() != Some(true) {
        return None;
    }
    Some(CompletionCapabilities {
        trigger_characters: chars_of(p.get("triggerCharacters")),
        all_commit_characters: chars_of(p.get("allCommitCharacters")),
        resolve: p.get("resolveProvider").and_then(Value::as_bool) == Some(true),
    })
}

/// `signatureHelpProvider` of an `initialize` result's capabilities.
pub(crate) fn signature_capabilities(caps: &Value) -> Option<SignatureHelpCapabilities> {
    let p = caps.get("signatureHelpProvider")?;
    if !p.is_object() && p.as_bool() != Some(true) {
        return None;
    }
    Some(SignatureHelpCapabilities {
        trigger_characters: chars_of(p.get("triggerCharacters")),
        retrigger_characters: chars_of(p.get("retriggerCharacters")),
    })
}

/// Adds the completion and signature help capabilities to the client
/// capabilities sent at `initialize`. Only what Lector implements is
/// declared: snippets only when `snippets` (the `editor.completion.snippets`
/// setting) is on.
pub(crate) fn extend_client_capabilities(capabilities: &mut Value, snippets: bool) {
    let Some(text_document) = capabilities
        .get_mut("textDocument")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    text_document.insert(
        "completion".into(),
        json!({
            "dynamicRegistration": false,
            "contextSupport": true,
            "insertTextMode": 2,
            "completionItem": {
                "snippetSupport": snippets,
                "commitCharactersSupport": true,
                "documentationFormat": ["markdown", "plaintext"],
                "deprecatedSupport": true,
                "preselectSupport": true,
                "tagSupport": {"valueSet": [1]},
                "insertReplaceSupport": true,
                "resolveSupport": {"properties": ["documentation", "detail", "additionalTextEdits"]},
                "insertTextModeSupport": {"valueSet": [1, 2]},
                "labelDetailsSupport": true
            },
            "completionItemKind": {"valueSet": (1..=25).collect::<Vec<i32>>()},
            "completionList": {
                "itemDefaults": ["commitCharacters", "editRange", "insertTextFormat",
                                 "insertTextMode", "data"]
            }
        }),
    );
    text_document.insert(
        "signatureHelp".into(),
        json!({
            "dynamicRegistration": false,
            "contextSupport": true,
            "signatureInformation": {
                "documentationFormat": ["markdown", "plaintext"],
                "parameterInformation": {"labelOffsetSupport": true},
                "activeParameterSupport": true
            }
        }),
    );
}

/// The `params` of `textDocument/completion`.
pub(crate) fn completion_params(uri: &str, request: &CompletionRequest) -> Value {
    let context = match request.trigger {
        CompletionTrigger::Invoked => json!({"triggerKind": 1}),
        CompletionTrigger::TriggerCharacter(c) => {
            json!({"triggerKind": 2, "triggerCharacter": c.to_string()})
        }
        CompletionTrigger::Incomplete => json!({"triggerKind": 3}),
    };
    json!({
        "textDocument": {"uri": uri},
        "position": {"line": request.pos.row, "character": request.pos.column},
        "context": context,
    })
}

#[derive(Default)]
struct Defaults {
    commit_characters: Option<Vec<char>>,
    edit_range: Option<(Range<PointUtf16>, Range<PointUtf16>)>,
    format: Option<InsertFormat>,
    mode: Option<InsertTextMode>,
    data: Option<Value>,
}

fn format_of(v: Option<&Value>) -> Option<InsertFormat> {
    match v.and_then(Value::as_i64)? {
        2 => Some(InsertFormat::Snippet),
        1 => Some(InsertFormat::PlainText),
        _ => None,
    }
}

fn mode_of(v: Option<&Value>) -> Option<InsertTextMode> {
    match v.and_then(Value::as_i64)? {
        1 => Some(InsertTextMode::AsIs),
        2 => Some(InsertTextMode::AdjustIndentation),
        _ => None,
    }
}

/// `Range | { insert, replace }` (the `editRange` of `itemDefaults`) as (insert, replace).
fn edit_ranges(v: &Value) -> Option<(Range<PointUtf16>, Range<PointUtf16>)> {
    if v.get("insert").is_some() {
        let insert = convert::json_range(v.get("insert")?)?;
        let replace = convert::json_range(v.get("replace")?)?;
        Some((insert, replace))
    } else {
        let r = convert::json_range(v)?;
        Some((r.clone(), r))
    }
}

/// `TextEdit | InsertReplaceEdit` of an item as (insert, replace).
fn item_edit_ranges(te: &Value) -> Option<(Range<PointUtf16>, Range<PointUtf16>)> {
    match te.get("range") {
        Some(range) => edit_ranges(range),
        None => edit_ranges(te),
    }
}

fn parse_defaults(v: Option<&Value>) -> Defaults {
    let Some(v) = v else {
        return Defaults::default();
    };
    Defaults {
        commit_characters: v.get("commitCharacters").map(|c| chars_of(Some(c))),
        edit_range: v.get("editRange").and_then(edit_ranges),
        format: format_of(v.get("insertTextFormat")),
        mode: mode_of(v.get("insertTextMode")),
        data: v.get("data").filter(|d| !d.is_null()).cloned(),
    }
}

fn string_of(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn label_details(v: Option<&Value>) -> Option<LabelDetails> {
    let v = v?;
    let details = LabelDetails {
        detail: string_of(v, "detail"),
        description: string_of(v, "description"),
    };
    (details.detail.is_some() || details.description.is_some()).then_some(details)
}

fn parse_item(mut raw: Value, defaults: &Defaults, all_commit: &[char]) -> Option<CompletionItem> {
    let label = raw.get("label")?.as_str()?.to_string();
    if label.is_empty() {
        return None;
    }
    let format = format_of(raw.get("insertTextFormat"))
        .or(defaults.format)
        .unwrap_or(InsertFormat::PlainText);
    let insert_mode = mode_of(raw.get("insertTextMode"))
        .or(defaults.mode)
        .unwrap_or(InsertTextMode::AdjustIndentation);
    let edit = match raw.get("textEdit") {
        Some(te) if te.is_object() => {
            let new_text = te.get("newText").and_then(Value::as_str)?.to_string();
            let (insert, replace) = item_edit_ranges(te)?;
            Some(ItemEdit {
                insert,
                replace,
                new_text,
            })
        }
        _ => defaults
            .edit_range
            .clone()
            .map(|(insert, replace)| ItemEdit {
                insert,
                replace,
                new_text: string_of(&raw, "textEditText").unwrap_or_else(|| label.clone()),
            }),
    };
    let deprecated = raw.get("deprecated").and_then(Value::as_bool) == Some(true)
        || raw
            .get("tags")
            .and_then(Value::as_array)
            .is_some_and(|t| t.iter().any(|t| t.as_i64() == Some(1)));
    let commit_characters = match raw.get("commitCharacters") {
        Some(c) => chars_of(Some(c)),
        None => defaults
            .commit_characters
            .clone()
            .unwrap_or_else(|| all_commit.to_vec()),
    };
    let additional_edits = match raw.get("additionalTextEdits") {
        Some(v) if !v.is_null() => convert::text_edits(v).unwrap_or_default(),
        _ => vec![],
    };
    let item = CompletionItem {
        label_details: label_details(raw.get("labelDetails")),
        kind: CompletionKind::from_lsp(raw.get("kind").and_then(Value::as_i64).unwrap_or(0)),
        detail: string_of(&raw, "detail"),
        documentation: raw.get("documentation").and_then(Documentation::from_json),
        deprecated,
        preselect: raw.get("preselect").and_then(Value::as_bool) == Some(true),
        sort_text: string_of(&raw, "sortText"),
        filter_text: string_of(&raw, "filterText"),
        insert_text: raw
            .get("insertText")
            .and_then(Value::as_str)
            .map(str::to_string),
        format,
        insert_mode,
        edit,
        additional_edits,
        commit_characters,
        command: raw
            .get("command")
            .filter(|c| c.is_object())
            .and_then(convert::command),
        resolved: false,
        label,
        raw: Value::Null,
    };
    if raw.get("data").is_none_or(Value::is_null) {
        if let (Some(data), Some(obj)) = (&defaults.data, raw.as_object_mut()) {
            obj.insert("data".into(), data.clone());
        }
    }
    Some(CompletionItem { raw, ..item })
}

/// Parses a `textDocument/completion` result (`null`, `CompletionItem[]` or a
/// `CompletionList` with `itemDefaults`) into items and `isIncomplete`.
/// `all_commit` are the server's `allCommitCharacters`.
pub(crate) fn parse_completion(
    v: Value,
    all_commit: &[char],
) -> Result<(Vec<CompletionItem>, bool)> {
    match v {
        Value::Null => Ok((vec![], false)),
        Value::Array(items) => Ok((
            items
                .into_iter()
                .filter_map(|i| parse_item(i, &Defaults::default(), all_commit))
                .collect(),
            false,
        )),
        Value::Object(mut o) => {
            let defaults = parse_defaults(o.get("itemDefaults"));
            let incomplete = o.get("isIncomplete").and_then(Value::as_bool) == Some(true);
            let items = match o.remove("items") {
                Some(Value::Array(a)) => a,
                Some(Value::Null) | None => vec![],
                Some(_) => {
                    return Err(LspError::Protocol(
                        "completion items must be an array".into(),
                    ))
                }
            };
            Ok((
                items
                    .into_iter()
                    .filter_map(|i| parse_item(i, &defaults, all_commit))
                    .collect(),
                incomplete,
            ))
        }
        _ => Err(LspError::Protocol(
            "unexpected completion result".to_string(),
        )),
    }
}

/// Parses a `completionItem/resolve` answer.
pub(crate) fn parse_resolved(v: &Value) -> ResolvedCompletion {
    ResolvedCompletion {
        detail: string_of(v, "detail"),
        documentation: v.get("documentation").and_then(Documentation::from_json),
        additional_edits: v
            .get("additionalTextEdits")
            .filter(|e| !e.is_null())
            .and_then(|e| convert::text_edits(e).ok())
            .unwrap_or_default(),
        label_details: label_details(v.get("labelDetails")),
        doc_version: 0,
        tag: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(row: u32, column: u32) -> PointUtf16 {
        PointUtf16 { row, column }
    }

    #[test]
    fn plain_array_and_label_only_items() {
        let (items, incomplete) = parse_completion(
            json!([{"label": "foo"}, {"label": "bar", "kind": 3, "detail": "fn()",
                    "insertText": "bar()", "deprecated": true}, {"nolabel": 1}]),
            &[],
        )
        .unwrap();
        assert!(!incomplete);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "foo");
        assert_eq!(items[0].kind, CompletionKind::Other);
        assert!(items[0].edit.is_none() && items[0].insert_text.is_none());
        assert_eq!(items[1].kind, CompletionKind::Function);
        assert_eq!(items[1].insert_text.as_deref(), Some("bar()"));
        assert!(items[1].deprecated);
        assert_eq!(items[1].insert_mode, InsertTextMode::AdjustIndentation);
    }

    #[test]
    fn text_edit_and_insert_replace_edit() {
        let (items, _) = parse_completion(
            json!({"isIncomplete": true, "items": [
                {"label": "a", "textEdit": {"range": {"start": {"line": 1, "character": 2},
                    "end": {"line": 1, "character": 4}}, "newText": "alpha"}},
                {"label": "b", "textEdit": {"newText": "beta",
                    "insert": {"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 4}},
                    "replace": {"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 7}}}}]}),
            &[],
        )
        .unwrap();
        let a = items[0].edit.as_ref().unwrap();
        assert_eq!(
            (a.insert.clone(), a.replace.clone()),
            (pt(1, 2)..pt(1, 4), pt(1, 2)..pt(1, 4))
        );
        let b = items[1].edit.as_ref().unwrap();
        assert_eq!(b.insert, pt(1, 2)..pt(1, 4));
        assert_eq!(b.replace, pt(1, 2)..pt(1, 7));
        assert_eq!(b.new_text, "beta");
    }

    #[test]
    fn item_defaults_apply_to_items_without_their_own() {
        let range =
            json!({"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 3}});
        let (items, incomplete) = parse_completion(
            json!({"isIncomplete": false,
                "itemDefaults": {"commitCharacters": [".", "("], "editRange": range,
                                 "insertTextFormat": 2, "insertTextMode": 1, "data": {"d": 1}},
                "items": [
                    {"label": "x"},
                    {"label": "y", "textEditText": "why", "insertTextFormat": 1,
                     "commitCharacters": [";"], "data": {"own": true}},
                    {"label": "z", "textEdit": {"range": range, "newText": "zed"}}]}),
            &['!'],
        )
        .unwrap();
        assert!(!incomplete);
        assert_eq!(items[0].commit_characters, vec!['.', '(']);
        assert_eq!(items[0].format, InsertFormat::Snippet);
        assert_eq!(items[0].insert_mode, InsertTextMode::AsIs);
        let e = items[0].edit.as_ref().unwrap();
        assert_eq!(
            (e.replace.clone(), e.new_text.as_str()),
            (pt(0, 1)..pt(0, 3), "x")
        );
        assert_eq!(items[0].raw["data"], json!({"d": 1}));
        assert_eq!(items[1].edit.as_ref().unwrap().new_text, "why");
        assert_eq!(items[1].format, InsertFormat::PlainText);
        assert_eq!(items[1].commit_characters, vec![';']);
        assert_eq!(items[1].raw["data"], json!({"own": true}));
        assert_eq!(items[2].edit.as_ref().unwrap().new_text, "zed");
    }

    #[test]
    fn server_wide_commit_characters_are_the_last_default() {
        let (items, _) = parse_completion(
            json!([{"label": "a"}, {"label": "b", "commitCharacters": []}]),
            &['.'],
        )
        .unwrap();
        assert_eq!(items[0].commit_characters, vec!['.']);
        assert!(items[1].commit_characters.is_empty());
    }

    #[test]
    fn documentation_tags_label_details() {
        let (items, _) = parse_completion(
            json!([{"label": "a", "tags": [1], "documentation": {"kind": "markdown", "value": "**d**"},
                    "labelDetails": {"detail": "(x)", "description": "mod"}},
                   {"label": "b", "documentation": "plain"}]),
            &[],
        )
        .unwrap();
        assert!(items[0].deprecated);
        let d = items[0].documentation.as_ref().unwrap();
        assert!(d.markdown && d.value == "**d**");
        assert_eq!(
            items[0].label_details.as_ref().unwrap().detail.as_deref(),
            Some("(x)")
        );
        assert!(!items[1].documentation.as_ref().unwrap().markdown);
    }

    #[test]
    fn bad_results() {
        assert!(parse_completion(json!(3), &[]).is_err());
        assert!(parse_completion(json!({"items": 3}), &[]).is_err());
        assert_eq!(parse_completion(Value::Null, &[]).unwrap().0.len(), 0);
    }

    #[test]
    fn capabilities_and_client_declaration() {
        let caps = json!({"completionProvider": {"triggerCharacters": [".", "::"],
                "allCommitCharacters": [";"], "resolveProvider": true},
            "signatureHelpProvider": {"triggerCharacters": ["("], "retriggerCharacters": [","]}});
        let c = completion_capabilities(&caps).unwrap();
        assert_eq!(c.trigger_characters, vec!['.', ':']);
        assert!(c.resolve && c.is_trigger('.') && !c.is_trigger(','));
        let s = signature_capabilities(&caps).unwrap();
        assert_eq!(
            (s.trigger_characters, s.retrigger_characters),
            (vec!['('], vec![','])
        );
        assert!(completion_capabilities(&json!({})).is_none());

        let mut client = json!({"textDocument": {}});
        extend_client_capabilities(&mut client, false);
        let item = &client["textDocument"]["completion"]["completionItem"];
        assert_eq!(item["snippetSupport"], json!(false));
        assert_eq!(item["insertReplaceSupport"], json!(true));
        let mut client = json!({"textDocument": {}});
        extend_client_capabilities(&mut client, true);
        assert_eq!(
            client["textDocument"]["completion"]["completionItem"]["snippetSupport"],
            json!(true)
        );
        assert_eq!(
            client["textDocument"]["signatureHelp"]["signatureInformation"]["parameterInformation"]
                ["labelOffsetSupport"],
            json!(true)
        );
    }

    #[test]
    fn request_params() {
        let p = completion_params(
            "file:///a",
            &CompletionRequest {
                pos: pt(2, 5),
                trigger: CompletionTrigger::TriggerCharacter('.'),
            },
        );
        assert_eq!(p["position"], json!({"line": 2, "character": 5}));
        assert_eq!(
            p["context"],
            json!({"triggerKind": 2, "triggerCharacter": "."})
        );
        let p = completion_params(
            "file:///a",
            &CompletionRequest {
                pos: pt(0, 0),
                trigger: CompletionTrigger::Incomplete,
            },
        );
        assert_eq!(p["context"]["triggerKind"], json!(3));
    }
}
