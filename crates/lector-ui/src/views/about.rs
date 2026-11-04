//! The About dialog (`workspace::About`) and the version/environment text
//! shared with `workspace::ReportIssue`.

use gpui::{
    div, prelude::*, px, App, ClipboardItem, DismissEvent, EventEmitter, FocusHandle, Focusable,
    FontWeight, KeyDownEvent, SharedString, Window,
};

use super::welcome::logo;
use crate::components::popover::surface;
use crate::components::{h_flex, metrics, v_flex, Button, ButtonStyle};
use crate::icons::IconName;
use crate::theme::{ActiveTheme, ColorExt};

/// The project's home page.
pub const REPOSITORY_URL: &str = "https://github.com/meetdhruva/lector";
/// Documentation (the README).
pub const DOCUMENTATION_URL: &str = "https://github.com/meetdhruva/lector#readme";
/// New issue form.
pub const NEW_ISSUE_URL: &str = "https://github.com/meetdhruva/lector/issues/new";
/// License of Lector itself.
pub const LICENSE: &str = "MIT OR Apache-2.0";

/// The build's commit, when the build set `LECTOR_COMMIT`.
pub fn commit() -> Option<&'static str> {
    option_env!("LECTOR_COMMIT").filter(|c| !c.is_empty())
}

/// "0.2.0 (abc1234)".
pub fn version_string() -> String {
    match commit() {
        Some(c) => format!("{} ({})", env!("CARGO_PKG_VERSION"), &c[..c.len().min(9)]),
        None => env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Multi-line version and environment info (About → Copy, issue reports).
pub fn version_info() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let session = if cfg!(target_os = "linux") {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            " (Wayland)"
        } else {
            " (X11)"
        }
    } else {
        ""
    };
    format!(
        "Lector {}\nOS: {os} {arch}{session}\nPortable: {}",
        version_string(),
        if lector_config::paths::is_portable() {
            "yes"
        } else {
            "no"
        }
    )
}

/// The new-issue URL with the version info prefilled in the body.
pub fn issue_url() -> String {
    let body = format!(
        "**Describe the problem**\n\n\n**Steps to reproduce**\n\n1. \n\n**Environment**\n\n```\n{}\n```\n",
        version_info()
    );
    format!("{NEW_ISSUE_URL}?body={}", percent_encode(&body))
}

/// Percent-encodes `s` for a URL query value (RFC 3986 unreserved kept).
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The About dialog (a modal).
pub struct AboutDialog {
    focus_handle: FocusHandle,
    copied: bool,
}

impl EventEmitter<DismissEvent> for AboutDialog {}

impl Focusable for AboutDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl AboutDialog {
    /// A new dialog.
    pub fn new(cx: &mut Context<Self>) -> Self {
        AboutDialog {
            focus_handle: cx.focus_handle(),
            copied: false,
        }
    }
}

impl Render for AboutDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let link = |id: &'static str, label: &'static str, url: &'static str, cx: &App| {
            let ui = &cx.theme().ui;
            div()
                .id(id)
                .text_size(metrics::TEXT_SM)
                .text_color(ui.text_accent.hsla())
                .cursor_pointer()
                .hover(|s| s.underline())
                .on_click(move |_, _, cx| cx.open_url(url))
                .child(label)
        };
        let row = |label: &'static str, value: SharedString, cx: &App| {
            let ui = &cx.theme().ui;
            h_flex()
                .gap(px(12.))
                .text_size(metrics::TEXT_SM)
                .child(
                    div()
                        .w(px(70.))
                        .text_color(ui.text_faint.hsla())
                        .child(label),
                )
                .child(div().text_color(ui.text_muted.hsla()).child(value))
        };
        surface(cx)
            .id("about-dialog")
            .key_context("AboutDialog")
            .track_focus(&self.focus_handle)
            .w(px(420.))
            .on_key_down(cx.listener(|_, e: &KeyDownEvent, _, cx| {
                if matches!(e.keystroke.key.as_str(), "escape" | "enter") {
                    cx.emit(DismissEvent);
                    cx.stop_propagation();
                }
            }))
            .child(
                v_flex()
                    .items_center()
                    .pt(px(28.))
                    .pb(px(18.))
                    .px(px(28.))
                    .gap(px(6.))
                    .child(logo(76., cx))
                    .child(
                        div()
                            .mt(px(10.))
                            .text_size(px(22.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui.text.hsla())
                            .child("Lector"),
                    )
                    .child(
                        div()
                            .text_size(metrics::TEXT)
                            .text_color(ui.text_muted.hsla())
                            .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                    )
                    .child(
                        div()
                            .text_size(metrics::TEXT_SM)
                            .text_color(ui.text_faint.hsla())
                            .text_center()
                            .child("A fast, reading-first code viewer and git review tool."),
                    ),
            )
            .child(
                v_flex()
                    .mx(px(28.))
                    .py(px(12.))
                    .gap(px(6.))
                    .border_t_1()
                    .border_b_1()
                    .border_color(ui.border_variant.hsla())
                    .child(row(
                        "Commit",
                        commit().unwrap_or("development build").to_string().into(),
                        cx,
                    ))
                    .child(row(
                        "Platform",
                        format!("{} {}", std::env::consts::OS, std::env::consts::ARCH).into(),
                        cx,
                    ))
                    .child(row("License", LICENSE.into(), cx))
                    .child(row("Built with", "GPUI, tree-sitter, gitoxide".into(), cx)),
            )
            .child(
                h_flex()
                    .justify_center()
                    .gap(px(18.))
                    .py(px(12.))
                    .child(link("about-repo", "GitHub", REPOSITORY_URL, cx))
                    .child(link("about-docs", "Documentation", DOCUMENTATION_URL, cx))
                    .child(link("about-issue", "Report an Issue", NEW_ISSUE_URL, cx)),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap(px(8.))
                    .px(px(16.))
                    .py(px(12.))
                    .bg(ui.surface.hsla())
                    .border_t_1()
                    .border_color(ui.border_variant.hsla())
                    .child(
                        Button::new(
                            "about-copy",
                            if self.copied {
                                "Copied"
                            } else {
                                "Copy Version Info"
                            },
                        )
                        .icon(if self.copied {
                            IconName::Check
                        } else {
                            IconName::Copy
                        })
                        .style(ButtonStyle::Outlined)
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(version_info()));
                            this.copied = true;
                            cx.notify();
                        })),
                    )
                    .child(
                        Button::new("about-close", "Close")
                            .style(ButtonStyle::Accent)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_url_is_encoded() {
        assert_eq!(percent_encode("a b\n#"), "a%20b%0A%23");
        let url = issue_url();
        assert!(url.starts_with(NEW_ISSUE_URL));
        assert!(url.contains(&percent_encode(env!("CARGO_PKG_VERSION"))));
        assert!(!url.contains(' ') && !url.contains('\n'));
    }
}
