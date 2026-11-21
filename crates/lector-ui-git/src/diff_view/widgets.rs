//! Small views shared by the diff and review panels: the `+N -M` label and the viewed checkbox.

use super::*;

/// "+12 −3" with diff colors.
pub fn stats_label(s: DiffStats, theme: &lector_theme::Theme) -> impl IntoElement {
    h_flex()
        .flex_none()
        .gap(px(6.))
        .text_size(metrics::text_sm())
        .font_weight(FontWeight::MEDIUM)
        .child(
            div()
                .text_color(theme.diff.added_text.hsla())
                .child(format!("+{}", s.added)),
        )
        .child(
            div()
                .text_color(theme.diff.removed_text.hsla())
                .child(format!("−{}", s.removed)),
        )
}

/// A "Viewed" checkbox pill.
pub fn viewed_checkbox(id: &'static str, viewed: bool, cx: &App) -> gpui::Stateful<gpui::Div> {
    let ui = &cx.theme().ui;
    let accent = ui.accent.hsla();
    h_flex()
        .id(id)
        .flex_none()
        .h(px(24.))
        .px(px(8.))
        .gap(px(6.))
        .rounded(px(5.))
        .border_1()
        .border_color(if viewed {
            accent.opacity(0.5)
        } else {
            ui.border.hsla()
        })
        .when(viewed, |el| el.bg(accent.opacity(0.12)))
        .text_size(metrics::text_sm())
        .text_color(if viewed {
            ui.text.hsla()
        } else {
            ui.text_muted.hsla()
        })
        .cursor_pointer()
        .hover(|s| s.bg(ui.hover.hsla()))
        .tooltip(Tooltip::for_action(
            "Toggle Viewed",
            &diff_actions::ToggleViewed,
        ))
        .child(
            Icon::new(if viewed {
                IconName::CheckSquare
            } else {
                IconName::Square
            })
            .size(IconSize::Small)
            .color(if viewed { accent } else { ui.text_faint.hsla() }),
        )
        .child("Viewed")
}
