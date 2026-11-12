//! Signature help in the editor (v0.7): a small popup above the line with the
//! active overload, the active parameter highlighted, an overload counter
//! ("1/3") and the parameter's documentation.
//!
//! One [`SignatureController`] per file tab, created next to its completion
//! controller (`editor::completion`), which forwards code view changes to it.
//! It opens on the server's trigger characters (`(`, `,` ...), after accepting a
//! completion that asks for it and on `editor::TriggerParameterHints`; while
//! open it asks again (`ContentChange`) after edits and caret moves inside the
//! call; it closes on `)` past the call, Escape, the caret leaving the call or
//! the server answering "no signature here". Everything the server says goes
//! through [`lector_lsp::SignatureSession`]; this module is the view and the
//! wiring. Opening needs the setting `editor.signature_help.enabled` except for
//! the explicit command.
//!
//! Keys (rebindable, see `lector-config`): `alt-up` / `alt-down` switch overload
//! while open (`CodeView && signature_help`); with several overloads and no
//! completion list, plain `up` / `down` do too (`signature_overloads`); `escape`
//! closes only this popup.

use std::ops::Range;
use std::time::Duration;

use gpui::{
    div, prelude::*, App, Context, FontWeight, HighlightStyle, IntoElement, Render, SharedString,
    StyledText, Task, WeakEntity, Window,
};
use lector_lsp::{
    CancelToken, LspError, RequestOutcome, SignatureHelp, SignatureSession, SignatureTrigger,
};
use lector_text::Rope;

use super::completion::{caret_state, ensure_synced};
use super::DocumentModel;
use crate::code_view::{popup::DismissHook, CodeView, EditorPopup, Pos, Vertical};
use crate::components::popover::surface;
use crate::components::{h_flex, metrics, v_flex};
use crate::markdown::{self, RenderOptions};
use crate::scale::px;
use crate::settings::AppSettings;
use crate::theme::{ActiveTheme, ColorExt};

/// Popup key in [`CodeView::set_popup`].
pub const POPUP_KEY: &str = "signature_help";
/// Key context flag while the popup is open.
pub const FLAG_OPEN: &str = "signature_help";
/// Key context flag while the open popup has more than one overload.
pub const FLAG_OVERLOADS: &str = "signature_overloads";

/// Delay before the popup asks again after an edit or caret move inside the call.
pub const RETRIGGER_DELAY_MS: u64 = 40;
/// How far back / forward the call's parentheses are searched.
const SCAN_LIMIT: usize = 8 * 1024;
/// Most lines the signature and the parameter documentation take each.
const MAX_LABEL_LINES: usize = 4;
const MAX_DOC_LINES: usize = 3;

