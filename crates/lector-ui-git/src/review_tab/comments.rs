//! The review tab's comment side (v0.6, Agent FORGE): a "Comments" list per
//! changeset / pull request (jump to a thread, export, "Send to agent"), the pull
//! request header and "Submit review".

use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{div, prelude::*, ClipboardItem, FontWeight, SharedString, Window};
use lector_app::comments::{self as cm, CommentStore, DiffSide, Thread, ThreadFilter, ThreadState};
use lector_ui::components::{h_flex, metrics, v_flex, Button, ButtonStyle, Tooltip};
use lector_ui::icons::{Icon, IconName, IconSize};
use lector_ui::scale::px;
use lector_ui::{ActiveTheme, ColorExt};
use lector_workspace::review::changeset_key;

use super::ReviewView;
use crate::util::{now_secs, relative_time};

/// What the left column lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListMode {
    /// Changed files.
    Files,
    /// Comment threads.
    Comments,
}

/// The agent command "Send to agent" invokes (registered by lector-agent).
pub const SEND_TO_AGENT: &str = "agent.send_comments";

impl ReviewView {
    /// Review key of this tab's comments.
    pub(crate) fn comments_key(&self) -> String {
        match (&self.pr, &self.view) {
            (Some(pr), _) => pr.key.clone(),
            (None, Some(v)) => v.key.clone(),
            (None, None) => changeset_key(&self.changeset),
        }
    }

    fn comment_threads(&mut self, cx: &mut Context<Self>) -> Rc<Vec<Thread>> {
        if let Some((at, t)) = &self.comment_cache {
            if at.elapsed() < Duration::from_secs(1) {
                return t.clone();
            }
        }
        let Some(workdir) = self
            .store
            .read(cx)
            .repo()
            .map(|r| r.workdir().to_path_buf())
        else {
            return Rc::new(Vec::new());
        };
        let store = CommentStore::for_core(&lector_ui::commands_bridge::core(cx));
        let threads = Rc::new(
            store
                .threads(&workdir, &ThreadFilter::changeset(self.comments_key()))
                .unwrap_or_default(),
        );
        self.comment_cache = Some((Instant::now(), threads.clone()));
        threads
    }

    fn set_list_mode(&mut self, mode: ListMode, cx: &mut Context<Self>) {
        self.list_mode = mode;
        self.comment_cache = None;
        cx.notify();
    }

