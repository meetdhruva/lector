//! Dialogs of the sync UI (v0.7), all shown in the workspace's modal layer:
//!
//! * [`NoticeDialog`]: a failure (or a choice) with its explanation, the files involved,
//!   git's or a hook's output in the commit composer's output box, and next-step buttons.
//! * [`PromptDialog`]: a credential prompt from git / ssh (askpass): username, password or
//!   token (masked), SSH key passphrase (masked, key path shown), unknown host key (host,
//!   key type and fingerprint; "Trust and continue" / "Cancel", Cancel is the default and
//!   the answer is the literal `yes`), or the prompt verbatim.
//! * [`InputDialog`]: a name (new branch, rename, stash message) validated as you type, an
//!   optional start point with a ref menu, and checkboxes.

use std::rc::Rc;
use std::sync::mpsc::Sender;

use gpui::{
    div, prelude::*, App, Corner, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, KeyDownEvent, MouseButton, MouseDownEvent, SharedString, Subscription, Window,
};
use lector_process::askpass::{PromptReply, Secret};
use lector_ui::components::context_menu::ContextMenu;
use lector_ui::components::popover::Popover;
use lector_ui::components::text_input::{TextInput, TextInputEvent};
use lector_ui::components::{h_flex, metrics, v_flex, Button, ButtonStyle};
use lector_ui::icons::{Icon, IconName, IconSize};
use lector_ui::scale::px;
use lector_ui::views::Workspace;
use lector_ui::{ActiveTheme, ColorExt};

use crate::sync_model::{PromptDialogKind, PromptSpec, HOST_KEY_TRUST_ANSWER};

/// How a dialog is styled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Something failed.
    Error,
    /// A choice with consequences.
    Warning,
    /// Information.
    Info,
}

type ClickFn = Box<dyn FnOnce(&mut Window, &mut App)>;

/// A button of a [`NoticeDialog`].
pub struct DialogButton {
    /// Label.
    pub label: SharedString,
    /// Drawn as the accent (primary) button.
    pub primary: bool,
    /// Runs after the dialog closed.
    pub on_click: ClickFn,
}

impl DialogButton {
    /// A button.
    pub fn new(
        label: impl Into<SharedString>,
        primary: bool,
        on_click: impl FnOnce(&mut Window, &mut App) + 'static,
    ) -> Self {
        DialogButton {
            label: label.into(),
            primary,
            on_click: Box::new(on_click),
        }
    }
}

/// What a [`NoticeDialog`] shows.
pub struct NoticeSpec {
    /// Title.
    pub title: SharedString,
    /// Style.
    pub level: Level,
    /// Paragraphs.
    pub message: Vec<SharedString>,
    /// Files or commits listed (shortened after 8).
    pub items: Vec<SharedString>,
    /// git's or a hook's output (monospace, scrollable).
    pub details: Option<SharedString>,
    /// Label of the button that just closes ("Close", "Cancel").
    pub close_label: SharedString,
}

/// A failure or choice dialog with next-step buttons. Enter runs the first primary button
/// (or closes), Escape closes.
pub struct NoticeDialog {
    spec: NoticeSpec,
    buttons: Vec<Option<DialogButton>>,
    focus_handle: FocusHandle,
}

impl EventEmitter<DismissEvent> for NoticeDialog {}

impl Focusable for NoticeDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

const MAX_ITEMS: usize = 8;

impl NoticeDialog {
    fn click(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let button = self.buttons.get_mut(ix).and_then(Option::take);
        cx.emit(DismissEvent);
        if let Some(b) = button {
            (b.on_click)(window, cx);
        }
    }

    fn default_button(&self) -> Option<usize> {
        self.buttons
            .iter()
            .position(|b| b.as_ref().is_some_and(|b| b.primary))
    }

    /// Labels of the next-step buttons (tests).
    pub fn button_labels(&self) -> Vec<SharedString> {
        self.buttons
            .iter()
            .flatten()
            .map(|b| b.label.clone())
            .collect()
    }

    /// The title (tests).
    pub fn title(&self) -> &SharedString {
        &self.spec.title
    }
}

