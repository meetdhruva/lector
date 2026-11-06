//! The language server popover opened from the status bar: every server of
//! the project with its state, and a shortcut to the settings.

use crate::scale::px;
use gpui::{
    div, prelude::*, App, DismissEvent, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla,
    SharedString, Window,
};
use lector_lsp::{ServerCapabilities, ServerState, ServerStatus};

use crate::actions::workspace::{OpenLogFile, OpenSettings};
use crate::components::popover::surface;
use crate::components::{h_flex, metrics, v_flex, Button, IconButton, Tooltip};
use crate::icons::{IconName, IconSize};
use crate::theme::{ActiveTheme, ColorExt};

/// Popover listing language servers.
pub struct LspStatusPopover {
    servers: Vec<ServerStatus>,
    enabled: bool,
    focus_handle: FocusHandle,
    capabilities: Vec<(String, ServerCapabilities)>,
}

impl EventEmitter<DismissEvent> for LspStatusPopover {}

impl Focusable for LspStatusPopover {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl LspStatusPopover {
    /// `servers` from `LspManager::status()`; `enabled` is false when LSP is
    /// turned off in the settings.
    pub fn new(servers: Vec<ServerStatus>, enabled: bool, cx: &mut Context<Self>) -> Self {
        LspStatusPopover {
            servers,
            enabled,
            focus_handle: cx.focus_handle(),
            capabilities: Vec::new(),
        }
    }

    /// Capabilities of running servers (by name), listed under each.
    pub fn set_capabilities(&mut self, capabilities: Vec<(String, ServerCapabilities)>) {
        self.capabilities = capabilities;
    }
}

/// "Hover · Definition · Format · Rename · Code actions" for a server.
pub fn capability_summary(c: &ServerCapabilities) -> String {
    [
        (c.hover, "Hover"),
        (c.definition, "Definition"),
        (c.references, "References"),
        (c.formatting, "Format"),
        (c.range_formatting && !c.formatting, "Range format"),
        (c.rename, "Rename"),
        (c.code_actions, "Code actions"),
    ]
    .iter()
    .filter(|(on, _)| *on)
    .map(|(_, l)| *l)
    .collect::<Vec<_>>()
    .join(" · ")
}

/// "Install with …" rows (command and copy button) of a server that is not on `PATH`.
fn install_rows(server: &str, ui: &lector_theme::UiColors, cx: &App) -> Vec<gpui::AnyElement> {
    let _ = cx;
    let Some(hint) = lector_lsp::install::install_hint(server) else {
        return Vec::new();
    };
    hint.options_for(lector_lsp::install::Os::current())
        .into_iter()
        .enumerate()
        .map(|(ix, option)| {
            let command = option.command;
            let id: SharedString = format!("lsp-install-{server}-{ix}").into();
            h_flex()
                .pl(px(16.))
                .gap(px(6.))
                .child(
                    div()
                        .flex_none()
                        .text_size(metrics::text_xs())
                        .text_color(ui.text_faint.hsla())
                        .child(format!("Install {}:", option.label)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .font_family("monospace")
                        .text_size(metrics::text_xs())
                        .text_color(ui.text.hsla())
                        .child(command),
                )
                .child(
                    IconButton::new(id, IconName::Copy)
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Copy the command"))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                command.to_string(),
                            ))
                        }),
                )
                .into_any_element()
        })
        .collect()
}

fn state_label(state: &ServerState, cx: &App) -> (SharedString, Hsla) {
    let theme = cx.theme();
    match state {
        ServerState::Running => ("Running".into(), theme.git.added.hsla()),
        ServerState::Starting => ("Starting…".into(), theme.git.modified.hsla()),
        ServerState::Failed(err) => (format!("Failed: {err}").into(), theme.git.deleted.hsla()),
        ServerState::NotInstalled => (
            "Not installed (not on PATH)".into(),
            theme.ui.text_faint.hsla(),
        ),
    }
}

impl Render for LspStatusPopover {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let rows: Vec<_> = self
            .servers
            .iter()
            .map(|s| {
                let (label, color) = state_label(&s.state, cx);
                let caps = self
                    .capabilities
                    .iter()
                    .find(|(name, _)| *name == s.name)
                    .map(|(_, c)| capability_summary(c))
                    .filter(|c| !c.is_empty());
                let row = h_flex()
                    .gap(px(8.))
                    .py(px(4.))
                    .child(div().size(px(8.)).rounded_full().bg(color).flex_none())
                    .child(
                        div()
                            .w(px(150.))
                            .flex_none()
                            .text_size(metrics::text())
                            .text_color(ui.text.hsla())
                            .child(s.name.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(metrics::text_sm())
                            .text_color(ui.text_muted.hsla())
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(label),
                    );
                let install = matches!(s.state, ServerState::NotInstalled)
                    .then(|| install_rows(&s.name, &ui, cx))
                    .filter(|rows| !rows.is_empty());
                v_flex()
                    .child(row)
                    .children(install.into_iter().flatten())
                    .when_some(caps, |el, caps| {
                        el.child(
                            div()
                                .pl(px(16.))
                                .text_size(metrics::text_xs())
                                .text_color(ui.text_faint.hsla())
                                .child(caps),
                        )
                    })
            })
            .collect();
        let message: Option<&str> = if !self.enabled {
            Some("Language servers are turned off (lsp.enabled = false).")
        } else if self.servers.is_empty() {
            Some("No language server has started yet. Servers start when you open a file of their language.")
        } else {
            None
        };
        surface(cx)
            .track_focus(&self.focus_handle)
            .key_context("LspStatus")
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            .on_key_down(cx.listener(|_, e: &gpui::KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" {
                    cx.emit(DismissEvent);
                }
            }))
            .w(px(380.))
            .child(
                v_flex()
                    .p(px(12.))
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(metrics::text_xs())
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui.text_faint.hsla())
                            .child("LANGUAGE SERVERS"),
                    )
                    .children(rows)
                    .when_some(message, |el, m| {
                        el.child(
                            div()
                                .text_size(metrics::text_sm())
                                .text_color(ui.text_muted.hsla())
                                .child(m),
                        )
                    }),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap(px(6.))
                    .px(px(8.))
                    .py(px(6.))
                    .border_t_1()
                    .border_color(ui.border_variant.hsla())
                    .child(
                        Button::new("lsp-log", "Show Log")
                            .icon(IconName::ScrollText)
                            .on_click(cx.listener(|_, _, window, cx| {
                                cx.emit(DismissEvent);
                                window.dispatch_action(Box::new(OpenLogFile), cx)
                            })),
                    )
                    .child(
                        Button::new("lsp-settings", "Settings")
                            .icon(IconName::Settings)
                            .on_click(cx.listener(|_, _, window, cx| {
                                cx.emit(DismissEvent);
                                window.dispatch_action(Box::new(OpenSettings), cx)
                            })),
                    ),
            )
    }
}