    /// "Files | Comments" switch above the list.
    pub(crate) fn render_mode_tabs(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let files = self.view.as_ref().map(|v| v.files.len()).unwrap_or(0);
        let threads = self.comment_threads(cx);
        let open = threads
            .iter()
            .filter(|t| t.state == ThreadState::Open)
            .count();
        let mode = self.list_mode;
        let tab = |id: &'static str, label: String, m: ListMode, cx: &mut Context<Self>| {
            let active = mode == m;
            div()
                .id(id)
                .px(px(10.))
                .h(px(28.))
                .flex()
                .items_center()
                .text_size(metrics::text_xs())
                .font_weight(if active {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::NORMAL
                })
                .text_color(if active {
                    ui.text.hsla()
                } else {
                    ui.text_muted.hsla()
                })
                .border_b_2()
                .border_color(if active {
                    ui.accent.hsla()
                } else {
                    gpui::transparent_black()
                })
                .cursor_pointer()
                .hover(|s| s.text_color(ui.text.hsla()))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.set_list_mode(m, cx)))
        };
        h_flex()
            .flex_none()
            .px(px(4.))
            .border_b_1()
            .border_color(ui.border_variant.hsla())
            .child(tab(
                "review-tab-files",
                format!("Files {files}"),
                ListMode::Files,
                cx,
            ))
            .child(tab(
                "review-tab-comments",
                if threads.is_empty() {
                    "Comments".to_string()
                } else {
                    format!("Comments {open}/{}", threads.len())
                },
                ListMode::Comments,
                cx,
            ))
    }

    /// Title, author, branches and "Submit review" of a pull request.
    pub(crate) fn render_pr_header(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let pr = self.pr.clone()?;
        let ui = cx.theme().ui.clone();
        let threads = self.comment_threads(cx);
        let drafts: usize = threads.iter().map(Thread::draft_count).sum();
        let p = &pr.pr;
        Some(
            v_flex()
                .flex_none()
                .px(px(10.))
                .py(px(8.))
                .gap(px(4.))
                .border_b_1()
                .border_color(ui.border_variant.hsla())
                .child(
                    h_flex()
                        .gap(px(6.))
                        .child(
                            Icon::new(IconName::GitPullRequest)
                                .size(IconSize::Small)
                                .color(ui.success.hsla()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_size(metrics::text_sm())
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(ui.text.hsla())
                                .child(p.title.clone()),
                        ),
                )
                .child(
                    div()
                        .text_size(metrics::text_xs())
                        .text_color(ui.text_faint.hsla())
                        .child(format!(
                            "#{} by {} · {} → {}",
                            p.number, p.author, p.head_ref, p.base_ref
                        )),
                )
                .child(
                    h_flex().pt(px(2.)).child(
                        Button::new(
                            "review-submit",
                            if drafts > 0 {
                                format!("Submit review ({drafts})")
                            } else {
                                "Submit review".to_string()
                            },
                        )
                        .icon(IconName::Check)
                        .style(ButtonStyle::Accent)
                        .tooltip(Tooltip::for_action(
                            "Submit Review",
                            &lector_ui::actions::review::SubmitReview,
                        ))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.submit_review(window, cx)),
                        ),
                    ),
                )
                .into_any_element(),
        )
    }

    /// `review::SubmitReview`.
    pub(crate) fn submit_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.pr.clone() else {
            if let Some(ws) = self.store.read(cx).workspace.upgrade() {
                ws.update(cx, |ws, cx| {
                    ws.show_toast(
                        "Submit Review works in pull request reviews (Pull Requests panel)",
                        cx,
                    )
                });
            }
            return;
        };
        let Some(ws) = self.store.read(cx).workspace.upgrade() else {
            return;
        };
        let this = cx.entity().downgrade();
        crate::pull_requests::show_submit_dialog(&ws, session, window, cx, move |_, cx| {
            let _ = this.update(cx, |this, cx| {
                this.comment_cache = None;
                this.diff.update(cx, |d, cx| d.reload(cx));
                cx.notify();
            });
        });
    }

    fn reveal_thread(&mut self, t: &Thread, window: &mut Window, cx: &mut Context<Self>) {
        let path = &t.anchor.path;
        let ix = self.view.as_ref().and_then(|v| {
            v.files.iter().position(|f| match t.anchor.side {
                DiffSide::New => f.change.new_path.as_deref() == Some(path.as_str()),
                DiffSide::Old => f.change.old_path.as_deref() == Some(path.as_str()),
            })
        });
        if let Some(ix) = ix {
            if ix != self.selected {
                self.selected = ix;
                self.show_selected(cx);
            }
        }
        let id = t.id;
        self.diff
            .update(cx, |d, cx| d.reveal_thread(id, window, cx));
    }

    fn send_to_agent(&mut self, threads: Vec<u64>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ws) = self.store.read(cx).workspace.upgrade() else {
            return;
        };
        let key = self.comments_key();
        ws.update(cx, |ws, cx| {
            lector_ui::commands_bridge::invoke(
                ws,
                SEND_TO_AGENT,
                serde_json::json!({ "changeset": key, "threads": threads }),
                true,
                window,
                cx,
            )
        });
    }

    /// The list of threads of this review.
    pub(crate) fn render_comments_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let threads = self.comment_threads(cx);
        let now = now_secs();
        let core = lector_ui::commands_bridge::core(cx);
        let agent = core.registry().get(SEND_TO_AGENT).is_some();
        let open_ids: Vec<u64> = threads
            .iter()
            .filter(|t| t.state != ThreadState::Resolved)
            .map(|t| t.id)
            .collect();
        let title = self.title(cx).to_string();
        let md = cm::export_markdown(&title, &threads);
        let rows = threads.iter().enumerate().map(|(k, t)| {
            let t2 = t.clone();
            let (icon, color) = match t.state {
                ThreadState::Open => (IconName::CircleDot, ui.accent.hsla()),
                ThreadState::Resolved => (IconName::CircleCheck, ui.success.hsla()),
                ThreadState::Outdated => (IconName::AlertTriangle, ui.warning.hsla()),
            };
            let author = t
                .comments
                .first()
                .map(|c| crate::diff_view::display_author(&c.author))
                .unwrap_or_default();
            v_flex()
                .id(SharedString::from(format!("review-comment-{k}")))
                .px(px(10.))
                .py(px(6.))
                .gap(px(2.))
                .border_b_1()
                .border_color(ui.border_variant.hsla())
                .cursor_pointer()
                .hover(|s| s.bg(ui.hover.hsla()))
                .on_click(
                    cx.listener(move |this, _, window, cx| this.reveal_thread(&t2, window, cx)),
                )
                .child(
                    h_flex()
                        .gap(px(6.))
                        .child(Icon::new(icon).size(IconSize::Small).color(color))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_size(metrics::text_xs())
                                .text_color(ui.text_muted.hsla())
                                .child(t.location()),
                        )
                        .when(t.draft_count() > 0, |el| {
                            el.child(
                                div()
                                    .px(px(5.))
                                    .rounded(px(8.))
                                    .bg(ui.accent.hsla())
                                    .text_size(metrics::text_xs())
                                    .text_color(ui.accent_foreground.hsla())
                                    .child("Pending"),
                            )
                        }),
                )
                .child(
                    div()
                        .pl(px(20.))
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_size(metrics::text_sm())
                        .text_color(if t.state == ThreadState::Resolved {
                            ui.text_faint.hsla()
                        } else {
                            ui.text.hsla()
                        })
                        .child(t.summary()),
                )
                .child(
                    div()
                        .pl(px(20.))
                        .text_size(metrics::text_xs())
                        .text_color(ui.text_faint.hsla())
                        .child(format!(
                            "{author} · {}{}",
                            relative_time(t.created, now),
                            match t.comments.len() {
                                1 => String::new(),
                                n => format!(" · {} replies", n - 1),
                            }
                        )),
                )
        });
        let ids = open_ids.clone();
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                v_flex()
                    .id("review-comments")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .when(threads.is_empty(), |el| {
                        el.child(
                            div()
                                .p(px(14.))
                                .text_size(metrics::text_xs())
                                .text_color(ui.text_faint.hsla())
                                .child("No comments yet. Hover a line in the diff and click + (or press C) to comment."),
                        )
                    })
                    .children(rows),
            )
            .child(
                h_flex()
                    .flex_none()
                    .p(px(8.))
                    .gap(px(6.))
                    .border_t_1()
                    .border_color(ui.border_variant.hsla())
                    .child(
                        Button::new("comments-export", "Copy as Markdown")
                            .icon(IconName::Copy)
                            .style(ButtonStyle::Subtle)
                            .disabled(threads.is_empty())
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(md.clone()))
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("comments-send-agent", "Send to agent")
                            .icon(IconName::Bot)
                            .style(ButtonStyle::Outlined)
                            .disabled(!agent || ids.is_empty())
                            .tooltip(Tooltip::text(if !agent {
                                "No agent integration is available (the agent.send_comments command is not registered)"
                            } else if ids.is_empty() {
                                "No open comments to send"
                            } else {
                                "Send the open comments to the coding agent as a prompt"
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.send_to_agent(open_ids.clone(), window, cx)
                            })),
                    ),
            )
    }
}