fn title_row(icon: IconName, color: gpui::Hsla, title: SharedString, cx: &App) -> gpui::Div {
    let ui = &cx.theme().ui;
    h_flex()
        .gap(px(8.))
        .child(Icon::new(icon).size(IconSize::Small).color(color))
        .child(
            div()
                .text_size(metrics::text())
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(ui.text.hsla())
                .child(title),
        )
}

fn dialog_frame(id: &'static str, width: f32, cx: &App) -> gpui::Stateful<gpui::Div> {
    let ui = &cx.theme().ui;
    v_flex()
        .id(id)
        .w(px(width))
        .max_w_full()
        .p(px(16.))
        .gap(px(10.))
        .rounded(metrics::radius_lg())
        .border_1()
        .border_color(ui.border.hsla())
        .bg(ui.elevated.hsla())
        .shadow_lg()
}

fn paragraph(text: SharedString, cx: &App) -> gpui::Div {
    div()
        .text_size(metrics::text_sm())
        .text_color(cx.theme().ui.text_muted.hsla())
        .child(text)
}

fn items_box(items: &[SharedString], cx: &App) -> gpui::Div {
    let ui = &cx.theme().ui;
    let more = items.len().saturating_sub(MAX_ITEMS);
    v_flex()
        .px(px(8.))
        .py(px(6.))
        .rounded(metrics::radius())
        .bg(ui.surface.hsla())
        .border_1()
        .border_color(ui.border_variant.hsla())
        .text_size(metrics::text_sm())
        .text_color(ui.text.hsla())
        .children(
            items
                .iter()
                .take(MAX_ITEMS)
                .map(|i| div().overflow_hidden().text_ellipsis().child(i.clone())),
        )
        .when(more > 0, |el| {
            el.child(
                div()
                    .text_color(ui.text_faint.hsla())
                    .child(format!("…and {more} more")),
            )
        })
}

impl Render for NoticeDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let (icon, color) = match self.spec.level {
            Level::Error => (IconName::CircleX, ui.error.hsla()),
            Level::Warning => (IconName::AlertTriangle, ui.warning.hsla()),
            Level::Info => (IconName::Info, ui.accent.hsla()),
        };
        let buttons: Vec<(usize, SharedString, bool)> = self
            .buttons
            .iter()
            .enumerate()
            .filter_map(|(ix, b)| b.as_ref().map(|b| (ix, b.label.clone(), b.primary)))
            .collect();
        dialog_frame("sync-notice", 480., cx)
            .key_context("ConfirmDialog")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                match e.keystroke.key.as_str() {
                    "enter" => match this.default_button() {
                        Some(ix) => this.click(ix, window, cx),
                        None => cx.emit(DismissEvent),
                    },
                    "escape" => cx.emit(DismissEvent),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(title_row(icon, color, self.spec.title.clone(), cx))
            .children(self.spec.message.iter().map(|m| paragraph(m.clone(), cx)))
            .when(!self.spec.items.is_empty(), |el| {
                el.child(items_box(&self.spec.items, cx))
            })
            .when_some(self.spec.details.clone(), |el, details| {
                el.child(crate::commit_composer::output_box(
                    "sync-notice-output",
                    details,
                    None,
                    cx,
                ))
            })
            .child(
                h_flex()
                    .pt(px(4.))
                    .gap(px(8.))
                    .justify_end()
                    .flex_wrap()
                    .child(
                        Button::new("sync-notice-close", self.spec.close_label.clone())
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .children(buttons.into_iter().map(|(ix, label, primary)| {
                        Button::new(("sync-notice-step", ix), label)
                            .style(if primary {
                                ButtonStyle::Accent
                            } else {
                                ButtonStyle::Outlined
                            })
                            .on_click(
                                cx.listener(move |this, _, window, cx| this.click(ix, window, cx)),
                            )
                    })),
            )
    }
}

