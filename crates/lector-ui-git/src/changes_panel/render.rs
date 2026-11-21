//! Rendering of the [`ChangesPanel`]: the mode label and totals, section
//! headers and their actions, file rows and their hover actions, and the
//! `Render` implementation.

use gpui::{div, prelude::*, App, ClickEvent, FontWeight, MouseButton, MouseDownEvent, Window};
use lector_diff::DiffStats;
use lector_git::ChangeKind;
use lector_review::risk::RiskLevel;
use lector_ui::actions::git as git_actions;
use lector_ui::actions::picker as picker_actions;
use lector_ui::actions::workspace as ws_actions;
use lector_ui::components::list::virtual_list;
use lector_ui::components::{h_flex, metrics, v_flex, Button, IconButton, Tooltip};
use lector_ui::icons::{FileIcon, Icon, IconName, IconSize};
use lector_ui::keymap::contexts;
use lector_ui::scale::px;
use lector_ui::{ActiveTheme, ColorExt};

use super::{ChangesMode, ChangesPanel, Row, Section};
use crate::util::{kind_color, kind_letter, split_path};

impl ChangesPanel {
    fn mode_label(&self, cx: &App) -> String {
        match &self.mode {
            ChangesMode::WorkingTree => "Working Tree".into(),
            ChangesMode::Changeset(cs) => {
                if let Some(content) = &self.content {
                    if let Some((_, v)) = content.sections.first() {
                        return v.title.clone();
                    }
                }
                lector_ui::commands_bridge::core(cx)
                    .review()
                    .changeset_title(self.store.read(cx).repo(), cs)
            }
        }
    }

    fn totals(&self) -> (usize, DiffStats) {
        let mut n = 0;
        let mut s = DiffStats::default();
        if let Some(c) = &self.content {
            for (_, v) in &c.sections {
                n += v.files.len();
                let t = v.total_stats();
                s.added += t.added;
                s.removed += t.removed;
            }
        }
        (n, s)
    }

