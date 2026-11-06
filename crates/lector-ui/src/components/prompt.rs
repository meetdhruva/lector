//! Themed window prompts ("Save changes?") for platforms without native ones.
//!
//! GPUI's fallback prompt (used on Linux) is an unthemed white box of fixed
//! width that cuts off long detail text and ignores Enter / Escape. Installed
//! with [`install`] from `lector_ui::init` on those platforms only, so macOS
//! and Windows keep their native dialogs and tests keep the test platform's.

use crate::scale::px;
use gpui::{
    div, prelude::*, App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, KeyDownEvent,
    PromptButton, PromptHandle, PromptLevel, PromptResponse, RenderablePromptHandle, SharedString,
    Window,
};

use super::{h_flex, metrics, v_flex, Button, ButtonStyle};
use crate::theme::{ActiveTheme, ColorExt};

/// Replaces GPUI's fallback prompt with [`ThemedPrompt`].
pub fn install(cx: &mut App) {
    cx.set_prompt_builder(build);
}

fn build(
    level: PromptLevel,
    message: &str,
    detail: Option<&str>,
    actions: &[PromptButton],
    handle: PromptHandle,
    window: &mut Window,
    cx: &mut App,
) -> RenderablePromptHandle {
    let view = cx.new(|cx| ThemedPrompt {
        level,
        message: message.to_string().into(),
        detail: detail.map(|d| d.to_string().into()),
        actions: actions.to_vec(),
        focus: cx.focus_handle(),
    });
    handle.with_view(view, window, cx)
}

/// A modal prompt drawn with the theme: wrapped message and detail, one button per
/// answer; Enter picks the first (default) answer, Escape the cancel answer.
pub struct ThemedPrompt {
    level: PromptLevel,
    message: SharedString,
    detail: Option<SharedString>,
    actions: Vec<PromptButton>,
    focus: FocusHandle,
}

impl ThemedPrompt {
    /// Index of the answer Escape picks: the cancel button, else the last one.
    fn cancel_index(&self) -> usize {
        self.actions
            .iter()
            .position(|a| matches!(a, PromptButton::Cancel(_)))
            .unwrap_or(self.actions.len().saturating_sub(1))
    }
}

impl EventEmitter<PromptResponse> for ThemedPrompt {}

impl Focusable for ThemedPrompt {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ThemedPrompt {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let title_color = match self.level {
            PromptLevel::Critical => ui.error.hsla(),
            _ => ui.text.hsla(),
        };
        let buttons = self.actions.iter().enumerate().map(|(ix, action)| {
            let style = if ix == 0 {
                ButtonStyle::Accent
            } else {
                ButtonStyle::Outlined
            };
            Button::new(("prompt-answer", ix), action.label().clone())
                .style(style)
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(PromptResponse(ix))))
        });
        div()
            .size_full()
            .occlude()
            .bg(gpui::black().opacity(0.35))
            .flex()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .id("themed-prompt")
                    .track_focus(&self.focus)
                    .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                        let ix = match e.keystroke.key.as_str() {
                            "enter" if !this.actions.is_empty() => 0,
                            "escape" if !this.actions.is_empty() => this.cancel_index(),
                            _ => return,
                        };
                        cx.stop_propagation();
                        cx.emit(PromptResponse(ix));
                    }))
                    .w(px(420.))
                    .max_w_full()
                    .p(px(16.))
                    .gap(px(10.))
                    .rounded(metrics::radius_lg())
                    .border_1()
                    .border_color(ui.border.hsla())
                    .bg(ui.elevated.hsla())
                    .shadow_lg()
                    .child(
                        div()
                            .text_size(metrics::text())
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(title_color)
                            .child(self.message.clone()),
                    )
                    .children(self.detail.clone().map(|d| {
                        div()
                            .text_size(metrics::text_sm())
                            .text_color(ui.text_muted.hsla())
                            .child(d)
                    }))
                    .child(
                        h_flex()
                            .pt(px(4.))
                            .gap(px(8.))
                            .flex_wrap()
                            .justify_end()
                            .children(buttons),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Empty, PromptLevel, TestAppContext};
    use lector_theme::{Appearance, ThemeRegistry};

    use crate::theme::{ThemeMode, ThemeState};

    struct Root;
    impl gpui::Render for Root {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            Empty
        }
    }

    #[gpui::test]
    fn enter_and_escape_answer_the_themed_prompt(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ThemeState::init(cx, ThemeRegistry::new(), ThemeMode::Dark, Appearance::Dark);
            super::install(cx);
        });
        let (_root, cx) = cx.add_window_view(|_, _| Root);
        let answers = ["Save", "Don't Save", "Cancel"];
        for (key, expected) in [("escape", 2usize), ("enter", 0)] {
            let rx = cx.update(|window, cx| {
                window.prompt(
                    PromptLevel::Warning,
                    "Do you want to save the changes you made to “a.txt”?",
                    Some("Your changes will be lost if you don't save them."),
                    &[
                        gpui::PromptButton::ok(answers[0]),
                        gpui::PromptButton::new(answers[1]),
                        gpui::PromptButton::cancel(answers[2]),
                    ],
                    cx,
                )
            });
            cx.run_until_parked();
            cx.simulate_keystrokes(key);
            cx.run_until_parked();
            let got = futures::executor::block_on(rx).expect("answered");
            assert_eq!(got, expected, "{key}");
        }
    }
}