/// Shows a [`NoticeDialog`] in `workspace`.
pub fn show_notice(
    workspace: &Entity<Workspace>,
    spec: NoticeSpec,
    buttons: Vec<DialogButton>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<NoticeDialog> {
    let layer = workspace.read(cx).modal_layer().clone();
    let dialog = cx.new(|cx| NoticeDialog {
        spec,
        buttons: buttons.into_iter().map(Some).collect(),
        focus_handle: cx.focus_handle(),
    });
    let d = dialog.clone();
    layer.update(cx, |layer, cx| layer.show_modal(d, window, cx));
    dialog
}

// ---------------------------------------------------------------------------------------------
// Credential prompts
// ---------------------------------------------------------------------------------------------

/// Which button of the host-key dialog Enter activates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKeyChoice {
    /// Cancel (the default).
    Cancel,
    /// Trust and continue.
    Trust,
}

/// A credential prompt (askpass). The answer goes back through `reply` exactly once;
/// closing the dialog any other way (Escape, outside click, the window closing, the
/// operation being cancelled) answers Cancel.
pub struct PromptDialog {
    spec: PromptSpec,
    /// "Push main → origin/main", "Automatic fetch".
    operation: SharedString,
    /// Remote URL, credentials removed.
    remote: Option<SharedString>,
    input: Entity<TextInput>,
    reply: Option<Sender<PromptReply>>,
    answered: bool,
    choice: HostKeyChoice,
    focus_handle: FocusHandle,
}

impl EventEmitter<DismissEvent> for PromptDialog {}

impl Focusable for PromptDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.spec.kind == PromptDialogKind::HostKey {
            self.focus_handle.clone()
        } else {
            self.input.read(cx).focus_handle(cx)
        }
    }
}

impl Drop for PromptDialog {
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(PromptReply::Cancel);
        }
    }
}

impl PromptDialog {
    /// A dialog for `spec`, answering through `reply`.
    pub fn new(
        spec: PromptSpec,
        operation: impl Into<SharedString>,
        remote: Option<String>,
        reply: Sender<PromptReply>,
        cx: &mut Context<Self>,
    ) -> Self {
        let masked = spec.masked;
        let input = cx.new(|cx| {
            let mut input = TextInput::new(
                match spec.kind {
                    PromptDialogKind::Username => "Username",
                    PromptDialogKind::Password => "Password or token",
                    PromptDialogKind::Passphrase => "Passphrase",
                    _ => "Answer",
                },
                cx,
            );
            input.set_masked(masked, cx);
            input
        });
        PromptDialog {
            spec,
            operation: operation.into(),
            remote: remote.map(SharedString::from),
            input,
            reply: Some(reply),
            answered: false,
            choice: HostKeyChoice::Cancel,
            focus_handle: cx.focus_handle(),
        }
    }

    /// The parsed prompt (tests).
    pub fn spec(&self) -> &PromptSpec {
        &self.spec
    }

    /// The host-key button Enter activates (tests).
    pub fn choice(&self) -> HostKeyChoice {
        self.choice
    }

    /// The input (tests).
    pub fn input(&self) -> &Entity<TextInput> {
        &self.input
    }

    /// Whether the user answered (rather than cancelled).
    pub fn answered(&self) -> bool {
        self.answered
    }

    fn finish(&mut self, reply: PromptReply, cx: &mut Context<Self>) {
        self.answered = matches!(reply, PromptReply::Answer(_));
        if let Some(tx) = self.reply.take() {
            let _ = tx.send(reply);
        }
        // The secret leaves the field at once (the window keeps no copy around).
        self.input.update(cx, |i, _| i.clear_secret());
        cx.emit(DismissEvent);
        // The next queued prompt, if any.
        cx.defer(crate::askpass_ui::advance);
    }

    /// Whether the prompt has been answered or cancelled.
    pub fn is_finished(&self) -> bool {
        self.reply.is_none()
    }

