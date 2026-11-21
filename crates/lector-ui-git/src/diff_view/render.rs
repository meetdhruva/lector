//! Rendering of the [`DiffView`]: the header, the per-hunk toolbars, the
//! body for each layout (code views, binary and image summaries) and the
//! `Render` implementation.

use std::sync::Arc;

use gpui::{
    div, img, prelude::*, ClickEvent, Focusable, FontWeight, Hsla, ImageSource, MouseButton,
    MouseDownEvent, SharedString, Window,
};
use lector_git::ChangeKind;
use lector_review::review::{DiffKind, HunkAction};
use lector_ui::actions::{diff as diff_actions, git as git_actions};
use lector_ui::components::{h_flex, metrics, v_flex, IconButton, Tooltip};
use lector_ui::icons::{FileIcon, Icon, IconName, IconSize};
use lector_ui::keymap::contexts;
use lector_ui::scale::px;
use lector_ui::{ActiveTheme, ColorExt};

use super::widgets::{stats_label, viewed_checkbox};
use super::{DiffTarget, DiffView, Layout};
use crate::util::{format_size, kind_color, kind_label, split_path};

impl DiffView {
    /// Floating Stage / Unstage / Discard buttons next to each visible hunk.
    fn render_hunk_toolbars(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let actions = self.hunk_actions();
        let reviewable = self.hunk_review_enabled();
        if actions.is_empty() && !reviewable {
            return Vec::new();
        }
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let lh = Self::line_height(cx);
        let view = match self.last_layout {
            Layout::Split => &self.new_view,
            Layout::Unified => &self.unified_view,
        };
        let scroll = view.read(cx).scroll_offset().y;
        let viewport = window.viewport_size().height;
        let busy = self.op_task.is_some();
        let current = self.current_hunk.or_else(|| self.hunk_at_cursor(cx));
        let mut out = Vec::new();
        for (k, start) in self.hunk_starts().iter().enumerate() {
            let y = lh * *start as f32 - scroll;
            if y < -lh || y > viewport {
                continue;
            }
            let top = (y - lh + (lh - px(20.)) / 2.).max(px(0.));
            let mut bar = h_flex()
                .absolute()
                .top(top)
                .right(px(18.))
                .h(px(20.))
                .px(px(2.))
                .gap(px(1.))
                .rounded(px(5.))
                .border_1()
                .border_color(if current == Some(k) {
                    ui.border_focused.hsla()
                } else {
                    ui.border.hsla()
                })
                .bg(ui.elevated.hsla())
                .shadow_sm()
                .text_size(metrics::text_xs());
            if reviewable {
                bar = bar.child(self.reviewed_chip(k, cx));
            }
            for action in actions {
                let (label, icon, tip): (&str, IconName, &str) = match action {
                    HunkAction::Stage => ("Stage", IconName::Plus, "Stage Hunk (s)"),
                    HunkAction::Unstage => ("Unstage", IconName::Minus, "Unstage Hunk (u)"),
                    HunkAction::Discard => ("Discard", IconName::Undo, "Discard Hunk…"),
                };
                let action = *action;
                bar = bar.child(
                    h_flex()
                        .id(("hunk-action", k * 4 + action as usize))
                        .h(px(16.))
                        .px(px(5.))
                        .gap(px(3.))
                        .rounded(px(3.))
                        .text_color(if busy {
                            ui.text_faint.hsla()
                        } else {
                            ui.text_muted.hsla()
                        })
                        .when(!busy, |el| {
                            el.cursor_pointer()
                                .hover(|s| s.bg(ui.hover.hsla()).text_color(ui.text.hsla()))
                        })
                        .child(
                            Icon::new(icon)
                                .size(IconSize::XSmall)
                                .color(ui.text_muted.hsla()),
                        )
                        .child(label)
                        .tab_index(0)
                        .focus(lector_ui::a11y::ring_focus(cx))
                        .tooltip(Tooltip::text(tip))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            if this.op_task.is_none() {
                                this.hunk_action(action, Some(k), false, window, cx);
                            }
                        })),
                );
            }
            out.push(bar.into_any_element());
        }
        out
    }

    // Rendering.

    fn render_header(&self, layout: Layout, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let path: SharedString = match (&self.title_override, &self.loaded, &self.target) {
            (Some(t), _, _) => t.clone(),
            (None, Some(l), _) => l.diff.display_path().into(),
            (None, None, Some(t)) => t.path().to_string().into(),
            _ => SharedString::default(),
        };
        let file_name = self
            .target
            .as_ref()
            .map(|t| split_path(t.path()).0.to_string())
            .unwrap_or_default();
        let (name, dir) = if self.title_override.is_some() || path.contains(" → ") {
            (path.to_string(), String::new())
        } else {
            let (n, d) = split_path(&path);
            (n.to_string(), d.to_string())
        };
        let kind = match (&self.loaded, &self.target) {
            (Some(l), _) => Some(l.diff.change.kind),
            (None, Some(DiffTarget::Change(c))) => Some(c.kind),
            _ => None,
        };
        let stats = self
            .loaded
            .as_ref()
            .filter(|l| l.diff.kind == DiffKind::Text)
            .map(|l| l.diff.stats())
            .filter(|s| s.added + s.removed > 0);
        let viewed = self.viewed.as_ref().map(|v| v.file.viewed);
        let hunks = self.hunk_starts().len();
        let split = layout == Layout::Split;
        let review_controls = self.render_review_controls(cx);
        let seg =
            |id: &'static str, label: &'static str, icon: IconName, selected: bool, l: Layout| {
                let accent = ui.text.hsla();
                let faint = ui.text_muted.hsla();
                h_flex()
                    .id(id)
                    .h(px(22.))
                    .px(px(8.))
                    .gap(px(5.))
                    .rounded(px(4.))
                    .text_size(metrics::text_sm())
                    .cursor_pointer()
                    .text_color(if selected { accent } else { faint })
                    .when(selected, |el| el.bg(ui.tab_active_bg.hsla()).shadow_sm())
                    .when(!selected, |el| el.hover(|s| s.text_color(accent)))
                    .child(Icon::new(icon).size(IconSize::XSmall).color(if selected {
                        accent
                    } else {
                        faint
                    }))
                    .tab_index(0)
                    .focus(lector_ui::a11y::ring_focus(cx))
                    .child(label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.set_layout(l, window, cx)
                    }))
            };
        h_flex()
            .flex_none()
            .h(px(40.))
            .px(px(12.))
            .gap(px(8.))
            .border_b_1()
            .border_color(ui.border.hsla())
            .bg(ui.tab_bar_bg.hsla())
            .child(FileIcon::file(file_name).size(px(16.)))
            .child(
                h_flex()
                    .min_w_0()
                    .flex_shrink()
                    .gap(px(6.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui.text.hsla())
                            .child(name),
                    )
                    .when(!dir.is_empty(), |el| {
                        el.child(
                            div()
                                .text_size(metrics::text_sm())
                                .text_color(ui.text_faint.hsla())
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(dir),
                        )
                    }),
            )
            .when_some(kind, |el, kind| {
                let color = kind_color(kind, cx);
                el.child(
                    div()
                        .flex_none()
                        .px(px(6.))
                        .py(px(1.))
                        .rounded(px(4.))
                        .text_size(metrics::text_xs())
                        .font_weight(FontWeight::MEDIUM)
                        .bg(color.opacity(0.14))
                        .text_color(color)
                        .child(kind_label(kind)),
                )
            })
            .when_some(stats, |el, s| el.child(stats_label(s, &theme)))
            .child(div().flex_1())
            .when(self.loading, |el| {
                el.child(
                    div()
                        .text_size(metrics::text_sm())
                        .text_color(ui.text_faint.hsla())
                        .child("Loading…"),
                )
            })
            .when(hunks > 0, |el| {
                el.child(
                    h_flex()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(metrics::text_xs())
                                .text_color(ui.text_faint.hsla())
                                .mr(px(2.))
                                .child(format!(
                                    "{hunks} hunk{}",
                                    if hunks == 1 { "" } else { "s" }
                                )),
                        )
                        .child(
                            IconButton::new("prev-hunk", IconName::ArrowUp)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::for_action(
                                    "Previous Hunk",
                                    &diff_actions::PrevHunk,
                                ))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.go_to_hunk(-1, cx);
                                })),
                        )
                        .child(
                            IconButton::new("next-hunk", IconName::ArrowDown)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::for_action("Next Hunk", &diff_actions::NextHunk))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.go_to_hunk(1, cx);
                                })),
                        ),
                )
            })
            .children(review_controls)
            .child(
                IconButton::new(
                    "expand-all",
                    if self.expand_all {
                        IconName::ListCollapse
                    } else {
                        IconName::ChevronsUpDown
                    },
                )
                .icon_size(IconSize::Small)
                .selected(self.expand_all)
                .tooltip(Tooltip::for_action(
                    if self.expand_all {
                        "Collapse Unchanged Lines"
                    } else {
                        "Expand All Lines"
                    },
                    &diff_actions::ExpandAllContext,
                ))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_expand_all(cx))),
            )
            .child(
                IconButton::new("ignore-ws", IconName::WholeWord)
                    .icon_size(IconSize::Small)
                    .selected(self.ignore_whitespace)
                    .tooltip(Tooltip::for_action(
                        if self.ignore_whitespace {
                            "Show Whitespace Changes"
                        } else {
                            "Ignore Whitespace"
                        },
                        &diff_actions::ToggleIgnoreWhitespace,
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_ignore_whitespace(cx))),
            )
            .child(
                h_flex()
                    .p(px(2.))
                    .gap(px(2.))
                    .rounded(px(6.))
                    .bg(ui.hover.hsla())
                    .child(seg(
                        "layout-split",
                        "Split",
                        IconName::Columns,
                        split,
                        Layout::Split,
                    ))
                    .child(seg(
                        "layout-unified",
                        "Unified",
                        IconName::AlignJustify,
                        !split,
                        Layout::Unified,
                    )),
            )
            .child(
                IconButton::new("open-file", IconName::FileCode)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::for_action(
                        "Open File",
                        &diff_actions::OpenInEditor,
                    ))
                    .on_click(cx.listener(|this, _, window, cx| this.open_in_editor(window, cx))),
            )
            .when_some(viewed, |el, viewed| {
                el.child(
                    viewed_checkbox("viewed-toggle", viewed, cx)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_viewed(cx))),
                )
            })
    }

    fn render_body(&self, layout: Layout, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let message = |icon: IconName, title: String, detail: Option<String>| {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .child(
                    Icon::new(icon)
                        .size(IconSize::Custom(px(32.)))
                        .color(ui.text_faint.hsla()),
                )
                .child(div().text_color(ui.text_muted.hsla()).child(title))
                .when_some(detail, |el, d| {
                    el.child(
                        div()
                            .text_size(metrics::text_sm())
                            .text_color(ui.text_faint.hsla())
                            .child(d),
                    )
                })
                .into_any_element()
        };
        if let Some(err) = &self.error {
            return message(
                IconName::AlertTriangle,
                "Could not load diff".into(),
                Some(err.to_string()),
            );
        }
        let Some(loaded) = &self.loaded else {
            return div().size_full().into_any_element();
        };
        let diff = &loaded.diff;
        let sizes = format!(
            "{} → {}",
            if diff.old.exists {
                format_size(diff.old.size)
            } else {
                "—".into()
            },
            if diff.new.exists {
                format_size(diff.new.size)
            } else {
                "—".into()
            }
        );
        match diff.kind {
            DiffKind::Binary => {
                return message(
                    IconName::Binary,
                    match diff.change.kind {
                        ChangeKind::Added => "Binary file added".into(),
                        ChangeKind::Deleted => "Binary file deleted".into(),
                        _ => "Binary file changed".into(),
                    },
                    Some(sizes),
                )
            }
            DiffKind::TooLarge => {
                return message(
                    IconName::FileText,
                    "File too large to diff".into(),
                    Some(sizes),
                )
            }
            DiffKind::Image(_) => {
                let side = |label: &'static str,
                            image: Option<Arc<gpui::Image>>,
                            size: u64,
                            bg: Hsla| {
                    v_flex()
                        .flex_1()
                        .h_full()
                        .min_w_0()
                        .items_center()
                        .gap(px(8.))
                        .p(px(16.))
                        .child(
                            div()
                                .text_size(metrics::text_sm())
                                .text_color(ui.text_muted.hsla())
                                .child(match &image {
                                    Some(_) => format!("{label} · {}", format_size(size)),
                                    None => format!("{label} · none"),
                                }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .w_full()
                                .min_h_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(6.))
                                .border_1()
                                .border_color(bg)
                                .bg(ui.surface.hsla())
                                .when_some(image, |el, image| {
                                    el.child(
                                        img(ImageSource::Image(image)).max_w_full().max_h_full(),
                                    )
                                }),
                        )
                };
                let (old_img, new_img) = loaded.images.clone().unwrap_or((None, None));
                return h_flex()
                    .size_full()
                    .items_start()
                    .child(side(
                        "Before",
                        old_img,
                        diff.old.size,
                        theme.diff.removed_gutter.hsla().opacity(0.6),
                    ))
                    .child(side(
                        "After",
                        new_img,
                        diff.new.size,
                        theme.diff.added_gutter.hsla().opacity(0.6),
                    ))
                    .into_any_element();
            }
            DiffKind::Text => {}
        }
        if diff.is_identical() {
            let detail = if diff.change.kind == ChangeKind::Renamed {
                Some(diff.display_path())
            } else if self.ignore_whitespace {
                Some("Only whitespace changed".into())
            } else {
                None
            };
            let title = match diff.change.kind {
                ChangeKind::Renamed => "File renamed without changes",
                ChangeKind::Added => "Empty file added",
                ChangeKind::Deleted => "Empty file deleted",
                _ => "No changes",
            };
            return message(IconName::CircleCheck, title.into(), detail);
        }
        match layout {
            Layout::Split => h_flex()
                .size_full()
                .items_start()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(self.old_view.clone()),
                )
                .child(div().w(px(1.)).h_full().bg(ui.border.hsla()))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(self.new_view.clone()),
                )
                .into_any_element(),
            Layout::Unified => div()
                .size_full()
                .child(self.unified_view.clone())
                .into_any_element(),
        }
    }
}

