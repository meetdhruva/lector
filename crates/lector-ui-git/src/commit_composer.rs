//! The commit composer at the top of the Changes panel: a multi-line message
//! editor (ruler, subject-length hint), Amend and Sign-off toggles, the identity
//! git will commit as, the Commit button (disabled with an explanation), hook and
//! commit errors, and a success toast with the short sha.

use gpui::{
    div, prelude::*, px, App, AppContext, ClipboardItem, Context, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, Hsla, SharedString, Subscription, Task, Window,
};
use lector_git::CommitOptions;
use lector_ui::actions::git as git_actions;
use lector_ui::components::text_area::{first_line_chars, TextArea, TextAreaEvent};
use lector_ui::components::{h_flex, metrics, v_flex, Button, ButtonStyle, IconButton, Tooltip};
use lector_ui::icons::{Icon, IconName, IconSize};
use lector_ui::keymap::contexts;
use lector_ui::{ActiveTheme, ColorExt};

use crate::staging::{git_error_text, notify_repo_changed};
use crate::store::{GitStore, GitStoreEvent};

/// Conventional subject length (soft limit) and hard limit.
pub const SUBJECT_SOFT: usize = 50;
/// Hard subject limit (most tools truncate after this).
pub const SUBJECT_HARD: usize = 72;

/// Events from the composer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitComposerEvent {
    /// A commit was created (short sha).
    Committed(String),
}

/// Who git will commit as; `Missing` when git has no identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityState {
    /// Not loaded yet.
    Loading,
    /// `user.name` / `user.email` (or env) as git resolves them.
    Known(String, String),
    /// `git commit` would fail with "Author identity unknown".
    Missing,
}

/// The commit composer.
pub struct CommitComposer {
    store: Entity<GitStore>,
    editor: Entity<TextArea>,
    amend: bool,
    sign_off: bool,
    /// The message the user had typed before turning Amend on.
    saved_message: Option<String>,
    identity: IdentityState,
    error: Option<SharedString>,
    committing: bool,
    staged_count: usize,
    has_head: bool,
    identity_task: Option<Task<()>>,
    commit_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CommitComposerEvent> for CommitComposer {}

impl Focusable for CommitComposer {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl CommitComposer {
    /// A composer committing in `store`'s repository.
    pub fn new(store: Entity<GitStore>, cx: &mut Context<Self>) -> Self {
        let settings = store.read(cx).settings(cx);
        let editor = cx.new(|cx| {
            let mut area = TextArea::new("Commit message (Ctrl+Enter to commit)", cx);
            if cfg!(target_os = "macos") {
                area.set_placeholder("Commit message (⌘↩ to commit)", cx);
            }
            area.set_line_limits(3, 14, cx);
            let ruler = settings.git.commit_message_ruler;
            area.set_ruler((ruler > 0).then_some(ruler), cx);
            area.set_font_family(
                Some(lector_ui::code_view::EditorSettings::get(cx).font_family),
                cx,
            );
            area
        });
        let subs = vec![
            cx.subscribe(&editor, |this, _, _: &TextAreaEvent, cx| {
                this.error = None;
                cx.notify();
            }),
            cx.subscribe(&store, |this, store, _: &GitStoreEvent, cx| {
                let has_head = store
                    .read(cx)
                    .head
                    .as_ref()
                    .is_some_and(|h| h.oid.is_some());
                if has_head != this.has_head {
                    this.has_head = has_head;
                    cx.notify();
                }
            }),
        ];
        let has_head = store
            .read(cx)
            .head
            .as_ref()
            .is_some_and(|h| h.oid.is_some());
        let mut composer = CommitComposer {
            store,
            editor,
            amend: false,
            sign_off: settings.git.commit_sign_off,
            saved_message: None,
            identity: IdentityState::Loading,
            error: None,
            committing: false,
            staged_count: 0,
            has_head,
            identity_task: None,
            commit_task: None,
            _subscriptions: subs,
        };
        composer.load_identity(cx);
        composer
    }