    /// Submits the typed answer (or, for host keys, the selected button).
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        let reply = match self.spec.kind {
            PromptDialogKind::HostKey => match self.choice {
                HostKeyChoice::Trust => PromptReply::Answer(Secret::new(HOST_KEY_TRUST_ANSWER)),
                HostKeyChoice::Cancel => PromptReply::Cancel,
            },
            _ => PromptReply::Answer(Secret::new(self.input.read(cx).text().to_string())),
        };
        self.finish(reply, cx);
    }

    /// Answers Cancel.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.finish(PromptReply::Cancel, cx);
    }

    fn toggle_choice(&mut self, cx: &mut Context<Self>) {
        self.choice = match self.choice {
            HostKeyChoice::Cancel => HostKeyChoice::Trust,
            HostKeyChoice::Trust => HostKeyChoice::Cancel,
        };
        cx.notify();
    }

    fn trust(&mut self, cx: &mut Context<Self>) {
        self.choice = HostKeyChoice::Trust;
        self.submit(cx);
    }

    fn field(&self, label: &'static str, value: SharedString, mono: bool, cx: &App) -> gpui::Div {
        let ui = &cx.theme().ui;
        h_flex()
            .gap(px(8.))
            .items_start()
            .text_size(metrics::text_sm())
            .child(
                div()
                    .w(px(90.))
                    .flex_none()
                    .text_color(ui.text_faint.hsla())
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(ui.text.hsla())
                    .when(mono, |el| {
                        el.font_family(lector_ui::code_view::EditorSettings::get(cx).font_family)
                    })
                    .child(value),
            )
    }
}