/// The byte offset of the `(` of the innermost call that contains `caret`
/// (parentheses only, no strings or comments), searching at most 8 KiB back.
pub fn enclosing_open_paren(text: &Rope, caret: usize) -> Option<usize> {
    let caret = caret.min(text.len());
    let start = caret.saturating_sub(SCAN_LIMIT);
    let start = text.clip_offset(start, lector_text::Bias::Right);
    let slice = text.text_for_range(start..caret);
    let mut depth = 0usize;
    for (i, c) in slice.char_indices().rev() {
        match c {
            ')' => depth += 1,
            '(' if depth == 0 => return Some(start + i),
            '(' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// The byte offset of the `)` that closes the `(` at `open`, if it is within 8 KiB.
pub fn matching_close(text: &Rope, open: usize) -> Option<usize> {
    if open >= text.len() {
        return None;
    }
    let end = (open + SCAN_LIMIT).min(text.len());
    let end = text.clip_offset(end, lector_text::Bias::Left);
    let slice = text.text_for_range(open..end);
    let mut depth = 0usize;
    for (i, c) in slice.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether the caret is still inside the call opened at `open`: after the `(`
/// and not past its `)`. `)` itself is inside: the popup closes once the caret
/// is behind it.
pub fn caret_in_call(text: &Rope, open: usize, caret: usize) -> bool {
    if caret <= open || text.text_for_range(open..(open + 1).min(text.len())) != "(" {
        return false;
    }
    match matching_close(text, open) {
        Some(close) => caret <= close,
        None => true,
    }
}

/// Number of lines `chars` characters take in a box that fits `per_line`
/// (at least 1, at most `max`).
fn lines_for(chars: usize, per_line: usize, max: usize) -> usize {
    chars.div_ceil(per_line.max(1)).clamp(1, max)
}

/// The signature help popup of one file tab. Also the popup's view.
pub struct SignatureController {
    code_view: WeakEntity<CodeView>,
    model: WeakEntity<DocumentModel>,
    session: SignatureSession,
    /// Byte offset of the `(` of the call, when known.
    open_paren: Option<usize>,
    /// Where the popup hangs (the caret).
    anchor: Pos,
    id: u64,
    token: CancelToken,
    request: Option<Task<()>>,
    retrigger: Option<Task<()>>,
    last_key: Option<(u64, usize)>,
    this: WeakEntity<SignatureController>,
}

impl SignatureController {
    pub(crate) fn new(
        code_view: WeakEntity<CodeView>,
        model: WeakEntity<DocumentModel>,
        cx: &mut Context<Self>,
    ) -> Self {
        SignatureController {
            code_view,
            model,
            session: SignatureSession::new(),
            open_paren: None,
            anchor: Pos::new(0, 0),
            id: 0,
            token: CancelToken::new(),
            request: None,
            retrigger: None,
            last_key: None,
            this: cx.weak_entity(),
        }
    }

    /// The popup is showing.
    pub fn is_open(&self) -> bool {
        self.session.is_open()
    }

    /// The active overload, its position and the highlighted parameter text (tests, a11y).
    pub fn active_summary(&self) -> Option<SignatureSummary> {
        let a = self.session.active()?;
        Some(SignatureSummary {
            label: a.signature.label.clone(),
            index: a.index,
            count: a.count,
            parameter: a.parameter,
            highlighted: a
                .highlight
                .as_ref()
                .map(|r| a.signature.label[r.clone()].to_string()),
            parameter_documentation: a.parameter_documentation.map(|d| d.value.clone()),
        })
    }

    /// Shows the next overload.
    pub fn next_signature(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.session.is_open() {
            return false;
        }
        self.session.next_signature();
        self.refresh(cx);
        true
    }

    /// Shows the previous overload.
    pub fn previous_signature(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.session.is_open() {
            return false;
        }
        self.session.previous_signature();
        self.refresh(cx);
        true
    }

    /// Closes the popup (Escape). `false` when it was not open.
    pub fn dismiss(&mut self, cx: &mut Context<Self>) -> bool {
        let was_open = self.session.is_open();
        self.close(cx);
        was_open
    }

    /// Closes the popup and forgets the call.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.token.cancel();
        self.request = None;
        self.retrigger = None;
        self.id += 1;
        let was_open = self.session.is_open();
        self.session.close();
        self.open_paren = None;
        if was_open {
            self.refresh(cx);
        }
    }

    /// `editor::TriggerParameterHints`: ask the server now. The error is a
    /// message for the user.
    pub fn trigger_manual(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        self.start(SignatureTrigger::Invoked, true, cx)
    }

    /// After accepting a completion whose command asks for signature help.
    pub fn trigger_after_accept(&mut self, cx: &mut Context<Self>) {
        if AppSettings::get(cx).editor.signature_help.enabled {
            let _ = self.start(SignatureTrigger::Invoked, false, cx);
        }
    }

    /// The code view changed (an edit, a caret move); `typed` is the text of
    /// the keystroke that caused it, if any. Called by the completion controller.
    pub(crate) fn after_change(&mut self, typed: Option<&str>, cx: &mut Context<Self>) {
        let Some((rope, caret, _)) = caret_state(&self.code_view, cx) else {
            return;
        };
        let key = (rope.version(), caret);
        if typed.is_none() && self.last_key == Some(key) {
            return;
        }
        self.last_key = Some(key);
        let enabled = AppSettings::get(cx).editor.signature_help.enabled;
        if self.session.is_open() {
            // Still inside the call? Edits before the `(` move it: look it up again.
            let open = self
                .open_paren
                .filter(|o| rope.text_for_range(*o..(*o + 1).min(rope.len())) == "(")
                .or_else(|| enclosing_open_paren(&rope, caret));
            match open {
                Some(open) if caret_in_call(&rope, open, caret) => self.open_paren = Some(open),
                _ => {
                    self.close(cx);
                    return;
                }
            }
            if !enabled {
                self.close(cx);
                return;
            }
            let trigger = typed
                .and_then(single_char)
                .and_then(|c| self.typed_trigger(c, cx));
            match trigger {
                Some(trigger) => {
                    let _ = self.start(trigger, false, cx);
                }
                None => self.schedule_retrigger(cx),
            }
            return;
        }
        if !enabled {
            return;
        }
        if let Some(c) = typed.and_then(single_char) {
            if let Some(trigger) = self.typed_trigger(c, cx) {
                let _ = self.start(trigger, false, cx);
            }
        }
    }

    fn typed_trigger(&self, c: char, cx: &App) -> Option<SignatureTrigger> {
        let caps = self.capabilities(cx)?;
        self.session.trigger_for_typed(&caps, c)
    }

    fn capabilities(&self, cx: &App) -> Option<lector_lsp::SignatureHelpCapabilities> {
        let model = self.model.upgrade()?;
        let m = model.read(cx);
        m.project().lsp()?.capabilities(m.path())?.signature_help
    }

    fn schedule_retrigger(&mut self, cx: &mut Context<Self>) {
        self.retrigger = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(RETRIGGER_DELAY_MS))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.session.is_open() {
                    let _ = this.start(SignatureTrigger::ContentChange, false, cx);
                }
            });
        }));
    }

    /// Sends a request now. `explicit`: the user asked (errors come back as a message).
    fn start(
        &mut self,
        trigger: SignatureTrigger,
        explicit: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.start_with(trigger, explicit, 0, cx)
    }

    fn start_with(
        &mut self,
        trigger: SignatureTrigger,
        explicit: bool,
        retries: u8,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.retrigger = None;
        let (Some(code_view), Some(model)) = (self.code_view.upgrade(), self.model.upgrade())
        else {
            return Ok(());
        };
        if !code_view.read(cx).is_editable() {
            return if explicit {
                Err("This tab is read-only".into())
            } else {
                Ok(())
            };
        }
        let (path, lsp) = {
            let m = model.read(cx);
            (m.path().to_path_buf(), m.project().lsp().cloned())
        };
        let Some(lsp) = lsp else {
            return if explicit {
                Err("No language server is running for this file".into())
            } else {
                Ok(())
            };
        };
        match lsp.capabilities(&path) {
            Some(caps) if caps.signature_help.is_some() => {}
            Some(_) => {
                return if explicit {
                    Err("The language server offers no signature help".into())
                } else {
                    Ok(())
                };
            }
            None => {
                return if explicit {
                    Err("The language server is not ready yet".into())
                } else {
                    Ok(())
                };
            }
        }
        let Some(rope) = ensure_synced(&model, cx) else {
            return Ok(());
        };
        let Some((_, caret, _)) = caret_state(&self.code_view, cx) else {
            return Ok(());
        };
        let pos = crate::lsp::point_for_offset(&rope, caret);
        let request = self.session.request(pos, trigger);
        self.token.cancel();
        self.token = CancelToken::new();
        self.id += 1;
        let id = self.id;
        let future = lsp.signature_help(&path, request, Some(&self.token));
        self.request = Some(cx.spawn(async move |this, cx| {
            let outcome = future.await;
            let _ = this.update(cx, |this, cx| {
                this.on_response(id, trigger, retries, outcome, cx)
            });
        }));
        Ok(())
    }

    fn on_response(
        &mut self,
        id: u64,
        trigger: SignatureTrigger,
        retries: u8,
        outcome: Result<RequestOutcome<Option<SignatureHelp>>, LspError>,
        cx: &mut Context<Self>,
    ) {
        if id != self.id {
            return;
        }
        match outcome {
            Ok(RequestOutcome::Done(help)) => {
                let had_call = self.open_paren.is_some();
                self.session.set_help(help);
                if !self.session.is_open() {
                    self.close(cx);
                    return;
                }
                if !had_call || self.open_paren.is_none() {
                    if let Some((rope, caret, _)) = caret_state(&self.code_view, cx) {
                        self.open_paren = enclosing_open_paren(&rope, caret);
                    }
                }
                self.refresh(cx);
            }
            Ok(RequestOutcome::Stale) if retries < 2 => {
                // The text moved on between the sync and the answer: ask again.
                let _ = self.start_with(trigger, false, retries + 1, cx);
            }
            Ok(RequestOutcome::Stale | RequestOutcome::Cancelled) => {}
            Ok(RequestOutcome::Unavailable) => self.close(cx),
            Err(err) => {
                tracing::debug!(target: "lector_ui::lsp", "signature help failed: {err}");
                self.close(cx);
            }
        }
    }

    /// Updates the popup, its key context flags and its anchor.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(code_view) = self.code_view.upgrade() else {
            return;
        };
        let Some(active) = self.session.active() else {
            code_view.update(cx, |v, cx| {
                v.set_popup(POPUP_KEY, None, cx);
                v.set_context_flag(FLAG_OPEN, false, cx);
                v.set_context_flag(FLAG_OVERLOADS, false, cx);
            });
            cx.notify();
            return;
        };
        let overloads = active.count > 1;
        let size = self.popup_size(cx);
        if let Some((_, _, pos)) = caret_state(&self.code_view, cx) {
            self.anchor = pos;
        }
        let anchor = self.anchor;
        let view = cx.entity();
        let weak = self.this.clone();
        let on_dismiss: DismissHook = std::rc::Rc::new(move |_, cx: &mut App| {
            if let Some(this) = weak.upgrade() {
                this.update(cx, |this, cx| this.close(cx));
            }
        });
        code_view.update(cx, |v, cx| {
            v.set_context_flag(FLAG_OPEN, true, cx);
            v.set_context_flag(FLAG_OVERLOADS, overloads, cx);
            v.set_popup(
                POPUP_KEY,
                Some(EditorPopup {
                    view: view.into(),
                    anchor,
                    size,
                    prefer: Vertical::Above,
                    on_dismiss: Some(on_dismiss),
                }),
                cx,
            );
        });
        cx.notify();
    }

    /// Exact size of the popup for the active overload.
    fn popup_size(&self, cx: &App) -> gpui::Size<gpui::Pixels> {
        let Some(active) = self.session.active() else {
            return gpui::size(px(0.), px(0.));
        };
        let editor = crate::code_view::EditorSettings::get(cx);
        let char_w = editor.font_size * 0.6;
        let width = (char_w * (active.signature.label.chars().count() + 12) as f32 + px(24.))
            .clamp(px(260.), px(640.));
        let per_line = ((width - px(24.)) / char_w).floor().max(1.) as usize;
        let label_lines = lines_for(
            active.signature.label.chars().count(),
            per_line,
            MAX_LABEL_LINES,
        );
        let line_h = editor.font_size * 1.45;
        let mut height = line_h * label_lines as f32 + px(14.);
        if let Some(doc) = active.parameter_documentation {
            let ui_char_w = metrics::text_sm() * 0.55;
            let per = ((width - px(24.)) / ui_char_w).floor().max(1.) as usize;
            let lines = lines_for(doc.value.chars().count(), per, MAX_DOC_LINES);
            height += metrics::text_sm() * 1.5 * lines as f32 + px(9.);
        }
        gpui::size(width, height)
    }
}