    /// The message editor.
    pub fn editor(&self) -> &Entity<TextArea> {
        &self.editor
    }

    /// Number of staged files (from the Changes panel), for the Commit button.
    pub fn set_staged_count(&mut self, n: usize, cx: &mut Context<Self>) {
        if self.staged_count != n {
            self.staged_count = n;
            cx.notify();
        }
    }

    /// Re-reads the identity git will use (e.g. after the user configured it).
    pub fn load_identity(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.store.read(cx).repo().cloned() else {
            return;
        };
        let work = cx.background_spawn(async move { repo.identity() });
        self.identity_task = Some(cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                this.identity = match result {
                    Ok(Some((name, email))) => IdentityState::Known(name, email),
                    Ok(None) => IdentityState::Missing,
                    Err(e) => {
                        tracing::warn!("git identity: {e}");
                        IdentityState::Missing
                    }
                };
                cx.notify();
            });
        }));
    }

    /// Focuses the message editor.
    pub fn focus_message(&self, window: &mut Window, cx: &App) {
        self.editor.read(cx).focus(window);
    }

    /// Whether Amend is on.
    pub fn amend(&self) -> bool {
        self.amend
    }

    /// Turns Amend on (loading the last commit's message) or off (restoring the
    /// message typed before).
    pub fn toggle_amend(&mut self, cx: &mut Context<Self>) {
        if self.amend {
            self.amend = false;
            let restore = self.saved_message.take().unwrap_or_default();
            self.editor.update(cx, |e, cx| e.set_text(restore, cx));
            cx.notify();
            return;
        }
        if !self.has_head {
            self.error = Some("Nothing to amend: this branch has no commits yet.".into());
            cx.notify();
            return;
        }
        let Some(repo) = self.store.read(cx).repo().cloned() else {
            return;
        };
        self.amend = true;
        self.saved_message = Some(self.editor.read(cx).text().to_string());
        let work = cx.background_spawn(async move { repo.last_commit_message() });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if !this.amend {
                    return;
                }
                match result {
                    Ok(Some(msg)) => {
                        let msg = msg.trim_end().to_string();
                        this.editor.update(cx, |e, cx| e.set_text(msg, cx));
                    }
                    Ok(None) => {}
                    Err(e) => this.error = Some(git_error_text(&e).into()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Why committing is not possible right now (`None` = it is).
    pub fn blocked_reason(&self, cx: &App) -> Option<&'static str> {
        if self.committing {
            return Some("Committing…");
        }
        if self.editor.read(cx).text().trim().is_empty() {
            return Some("Write a commit message first");
        }
        if self.staged_count == 0 && !self.amend {
            return Some("Nothing staged: stage files, hunks or lines first");
        }
        if self.identity == IdentityState::Missing {
            return Some("Git doesn't know who you are: set user.name and user.email first");
        }
        None
    }

    /// Creates the commit (the `git::Commit` action). Shows why when it can't.
    pub fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(reason) = self.blocked_reason(cx) {
            if !self.committing {
                self.error = Some(reason.into());
                self.focus_message(window, cx);
                cx.notify();
            }
            return;
        }
        let Some(repo) = self.store.read(cx).repo().cloned() else {
            return;
        };
        let opts = CommitOptions {
            message: self.editor.read(cx).text().to_string(),
            amend: self.amend,
            sign_off: self.sign_off,
        };
        self.committing = true;
        self.error = None;
        let work = cx.background_spawn(async move {
            let oid = repo.create_commit(&opts)?;
            let summary = repo
                .last_commit_message()
                .ok()
                .flatten()
                .and_then(|m| m.lines().next().map(str::to_string))
                .unwrap_or_default();
            Ok::<_, lector_git::Error>((oid, summary))
        });
        let store = self.store.clone();
        self.commit_task = Some(cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                this.committing = false;
                match result {
                    Ok((oid, summary)) => {
                        let short = oid.short();
                        let verb = if this.amend { "Amended" } else { "Committed" };
                        this.amend = false;
                        this.saved_message = None;
                        this.editor.update(cx, |e, cx| e.set_text("", cx));
                        if let Some(ws) = store.read(cx).workspace.upgrade() {
                            ws.update(cx, |ws, cx| {
                                ws.show_toast(format!("{verb} {short}  {summary}"), cx)
                            });
                        }
                        cx.emit(CommitComposerEvent::Committed(short));
                    }
                    Err(e) => {
                        let text = git_error_text(&e);
                        if text.contains("tell me who you are") {
                            this.identity = IdentityState::Missing;
                        }
                        this.error = Some(text.into());
                    }
                }
                cx.notify();
            });
            let _ = cx.update(|cx| notify_repo_changed(&store, cx));
        }));
        cx.notify();
    }

    fn render_identity(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        match &self.identity {
            IdentityState::Loading => div().into_any_element(),
            IdentityState::Known(name, email) => h_flex()
                .gap(px(5.))
                .min_w_0()
                .text_size(metrics::TEXT_XS)
                .text_color(ui.text_faint.hsla())
                .child(
                    Icon::new(IconName::User)
                        .size(IconSize::XSmall)
                        .color(ui.text_faint.hsla()),
                )
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(format!("Committing as {name} <{email}>")),
                )
                .into_any_element(),
            IdentityState::Missing => {
                let commands = "git config --global user.name \"Your Name\"\n\
                                git config --global user.email \"you@example.com\"";
                v_flex()
                    .gap(px(4.))
                    .p(px(8.))
                    .rounded(metrics::RADIUS)
                    .border_1()
                    .border_color(ui.warning.hsla().opacity(0.6))
                    .bg(ui.warning.hsla().opacity(0.08))
                    .text_size(metrics::TEXT_XS)
                    .text_color(ui.text_muted.hsla())
                    .child(
                        h_flex()
                            .gap(px(5.))
                            .child(
                                Icon::new(IconName::AlertTriangle)
                                    .size(IconSize::XSmall)
                                    .color(ui.warning.hsla()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(ui.text.hsla())
                                    .child("Git doesn't know who you are"),
                            )
                            .child(
                                IconButton::new("identity-copy", IconName::Copy)
                                    .icon_size(IconSize::XSmall)
                                    .tooltip(Tooltip::text("Copy commands"))
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            commands.to_string(),
                                        ))
                                    }),
                            )
                            .child(
                                IconButton::new("identity-reload", IconName::Refresh)
                                    .icon_size(IconSize::XSmall)
                                    .tooltip(Tooltip::text("Check again"))
                                    .on_click(cx.listener(|this, _, _, cx| this.load_identity(cx))),
                            ),
                    )
                    .child("Set your name and email in a terminal, then check again:")
                    .child(
                        div()
                            .p(px(6.))
                            .rounded(metrics::RADIUS)
                            .bg(ui.surface.hsla())
                            .font_family(lector_ui::code_view::EditorSettings::get(cx).font_family)
                            .text_color(ui.text.hsla())
                            .child(commands),
                    )
                    .child("Leave out --global to set it for this repository only.")
                    .into_any_element()
            }
        }
    }
}

