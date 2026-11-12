//! Signature help: protocol data and the UI-free model of the popup.

use std::ops::Range;

use serde_json::{json, Value};

use crate::completion::{Documentation, SignatureHelpCapabilities};
use crate::PointUtf16;

/// Why signature help is requested (LSP `SignatureHelpTriggerKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureTrigger {
    /// The manual command (kind 1).
    Invoked,
    /// A trigger or retrigger character was typed (kind 2).
    TriggerCharacter(char),
    /// The text or caret changed while the popup is open (kind 3).
    ContentChange,
}

/// A `textDocument/signatureHelp` request.
#[derive(Debug, Clone, PartialEq)]
pub struct SignatureRequest {
    /// The caret, UTF-16 (the document must be synced with that text).
    pub pos: PointUtf16,
    /// Why.
    pub trigger: SignatureTrigger,
    /// The help currently shown, if any (`isRetrigger` is true then, and the
    /// server gets it back as `activeSignatureHelp`).
    pub active: Option<SignatureHelp>,
}

/// One parameter of a signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterInfo {
    /// The parameter's text as the server labelled it (the substring of the
    /// signature label when the server gave offsets).
    pub label: String,
    /// Where the parameter sits in [`SignatureInfo::label`] (byte range on
    /// char boundaries), for highlighting. `None` when the server's label
    /// does not occur in the signature or its offsets are invalid.
    pub range: Option<Range<usize>>,
    /// Documentation of the parameter.
    pub documentation: Option<Documentation>,
}

/// One overload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureInfo {
    /// The full signature text.
    pub label: String,
    /// Documentation of the signature.
    pub documentation: Option<Documentation>,
    /// Parameters in order.
    pub parameters: Vec<ParameterInfo>,
    /// This signature's active parameter, overriding the help's.
    pub active_parameter: Option<usize>,
}

/// The server's signature help.
#[derive(Debug, Clone, PartialEq)]
pub struct SignatureHelp {
    /// Overloads (never empty).
    pub signatures: Vec<SignatureInfo>,
    /// The overload the server considers active (clamped into range).
    pub active_signature: usize,
    /// The server's active parameter (before the per-signature override).
    pub active_parameter: Option<usize>,
    /// The server's JSON, sent back as `activeSignatureHelp`.
    pub raw: Value,
}

/// Byte offset of UTF-16 offset `units` in `s`; `None` past the end or inside
/// a surrogate pair.
fn utf16_to_byte(s: &str, units: usize) -> Option<usize> {
    let mut u = 0usize;
    for (i, c) in s.char_indices() {
        if u == units {
            return Some(i);
        }
        if u > units {
            return None;
        }
        u += c.len_utf16();
    }
    (u == units).then_some(s.len())
}