/// What the popup shows, for tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureSummary {
    /// The signature text.
    pub label: String,
    /// 0-based overload index.
    pub index: usize,
    /// Number of overloads.
    pub count: usize,
    /// Active parameter index.
    pub parameter: Option<usize>,
    /// The highlighted part of the label.
    pub highlighted: Option<String>,
    /// Documentation of the active parameter.
    pub parameter_documentation: Option<String>,
}

fn single_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

/// Byte ranges of `label` to draw as the active parameter (at most one).
fn highlight_ranges(highlight: Option<Range<usize>>, label: &str) -> Vec<Range<usize>> {
    highlight
        .filter(|r| {
            r.start < r.end
                && r.end <= label.len()
                && label.is_char_boundary(r.start)
                && label.is_char_boundary(r.end)
        })
        .into_iter()
        .collect()
}

impl Render for SignatureController {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(active) = self.session.active() else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let editor = crate::code_view::EditorSettings::get(cx);
        let label: SharedString = active.signature.label.clone().into();
        let ranges = highlight_ranges(active.highlight.clone(), &label);
        let highlight = HighlightStyle {
            color: Some(ui.text_accent.hsla()),
            font_weight: Some(FontWeight::BOLD),
            underline: Some(gpui::UnderlineStyle {
                thickness: px(1.),
                color: Some(ui.text_accent.hsla()),
                wavy: false,
            }),
            ..Default::default()
        };
        let styled =
            StyledText::new(label).with_highlights(ranges.into_iter().map(|r| (r, highlight)));
        let counter: Option<SharedString> =
            (active.count > 1).then(|| format!("{}/{}", active.index + 1, active.count).into());
        let doc_blocks = active.parameter_documentation.map(|d| {
            if d.markdown {
                markdown::parse(&d.value)
            } else {
                markdown::parse(&escape_plain(&d.value))
            }
        });
        let docs = doc_blocks.map(|blocks| {
            let opts = RenderOptions {
                base_dir: None,
                on_link: None,
                compact: true,
                id: "signature-docs".into(),
            };
            div()
                .mt(px(4.))
                .pt(px(4.))
                .border_t_1()
                .border_color(ui.border_variant.hsla())
                .text_size(metrics::text_sm())
                .text_color(ui.text_muted.hsla())
                .overflow_hidden()
                .children(markdown::render_blocks(&blocks, &opts, window, cx))
        });
        surface(cx)
            .size_full()
            .child(
                v_flex()
                    .size_full()
                    .px(px(10.))
                    .py(px(6.))
                    .child(
                        h_flex()
                            .items_start()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .font_family(editor.font_family.clone())
                                    .text_size(editor.font_size)
                                    .line_height(editor.font_size * 1.45)
                                    .text_color(ui.text.hsla())
                                    .child(styled),
                            )
                            .children(counter.map(|c| {
                                div()
                                    .flex_none()
                                    .text_size(metrics::text_xs())
                                    .text_color(ui.text_faint.hsla())
                                    .child(c)
                            })),
                    )
                    .children(docs),
            )
            .into_any_element()
    }
}

