//! Rendering of the [`AgentPanel`]: header (agent picker, status), the conversation
//! (turn headers, prompts, Markdown answers, thoughts, tool-call cards with mini diffs,
//! inline permission prompts, plan checklists) and the prompt box with context chips.

use std::rc::Rc;

use gpui::{
    div, prelude::*, AnyElement, App, Context, Focusable, FontWeight, Hsla, MouseButton,
    SharedString, Window,
};
use lector_agent::{
    MessageKind, PermissionChoice, PermissionRequest, PlanStatus, SessionStatus, ToolCall,
    ToolDiff, ToolStatus, Turn, TurnItem, TurnState,
};
use lector_diff::{diff_lines, DiffOptions};
use lector_ui::actions::agent as agent_actions;
use lector_ui::components::{h_flex, metrics, v_flex, Button, ButtonStyle, IconButton, Tooltip};
use lector_ui::icons::{Icon, IconName, IconSize};
use lector_ui::markdown::{self, RenderOptions};
use lector_ui::scale::px;
use lector_ui::{ActiveTheme, ColorExt};

use crate::panel::AgentPanel;

/// Changed lines shown per tool-call diff.
const MINI_DIFF_LINES: usize = 8;

fn kind_icon(kind: &str) -> IconName {
    match kind {
        "edit" => IconName::FileDiff,
        "read" => IconName::FileText,
        "search" => IconName::Search,
        "execute" => IconName::Command,
        "delete" => IconName::Trash,
        "move" => IconName::ArrowRight,
        "fetch" => IconName::Globe,
        "think" => IconName::Lightbulb,
        _ => IconName::Code,
    }
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

impl AgentPanel {
    fn mono(&self, cx: &App) -> SharedString {
        lector_ui::code_view::EditorSettings::get(cx).font_family
    }

    fn render_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let name: SharedString = match (&self.session, self.selected()) {
            (Some(s), _) => s.agent_name().into(),
            (None, Some(c)) => c.name.clone().into(),
            (None, None) => "No agent".into(),
        };
        let (dot, label): (Hsla, &str) = match (&self.status, self.starting) {
            (_, true) => (ui.warning.hsla(), "Starting…"),
            (Some(SessionStatus::Ready), _) => (ui.success.hsla(), "Ready"),
            (Some(SessionStatus::Busy(_)), _) => (ui.accent.hsla(), "Working"),
            (Some(SessionStatus::Crashed(_)), _) => (ui.error.hsla(), "Exited"),
            (Some(SessionStatus::Starting), _) => (ui.warning.hsla(), "Starting…"),
            (Some(SessionStatus::Stopped), _) => (ui.text_faint.hsla(), "Stopped"),
            (None, _) => (ui.text_faint.hsla(), "Not started"),
        };
        let can_pick = self.configs.len() > 1;
        h_flex()
            .px(px(8.))
            .py(px(6.))
            .gap(px(6.))
            .border_b_1()
            .border_color(ui.border_variant.hsla())
            .child(
                div()
                    .id("agent-picker")
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .px(px(6.))
                    .py(px(2.))
                    .rounded(metrics::radius())
                    .min_w_0()
                    .when(can_pick, |el| {
                        el.hover(|s| s.bg(ui.hover.hsla())).cursor_pointer()
                    })
                    .child(
                        Icon::new(IconName::Bot)
                            .size(IconSize::Small)
                            .color(ui.text_accent.hsla()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(metrics::text_sm())
                            .text_color(ui.text.hsla())
                            .child(name),
                    )
                    .when(can_pick, |el| {
                        el.child(
                            Icon::new(IconName::ChevronDown)
                                .size(IconSize::XSmall)
                                .color(ui.text_faint.hsla()),
                        )
                    })
                    .tooltip(Tooltip::text(
                        self.selected()
                            .map(|c| c.command_line())
                            .unwrap_or_else(|| "No agent configured".into()),
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &gpui::MouseDownEvent, window, cx| {
                            if this.configs.len() > 1 {
                                this.deploy_picker(ev.position, false, window, cx);
                            }
                        }),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(4.))
                    .text_size(metrics::text_xs())
                    .text_color(ui.text_muted.hsla())
                    .child(div().size(px(7.)).rounded_full().bg(dot))
                    .child(label),
            )
            .child(div().flex_1())
            .when(
                matches!(
                    self.status,
                    Some(SessionStatus::Crashed(_)) | Some(SessionStatus::Stopped)
                ),
                |el| {
                    el.child(
                        IconButton::new("agent-restart", IconName::Refresh)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Restart the agent"))
                            .on_click(cx.listener(|this, _, _, cx| this.restart(cx))),
                    )
                },
            )
            .child(
                IconButton::new("agent-new", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("New session"))
                    .on_click(cx.listener(|this, _, _, cx| this.start(cx))),
            )
    }

    fn render_empty(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let ready_name = match (&self.session, &self.status) {
            (Some(s), Some(SessionStatus::Ready)) => Some(s.agent_name()),
            _ => None,
        };
        let (title, body): (SharedString, String) = if let Some(name) = ready_name {
            (
                format!("{name} is ready").into(),
                "Describe a change below. Attach the active file, the selected lines or the \
                 current diff with the buttons under the prompt."
                    .into(),
            )
        } else if self.configs.is_empty() {
            (
                "No agent found".into(),
                "Install an Agent Client Protocol agent — Gemini CLI, claude-code-acp or codex-acp — \
                 or add one to settings.toml under [[agent.servers]]."
                    .into(),
            )
        } else if self.starting {
            ("Starting the agent…".into(), String::new())
        } else {
            (
                "Work with a coding agent".into(),
                "Each turn is checkpointed: review what the agent changed as a diff, send feedback \
                 on lines, or undo the whole turn."
                    .into(),
            )
        };
        v_flex()
            .items_center()
            .gap(px(8.))
            .px(px(16.))
            .py(px(28.))
            .child(
                Icon::new(IconName::Sparkles)
                    .size(IconSize::Large)
                    .color(ui.text_faint.hsla()),
            )
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui.text.hsla())
                    .child(title),
            )
            .when(!body.is_empty(), |el| {
                el.child(
                    div()
                        .text_center()
                        .text_size(metrics::text_sm())
                        .text_color(ui.text_muted.hsla())
                        .child(body),
                )
            })
            .when(
                !self.configs.is_empty() && self.session.is_none() && !self.starting,
                |el| {
                    let label = format!(
                        "Start {}",
                        self.selected().map(|c| c.name.as_str()).unwrap_or("agent")
                    );
                    el.child(
                        Button::new("agent-start", label)
                            .icon(IconName::Bot)
                            .style(ButtonStyle::Accent)
                            .on_click(cx.listener(|this, _, _, cx| this.start(cx))),
                    )
                },
            )
            .into_any_element()
    }

    fn render_turn(
        &mut self,
        turn: &Turn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let id = turn.id;
        let mut col = v_flex().gap(px(8.)).w_full();

        // Header: "Turn 3 · completed" + "3 files changed · Review · Undo turn".
        let (state_icon, state_color, state_label): (IconName, Hsla, String) = match &turn.state {
            TurnState::Starting | TurnState::Running => {
                (IconName::Loader, ui.accent.hsla(), "Working".into())
            }
            TurnState::Completed(r) if r == "end_turn" => {
                (IconName::CircleCheck, ui.success.hsla(), "Done".into())
            }
            TurnState::Completed(r) => (IconName::Info, ui.warning.hsla(), format!("Stopped: {r}")),
            TurnState::Cancelled => (IconName::CircleX, ui.text_faint.hsla(), "Cancelled".into()),
            TurnState::Failed(_) => (IconName::AlertTriangle, ui.error.hsla(), "Failed".into()),
        };
        let n = turn.changed_files.len();
        let finished = turn.state.is_finished();
        let busy = self.busy_turn_action == Some(id);
        col = col.child(
            h_flex()
                .gap(px(6.))
                .pt(px(4.))
                .text_size(metrics::text_xs())
                .text_color(ui.text_faint.hsla())
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(ui.text_muted.hsla())
                        .child(format!("TURN {id}")),
                )
                .child(
                    Icon::new(state_icon)
                        .size(IconSize::XSmall)
                        .color(state_color),
                )
                .child(div().text_color(state_color).child(state_label))
                .child(div().flex_1().h(px(1.)).bg(ui.border_variant.hsla())),
        );
        let prompt_box = v_flex()
            .gap(px(4.))
            .px(px(10.))
            .py(px(8.))
            .rounded(metrics::radius_lg())
            .bg(ui.surface.hsla())
            .border_1()
            .border_color(ui.border_variant.hsla())
            .text_size(metrics::text_sm())
            .text_color(ui.text.hsla())
            .child(turn.prompt.clone())
            .when(!turn.context.is_empty(), |el| {
                el.child(
                    h_flex()
                        .flex_wrap()
                        .gap(px(4.))
                        .children(turn.context.iter().map(|c| chip_label(c.clone(), cx))),
                )
            });
        col = col.child(prompt_box);

        for (pos, item) in turn.timeline.iter().enumerate() {
            let el = match *item {
                TurnItem::Message(ix) => self.render_message(turn, ix, window, cx),
                TurnItem::ToolCall(ix) => self.render_tool_call(turn, ix, cx),
                TurnItem::Permission(ix) => self.render_permission(&turn.permissions[ix], pos, cx),
            };
            col = col.child(el);
        }
        if !turn.plan.is_empty() {
            col = col.child(self.render_plan(turn, cx));
        }
        if !finished {
            col = col.child(
                h_flex()
                    .gap(px(6.))
                    .text_size(metrics::text_xs())
                    .text_color(ui.text_muted.hsla())
                    .child(
                        Icon::new(IconName::Loader)
                            .size(IconSize::XSmall)
                            .color(ui.accent.hsla()),
                    )
                    .child(
                        if turn.before.is_none() && matches!(turn.state, TurnState::Starting) {
                            "Taking a checkpoint…"
                        } else {
                            "Working…"
                        },
                    )
                    .when(!turn.written.is_empty(), |el| {
                        el.child(format!("· {} written", plural(turn.written.len(), "file")))
                    }),
            );
        }
        if let TurnState::Failed(msg) = &turn.state {
            let crashed = matches!(self.status, Some(SessionStatus::Crashed(_)));
            col = col.child(
                v_flex()
                    .gap(px(6.))
                    .p(px(8.))
                    .rounded(metrics::radius())
                    .border_1()
                    .border_color(ui.error.hsla().opacity(0.6))
                    .bg(ui.error.hsla().opacity(0.07))
                    .text_size(metrics::text_xs())
                    .child(
                        div()
                            .font_family(self.mono(cx))
                            .text_color(ui.text.hsla())
                            .child(msg.clone()),
                    )
                    .when(crashed, |el| {
                        el.child(
                            h_flex().child(
                                Button::new(("agent-restart-turn", id.0 as usize), "Restart agent")
                                    .icon(IconName::Refresh)
                                    .style(ButtonStyle::Outlined)
                                    .compact()
                                    .on_click(cx.listener(|this, _, _, cx| this.restart(cx))),
                            ),
                        )
                    }),
            );
        }
        // Footer: what changed, review, undo.
        if finished && turn.before.is_some() {
            let footer = h_flex()
                .flex_wrap()
                .gap(px(4.))
                .px(px(8.))
                .py(px(4.))
                .rounded(metrics::radius())
                .bg(ui.surface.hsla())
                .text_size(metrics::text_xs())
                .text_color(ui.text_muted.hsla())
                .child(
                    Icon::new(if n == 0 { IconName::Circle } else { IconName::FileDiff })
                        .size(IconSize::XSmall)
                        .color(if n == 0 { ui.text_faint.hsla() } else { theme.git.modified.hsla() }),
                )
                .child(if n == 0 {
                    "No files changed".to_string()
                } else {
                    format!("{} changed", plural(n, "file"))
                })
                .child(div().flex_1())
                .when(n > 0, |el| {
                    el.child(
                        Button::new(("agent-review", id.0 as usize), "Review")
                            .icon(IconName::GitPullRequest)
                            .compact()
                            .tooltip(Tooltip::text("Open this turn's changes in a review tab"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.review_turn(id, window, cx)
                            })),
                    )
                    .child(if turn.undone {
                        div()
                            .px(px(6.))
                            .text_color(ui.text_faint.hsla())
                            .child("Undone")
                            .into_any_element()
                    } else {
                        Button::new(("agent-undo", id.0 as usize), if busy { "Undoing…" } else { "Undo turn" })
                            .icon(IconName::Undo)
                            .compact()
                            .disabled(busy)
                            .tooltip(Tooltip::text(
                                "Undo turn: restore the files this turn changed (undoable from the Activity Log)",
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| this.undo_turn(id, false, cx)))
                            .into_any_element()
                    })
                });
            col = col.child(footer);
            if let Some(conflict) = self.undo_conflict.clone().filter(|c| c.turn == id) {
                col = col.child(self.render_undo_conflict(conflict, cx));
            }
            if n > 0 {
                col = col.child(
                    v_flex().gap(px(1.)).pl(px(8.)).children(
                        turn.changed_files
                            .iter()
                            .take(12)
                            .enumerate()
                            .map(|(ix, f)| {
                                let path = f.path.clone();
                                let color = match f.kind {
                                    lector_git::ChangeKind::Added => theme.git.added.hsla(),
                                    lector_git::ChangeKind::Deleted => theme.git.deleted.hsla(),
                                    _ => theme.git.modified.hsla(),
                                };
                                let letter = match f.kind {
                                    lector_git::ChangeKind::Added => "A",
                                    lector_git::ChangeKind::Deleted => "D",
                                    _ => "M",
                                };
                                let abs = self.project.repo().map(|r| r.workdir().join(&path));
                                h_flex()
                                    .id(("agent-file", id.0 as usize * 1000 + ix))
                                    .gap(px(6.))
                                    .px(px(4.))
                                    .rounded(metrics::radius())
                                    .cursor_pointer()
                                    .hover(|s| s.bg(ui.hover.hsla()))
                                    .text_size(metrics::text_xs())
                                    .child(
                                        div()
                                            .w(px(10.))
                                            .text_color(color)
                                            .font_weight(FontWeight::BOLD)
                                            .child(letter),
                                    )
                                    .child(div().text_color(ui.text_muted.hsla()).child(path))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if let Some(abs) = abs.clone() {
                                            this.open_location(
                                                abs.to_string_lossy().into_owned(),
                                                None,
                                                window,
                                                cx,
                                            );
                                        }
                                    }))
                            }),
                    ),
                );
            }
        }
        col.into_any_element()
    }

    fn render_undo_conflict(
        &mut self,
        conflict: crate::panel::UndoConflict,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let id = conflict.turn;
        v_flex()
            .gap(px(6.))
            .p(px(8.))
            .rounded(metrics::radius())
            .border_1()
            .border_color(ui.warning.hsla().opacity(0.7))
            .bg(ui.warning.hsla().opacity(0.08))
            .text_size(metrics::text_xs())
            .text_color(ui.text.hsla())
            .child(
                h_flex()
                    .gap(px(5.))
                    .child(Icon::new(IconName::AlertTriangle).size(IconSize::XSmall).color(ui.warning.hsla()))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("{} edited since turn {id}", plural(conflict.paths.len(), "file"))),
                    ),
            )
            .child(
                div()
                    .font_family(self.mono(cx))
                    .text_color(ui.text_muted.hsla())
                    .child(conflict.paths.join("\n")),
            )
            .child("Undoing the turn overwrites those edits (they are backed up first, so this can be undone too).")
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(
                        Button::new(("agent-undo-force", id.0 as usize), "Restore anyway")
                            .style(ButtonStyle::Outlined)
                            .compact()
                            .on_click(cx.listener(move |this, _, _, cx| this.undo_turn(id, true, cx))),
                    )
                    .child(
                        Button::new(("agent-undo-keep", id.0 as usize), "Keep my edits")
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.undo_conflict = None;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_message(
        &mut self,
        turn: &Turn,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let msg = &turn.messages[ix];
        let key = (turn.id, ix);
        match msg.kind {
            MessageKind::Thought => {
                let expanded = self.expanded_thoughts.contains(&key);
                let text: String = if expanded {
                    msg.text.clone()
                } else {
                    let first = msg.text.lines().next().unwrap_or("").trim();
                    let short: String = first.chars().take(90).collect();
                    if short.len() < msg.text.trim().len() {
                        format!("{short}…")
                    } else {
                        short
                    }
                };
                h_flex()
                    .id(("agent-thought", turn.id.0 as usize * 1000 + ix))
                    .items_start()
                    .gap(px(6.))
                    .cursor_pointer()
                    .text_size(metrics::text_xs())
                    .text_color(ui.text_faint.hsla())
                    .italic()
                    .child(
                        Icon::new(IconName::Lightbulb)
                            .size(IconSize::XSmall)
                            .color(ui.text_faint.hsla()),
                    )
                    .child(div().flex_1().child(text))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.expanded_thoughts.remove(&key) {
                            this.expanded_thoughts.insert(key);
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            }
            MessageKind::User => div()
                .text_size(metrics::text_sm())
                .text_color(ui.text_muted.hsla())
                .child(msg.text.clone())
                .into_any_element(),
            MessageKind::Agent => {
                let len = msg.text.len();
                let blocks = match self.markdown.get(&key) {
                    Some((l, b)) if *l == len => b.clone(),
                    _ => {
                        let b = Rc::new(markdown::parse(&msg.text));
                        self.markdown.insert(key, (len, b.clone()));
                        b
                    }
                };
                let opts = RenderOptions {
                    base_dir: Some(self.project.root().to_path_buf()),
                    on_link: None,
                    compact: true,
                    id: format!("agent-md-{}-{ix}", turn.id).into(),
                };
                v_flex()
                    .gap(px(4.))
                    .text_size(metrics::text_sm())
                    .text_color(ui.text.hsla())
                    .children(markdown::render_blocks(&blocks, &opts, window, cx))
                    .into_any_element()
            }
        }
    }

    fn render_tool_call(&mut self, turn: &Turn, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let call: &ToolCall = &turn.tool_calls[ix];
        let key = (turn.id, ix);
        let collapsed = self.collapsed_tools.contains(&key);
        let (status_color, status_label) = match call.status {
            ToolStatus::Pending => (ui.text_faint.hsla(), "pending"),
            ToolStatus::InProgress => (ui.accent.hsla(), "running"),
            ToolStatus::Completed => (ui.success.hsla(), "done"),
            ToolStatus::Failed => (ui.error.hsla(), "failed"),
        };
        let mono = self.mono(cx);
        let id_base = turn.id.0 as usize * 1000 + ix;
        let mut card = v_flex()
            .rounded(metrics::radius())
            .border_1()
            .border_color(ui.border.hsla())
            .bg(ui.panel.hsla())
            .overflow_hidden()
            .child(
                h_flex()
                    .id(("agent-tool", id_base))
                    .gap(px(6.))
                    .px(px(8.))
                    .py(px(5.))
                    .cursor_pointer()
                    .hover(|s| s.bg(ui.hover.hsla()))
                    .child(
                        Icon::new(kind_icon(&call.kind))
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
                            .text_size(metrics::text_xs())
                            .text_color(ui.text.hsla())
                            .child(if call.title.is_empty() {
                                call.kind.clone()
                            } else {
                                call.title.clone()
                            }),
                    )
                    .child(
                        div()
                            .px(px(5.))
                            .rounded(metrics::radius())
                            .bg(status_color.opacity(0.14))
                            .text_size(metrics::text_xs())
                            .text_color(status_color)
                            .child(status_label),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.collapsed_tools.remove(&key) {
                            this.collapsed_tools.insert(key);
                        }
                        cx.notify();
                    })),
            );
        if collapsed {
            return card.into_any_element();
        }
        for (lix, loc) in call.locations.iter().take(3).enumerate() {
            let label = match loc.line {
                Some(l) => format!("{}:{}", self.rel(std::path::Path::new(&loc.path)), l + 1),
                None => self.rel(std::path::Path::new(&loc.path)),
            };
            let (path, line) = (loc.path.clone(), loc.line);
            card = card.child(
                div()
                    .id(("agent-loc", id_base * 4 + lix))
                    .px(px(8.))
                    .pb(px(3.))
                    .text_size(metrics::text_xs())
                    .text_color(ui.link.hsla())
                    .cursor_pointer()
                    .hover(|s| s.underline())
                    .child(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_location(path.clone(), line, window, cx)
                    })),
            );
        }
        for d in call.diffs.iter().take(2) {
            card = card.child(mini_diff(d, &mono, cx));
        }
        if call.diffs.is_empty() && !call.output.trim().is_empty() {
            let out: String = call.output.lines().take(6).collect::<Vec<_>>().join("\n");
            card = card.child(
                div()
                    .mx(px(8.))
                    .mb(px(6.))
                    .p(px(6.))
                    .rounded(metrics::radius())
                    .bg(ui.surface.hsla())
                    .font_family(mono)
                    .text_size(metrics::text_xs())
                    .text_color(ui.text_muted.hsla())
                    .child(out),
            );
        }
        card.into_any_element()
    }

    fn render_permission(
        &mut self,
        req: &PermissionRequest,
        pos: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let id = req.id;
        if !req.pending {
            let (icon, color, text) = match req.answer {
                Some(PermissionChoice::AllowOnce) if req.automatic => (
                    IconName::Check,
                    ui.text_faint.hsla(),
                    "Allowed automatically",
                ),
                Some(PermissionChoice::AllowOnce) => {
                    (IconName::Check, ui.success.hsla(), "Allowed once")
                }
                Some(PermissionChoice::AllowAlways) if req.automatic => (
                    IconName::Check,
                    ui.text_faint.hsla(),
                    "Allowed (always for this session)",
                ),
                Some(PermissionChoice::AllowAlways) => (
                    IconName::Check,
                    ui.success.hsla(),
                    "Always allowed for this session",
                ),
                Some(PermissionChoice::Reject) => (IconName::CircleX, ui.error.hsla(), "Rejected"),
                None => (IconName::CircleX, ui.text_faint.hsla(), "Cancelled"),
            };
            return h_flex()
                .gap(px(5.))
                .text_size(metrics::text_xs())
                .text_color(ui.text_faint.hsla())
                .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(format!("{text}: {}", req.title)),
                )
                .into_any_element();
        }
        let ix = id.0 as usize * 10 + pos;
        v_flex()
            .gap(px(6.))
            .p(px(8.))
            .rounded(metrics::radius())
            .border_1()
            .border_color(ui.accent.hsla().opacity(0.7))
            .bg(ui.accent.hsla().opacity(0.07))
            .child(
                h_flex()
                    .items_start()
                    .gap(px(6.))
                    .child(
                        Icon::new(IconName::Lock)
                            .size(IconSize::XSmall)
                            .color(ui.accent.hsla()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(metrics::text_sm())
                            .text_color(ui.text.hsla())
                            .child(format!("The agent wants to: {}", req.title)),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(
                        Button::new(("perm-once", ix), "Allow once")
                            .style(ButtonStyle::Accent)
                            .compact()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.answer(id, PermissionChoice::AllowOnce, cx)
                            })),
                    )
                    .child(
                        Button::new(("perm-always", ix), "Always")
                            .style(ButtonStyle::Outlined)
                            .compact()
                            .tooltip(Tooltip::text(format!(
                                "Allow every `{}` request for this session",
                                req.kind
                            )))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.answer(id, PermissionChoice::AllowAlways, cx)
                            })),
                    )
                    .child(
                        Button::new(("perm-reject", ix), "Reject")
                            .compact()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.answer(id, PermissionChoice::Reject, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_plan(&mut self, turn: &Turn, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let done = turn
            .plan
            .iter()
            .filter(|e| e.status == PlanStatus::Completed)
            .count();
        v_flex()
            .gap(px(3.))
            .p(px(8.))
            .rounded(metrics::radius())
            .border_1()
            .border_color(ui.border_variant.hsla())
            .child(
                div()
                    .text_size(metrics::text_xs())
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui.text_muted.hsla())
                    .child(format!("Plan · {done}/{}", turn.plan.len())),
            )
            .children(turn.plan.iter().map(|e| {
                let (icon, color) = match e.status {
                    PlanStatus::Completed => (IconName::CheckSquare, ui.success.hsla()),
                    PlanStatus::InProgress => (IconName::CircleDot, ui.accent.hsla()),
                    PlanStatus::Pending => (IconName::Square, ui.text_faint.hsla()),
                };
                h_flex()
                    .items_start()
                    .gap(px(6.))
                    .text_size(metrics::text_xs())
                    .text_color(if e.status == PlanStatus::Completed {
                        ui.text_faint.hsla()
                    } else {
                        ui.text.hsla()
                    })
                    .when(e.status == PlanStatus::Completed, |el| el.line_through())
                    .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                    .child(div().flex_1().child(e.content.clone()))
            }))
            .into_any_element()
    }

    fn render_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let busy = matches!(self.status, Some(SessionStatus::Busy(_)));
        let focused = self
            .composer
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx);
        let empty = self.composer.read(cx).is_empty() && self.chips.is_empty();
        v_flex()
            .key_context("AgentComposer")
            .on_action(
                cx.listener(|this, _: &agent_actions::SendPrompt, window, cx| {
                    this.send(window, cx)
                }),
            )
            .gap(px(6.))
            .p(px(8.))
            .border_t_1()
            .border_color(ui.border_variant.hsla())
            .when_some(self.error.clone(), |el, err| {
                el.child(
                    h_flex()
                        .items_start()
                        .gap(px(5.))
                        .text_size(metrics::text_xs())
                        .text_color(ui.error.hsla())
                        .child(
                            Icon::new(IconName::AlertTriangle)
                                .size(IconSize::XSmall)
                                .color(ui.error.hsla()),
                        )
                        .child(div().flex_1().child(err))
                        .child(
                            IconButton::new("agent-error-close", IconName::Close)
                                .icon_size(IconSize::XSmall)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .when(!self.chips.is_empty(), |el| {
                el.child(h_flex().flex_wrap().gap(px(4.)).children(
                    self.chips.iter().enumerate().map(|(ix, c)| {
                        h_flex()
                            .gap(px(2.))
                            .pl(px(6.))
                            .rounded(metrics::radius())
                            .bg(ui.accent.hsla().opacity(0.12))
                            .text_size(metrics::text_xs())
                            .text_color(ui.text_accent.hsla())
                            .max_w_full()
                            .child(
                                div()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(c.label.clone()),
                            )
                            .child(
                                IconButton::new(("agent-chip-x", ix), IconName::Close)
                                    .icon_size(IconSize::XSmall)
                                    .tooltip(Tooltip::text("Remove"))
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| this.remove_chip(ix, cx)),
                                    ),
                            )
                    }),
                ))
            })
            .child(
                div()
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(metrics::radius())
                    .border_1()
                    .border_color(if focused {
                        ui.border_focused.hsla()
                    } else {
                        ui.border.hsla()
                    })
                    .bg(ui.input_bg.hsla())
                    .text_size(metrics::text_sm())
                    .text_color(ui.text.hsla())
                    .child(self.composer.clone()),
            )
            .child(
                h_flex()
                    .gap(px(2.))
                    .child(
                        IconButton::new("agent-attach-file", IconName::FileText)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Attach the active file (@file)"))
                            .on_click(cx.listener(|this, _, _, cx| this.attach_active_file(cx))),
                    )
                    .child(
                        IconButton::new("agent-attach-selection", IconName::TextSearch)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Attach the selected lines"))
                            .on_click(cx.listener(|this, _, _, cx| this.attach_selection(cx))),
                    )
                    .child(
                        IconButton::new("agent-attach-diff", IconName::Diff)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Attach the current diff"))
                            .on_click(cx.listener(|this, _, _, cx| this.attach_diff(cx))),
                    )
                    .child(div().flex_1())
                    .child(if busy {
                        Button::new("agent-send-stop", "Stop")
                            .icon(IconName::Square)
                            .style(ButtonStyle::Outlined)
                            .tooltip(Tooltip::text("Stop the agent (Escape)"))
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx)))
                            .into_any_element()
                    } else {
                        Button::new("agent-send", "Send")
                            .icon(IconName::ArrowUp)
                            .style(if empty {
                                ButtonStyle::Outlined
                            } else {
                                ButtonStyle::Accent
                            })
                            .disabled(empty || self.starting)
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
                            .into_any_element()
                    }),
            )
    }
}