fn checkbox(
    id: &'static str,
    label: &'static str,
    checked: bool,
    tooltip: &'static str,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let ui = &cx.theme().ui;
    h_flex()
        .id(id)
        .gap(px(5.))
        .h(px(22.))
        .px(px(4.))
        .rounded(metrics::RADIUS)
        .cursor_pointer()
        .text_size(metrics::TEXT_SM)
        .text_color(if checked {
            ui.text.hsla()
        } else {
            ui.text_muted.hsla()
        })
        .hover(|s| s.bg(ui.hover.hsla()))
        .child(
            Icon::new(if checked {
                IconName::CheckSquare
            } else {
                IconName::Square
            })
            .size(IconSize::Small)
            .color(if checked {
                ui.accent.hsla()
            } else {
                ui.text_faint.hsla()
            }),
        )
        .child(label)
        .tooltip(Tooltip::text(tooltip))
}

impl Render for CommitComposer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let text = self.editor.read(cx).text().to_string();
        let subject = first_line_chars(&text);
        let focused = self
            .editor
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx);
        let subject_color: Hsla = if subject > SUBJECT_HARD {
            ui.error.hsla()
        } else if subject > SUBJECT_SOFT {
            ui.warning.hsla()
        } else {
            ui.text_faint.hsla()
        };
        let blocked = self.blocked_reason(cx);
        let label = match (self.committing, self.amend) {
            (true, _) => "Committing…",
            (false, true) => "Amend",
            (false, false) => "Commit",
        };
        let staged = self.staged_count;
        let tooltip_text: SharedString = match blocked {
            Some(r) => r.into(),
            None if self.amend => {
                "Replace the last commit with the staged changes and this message".into()
            }
            None => format!(
                "Commit {staged} staged file{}",
                if staged == 1 { "" } else { "s" }
            )
            .into(),
        };
        v_flex()
            .id("commit-composer")
            .key_context(contexts::COMMIT_EDITOR)
            .on_action(cx.listener(|this, _: &git_actions::Commit, window, cx| this.commit(window, cx)))
            .on_action(cx.listener(|this, _: &git_actions::ToggleAmend, _, cx| this.toggle_amend(cx)))
            .gap(px(6.))
            .child(
                v_flex()
                    .relative()
                    .px(px(8.))
                    .pt(px(6.))
                    .pb(px(16.))
                    .rounded(metrics::RADIUS)
                    .border_1()
                    .border_color(if focused {
                        ui.border_focused.hsla()
                    } else {
                        ui.border.hsla()
                    })
                    .bg(ui.input_bg.hsla())
                    .text_size(metrics::TEXT)
                    .text_color(ui.text.hsla())
                    .child(self.editor.clone())
                    .child(
                        div()
                            .id("subject-length")
                            .absolute()
                            .right(px(6.))
                            .bottom(px(2.))
                            .text_size(metrics::TEXT_XS)
                            .text_color(subject_color)
                            .when(subject > 0, |el| el.child(subject.to_string()))
                            .tooltip(Tooltip::text(format!(
                                "Subject line length: keep it under {SUBJECT_SOFT} characters ({SUBJECT_HARD} at most)"
                            ))),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(2.))
                    .child(
                        checkbox(
                            "commit-amend",
                            "Amend",
                            self.amend,
                            "Amend the last commit (loads its message)",
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_amend(cx))),
                    )
                    .child(
                        checkbox(
                            "commit-signoff",
                            "Sign off",
                            self.sign_off,
                            "Add a Signed-off-by trailer",
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sign_off = !this.sign_off;
                            cx.notify();
                        })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("commit-button", label)
                            .icon(IconName::GitCommit)
                            .style(if blocked.is_none() {
                                ButtonStyle::Accent
                            } else {
                                ButtonStyle::Outlined
                            })
                            .disabled(blocked.is_some())
                            .tooltip(Tooltip::text(tooltip_text))
                            .on_click(cx.listener(|this, _, window, cx| this.commit(window, cx))),
                    ),
            )
            .child(self.render_identity(cx))
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    v_flex()
                        .relative()
                        .rounded(metrics::RADIUS)
                        .border_1()
                        .border_color(ui.error.hsla().opacity(0.7))
                        .bg(ui.error.hsla().opacity(0.08))
                        .child(
                            div()
                                .id("commit-error")
                                .max_h(px(160.))
                                .overflow_y_scroll()
                                .p(px(8.))
                                .pr(px(24.))
                                .font_family(lector_ui::code_view::EditorSettings::get(cx).font_family)
                                .text_size(metrics::TEXT_XS)
                                .text_color(ui.text.hsla())
                                .child(error),
                        )
                        .child(
                            div().absolute().top(px(2.)).right(px(2.)).child(
                                IconButton::new("commit-error-close", IconName::Close)
                                    .icon_size(IconSize::XSmall)
                                    .tooltip(Tooltip::text("Dismiss"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.error = None;
                                        cx.notify();
                                    })),
                            ),
                        ),
                )
            })
    }
}