/// Plain-text documentation as Markdown source (every Markdown character escaped).
fn escape_plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if "\\`*_{}[]<>()#+-.!|~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        Rope::from_str(s)
    }

    #[test]
    fn finds_the_open_paren_of_the_innermost_call() {
        let t = rope("foo(a, bar(b, c), d)");
        // After `d`.
        assert_eq!(enclosing_open_paren(&t, 18), Some(3));
        // Between `b,` and `c`.
        assert_eq!(enclosing_open_paren(&t, 13), Some(10));
        // After the inner call closed.
        assert_eq!(enclosing_open_paren(&t, 16), Some(3));
        // Not in a call.
        assert_eq!(enclosing_open_paren(&t, 2), None);
        assert_eq!(enclosing_open_paren(&rope("(a) + b"), 7), None);
        assert_eq!(enclosing_open_paren(&rope(""), 0), None);
    }

    #[test]
    fn call_extent() {
        let t = rope("f(a, (b))\nnext");
        assert_eq!(matching_close(&t, 1), Some(8));
        assert!(caret_in_call(&t, 1, 2));
        assert!(caret_in_call(&t, 1, 8), "before the closing paren");
        assert!(!caret_in_call(&t, 1, 9), "after the closing paren");
        assert!(
            !caret_in_call(&t, 1, 1),
            "before the first argument position"
        );
        assert!(!caret_in_call(&t, 0, 3), "offset 0 is not a paren");
        // An unclosed call stays open to the end of the scan window.
        let t = rope("f(a, ");
        assert_eq!(matching_close(&t, 1), None);
        assert!(caret_in_call(&t, 1, 5));
    }

    #[test]
    fn multibyte_text_does_not_panic() {
        let t = rope("é(😀, é)\n");
        let open = enclosing_open_paren(&t, "é(😀, ".len());
        assert_eq!(open, Some(2));
        assert!(caret_in_call(&t, 2, "é(😀, ".len()));
        let text = t.to_string();
        for caret in 0..=t.len() {
            if text.is_char_boundary(caret) {
                let _ = enclosing_open_paren(&t, caret);
            }
        }
    }

    #[test]
    fn line_estimates_are_clamped() {
        assert_eq!(lines_for(0, 40, 4), 1);
        assert_eq!(lines_for(41, 40, 4), 2);
        assert_eq!(lines_for(10_000, 40, 4), 4);
        assert_eq!(lines_for(5, 0, 4), 4, "a zero width never divides by zero");
    }

    #[test]
    fn highlight_ranges_reject_bad_ranges() {
        assert_eq!(highlight_ranges(Some(2..4), "abcdef"), vec![2..4]);
        assert!(highlight_ranges(Some(4..4), "abcdef").is_empty());
        assert!(highlight_ranges(Some(2..99), "abcdef").is_empty());
        assert!(
            highlight_ranges(Some(1..2), "é").is_empty(),
            "inside a char"
        );
        assert!(highlight_ranges(None, "abc").is_empty());
    }

    #[test]
    fn plain_docs_are_escaped() {
        assert_eq!(escape_plain("a*b_c"), "a\\*b\\_c");
    }
}