impl Render for DiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.relayout_comments_if_resized(window, cx);
        // Focused while there was nothing to show (loading, "No changes"): hand
        // the focus to the code view once the diff appears.
        if self.focus_handle.is_focused(window) && self.shows_code_views() {
            let handle = Focusable::focus_handle(self, cx);
            window.focus(&handle);
        }
        let layout = self.layout(window, cx);
        if layout != self.last_layout {
            self.last_layout = layout;
            self.current_hunk = None;
        }
        let ui = cx.theme().ui.clone();
        let has_actions = !self.hunk_actions().is_empty();
        let toolbars = self.render_hunk_toolbars(window, cx);
        v_flex()
            .id("diff-view")
            .key_context(contexts::DIFF_VIEW)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(ui.background.hsla())
            .on_action(
                cx.listener(|this, _: &diff_actions::ToggleLayout, window, cx| {
                    this.toggle_layout(window, cx)
                }),
            )
            .on_action(cx.listener(
                |this, _: &lector_ui::actions::review::AddComment, window, cx| {
                    this.add_comment_at_cursor(window, cx)
                },
            ))
            .on_action(cx.listener(
                |this, _: &lector_ui::actions::review::NextComment, window, cx| {
                    if !this.go_to_comment(1, window, cx) {
                        cx.propagate();
                    }
                },
            ))
            .on_action(cx.listener(
                |this, _: &lector_ui::actions::review::PrevComment, window, cx| {
                    if !this.go_to_comment(-1, window, cx) {
                        cx.propagate();
                    }
                },
            ))
            .on_action(cx.listener(|this, _: &diff_actions::NextHunk, _, cx| {
                if !this.go_to_hunk(1, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &diff_actions::PrevHunk, _, cx| {
                if !this.go_to_hunk(-1, cx) {
                    cx.propagate();
                }
            }))
            .on_action(
                cx.listener(|this, _: &diff_actions::ToggleIgnoreWhitespace, _, cx| {
                    this.toggle_ignore_whitespace(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &diff_actions::ExpandAllContext, _, cx| {
                    this.toggle_expand_all(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &diff_actions::OpenInEditor, window, cx| {
                    this.open_in_editor(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &diff_actions::ToggleMovedCode, _, cx| {
                    this.toggle_moved_code(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &diff_actions::ToggleStructural, _, cx| {
                    this.toggle_structural(cx)
                }),
            )
            .on_action(cx.listener(
                |this, _: &lector_ui::actions::review::ToggleHunkReviewed, _, cx| {
                    this.toggle_hunk_reviewed_at_cursor(cx)
                },
            ))
            .on_action(cx.listener(
                |this, _: &lector_ui::actions::review::NextUnreviewed, _, cx| {
                    this.next_unreviewed_action(cx)
                },
            ))
            .on_action(cx.listener(|this, _: &diff_actions::ToggleViewed, _, cx| {
                if this.viewed.is_some() {
                    this.toggle_viewed(cx)
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &git_actions::StageHunk, window, cx| {
                if this.hunk_actions().contains(&HunkAction::Stage) {
                    this.hunk_action(HunkAction::Stage, None, false, window, cx)
                } else {
                    cx.propagate();
                }
            }))
            .on_action(
                cx.listener(|this, _: &git_actions::UnstageHunk, window, cx| {
                    if this.hunk_actions().contains(&HunkAction::Unstage) {
                        this.hunk_action(HunkAction::Unstage, None, false, window, cx)
                    } else {
                        cx.propagate();
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &git_actions::DiscardHunk, window, cx| {
                    if this.hunk_actions().contains(&HunkAction::Discard) {
                        this.hunk_action(HunkAction::Discard, None, false, window, cx)
                    } else {
                        cx.propagate();
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &git_actions::StageSelectedLines, window, cx| {
                    if this.hunk_actions().contains(&HunkAction::Stage) {
                        this.hunk_action(HunkAction::Stage, None, true, window, cx)
                    } else {
                        cx.propagate();
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &git_actions::UnstageSelectedLines, window, cx| {
                    if this.hunk_actions().contains(&HunkAction::Unstage) {
                        this.hunk_action(HunkAction::Unstage, None, true, window, cx)
                    } else {
                        cx.propagate();
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &git_actions::Stage, window, cx| {
                this.file_action(HunkAction::Stage, window, cx)
            }))
            .on_action(cx.listener(|this, _: &git_actions::Unstage, window, cx| {
                this.file_action(HunkAction::Unstage, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &git_actions::DiscardChanges, window, cx| {
                    this.file_action(HunkAction::Discard, window, cx)
                }),
            )
            .child(self.render_header(layout, cx))
            .children(self.render_structural_notice(cx))
            .when_some(self.op_error.clone(), |el, e| {
                el.child(
                    h_flex()
                        .flex_none()
                        .items_start()
                        .gap(px(8.))
                        .px(px(12.))
                        .py(px(6.))
                        .border_b_1()
                        .border_color(ui.error.hsla().opacity(0.6))
                        .bg(ui.error.hsla().opacity(0.08))
                        .child(
                            Icon::new(IconName::CircleX)
                                .size(IconSize::Small)
                                .color(ui.error.hsla()),
                        )
                        .child(
                            div()
                                .id("diff-op-error")
                                .flex_1()
                                .max_h(px(120.))
                                .overflow_y_scroll()
                                .text_size(metrics::text_sm())
                                .text_color(ui.text.hsla())
                                .child(e),
                        )
                        .child(
                            IconButton::new("diff-op-error-close", IconName::Close)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Dismiss"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.op_error = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .when(has_actions, |el| {
                        el.capture_any_mouse_down(cx.listener(
                            |this, e: &MouseDownEvent, window, cx| {
                                if e.button == MouseButton::Right {
                                    cx.stop_propagation();
                                    this.deploy_menu(e.position, window, cx);
                                }
                            },
                        ))
                    })
                    .child(self.render_body(layout, cx))
                    .children(toolbars),
            )
            .children(self.menu.render())
            .children(self.render_provenance(window, cx))
    }
}