    /// Icon buttons of a section header (Stage All, Unstage All, Discard All).
    fn header_actions(&self, section: Section, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let busy = self.op_task.is_some();
        let button = |id: &'static str, icon: IconName, tip: &'static str| {
            IconButton::new(id, icon)
                .icon_size(IconSize::XSmall)
                .disabled(busy)
                .tooltip(Tooltip::text(tip))
        };
        match section {
            Section::Staged => vec![button("unstage-all", IconName::Minus, "Unstage All")
                .on_click(cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.unstage_all(cx);
                }))
                .into_any_element()],
            Section::Unstaged => vec![
                button(
                    "discard-all-unstaged",
                    IconName::Undo,
                    "Discard All Changes",
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.discard_all(Some(Section::Unstaged), window, cx);
                }))
                .into_any_element(),
                button("stage-all-unstaged", IconName::Plus, "Stage All Changes")
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.stage_all(Some(Section::Unstaged), cx);
                    }))
                    .into_any_element(),
            ],
            Section::Untracked => vec![
                button(
                    "delete-all-untracked",
                    IconName::Trash,
                    "Delete All Untracked Files",
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.discard_all(Some(Section::Untracked), window, cx);
                }))
                .into_any_element(),
                button(
                    "stage-all-untracked",
                    IconName::Plus,
                    "Stage All Untracked Files",
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.stage_all(Some(Section::Untracked), cx);
                }))
                .into_any_element(),
            ],
            Section::Conflicts | Section::Files => Vec::new(),
        }
    }

    /// Hover buttons of a file row (stage / unstage / discard).
    fn row_actions(
        &self,
        section: Section,
        si: usize,
        fi: usize,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let busy = self.op_task.is_some();
        let button = |name: &'static str, icon: IconName, tip: &'static str| {
            IconButton::new((name, ix), icon)
                .icon_size(IconSize::Small)
                .square(px(20.))
                .disabled(busy)
                .tooltip(Tooltip::text(tip))
        };
        let stage = || {
            button("row-stage", IconName::Plus, "Stage").on_click(cx.listener(
                move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_stage(si, fi, cx);
                },
            ))
        };
        match section {
            Section::Staged => vec![button("row-unstage", IconName::Minus, "Unstage")
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_stage(si, fi, cx);
                }))
                .into_any_element()],
            Section::Unstaged => vec![
                button("row-discard", IconName::Undo, "Discard Changes")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.discard_file(si, fi, window, cx);
                    }))
                    .into_any_element(),
                stage().into_any_element(),
            ],
            Section::Untracked => vec![
                button("row-delete", IconName::Trash, "Delete File")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.discard_file(si, fi, window, cx);
                    }))
                    .into_any_element(),
                stage().into_any_element(),
            ],
            Section::Conflicts => {
                vec![
                    button("row-resolve", IconName::Check, "Mark Resolved (Stage)")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_stage(si, fi, cx);
                        }))
                        .into_any_element(),
                ]
            }
            Section::Files => Vec::new(),
        }
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        match self.rows.get(ix).cloned() {
            Some(Row::Header(section, si)) => {
                let count = self
                    .content
                    .as_ref()
                    .and_then(|c| c.sections.get(si))
                    .map(|(_, v)| v.files.len())
                    .unwrap_or(0);
                let collapsed = self.collapsed.contains(&section);
                h_flex()
                    .id(("changes-header", ix))
                    .w_full()
                    .h(metrics::row_height())
                    .pl(px(6.))
                    .pr(px(10.))
                    .gap(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(ui.hover.hsla()))
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::XSmall)
                        .color(ui.text_faint.hsla()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(metrics::text_xs())
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(if section == Section::Conflicts {
                                theme.git.conflicted.hsla()
                            } else {
                                ui.text_muted.hsla()
                            })
                            .child(section.title().to_uppercase()),
                    )
                    .children(self.header_actions(section, cx))
                    .child(
                        div()
                            .ml(px(2.))
                            .px(px(6.))
                            .rounded(px(8.))
                            .bg(ui.hover.hsla())
                            .text_size(metrics::text_xs())
                            .text_color(ui.text_muted.hsla())
                            .child(count.to_string()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.collapsed.remove(&section) {
                            this.collapsed.insert(section);
                        }
                        this.rebuild_rows();
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Some(Row::File(si, fi)) => {
                let Some((section, _, file)) = self.file(si, fi) else {
                    return div().into_any_element();
                };
                let path = file.path().to_string();
                let (name, dir) = split_path(&path);
                let dir = match (&file.change.old_path, &file.change.new_path) {
                    (Some(o), Some(n)) if o != n => format!("← {o}"),
                    _ => dir.to_string(),
                };
                // Risky files show why instead of where (the path is in the tooltip).
                let risk = self.file_risk(file).cloned();
                let reason = risk
                    .as_ref()
                    .filter(|r| r.level != RiskLevel::Low)
                    .and_then(|r| r.reason.clone());
                let badge = risk.as_ref().and_then(|r| match r.level {
                    RiskLevel::High => Some(("High", theme.ui.error.hsla())),
                    RiskLevel::Medium => Some(("Med", theme.git.modified.hsla())),
                    RiskLevel::Low => None,
                });
                let dir_or_reason = reason.clone().unwrap_or(dir);
                let selected = self.selected.as_ref() == Some(&(section, path.clone()));
                let kind = if section == Section::Conflicts {
                    None
                } else {
                    Some(file.change.kind)
                };
                let letter_color = match kind {
                    Some(k) if section == Section::Untracked => {
                        let _ = k;
                        theme.git.untracked.hsla()
                    }
                    Some(k) => kind_color(k, cx),
                    None => theme.git.conflicted.hsla(),
                };
                let letter = match (section, kind) {
                    (Section::Untracked, _) => "U",
                    (_, Some(k)) => kind_letter(k),
                    (_, None) => "!",
                };
                let deleted = file.change.kind == ChangeKind::Deleted;
                let stats = file.stats;
                let row_bg = if selected {
                    ui.active.hsla()
                } else {
                    ui.hover.hsla()
                };
                let actions = self.row_actions(section, si, fi, ix, cx);
                let has_actions = !actions.is_empty();
                h_flex()
                    .id(("changes-file", ix))
                    .group("changes-row")
                    .relative()
                    .w_full()
                    .h(metrics::row_height())
                    .pl(px(22.))
                    .pr(px(10.))
                    .gap(px(6.))
                    .cursor_pointer()
                    .when(selected, |el| el.bg(ui.active.hsla()))
                    .when(!selected, |el| el.hover(|s| s.bg(ui.hover.hsla())))
                    .when(!actions.is_empty(), |el| {
                        el.child(
                            h_flex()
                                .absolute()
                                .right(px(26.))
                                .top_0()
                                .h_full()
                                .pl(px(4.))
                                .gap(px(1.))
                                .bg(row_bg)
                                .invisible()
                                .group_hover("changes-row", |s| s.visible())
                                .children(actions),
                        )
                    })
                    .child(FileIcon::file(name.to_string()).size(px(16.)))
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(6.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(metrics::text())
                                    .text_color(if deleted {
                                        ui.text_muted.hsla()
                                    } else {
                                        ui.text.hsla()
                                    })
                                    .when(deleted, |el| el.line_through())
                                    .child(name.to_string()),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_size(metrics::text_sm())
                                    .text_color(if reason.is_some() {
                                        ui.text_muted.hsla()
                                    } else {
                                        ui.text_faint.hsla()
                                    })
                                    .child(dir_or_reason),
                            ),
                    )
                    .when_some(badge, |el, (label, color)| {
                        el.child(
                            div()
                                .flex_none()
                                .px(px(5.))
                                .rounded(px(3.))
                                .text_size(metrics::text_xs())
                                .font_weight(FontWeight::MEDIUM)
                                .bg(color.opacity(0.14))
                                .text_color(color)
                                .child(label),
                        )
                    })
                    .when_some(stats.filter(|s| s.added + s.removed > 0), |el, s| {
                        el.child(
                            h_flex()
                                .flex_none()
                                .gap(px(4.))
                                .when(has_actions, |el| {
                                    el.group_hover("changes-row", |s| s.invisible())
                                })
                                .text_size(metrics::text_xs())
                                .when(s.added > 0, |el| {
                                    el.child(
                                        div()
                                            .text_color(theme.diff.added_text.hsla())
                                            .child(format!("+{}", s.added)),
                                    )
                                })
                                .when(s.removed > 0, |el| {
                                    el.child(
                                        div()
                                            .text_color(theme.diff.removed_text.hsla())
                                            .child(format!("−{}", s.removed)),
                                    )
                                }),
                        )
                    })
                    .when(file.binary, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(metrics::text_xs())
                                .text_color(ui.text_faint.hsla())
                                .child("bin"),
                        )
                    })
                    .when(file.viewed, |el| {
                        el.child(
                            Icon::new(IconName::Check)
                                .size(IconSize::XSmall)
                                .color(ui.text_faint.hsla()),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .w(px(12.))
                            .text_size(metrics::text_sm())
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(letter_color)
                            .child(letter),
                    )
                    .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                        window.focus(&this.focus_handle);
                        this.open_file_diff(si, fi, e.click_count() < 2, window, cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                            this.deploy_menu(si, fi, e.position, window, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .into_any_element()
            }
            Some(Row::LowHeader(count)) => {
                let collapsed = self.low_collapsed;
                h_flex()
                    .id(("changes-low-header", ix))
                    .w_full()
                    .h(metrics::row_height())
                    .pl(px(14.))
                    .pr(px(10.))
                    .gap(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(ui.hover.hsla()))
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::XSmall)
                        .color(ui.text_faint.hsla()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(metrics::text_xs())
                            .text_color(ui.text_faint.hsla())
                            .child("Low-risk & generated"),
                    )
                    .child(
                        div()
                            .text_size(metrics::text_xs())
                            .text_color(ui.text_faint.hsla())
                            .child(count.to_string()),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.low_collapsed = !this.low_collapsed;
                        this.rebuild_rows();
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Some(Row::Empty) => v_flex()
                .w_full()
                .h(metrics::row_height() * 3.)
                .items_center()
                .justify_center()
                .text_size(metrics::text_sm())
                .text_color(ui.text_faint.hsla())
                .child(match self.mode {
                    ChangesMode::WorkingTree => "Working tree clean",
                    _ => "No changes",
                })
                .into_any_element(),
            None => div().into_any_element(),
        }
    }
}

impl Render for ChangesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let (count, totals) = self.totals();
        let label = self.mode_label(cx);
        let rows = self.rows.len();
        let entity = cx.entity();
        let working_tree = self.mode == ChangesMode::WorkingTree;
        v_flex()
            .id("changes-panel")
            .size_full()
            .child(self.banner.clone())
            .child(
                v_flex()
                    .flex_none()
                    .px(px(8.))
                    .pb(px(6.))
                    .gap(px(6.))
                    .border_b_1()
                    .border_color(ui.border_variant.hsla())
                    .children(self.sync_bar.clone())
                    .child(
                        h_flex()
                            .gap(px(4.))
                            .child(
                                h_flex()
                                    .id("changes-mode")
                                    .flex_1()
                                    .min_w_0()
                                    .h(px(26.))
                                    .px(px(8.))
                                    .gap(px(6.))
                                    .rounded(px(5.))
                                    .border_1()
                                    .border_color(ui.border.hsla())
                                    .bg(ui.input_bg.hsla())
                                    .cursor_pointer()
                                    .hover(|s| s.border_color(ui.text_faint.hsla()))
                                    .child(
                                        Icon::new(IconName::GitCompare)
                                            .size(IconSize::XSmall)
                                            .color(ui.text_muted.hsla()),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .text_size(metrics::text_sm())
                                            .child(label),
                                    )
                                    .child(
                                        Icon::new(IconName::ChevronDown)
                                            .size(IconSize::XSmall)
                                            .color(ui.text_faint.hsla()),
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, e: &MouseDownEvent, window, cx| {
                                            this.deploy_mode_menu(e.position, window, cx);
                                            cx.stop_propagation();
                                        }),
                                    ),
                            )
                            .child(
                                IconButton::new("changes-refresh", IconName::Refresh)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Refresh"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.refresh(cx);
                                    })),
                            ),
                    )
                    .when(working_tree, |el| el.child(self.composer.clone()))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .pl(px(2.))
                            .text_size(metrics::text_sm())
                            .child(div().text_color(ui.text_muted.hsla()).child(format!(
                                "{count} file{}",
                                if count == 1 { "" } else { "s" }
                            )))
                            .when(totals.added + totals.removed > 0, |el| {
                                el.child(crate::diff_view::stats_label(totals, &theme))
                            })
                            .child(div().flex_1())
                            .child(
                                Button::new("review-all", "Review")
                                    .icon(IconName::Eye)
                                    .compact()
                                    .disabled(count == 0)
                                    .tooltip(Tooltip::text("Review all files in a tab"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let cs = this.review_changeset();
                                        crate::review_tab::open_review(&this.store, cs, window, cx);
                                    })),
                            ),
                    ),
            )
            .when_some(self.op_error.clone(), |el, e| {
                el.child(
                    h_flex()
                        .mx(px(8.))
                        .mt(px(6.))
                        .items_start()
                        .gap(px(6.))
                        .p(px(8.))
                        .rounded(metrics::radius())
                        .border_1()
                        .border_color(ui.error.hsla().opacity(0.7))
                        .bg(ui.error.hsla().opacity(0.08))
                        .child(
                            Icon::new(IconName::CircleX)
                                .size(IconSize::XSmall)
                                .color(ui.error.hsla()),
                        )
                        .child(
                            div()
                                .id("changes-op-error")
                                .flex_1()
                                .min_w_0()
                                .max_h(px(120.))
                                .overflow_y_scroll()
                                .text_size(metrics::text_xs())
                                .text_color(ui.text.hsla())
                                .child(e),
                        )
                        .child(
                            IconButton::new("changes-op-error-close", IconName::Close)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text("Dismiss"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.op_error = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .when_some(self.error.clone(), |el, e| {
                el.child(
                    div()
                        .p(px(12.))
                        .text_size(metrics::text_sm())
                        .text_color(ui.text_faint.hsla())
                        .child(e),
                )
            })
            .when(self.content.is_none() && self.error.is_none(), |el| {
                el.child(
                    div()
                        .p(px(12.))
                        .text_size(metrics::text_sm())
                        .text_color(ui.text_faint.hsla())
                        .child("Loading…"),
                )
            })
            .child(
                div()
                    .id("changes-list-container")
                    .key_context(contexts::CHANGES_PANEL)
                    .track_focus(&self.focus_handle)
                    .on_action(
                        cx.listener(|this, _: &git_actions::Stage, _, cx| this.toggle_selected(cx)),
                    )
                    .on_action(cx.listener(|this, _: &git_actions::Unstage, _, cx| {
                        this.unstage_selected(cx)
                    }))
                    .on_action(
                        cx.listener(|this, _: &git_actions::DiscardChanges, window, cx| {
                            this.discard_selected(window, cx)
                        }),
                    )
                    .on_action(cx.listener(|this, _: &picker_actions::SelectNext, _, cx| {
                        this.move_selection(1, cx)
                    }))
                    .on_action(cx.listener(|this, _: &picker_actions::SelectPrev, _, cx| {
                        this.move_selection(-1, cx)
                    }))
                    .on_action(
                        cx.listener(|this, _: &picker_actions::Confirm, window, cx| {
                            if let Some(Row::File(si, fi)) = this
                                .selected_row_index()
                                .and_then(|i| this.rows.get(i).cloned())
                            {
                                this.open_file_diff(si, fi, true, window, cx);
                            }
                        }),
                    )
                    .on_action(cx.listener(|this, _: &git_actions::Commit, window, cx| {
                        this.composer.update(cx, |c, cx| c.commit(window, cx));
                    }))
                    .on_action(
                        cx.listener(|this, _: &ws_actions::ShowContextMenu, window, cx| {
                            let Some(ix) = this.selected_row_index() else {
                                cx.propagate();
                                return;
                            };
                            if let Some(Row::File(si, fi)) = this.rows.get(ix).cloned() {
                                let position = this.list.row_anchor(ix, metrics::row_height());
                                this.deploy_menu(si, fi, position, window, cx);
                            }
                        }),
                    )
                    .flex_1()
                    .min_h_0()
                    .pt(px(4.))
                    .child(virtual_list(
                        "changes-list",
                        rows,
                        &self.list,
                        cx,
                        move |range, _window, cx| {
                            entity.update(cx, |this, cx| {
                                range.map(|ix| this.render_row(ix, cx)).collect()
                            })
                        },
                    )),
            )
            .children(self.menu.render())
    }
}