fn parse_signature(v: &Value) -> Option<SignatureInfo> {
    let label = v.get("label")?.as_str()?.to_string();
    let mut search_from = 0usize;
    let parameters = v
        .get("parameters")
        .and_then(Value::as_array)
        .map(|params| {
            params
                .iter()
                .map(|p| {
                    let (text, range) = match p.get("label") {
                        Some(Value::String(s)) => {
                            // Substring of the signature label: look after the previous
                            // parameter first (names may repeat), then anywhere.
                            let found = label[search_from..]
                                .find(s.as_str())
                                .map(|i| i + search_from)
                                .or_else(|| label.find(s.as_str()));
                            let range = found.filter(|_| !s.is_empty()).map(|i| i..i + s.len());
                            (s.clone(), range)
                        }
                        Some(Value::Array(a)) if a.len() == 2 => {
                            let start = a[0].as_u64().map(|n| n as usize);
                            let end = a[1].as_u64().map(|n| n as usize);
                            let range = start.zip(end).filter(|(s, e)| s < e).and_then(|(s, e)| {
                                Some(utf16_to_byte(&label, s)?..utf16_to_byte(&label, e)?)
                            });
                            let text = range
                                .as_ref()
                                .map(|r| label[r.clone()].to_string())
                                .unwrap_or_default();
                            (text, range)
                        }
                        _ => (String::new(), None),
                    };
                    if let Some(r) = &range {
                        search_from = r.end;
                    }
                    ParameterInfo {
                        label: text,
                        range,
                        documentation: p.get("documentation").and_then(Documentation::from_json),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    Some(SignatureInfo {
        label,
        documentation: v.get("documentation").and_then(Documentation::from_json),
        parameters,
        active_parameter: v
            .get("activeParameter")
            .and_then(Value::as_u64)
            .map(|n| n as usize),
    })
}

/// Parses a `textDocument/signatureHelp` result; `None` for `null` or no signatures.
pub(crate) fn parse_signature_help(v: Value) -> Option<SignatureHelp> {
    let signatures: Vec<SignatureInfo> = v
        .get("signatures")?
        .as_array()?
        .iter()
        .filter_map(parse_signature)
        .collect();
    if signatures.is_empty() {
        return None;
    }
    let active_signature = v
        .get("activeSignature")
        .and_then(Value::as_u64)
        .map_or(0, |n| n as usize);
    let active_signature = if active_signature < signatures.len() {
        active_signature
    } else {
        0
    };
    Some(SignatureHelp {
        active_parameter: v
            .get("activeParameter")
            .and_then(Value::as_u64)
            .map(|n| n as usize),
        signatures,
        active_signature,
        raw: v,
    })
}

/// The `params` of `textDocument/signatureHelp`.
pub(crate) fn signature_params(uri: &str, request: &SignatureRequest) -> Value {
    let mut context = match request.trigger {
        SignatureTrigger::Invoked => json!({"triggerKind": 1}),
        SignatureTrigger::TriggerCharacter(c) => {
            json!({"triggerKind": 2, "triggerCharacter": c.to_string()})
        }
        SignatureTrigger::ContentChange => json!({"triggerKind": 3}),
    };
    context["isRetrigger"] = Value::Bool(request.active.is_some());
    if let Some(active) = &request.active {
        context["activeSignatureHelp"] = active.raw.clone();
    }
    json!({
        "textDocument": {"uri": uri},
        "position": {"line": request.pos.row, "character": request.pos.column},
        "context": context,
    })
}

/// What the popup shows for the active overload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSignature<'a> {
    /// The signature.
    pub signature: &'a SignatureInfo,
    /// 0-based index of this overload.
    pub index: usize,
    /// Number of overloads.
    pub count: usize,
    /// Index of the active parameter, if any (in range).
    pub parameter: Option<usize>,
    /// Byte range in `signature.label` to highlight.
    pub highlight: Option<Range<usize>>,
    /// Documentation of the active parameter.
    pub parameter_documentation: Option<&'a Documentation>,
}

/// The signature help popup model. The UI sends [`SignatureSession::request`]
/// through `LspManager::signature_help`, feeds the answer to
/// [`SignatureSession::set_help`] and renders [`SignatureSession::active`].
#[derive(Debug, Default)]
pub struct SignatureSession {
    help: Option<SignatureHelp>,
    /// Overload picked by the user (Up/Down in the popup), kept across answers
    /// as long as the same signature label is still offered.
    picked_label: Option<String>,
    selected: usize,
}

impl SignatureSession {
    /// A closed session.
    pub fn new() -> SignatureSession {
        SignatureSession::default()
    }

    /// Whether a popup should be visible.
    pub fn is_open(&self) -> bool {
        self.help.is_some()
    }

    /// Closes the popup (Escape, caret left the call).
    pub fn close(&mut self) {
        self.help = None;
        self.picked_label = None;
        self.selected = 0;
    }

    /// The request for `trigger` at `pos`: a retrigger (with the current help
    /// and the overload the user picked) when the popup is open.
    pub fn request(&self, pos: PointUtf16, trigger: SignatureTrigger) -> SignatureRequest {
        let active = self.help.as_ref().map(|h| {
            let mut h = h.clone();
            h.active_signature = self.selected.min(h.signatures.len().saturating_sub(1));
            if let Some(raw) = h.raw.as_object_mut() {
                raw.insert("activeSignature".into(), json!(h.active_signature));
            }
            h
        });
        SignatureRequest {
            pos,
            trigger,
            active,
        }
    }

    /// Whether typing `c` should request signature help: always for a trigger
    /// character, for a retrigger character only while the popup is open.
    pub fn trigger_for_typed(
        &self,
        caps: &SignatureHelpCapabilities,
        c: char,
    ) -> Option<SignatureTrigger> {
        let hit = caps.trigger_characters.contains(&c)
            || (self.is_open() && caps.retrigger_characters.contains(&c));
        hit.then_some(SignatureTrigger::TriggerCharacter(c))
    }

    /// Takes the server's answer: `None` closes the popup. The overload the
    /// user picked stays selected while the server still offers it.
    pub fn set_help(&mut self, help: Option<SignatureHelp>) {
        let Some(help) = help else {
            self.close();
            return;
        };
        let kept = self
            .picked_label
            .as_ref()
            .and_then(|l| help.signatures.iter().position(|s| &s.label == l));
        self.selected = kept.unwrap_or(help.active_signature);
        if kept.is_none() {
            self.picked_label = None;
        }
        self.help = Some(help);
    }

    fn pick(&mut self, index: usize) {
        if let Some(h) = &self.help {
            self.selected = index;
            self.picked_label = Some(h.signatures[index].label.clone());
        }
    }

    /// Shows the next overload (wraps around).
    pub fn next_signature(&mut self) {
        if let Some(count) = self.help.as_ref().map(|h| h.signatures.len()) {
            self.pick((self.selected + 1) % count);
        }
    }

    /// Shows the previous overload (wraps around).
    pub fn previous_signature(&mut self) {
        if let Some(count) = self.help.as_ref().map(|h| h.signatures.len()) {
            self.pick((self.selected + count - 1) % count);
        }
    }

    /// The overload to show with its active parameter and highlight range.
    pub fn active(&self) -> Option<ActiveSignature<'_>> {
        let help = self.help.as_ref()?;
        let index = self.selected.min(help.signatures.len() - 1);
        let signature = &help.signatures[index];
        let parameter = signature
            .active_parameter
            .or(help.active_parameter)
            .filter(|p| *p < signature.parameters.len());
        let param = parameter.map(|p| &signature.parameters[p]);
        Some(ActiveSignature {
            signature,
            index,
            count: help.signatures.len(),
            parameter,
            highlight: param.and_then(|p| p.range.clone()),
            parameter_documentation: param.and_then(|p| p.documentation.as_ref()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn help(v: Value) -> SignatureHelp {
        parse_signature_help(v).expect("help")
    }

    #[test]
    fn string_labels_are_located_in_order() {
        let h = help(
            json!({"signatures": [{"label": "fn f(a: i32, a2: i32, a: u8)",
            "parameters": [{"label": "a: i32"}, {"label": "a2: i32"}, {"label": "a: u8"}]}],
            "activeParameter": 2}),
        );
        let ranges: Vec<_> = h.signatures[0]
            .parameters
            .iter()
            .map(|p| p.range.clone())
            .collect();
        assert_eq!(ranges, vec![Some(5..11), Some(13..20), Some(22..27)]);
        let mut s = SignatureSession::new();
        s.set_help(Some(h));
        let a = s.active().unwrap();
        assert_eq!(a.parameter, Some(2));
        assert_eq!(&a.signature.label[a.highlight.unwrap()], "a: u8");
    }

    #[test]
    fn offset_labels_are_utf16_and_become_byte_ranges() {
        // "fn é(x: i32, 😀: u8)": é is 1 UTF-16 unit (2 bytes), 😀 is 2 units (4 bytes).
        let label = "fn é(x: i32, 😀: u8)";
        let h = help(json!({"signatures": [{"label": label,
            "parameters": [{"label": [5, 11]}, {"label": [13, 19]}]}], "activeParameter": 1}));
        let p = &h.signatures[0].parameters;
        assert_eq!(p[0].label, "x: i32");
        assert_eq!(p[1].label, "😀: u8");
        assert_eq!(&label[p[1].range.clone().unwrap()], "😀: u8");
        // Offsets splitting a surrogate pair or out of range are rejected.
        let h = help(json!({"signatures": [{"label": label,
            "parameters": [{"label": [14, 15]}, {"label": [3, 99]}, {"label": [4, 4]}]}]}));
        assert!(h.signatures[0].parameters.iter().all(|p| p.range.is_none()));
    }

    #[test]
    fn active_parameter_rules() {
        let sigs = json!([
            {"label": "f(a, b)", "parameters": [{"label": "a"}, {"label": "b"}]},
            {"label": "f(a)", "parameters": [{"label": "a"}], "activeParameter": 0}]);
        let mut s = SignatureSession::new();
        s.set_help(Some(help(
            json!({"signatures": sigs, "activeSignature": 0, "activeParameter": 1}),
        )));
        assert_eq!(s.active().unwrap().parameter, Some(1));
        // Second overload: its own activeParameter wins; the help's 1 would be out of range.
        s.next_signature();
        let a = s.active().unwrap();
        assert_eq!((a.index, a.count, a.parameter), (1, 2, Some(0)));
        // Out-of-range active parameter highlights nothing.
        s.set_help(Some(help(
            json!({"signatures": [{"label": "f()", "parameters": []}], "activeParameter": 3}),
        )));
        let a = s.active().unwrap();
        assert!(a.parameter.is_none() && a.highlight.is_none());
        // No parameter at all.
        s.set_help(Some(help(
            json!({"signatures": [{"label": "f(a)", "parameters": [{"label": "a"}]}]}),
        )));
        assert_eq!(s.active().unwrap().parameter, None);
    }

    #[test]
    fn overload_navigation_wraps_and_survives_new_answers() {
        let sigs = json!([{"label": "one"}, {"label": "two"}, {"label": "three"}]);
        let mut s = SignatureSession::new();
        s.set_help(Some(help(
            json!({"signatures": sigs.clone(), "activeSignature": 0}),
        )));
        s.previous_signature();
        assert_eq!(s.active().unwrap().signature.label, "three");
        s.next_signature();
        s.next_signature();
        assert_eq!(s.active().unwrap().index, 1);
        // The server answers again (retrigger) with activeSignature 0: the user's pick stays.
        s.set_help(Some(help(
            json!({"signatures": sigs, "activeSignature": 0}),
        )));
        assert_eq!(s.active().unwrap().signature.label, "two");
        // The retrigger carries the user's pick back to the server.
        let r = s.request(
            PointUtf16 { row: 0, column: 3 },
            SignatureTrigger::ContentChange,
        );
        assert_eq!(r.active.unwrap().active_signature, 1);
        // When the picked overload disappears, the server's choice is used.
        s.set_help(Some(help(
            json!({"signatures": [{"label": "other"}], "activeSignature": 0}),
        )));
        assert_eq!(s.active().unwrap().signature.label, "other");
        s.set_help(None);
        assert!(!s.is_open() && s.active().is_none());
    }

    #[test]
    fn request_context_and_triggers() {
        let mut s = SignatureSession::new();
        let r = s.request(
            PointUtf16 { row: 1, column: 2 },
            SignatureTrigger::TriggerCharacter('('),
        );
        let p = signature_params("file:///a", &r);
        assert_eq!(
            p["context"],
            json!({"triggerKind": 2, "triggerCharacter": "(", "isRetrigger": false})
        );
        assert_eq!(p["position"], json!({"line": 1, "character": 2}));
        let caps = SignatureHelpCapabilities {
            trigger_characters: vec!['('],
            retrigger_characters: vec![',', ')'],
        };
        assert_eq!(
            s.trigger_for_typed(&caps, '('),
            Some(SignatureTrigger::TriggerCharacter('('))
        );
        assert_eq!(
            s.trigger_for_typed(&caps, ','),
            None,
            "retrigger only while open"
        );
        s.set_help(Some(help(json!({"signatures": [{"label": "f(a)"}]}))));
        assert_eq!(
            s.trigger_for_typed(&caps, ','),
            Some(SignatureTrigger::TriggerCharacter(','))
        );
        let r = s.request(
            PointUtf16 { row: 1, column: 4 },
            SignatureTrigger::TriggerCharacter(','),
        );
        let p = signature_params("file:///a", &r);
        assert_eq!(p["context"]["isRetrigger"], json!(true));
        assert_eq!(
            p["context"]["activeSignatureHelp"]["signatures"][0]["label"],
            json!("f(a)")
        );
        let r = s.request(PointUtf16 { row: 1, column: 4 }, SignatureTrigger::Invoked);
        assert_eq!(
            signature_params("u", &r)["context"]["triggerKind"],
            json!(1)
        );
    }

    #[test]
    fn empty_or_null_help_is_none() {
        assert!(parse_signature_help(Value::Null).is_none());
        assert!(parse_signature_help(json!({"signatures": []})).is_none());
        let h = help(json!({"signatures": [{"label": "f()"}], "activeSignature": 9}));
        assert_eq!(h.active_signature, 0);
    }
}