impl Render for PromptDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let host_key = self.spec.kind == PromptDialogKind::HostKey;
        let (icon, color) = if host_key {
            (IconName::AlertTriangle, ui.warning.hsla())
        } else {
            (IconName::User, ui.accent.hsla())
        };
        let input_focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        let mut frame = dialog_frame("askpass-dialog", 480., cx)
            .key_context("AskpassDialog")
            .track_focus(&self.focus_handle)
            .when(host_key, |el| {
                // Tab / Shift-Tab (bound to focus moves) switch between the two buttons.
                el.on_action(cx.listener(
                    |this, _: &lector_ui::actions::workspace::FocusNextControl, _, cx| {
                        this.toggle_choice(cx)
                    },
                ))
                .on_action(cx.listener(
                    |this, _: &lector_ui::actions::workspace::FocusPrevControl, _, cx| {
                        this.toggle_choice(cx)
                    },
                ))
            })
            .on_key_down(cx.listener(move |this, e: &KeyDownEvent, _, cx| {
                let host_key = this.spec.kind == PromptDialogKind::HostKey;
                match e.keystroke.key.as_str() {
                    "enter" => this.submit(cx),
                    "escape" => this.cancel(cx),
                    "tab" | "left" | "right" if host_key => this.toggle_choice(cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(title_row(icon, color, self.spec.title.clone().into(), cx))
            .child(self.field("Operation", self.operation.clone(), false, cx))
            .when_some(self.remote.clone(), |el, remote| {
                el.child(self.field("Remote", remote, true, cx))
            })
            .when_some(self.spec.warning.clone(), |el, warning| {
                el.child(
                    div()
                        .p(px(8.))
                        .rounded(metrics::radius())
                        .border_1()
                        .border_color(ui.warning.hsla().opacity(0.6))
                        .bg(ui.warning.hsla().opacity(0.08))
                        .text_size(metrics::text_sm())
                        .text_color(ui.text.hsla())
                        .child(SharedString::from(warning)),
                )
            });
        frame = match self.spec.kind {
            PromptDialogKind::HostKey => frame
                .child(paragraph(
                    "ssh has never connected to this host before. Compare the fingerprint with the \
                     one the host publishes (for example on its documentation page) before you \
                     trust it; ssh then adds it to ~/.ssh/known_hosts."
                        .into(),
                    cx,
                ))
                .child(
                    v_flex()
                        .gap(px(6.))
                        .p(px(10.))
                        .rounded(metrics::radius())
                        .border_1()
                        .border_color(ui.warning.hsla().opacity(0.6))
                        .bg(ui.warning.hsla().opacity(0.06))
                        .child(self.field(
                            "Host",
                            self.spec.host.clone().unwrap_or_else(|| "unknown".into()).into(),
                            true,
                            cx,
                        ))
                        .child(self.field(
                            "Key type",
                            self.spec.key_type.clone().unwrap_or_else(|| "unknown".into()).into(),
                            true,
                            cx,
                        ))
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(self.field(
                                    "Fingerprint",
                                    self.spec
                                        .fingerprint
                                        .clone()
                                        .unwrap_or_else(|| "not shown by ssh".into())
                                        .into(),
                                    true,
                                    cx,
                                )),
                        ),
                ),
            kind => {
                let note: Option<SharedString> = match kind {
                    PromptDialogKind::Password => Some(
                        "Lector passes the answer to git and never stores or logs it. git's \
                         credential helper, if you configured one, may store it."
                            .into(),
                    ),
                    PromptDialogKind::Passphrase => Some(
                        "Lector passes the passphrase to ssh and never stores or logs it.".into(),
                    ),
                    _ => None,
                };
                frame
                    .when_some(self.spec.key_path.clone(), |el, key| {
                        el.child(self.field("Key", key.into(), true, cx))
                    })
                    // What the prompt itself names, always (it may differ from the remote).
                    .when_some(
                        self.spec.url.clone().or_else(|| self.spec.host.clone()),
                        |el, target| el.child(self.field("Asks for", target.into(), true, cx)),
                    )
                    .child(
                        div()
                            .p(px(8.))
                            .rounded(metrics::radius())
                            .bg(ui.surface.hsla())
                            .font_family(lector_ui::code_view::EditorSettings::get(cx).font_family)
                            .text_size(metrics::text_xs())
                            .text_color(ui.text_muted.hsla())
                            .child(SharedString::from(self.spec.prompt.trim_end().to_string())),
                    )
                    .child(
                        div()
                            .h(px(28.))
                            .px(px(8.))
                            .flex()
                            .items_center()
                            .rounded(metrics::radius())
                            .border_1()
                            .border_color(if input_focused {
                                ui.border_focused.hsla()
                            } else {
                                ui.border.hsla()
                            })
                            .bg(ui.input_bg.hsla())
                            .text_size(metrics::text())
                            .child(self.input.clone()),
                    )
                    .when_some(note, |el, note| {
                        el.child(
                            div()
                                .text_size(metrics::text_xs())
                                .text_color(ui.text_faint.hsla())
                                .child(note),
                        )
                    })
            }
        };
        let ring = |selected: bool, el: Button| -> gpui::AnyElement {
            div()
                .rounded(metrics::radius())
                .border_2()
                .border_color(if selected {
                    ui.border_focused.hsla()
                } else {
                    gpui::transparent_black()
                })
                .child(el)
                .into_any_element()
        };
        frame.child(if host_key {
            let choice = self.choice;
            h_flex()
                .pt(px(4.))
                .gap(px(8.))
                .justify_end()
                .child(ring(
                    choice == HostKeyChoice::Cancel,
                    Button::new("askpass-cancel", "Cancel")
                        .style(ButtonStyle::Accent)
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                ))
                .child(ring(
                    choice == HostKeyChoice::Trust,
                    Button::new("askpass-trust", "Trust and Continue")
                        .style(ButtonStyle::Outlined)
                        .on_click(cx.listener(|this, _, _, cx| this.trust(cx))),
                ))
        } else {
            h_flex()
                .pt(px(4.))
                .gap(px(8.))
                .justify_end()
                .child(
                    Button::new("askpass-cancel", "Cancel")
                        .style(ButtonStyle::Outlined)
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                )
                .child(
                    Button::new("askpass-ok", "Continue")
                        .style(ButtonStyle::Accent)
                        .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                )
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Input dialog (branch names, stash messages)
// ---------------------------------------------------------------------------------------------

/// What an [`InputDialog`] returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputResult {
    /// The name / message.
    pub text: String,
    /// The start point (`None` = default).
    pub start: Option<String>,
    /// Checkbox states, in order.
    pub checks: Vec<bool>,
}

type Validator = Rc<dyn Fn(&str) -> Result<(), String>>;
type OnConfirm = Box<dyn FnOnce(InputResult, &mut Window, &mut App)>;

/// What an [`InputDialog`] asks.
pub struct InputSpec {
    /// Title.
    pub title: SharedString,
    /// Explanation under the title.
    pub message: Option<SharedString>,
    /// Placeholder of the main field.
    pub placeholder: SharedString,
    /// Initial text.
    pub initial: String,
    /// Validates as you type (`None` = anything, empty allowed).
    pub validate: Option<Validator>,
    /// Offer a start point: (placeholder, choices for its menu).
    pub start_point: Option<(SharedString, Vec<String>)>,
    /// Checkboxes: (label, initial state).
    pub checks: Vec<(SharedString, bool)>,
    /// Confirm button label.
    pub confirm_label: SharedString,
}

/// A dialog with one validated field (plus optional start point and checkboxes).
pub struct InputDialog {
    spec: InputSpec,
    input: Entity<TextInput>,
    start: Option<Entity<TextInput>>,
    checks: Vec<bool>,
    error: Option<String>,
    on_confirm: Option<OnConfirm>,
    menu: Popover<ContextMenu>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DismissEvent> for InputDialog {}

impl Focusable for InputDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }
}

impl InputDialog {
    /// A dialog for `spec`; `on_confirm` runs after it closed with a valid value.
    pub fn new(
        spec: InputSpec,
        on_confirm: impl FnOnce(InputResult, &mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            let mut i = TextInput::new(spec.placeholder.clone(), cx);
            if !spec.initial.is_empty() {
                i.set_text(spec.initial.clone(), cx);
                i.select_all_text(cx);
            }
            i
        });
        let start = spec
            .start_point
            .as_ref()
            .map(|(placeholder, _)| cx.new(|cx| TextInput::new(placeholder.clone(), cx)));
        let subs = vec![cx.subscribe(&input, |this, _, _: &TextInputEvent, cx| {
            this.revalidate(cx);
            cx.notify();
        })];
        let checks = spec.checks.iter().map(|(_, on)| *on).collect();
        let mut dialog = InputDialog {
            spec,
            input,
            start,
            checks,
            error: None,
            on_confirm: Some(Box::new(on_confirm)),
            menu: Popover::new(),
            _subscriptions: subs,
        };
        dialog.revalidate(cx);
        dialog
    }

    fn revalidate(&mut self, cx: &App) {
        let text = self.input.read(cx).text().to_string();
        self.error = self.spec.validate.as_ref().and_then(|v| v(&text).err());
    }

    /// The validation message shown under the field (tests).
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The main field (tests).
    pub fn input(&self) -> &Entity<TextInput> {
        &self.input
    }

    /// Sets checkbox `ix` (tests, keyboard).
    pub fn set_check(&mut self, ix: usize, on: bool, cx: &mut Context<Self>) {
        if let Some(c) = self.checks.get_mut(ix) {
            *c = on;
            cx.notify();
        }
    }

    /// Confirms if the value is valid.
    pub fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.revalidate(cx);
        if self.error.is_some() {
            cx.notify();
            return;
        }
        let result = InputResult {
            text: self.input.read(cx).text().trim().to_string(),
            start: self
                .start
                .as_ref()
                .map(|s| s.read(cx).text().trim().to_string())
                .filter(|s| !s.is_empty()),
            checks: self.checks.clone(),
        };
        let on_confirm = self.on_confirm.take();
        cx.emit(DismissEvent);
        if let Some(f) = on_confirm {
            f(result, window, cx);
        }
    }

    fn deploy_start_menu(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(start), Some((_, choices))) =
            (self.start.clone(), self.spec.start_point.as_ref())
        else {
            return;
        };
        let choices = choices.clone();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            let s = start.clone();
            menu = menu.entry("HEAD (current commit)", move |_, cx| {
                s.update(cx, |i, cx| i.set_text("", cx))
            });
            menu = menu.separator();
            for c in choices.into_iter().take(40) {
                let s = start.clone();
                let label = c.clone();
                menu = menu.entry(label, move |_, cx| {
                    s.update(cx, |i, cx| i.set_text(c.clone(), cx))
                });
            }
            menu
        });
        self.menu.show(
            menu,
            position,
            Corner::TopLeft,
            window,
            cx,
            |this: &mut Self, _, cx| {
                this.menu.close();
                cx.notify();
            },
        );
        cx.notify();
    }
}

