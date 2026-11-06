//! The one-time, subtle banner that tells how to install a missing language server: the exact
//! command for the user's operating system, a copy button and "Don't show again". Lector never
//! runs the command itself.

use gpui::{div, prelude::*, ClipboardItem, Context, SharedString};

use super::Workspace;
use crate::components::{h_flex, metrics, Button, ButtonStyle, IconButton};
use crate::icons::{Icon, IconName, IconSize};
use crate::scale::px;
use crate::theme::{ActiveTheme, ColorExt};

/// Database key: set once the user chose "Don't show again".
const BANNER_OFF_KEY: &str = "ui/lsp-install-banner/off";

/// Database key: set once the banner was shown for a server (it is shown at most once per server).
fn shown_key(server: &str) -> String {
    format!("ui/lsp-install-banner/shown/{server}")
}

/// The visible banner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LspBanner {
    server: String,
    language: &'static str,
    command: &'static str,
}

/// What the banner says (tests, screenshots).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspBannerInfo {
    /// Server name.
    pub server: String,
    /// The install command shown.
    pub command: String,
}

impl Workspace {
    /// Offers the install command of `server` (once per server, unless turned off), when the
    /// server is not installed and Lector knows how to install it on this system.
    pub(super) fn offer_lsp_install(&mut self, server: &str, cx: &mut Context<Self>) {
        if self.lsp_banner.is_some() {
            return;
        }
        let Some(hint) = lector_lsp::install::install_hint(server) else {
            return;
        };
        let Some(option) = hint.primary(lector_lsp::install::Os::current()) else {
            return;
        };
        let db = crate::commands_bridge::core(cx).db().clone();
        let off = matches!(db.kv_get(BANNER_OFF_KEY), Ok(Some(_)));
        let shown = matches!(db.kv_get(&shown_key(server)), Ok(Some(_)));
        if off || shown {
            return;
        }
        if let Err(err) = db.kv_put(&shown_key(server), Some(b"1")) {
            tracing::warn!("cannot remember the install banner: {err}");
        }
        self.lsp_banner = Some(LspBanner {
            server: server.to_string(),
            language: hint.language,
            command: option.command,
        });
        cx.notify();
    }

    /// Test hook: offers the install command of `server`.
    #[cfg(test)]
    pub(crate) fn offer_lsp_install_for_tests(&mut self, server: &str, cx: &mut Context<Self>) {
        self.offer_lsp_install(server, cx);
    }

    /// The visible install banner, if any.
    pub fn lsp_banner(&self) -> Option<LspBannerInfo> {
        self.lsp_banner.as_ref().map(|b| LspBannerInfo {
            server: b.server.clone(),
            command: b.command.to_string(),
        })
    }

    /// Hides the banner; with `forever` it never appears again.
    pub fn dismiss_lsp_banner(&mut self, forever: bool, cx: &mut Context<Self>) {
        if forever {
            let db = crate::commands_bridge::core(cx).db().clone();
            if let Err(err) = db.kv_put(BANNER_OFF_KEY, Some(b"1")) {
                tracing::warn!("cannot remember the install banner choice: {err}");
            }
        }
        if self.lsp_banner.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn render_lsp_banner(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let banner = self.lsp_banner.as_ref()?;
        let ui = cx.theme().ui.clone();
        let command = banner.command;
        let message: SharedString = format!(
            "No {} language server found. Install {} for hover and go to definition:",
            banner.language, banner.server
        )
        .into();
        let this = cx.entity().downgrade();
        let (copy_this, off_this) = (this.clone(), this.clone());
        Some(
            div()
                .id("lsp-install-banner")
                .absolute()
                .bottom(metrics::status_bar_height() + px(12.))
                .left(px(16.))
                .max_w(px(760.))
                .pl(px(12.))
                .pr(px(6.))
                .py(px(5.))
                .rounded(metrics::radius_lg())
                .bg(ui.elevated.hsla())
                .border_1()
                .border_color(ui.border_variant.hsla())
                .shadow_md()
                .text_size(metrics::text_sm())
                .text_color(ui.text_muted.hsla())
                .child(
                    h_flex()
                        .gap(px(8.))
                        .child(
                            Icon::new(IconName::Info)
                                .size(IconSize::Small)
                                .color(ui.text_faint.hsla()),
                        )
                        .child(div().min_w_0().child(message))
                        .child(
                            div()
                                .flex_none()
                                .px(px(6.))
                                .py(px(1.))
                                .rounded(px(4.))
                                .bg(ui.surface.hsla())
                                .border_1()
                                .border_color(ui.border_variant.hsla())
                                .font_family("monospace")
                                .text_color(ui.text.hsla())
                                .child(command),
                        )
                        .child(
                            Button::new("lsp-banner-copy", "Copy")
                                .style(ButtonStyle::Subtle)
                                .compact()
                                .icon(IconName::Copy)
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        command.to_string(),
                                    ));
                                    let _ = copy_this
                                        .update(cx, |ws, cx| ws.dismiss_lsp_banner(false, cx));
                                }),
                        )
                        .child(
                            Button::new("lsp-banner-off", "Don't show again")
                                .compact()
                                .on_click(move |_, _, cx| {
                                    let _ = off_this
                                        .update(cx, |ws, cx| ws.dismiss_lsp_banner(true, cx));
                                }),
                        )
                        .child(
                            IconButton::new("lsp-banner-close", IconName::Close)
                                .icon_size(IconSize::XSmall)
                                .on_click(move |_, _, cx| {
                                    let _ =
                                        this.update(cx, |ws, cx| ws.dismiss_lsp_banner(false, cx));
                                }),
                        ),
                ),
        )
    }
}
