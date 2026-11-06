//! A small confirmation dialog for destructive operations (discard, delete):
//! says exactly what will be lost; Enter confirms, Escape cancels.

use gpui::{
    div, prelude::*, App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    KeyDownEvent, SharedString, Window,
};
use lector_ui::components::{h_flex, metrics, v_flex, Button, ButtonStyle};
use lector_ui::icons::{Icon, IconName, IconSize};
use lector_ui::scale::px;
use lector_ui::views::Workspace;
use lector_ui::{ActiveTheme, ColorExt};

type ConfirmFn = Box<dyn FnOnce(&mut Window, &mut App)>;

/// What a confirmation dialog shows.
pub struct ConfirmSpec {
    /// Title ("Discard changes in 3 files?").
    pub title: SharedString,
    /// Paragraphs explaining what will be lost.
    pub message: Vec<SharedString>,
    /// Affected items (paths); long lists are shortened.
    pub items: Vec<SharedString>,
    /// Label of the destructive button ("Discard", "Delete").
    pub confirm_label: SharedString,
}

/// The dialog view (shown in the workspace's modal layer).
pub struct ConfirmDialog {
    spec: ConfirmSpec,
    on_confirm: Option<ConfirmFn>,
    focus_handle: FocusHandle,
}

impl EventEmitter<DismissEvent> for ConfirmDialog {}

impl Focusable for ConfirmDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Most items listed before "and N more".
const MAX_ITEMS: usize = 8;

impl ConfirmDialog {
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let action = self.on_confirm.take();
        cx.emit(DismissEvent);
        if let Some(action) = action {
            action(window, cx);
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.on_confirm = None;
        cx.emit(DismissEvent);
    }
}

impl Render for ConfirmDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let more = self.spec.items.len().saturating_sub(MAX_ITEMS);
        v_flex()
            .id("confirm-dialog")
            .key_context("ConfirmDialog")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                match e.keystroke.key.as_str() {
                    "enter" => this.confirm(window, cx),
                    "escape" => this.cancel(cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .w(px(440.))
            .p(px(16.))
            .gap(px(10.))
            .rounded(metrics::radius_lg())
            .border_1()
            .border_color(ui.border.hsla())
            .bg(ui.elevated.hsla())
            .shadow_lg()
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        Icon::new(IconName::AlertTriangle)
                            .size(IconSize::Small)
                            .color(ui.warning.hsla()),
                    )
                    .child(
                        div()
                            .text_size(metrics::text())
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui.text.hsla())
                            .child(self.spec.title.clone()),
                    ),
            )
            .children(self.spec.message.iter().map(|m| {
                div()
                    .text_size(metrics::text_sm())
                    .text_color(ui.text_muted.hsla())
                    .child(m.clone())
            }))
            .when(!self.spec.items.is_empty(), |el| {
                el.child(
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
                            self.spec
                                .items
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
                        }),
                )
            })
            .child(
                h_flex()
                    .pt(px(4.))
                    .gap(px(8.))
                    .justify_end()
                    .child(
                        Button::new("confirm-cancel", "Cancel")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(
                        h_flex()
                            .id("confirm-ok")
                            .h(px(26.))
                            .px(px(12.))
                            .rounded(metrics::radius())
                            .bg(ui.error.hsla())
                            .text_size(metrics::text())
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(gpui::white())
                            .cursor_pointer()
                            .hover(|s| s.opacity(0.9))
                            .tab_index(0)
                            .focus(lector_ui::a11y::ring_focus(cx))
                            .child(self.spec.confirm_label.clone())
                            .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                    ),
            )
    }
}

/// Shows a confirmation dialog in `workspace`; `on_confirm` runs only if confirmed.
pub fn confirm(
    workspace: &Entity<Workspace>,
    spec: ConfirmSpec,
    window: &mut Window,
    cx: &mut App,
    on_confirm: impl FnOnce(&mut Window, &mut App) + 'static,
) {
    let layer = workspace.read(cx).modal_layer().clone();
    let dialog = cx.new(|cx| ConfirmDialog {
        spec,
        on_confirm: Some(Box::new(on_confirm)),
        focus_handle: cx.focus_handle(),
    });
    layer.update(cx, |layer, cx| layer.show_modal(dialog, window, cx));
}

/// Builds the dialog text for discarding `tracked` and deleting `untracked` paths.
pub fn discard_spec(tracked: &[String], untracked: &[String]) -> ConfirmSpec {
    let n = tracked.len() + untracked.len();
    let files = |n: usize| if n == 1 { "file" } else { "files" };
    let title: SharedString = match (tracked.len(), untracked.len()) {
        (0, u) => format!("Delete {u} untracked {}?", files(u)).into(),
        (t, 0) => format!("Discard changes in {t} {}?", files(t)).into(),
        _ => format!("Discard changes in {n} files?").into(),
    };
    let mut message: Vec<SharedString> = Vec::new();
    if !tracked.is_empty() {
        message.push(
            "Unstaged changes in tracked files are reverted to the staged version (or HEAD). \
             Staged changes are kept."
                .into(),
        );
    }
    if !untracked.is_empty() {
        message.push(
            format!(
                "{} untracked {} will be deleted from disk.",
                untracked.len(),
                files(untracked.len())
            )
            .into(),
        );
    }
    message.push(
        "Lector backs the changes up first; undo from the Activity log (ctrl/cmd-alt-z) if you change your mind."
            .into(),
    );
    let items = tracked
        .iter()
        .map(|p| SharedString::from(p.clone()))
        .chain(
            untracked
                .iter()
                .map(|p| SharedString::from(format!("{p}  (untracked — deleted)"))),
        )
        .collect();
    ConfirmSpec {
        title,
        message,
        items,
        confirm_label: if tracked.is_empty() {
            "Delete".into()
        } else {
            "Discard".into()
        },
    }
}