fn checkbox(
    id: (&'static str, usize),
    label: SharedString,
    on: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let ui = &cx.theme().ui;
    h_flex()
        .id(id)
        .gap(px(6.))
        .cursor_pointer()
        .text_size(metrics::text_sm())
        .text_color(ui.text.hsla())
        .child(
            Icon::new(if on {
                IconName::CheckSquare
            } else {
                IconName::Square
            })
            .size(IconSize::Small)
            .color(if on {
                ui.accent.hsla()
            } else {
                ui.text_muted.hsla()
            }),
        )
        .child(label)
}

impl Render for InputDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        let field = |input: Entity<TextInput>, focused: bool| {
            div()
                .flex_1()
                .h(px(28.))
                .px(px(8.))
                .flex()
                .items_center()
                .rounded(metrics::radius())
                .border_1()
                .border_color(if focused {
                    ui.border_focused.hsla()
                } else {
                    ui.border.hsla()
                })
                .bg(ui.input_bg.hsla())
                .text_size(metrics::text())
                .child(input)
        };
        let invalid = self.error.is_some();
        let checks: Vec<(usize, SharedString, bool)> = self
            .spec
            .checks
            .iter()
            .enumerate()
            .map(|(ix, (label, _))| (ix, label.clone(), self.checks[ix]))
            .collect();
        dialog_frame("sync-input", 440., cx)
            .key_context("InputDialog")
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                match e.keystroke.key.as_str() {
                    "enter" => this.confirm(window, cx),
                    "escape" => cx.emit(DismissEvent),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(title_row(
                IconName::GitBranch,
                ui.accent.hsla(),
                self.spec.title.clone(),
                cx,
            ))
            .when_some(self.spec.message.clone(), |el, m| {
                el.child(paragraph(m, cx))
            })
            .child(field(self.input.clone(), focused))
            .child(
                div()
                    .min_h(px(16.))
                    .text_size(metrics::text_xs())
                    .text_color(if invalid {
                        ui.error.hsla()
                    } else {
                        ui.text_faint.hsla()
                    })
                    .child(SharedString::from(self.error.clone().unwrap_or_default())),
            )
            .when_some(self.start.clone(), |el, start| {
                let start_focused = start.read(cx).focus_handle(cx).is_focused(window);
                el.child(
                    h_flex()
                        .gap(px(6.))
                        .child(
                            div()
                                .w(px(70.))
                                .text_size(metrics::text_sm())
                                .text_color(ui.text_muted.hsla())
                                .child("Start at"),
                        )
                        .child(field(start, start_focused))
                        .child(
                            div()
                                .id("sync-input-start-menu")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, e: &MouseDownEvent, window, cx| {
                                        this.deploy_start_menu(e.position, window, cx);
                                        cx.stop_propagation();
                                    }),
                                )
                                .child(
                                    Button::new("sync-input-start-button", "Choose…")
                                        .style(ButtonStyle::Outlined)
                                        .compact()
                                        .tooltip(lector_ui::components::Tooltip::text(
                                            "Start the branch at another branch or tag",
                                        )),
                                ),
                        ),
                )
            })
            .children(checks.into_iter().map(|(ix, label, on)| {
                checkbox(("sync-input-check", ix), label, on, cx).on_click(cx.listener(
                    move |this, _, _, cx| {
                        let now = !this.checks[ix];
                        this.set_check(ix, now, cx);
                    },
                ))
            }))
            .child(
                h_flex()
                    .pt(px(4.))
                    .gap(px(8.))
                    .justify_end()
                    .child(
                        Button::new("sync-input-cancel", "Cancel")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("sync-input-ok", self.spec.confirm_label.clone())
                            .style(if invalid {
                                ButtonStyle::Outlined
                            } else {
                                ButtonStyle::Accent
                            })
                            .disabled(invalid)
                            .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                    ),
            )
            .children(self.menu.render())
    }
}

/// Shows an [`InputDialog`] in `workspace`.
pub fn show_input(
    workspace: &Entity<Workspace>,
    spec: InputSpec,
    window: &mut Window,
    cx: &mut App,
    on_confirm: impl FnOnce(InputResult, &mut Window, &mut App) + 'static,
) -> Entity<InputDialog> {
    let layer = workspace.read(cx).modal_layer().clone();
    let dialog = cx.new(|cx| InputDialog::new(spec, on_confirm, cx));
    let d = dialog.clone();
    layer.update(cx, |layer, cx| layer.show_modal(d, window, cx));
    dialog
}