fn chip_label(label: String, cx: &App) -> AnyElement {
    let ui = &cx.theme().ui;
    div()
        .px(px(5.))
        .rounded(metrics::radius())
        .bg(ui.accent.hsla().opacity(0.10))
        .text_size(metrics::text_xs())
        .text_color(ui.text_accent.hsla())
        .child(label)
        .into_any_element()
}

/// The first changed lines of a tool-call diff.
fn mini_diff(d: &ToolDiff, mono: &SharedString, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let old = d.old_text.clone().unwrap_or_default();
    let diff = diff_lines(&old, &d.new_text, &DiffOptions::default());
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = d.new_text.split('\n').collect();
    let mut rows: Vec<(bool, String)> = Vec::new();
    let mut total = 0usize;
    for h in &diff.hunks {
        for r in h.old.clone() {
            total += 1;
            if rows.len() < MINI_DIFF_LINES {
                rows.push((
                    false,
                    old_lines.get(r as usize).copied().unwrap_or("").to_string(),
                ));
            }
        }
        for r in h.new.clone() {
            total += 1;
            if rows.len() < MINI_DIFF_LINES {
                rows.push((
                    true,
                    new_lines.get(r as usize).copied().unwrap_or("").to_string(),
                ));
            }
        }
    }
    let stats = diff.stats();
    v_flex()
        .mx(px(8.))
        .mb(px(6.))
        .rounded(metrics::radius())
        .overflow_hidden()
        .border_1()
        .border_color(theme.ui.border_variant.hsla())
        .font_family(mono.clone())
        .text_size(metrics::text_xs())
        .child(
            h_flex()
                .gap(px(6.))
                .px(px(6.))
                .py(px(2.))
                .bg(theme.ui.surface.hsla())
                .text_color(theme.ui.text_muted.hsla())
                .child(
                    div()
                        .flex_1()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(
                            std::path::Path::new(&d.path)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| d.path.clone()),
                        ),
                )
                .child(
                    div()
                        .text_color(theme.diff.added_text.hsla())
                        .child(format!("+{}", stats.added)),
                )
                .child(
                    div()
                        .text_color(theme.diff.removed_text.hsla())
                        .child(format!("−{}", stats.removed)),
                ),
        )
        .children(rows.into_iter().map(|(added, text)| {
            let (bg, fg, sign) = if added {
                (
                    theme.diff.added_line_bg.hsla(),
                    theme.diff.added_text.hsla(),
                    "+",
                )
            } else {
                (
                    theme.diff.removed_line_bg.hsla(),
                    theme.diff.removed_text.hsla(),
                    "−",
                )
            };
            h_flex()
                .px(px(6.))
                .bg(bg)
                .whitespace_nowrap()
                .overflow_hidden()
                .child(div().w(px(12.)).text_color(fg).child(sign))
                .child(div().text_color(theme.ui.text.hsla()).child(text))
        }))
        .when(total > MINI_DIFF_LINES, |el| {
            el.child(
                div()
                    .px(px(6.))
                    .py(px(1.))
                    .text_color(theme.ui.text_faint.hsla())
                    .child(format!("… {} more changed lines", total - MINI_DIFF_LINES)),
            )
        })
        .into_any_element()
}

impl Render for AgentPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        if self.focus_composer {
            self.focus_composer = false;
            self.composer.read(cx).focus(window);
        }
        let turns = self.turns.clone();
        let body: Vec<AnyElement> = if turns.is_empty() {
            vec![self.render_empty(cx)]
        } else {
            turns
                .iter()
                .map(|t| self.render_turn(t, window, cx))
                .collect()
        };
        v_flex()
            .size_full()
            .key_context("AgentPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &agent_actions::Cancel, _, cx| this.cancel(cx)))
            .bg(ui.panel.hsla())
            .child(self.render_header(cx))
            .child(
                div()
                    .id("agent-conversation")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .on_scroll_wheel(cx.listener(|this, _, _, _| this.update_follow()))
                    .child(v_flex().gap(px(14.)).px(px(10.)).py(px(10.)).children(body)),
            )
            .child(self.render_composer(window, cx))
            .children(self.picker_menu.render())
    }
}
