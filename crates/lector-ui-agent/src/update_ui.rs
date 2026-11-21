//! Update notice and health surfacing (v0.7): the first-run question, the Help menu commands, the
//! quiet status bar item, the update dialog, the crash log dialog and the note after an abnormal
//! exit. Policy and wording live in `update_model` / `health_model` / `notices` (UI-free, tested);
//! this module draws them.
//!
//! Lector only tells: it asks GitHub for the release list (at most once a day, and only when the
//! user turned that on), and it never downloads, installs or runs anything. Nothing about the user
//! or the code is sent. Crash reports and session records stay in Lector's data directory.

use lector_ui::background::{background, view_key, JobKind, Priority};
use std::path::PathBuf;

use gpui::{
    div, prelude::*, AnyWindowHandle, App, ClipboardItem, Context, DismissEvent, Entity,
    EventEmitter, FocusHandle, Focusable, FontWeight, KeyDownEvent, SharedString, WeakEntity,
    Window,
};
use lector_forge::releases::{UpdateOptions, UpdateStatus};
use lector_ui::actions::workspace as ws_actions;
use lector_ui::components::popover::surface;
use lector_ui::components::{h_flex, metrics, v_flex, Button, ButtonStyle, Tooltip};
use lector_ui::icons::{Icon, IconName, IconSize};
use lector_ui::scale::px;
use lector_ui::settings::AppSettings;
use lector_ui::views::Workspace;
use lector_ui::{ActiveTheme, ColorExt};

use crate::health_model::{health_model, health_text, HealthModel};
use crate::notices::{Notices, NoticesStore};
use crate::update_model::{
    dialog_model, forge_channel, is_newer_available, run_check, status_bar_label, toggle_text,
    UpdateButtonAction, UpdateDialogModel, FIRST_RUN_BODY, FIRST_RUN_TITLE,
};

/// The version of this build.
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Delay before the first-run question and the crash note, so the window is up first.
const STARTUP_DELAY: std::time::Duration = std::time::Duration::from_millis(1800);

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// Whether startup notices are suppressed (headless screenshots, automation).
fn notices_suppressed() -> bool {
    std::env::var_os("LECTOR_SCREENSHOT").is_some()
        || std::env::var_os("LECTOR_NO_NOTICES").is_some()
}

// ---------------------------------------------------------------------------------------------
// The hub (one per app)
// ---------------------------------------------------------------------------------------------

/// What the hub tells the windows.
#[derive(Clone)]
pub enum HubEvent {
    /// The known update status changed (the status bar item shows or hides).
    Changed,
    /// A check the user asked for finished; the answer belongs to the window that asked, focused
    /// or not (the dialog waits there until the user comes back).
    ManualResult(Box<UpdateStatus>, AnyWindowHandle),
}

/// The update state shared by all windows.
pub struct UpdateHub {
    notices: Notices,
    /// Where the notices are kept (`None` in tests: in memory only).
    store: Option<NoticesStore>,
    /// Where the update check keeps its state.
    update_state: Option<crate::update_model::UpdateStateStore>,
    status: Option<UpdateStatus>,
    checking: bool,
    /// Windows that asked and wait for the answer (several can ask during one check).
    askers: Vec<AnyWindowHandle>,
    startup_done: bool,
}

impl EventEmitter<HubEvent> for UpdateHub {}

impl UpdateHub {
    fn new(core: &lector_app::AppCore) -> Self {
        let store: NoticesStore = core.store().notices();
        let notices = Notices::load(&store);
        let status = notices
            .pending_update
            .clone()
            .filter(|s| is_newer_available(s, CURRENT_VERSION));
        UpdateHub {
            notices,
            store: Some(store),
            update_state: Some(core.store().update_state()),
            status,
            checking: false,
            askers: Vec::new(),
            startup_done: false,
        }
    }

    /// Changes the notices: in the store in one transaction (the in-memory copy becomes the
    /// stored value), or in memory when there is no store or it refused.
    fn persist<R>(&mut self, f: impl Fn(&mut Notices) -> R) -> R {
        if let Some(store) = &self.store {
            match Notices::update(store, &f) {
                Ok((r, n)) => {
                    self.notices = n;
                    return r;
                }
                Err(e) => tracing::warn!("notices not saved: {e}"),
            }
        }
        f(&mut self.notices)
    }

    /// The available update, if one is known.
    pub fn available(&self) -> Option<&UpdateStatus> {
        self.status
            .as_ref()
            .filter(|s| is_newer_available(s, CURRENT_VERSION))
    }

    /// Runs a check off the UI thread. `asker`: the window of the user who asked, which gets the
    /// answer (`None`: an automatic check, silent unless an update exists).
    fn check(&mut self, asker: Option<AnyWindowHandle>, cx: &mut Context<Self>) {
        if let Some(asker) = asker {
            if !self.askers.contains(&asker) {
                self.askers.push(asker);
            }
        }
        if self.checking {
            return;
        }
        self.checking = true;
        let channel = forge_channel(AppSettings::get(cx).updates.channel);
        let Some(state) = self.update_state.clone() else {
            self.checking = false;
            return;
        };
        // Network: the blocking lane.
        let key = view_key(JobKind::UpdateCheck, cx.entity_id());
        let work = background(cx, key, Priority::Background, move |_| {
            run_check(CURRENT_VERSION, channel, &UpdateOptions::default(), &state)
        });
        cx.spawn(async move |this, cx| {
            let status = work.await;
            let _ = this.update(cx, |this, cx| {
                this.checking = false;
                let Ok(status) = status else {
                    cx.notify();
                    return;
                };
                match &status {
                    UpdateStatus::Available { .. } => {
                        this.persist(|n| n.pending_update = Some(status.clone()));
                        this.status = Some(status.clone());
                    }
                    UpdateStatus::UpToDate => {
                        this.persist(|n| n.pending_update = None);
                        this.status = None;
                    }
                    // Keep the last known state when the check did not produce an answer.
                    UpdateStatus::CouldNotCheck { .. } => {}
                }
                cx.emit(HubEvent::Changed);
                for asker in std::mem::take(&mut this.askers) {
                    cx.emit(HubEvent::ManualResult(Box::new(status.clone()), asker));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The automatic check at startup: only when on, and at most once per 24 hours.
    fn automatic_check(&mut self, cx: &mut Context<Self>) {
        let setting = AppSettings::get(cx).updates.check;
        let effective = self.notices.effective_check(setting);
        let due = self.update_state.as_ref().is_some_and(|state| {
            crate::update_model::automatic_check_due_in(effective, state, now_secs())
        });
        if due {
            self.check(None, cx);
        }
    }

    fn toggle_auto(&mut self, cx: &mut Context<Self>) -> bool {
        let setting = AppSettings::get(cx).updates.check;
        self.persist(|n| n.toggle_auto_check(setting))
    }
}

// ---------------------------------------------------------------------------------------------
// Dialogs
// ---------------------------------------------------------------------------------------------

fn dialog_frame(id: &'static str, width: f32, cx: &App) -> gpui::Stateful<gpui::Div> {
    surface(cx).id(id).key_context(id).w(px(width))
}

/// The update dialog: version, date, notes link, verification and the action.
pub struct UpdateDialog {
    model: UpdateDialogModel,
    focus: FocusHandle,
    mono: SharedString,
}

impl EventEmitter<DismissEvent> for UpdateDialog {}

impl Focusable for UpdateDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl UpdateDialog {
    /// A dialog for `status`.
    pub fn new(status: &UpdateStatus, cx: &mut Context<Self>) -> Self {
        UpdateDialog {
            model: dialog_model(status, CURRENT_VERSION),
            focus: cx.focus_handle(),
            mono: lector_ui::code_view::EditorSettings::get(cx).font_family,
        }
    }
}

impl Render for UpdateDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let m = self.model.clone();
        dialog_frame("UpdateDialog", 460., cx)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|_, e: &KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" {
                    cx.emit(DismissEvent);
                    cx.stop_propagation();
                }
            }))
            .child(
                v_flex()
                    .p(px(18.))
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui.text.hsla())
                            .child(m.headline.clone()),
                    )
                    .children(m.details.iter().map(|d| {
                        div()
                            .text_size(metrics::text_sm())
                            .text_color(ui.text_muted.hsla())
                            .child(d.clone())
                    }))
                    .when_some(m.copyable.clone(), |el, (label, text)| {
                        el.child(
                            v_flex()
                                .gap(px(2.))
                                .child(
                                    div()
                                        .text_size(metrics::text_xs())
                                        .text_color(ui.text_faint.hsla())
                                        .child(label),
                                )
                                .child(
                                    div()
                                        .id("update-copyable")
                                        .px(px(8.))
                                        .py(px(5.))
                                        .rounded(metrics::radius())
                                        .bg(ui.surface.hsla())
                                        .font_family(self.mono.clone())
                                        .text_size(metrics::text_xs())
                                        .text_color(ui.text.hsla())
                                        .overflow_x_hidden()
                                        .child(text),
                                ),
                        )
                    })
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap(px(6.))
                            .pt(px(6.))
                            .justify_end()
                            .children(m.buttons.iter().enumerate().map(|(ix, b)| {
                                let action = b.action.clone();
                                Button::new(("update-button", ix), b.label.clone())
                                    .style(if b.primary {
                                        ButtonStyle::Accent
                                    } else {
                                        ButtonStyle::Subtle
                                    })
                                    .on_click(move |_, _, cx| match &action {
                                        UpdateButtonAction::OpenUrl(url) => {
                                            cx.open_url(&crate::update_model::safe_release_url(url))
                                        }
                                        UpdateButtonAction::Copy(text) => cx.write_to_clipboard(
                                            ClipboardItem::new_string(text.clone()),
                                        ),
                                    })
                            }))
                            .child(
                                Button::new("update-close", "Close")
                                    .style(ButtonStyle::Subtle)
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                            ),
                    ),
            )
    }
}

/// The one-time "Check for updates automatically?" question.
pub struct FirstRunPrompt {
    hub: WeakEntity<UpdateHub>,
    focus: FocusHandle,
}

impl EventEmitter<DismissEvent> for FirstRunPrompt {}

impl Focusable for FirstRunPrompt {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl FirstRunPrompt {
    fn answer(&mut self, turn_on: Option<bool>, cx: &mut Context<Self>) {
        let _ = self.hub.update(cx, |hub, cx| {
            hub.persist(|n| match turn_on {
                Some(on) => n.answer_update_question(on),
                None => n.dismiss_update_question(),
            });
            if turn_on == Some(true) {
                hub.check(None, cx);
            }
        });
        cx.emit(DismissEvent);
    }
}

impl Render for FirstRunPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        dialog_frame("FirstRunPrompt", 440., cx)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" {
                    this.answer(None, cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                v_flex()
                    .p(px(18.))
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui.text.hsla())
                            .child(FIRST_RUN_TITLE),
                    )
                    .child(
                        div()
                            .text_size(metrics::text_sm())
                            .text_color(ui.text_muted.hsla())
                            .child(FIRST_RUN_BODY),
                    )
                    .child(
                        h_flex()
                            .gap(px(6.))
                            .pt(px(6.))
                            .justify_end()
                            .child(
                                Button::new("update-not-now", "Not now")
                                    .style(ButtonStyle::Subtle)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.answer(Some(false), cx)),
                                    ),
                            )
                            .child(
                                Button::new("update-turn-on", "Turn on")
                                    .style(ButtonStyle::Accent)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.answer(Some(true), cx)),
                                    ),
                            ),
                    ),
            )
    }
}

/// The crash log: sessions, the crash-free rate and the recent reports.
pub struct CrashLogDialog {
    model: HealthModel,
    workspace: WeakEntity<Workspace>,
    focus: FocusHandle,
}

impl EventEmitter<DismissEvent> for CrashLogDialog {}

impl Focusable for CrashLogDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl CrashLogDialog {
    /// Reads the health log now.
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        CrashLogDialog {
            model: health_model(&lector_app::health::health_summary()),
            workspace,
            focus: cx.focus_handle(),
        }
    }
}

impl Render for CrashLogDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let m = self.model.clone();
        let dir: PathBuf = m.crash_dir.clone();
        let all_text = health_text(&m);
        let rows: Vec<gpui::AnyElement> = m
            .rows
            .iter()
            .enumerate()
            .map(|(ix, row)| {
                let (path, details) = (row.path.clone(), row.details.clone());
                let ws = self.workspace.clone();
                h_flex()
                    .gap(px(8.))
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(metrics::text_xs())
                                    .text_color(ui.text_faint.hsla())
                                    .child(row.when.clone()),
                            )
                            .child(
                                div()
                                    .text_size(metrics::text_sm())
                                    .text_color(ui.text.hsla())
                                    .child(row.message.clone()),
                            )
                            .when(!row.location.is_empty(), |el| {
                                el.child(
                                    div()
                                        .text_size(metrics::text_xs())
                                        .text_color(ui.text_muted.hsla())
                                        .child(row.location.clone()),
                                )
                            }),
                    )
                    .child(
                        Button::new(("crash-open", ix), "Open")
                            .style(ButtonStyle::Subtle)
                            .compact()
                            .on_click(cx.listener(move |_, _, window, cx| {
                                let path = path.clone();
                                let _ =
                                    ws.update(cx, |ws, cx| ws.open_path(path, false, window, cx));
                                cx.emit(DismissEvent);
                            })),
                    )
                    .child(
                        Button::new(("crash-copy", ix), "Copy details")
                            .style(ButtonStyle::Subtle)
                            .compact()
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(details.clone()))
                            }),
                    )
                    .into_any_element()
            })
            .collect();
        dialog_frame("CrashLogDialog", 560., cx)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|_, e: &KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" {
                    cx.emit(DismissEvent);
                    cx.stop_propagation();
                }
            }))
            .child(
                v_flex()
                    .p(px(18.))
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui.text.hsla())
                            .child("Crash log"),
                    )
                    .child(
                        v_flex()
                            .gap(px(2.))
                            .text_size(metrics::text_sm())
                            .text_color(ui.text_muted.hsla())
                            .child(m.sessions_line.clone())
                            .child(m.rate_line.clone())
                            .child(m.reports_line.clone())
                            .children(m.versions.iter().map(|v| {
                                div()
                                    .pl(px(10.))
                                    .text_size(metrics::text_xs())
                                    .child(v.clone())
                            })),
                    )
                    .child(
                        div()
                            .text_size(metrics::text_xs())
                            .text_color(ui.text_faint.hsla())
                            .child(m.privacy_line),
                    )
                    .when(rows.is_empty(), |el| {
                        el.child(
                            div()
                                .text_size(metrics::text_sm())
                                .text_color(ui.text_muted.hsla())
                                .child("No crash reports."),
                        )
                    })
                    .child(v_flex().gap(px(8.)).children(rows))
                    .child(
                        h_flex()
                            .gap(px(6.))
                            .pt(px(6.))
                            .justify_end()
                            .child(
                                Button::new("crash-folder", "Show folder")
                                    .style(ButtonStyle::Subtle)
                                    .on_click(move |_, _, cx| cx.reveal_path(&dir)),
                            )
                            .child(
                                Button::new("crash-copy-summary", "Copy summary")
                                    .style(ButtonStyle::Subtle)
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            all_text.clone(),
                                        ))
                                    }),
                            )
                            .child(
                                Button::new("crash-close", "Close")
                                    .style(ButtonStyle::Accent)
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                            ),
                    ),
            )
    }
}

// ---------------------------------------------------------------------------------------------
// The status bar item
// ---------------------------------------------------------------------------------------------

/// The quiet "Update 0.7.1" item at the right of the status bar.
pub struct UpdateStatusItem {
    hub: Entity<UpdateHub>,
}

impl Render for UpdateStatusItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let label = self
            .hub
            .read(cx)
            .available()
            .and_then(status_bar_label)
            .unwrap_or_default();
        h_flex()
            .id("status-update")
            .gap(px(4.))
            .px(px(6.))
            .h_full()
            .items_center()
            .cursor_pointer()
            .rounded(metrics::radius())
            .hover(|s| s.bg(ui.hover.hsla()))
            .text_size(metrics::text_sm())
            .text_color(ui.text_accent.hsla())
            .tooltip(Tooltip::text("A new version of Lector is available"))
            .child(
                Icon::new(IconName::Refresh)
                    .size(IconSize::XSmall)
                    .color(ui.text_accent.hsla()),
            )
            .child(label)
            .on_click(|_, window, cx| {
                window.dispatch_action(Box::new(ws_actions::CheckForUpdates), cx)
            })
    }
}

/// "State not saved": shown for as long as this process's state is read-only (ADR 0004). The
/// tooltip and a click say what is lost and why.
pub struct StateStoreStatusItem {
    indicator: lector_app::store::StateIndicator,
}

impl Render for StateStoreStatusItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let detail = self.indicator.detail.clone();
        let tooltip = detail.clone();
        h_flex()
            .id("status-state-store")
            .gap(px(4.))
            .px(px(6.))
            .h_full()
            .items_center()
            .cursor_pointer()
            .rounded(metrics::radius())
            .hover(|s| s.bg(ui.hover.hsla()))
            .text_size(metrics::text_sm())
            .text_color(ui.text_muted.hsla())
            .tooltip(Tooltip::text(tooltip))
            .child(
                Icon::new(IconName::AlertTriangle)
                    .size(IconSize::XSmall)
                    .color(ui.text_muted.hsla()),
            )
            .child(self.indicator.label.clone())
            .on_click(move |_, window, cx| {
                let detail = detail.clone();
                if let Some(ws) = window.root::<Workspace>().flatten() {
                    ws.update(cx, |ws, cx| {
                        ws.show_action_toast_level(
                            detail,
                            lector_ui::views::workspace::ToastLevel::Warning,
                            crate::health_model::STATE_STORE_BUTTON,
                            std::time::Duration::from_secs(20),
                            |_, _, _| {},
                            cx,
                        )
                    });
                }
            })
    }
}

// ---------------------------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------------------------

/// Registers the hub and the commands with every workspace window (also windows without a folder:
/// the Help menu works on the welcome screen).
pub fn init(cx: &mut App) {
    let core = lector_ui::commands_bridge::core(cx);
    let hub = cx.new(|_| UpdateHub::new(&core));
    install(cx, hub);
}

/// [`init`] with a given hub (tests).
fn install(cx: &mut App, hub: Entity<UpdateHub>) {
    cx.observe_new(move |ws: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let hub = hub.clone();
        let workspace = cx.entity();
        // The status bar item follows the hub.
        let item = cx.new(|_| UpdateStatusItem { hub: hub.clone() });
        let sync = {
            let (hub, item) = (hub.clone(), item.clone());
            move |ws: &mut Workspace, cx: &mut Context<Workspace>| {
                let available = hub.read(cx).available().is_some();
                ws.set_status_view("update", available.then(|| item.clone().into()), cx);
            }
        };
        sync(ws, cx);
        // Read-only state lasts for the whole process: a persistent item, not only a toast.
        let mode = lector_ui::commands_bridge::core(cx).store().mode();
        if let Some(indicator) = crate::health_model::state_status_item(&mode) {
            let item = cx.new(|_| StateStoreStatusItem { indicator });
            ws.set_status_view("state-store", Some(item.into()), cx);
        }
        let sync_on_event = sync.clone();
        cx.subscribe_in(
            &hub,
            window,
            move |ws, hub, event: &HubEvent, window, cx| {
                sync_on_event(ws, cx);
                if let HubEvent::ManualResult(status, asker) = event {
                    // The window that asked shows the answer, even if another one has focus now.
                    if *asker == window.window_handle() {
                        let status = status.clone();
                        ws.modal_layer().update(cx, |layer, cx| {
                            let dialog = cx.new(|cx| UpdateDialog::new(&status, cx));
                            layer.show_modal(dialog, window, cx)
                        });
                    }
                }
                let _ = hub;
            },
        )
        .detach();
        register_actions(ws, &hub);
        // Startup notices: once per process, in the first window.
        if notices_suppressed() {
            return;
        }
        let first = hub.update(cx, |h, _| !std::mem::replace(&mut h.startup_done, true));
        if first {
            cx.spawn_in(window, async move |ws, cx| {
                cx.background_executor().timer(STARTUP_DELAY).await;
                let _ = ws.update_in(cx, |ws, window, cx| startup_notices(ws, &hub, window, cx));
            })
            .detach();
        }
        let _ = workspace;
    })
    .detach();
}

fn register_actions(ws: &mut Workspace, hub: &Entity<UpdateHub>) {
    let h = hub.clone();
    ws.register_action(move |ws, _: &ws_actions::CheckForUpdates, window, cx| {
        ws.show_toast("Checking for updates\u{2026}", cx);
        let asker = window.window_handle();
        h.update(cx, |h, cx| h.check(Some(asker), cx));
    });
    let h = hub.clone();
    ws.register_action(move |ws, _: &ws_actions::ToggleAutoUpdateCheck, _, cx| {
        let on = h.update(cx, |h, cx| {
            let on = h.toggle_auto(cx);
            if on {
                h.check(None, cx);
            }
            on
        });
        ws.show_toast(toggle_text(on), cx);
    });
    ws.register_action(|ws, _: &ws_actions::ShowCrashLog, window, cx| {
        show_crash_log(ws, window, cx);
    });
}

fn show_crash_log(ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let weak = cx.entity().downgrade();
    ws.modal_layer().update(cx, |layer, cx| {
        let dialog = cx.new(|cx| CrashLogDialog::new(weak, cx));
        layer.show_modal(dialog, window, cx)
    });
}

/// The one-time first-run question, the cached update, the automatic check and the note after an
/// abnormal exit.
fn startup_notices(
    ws: &mut Workspace,
    hub: &Entity<UpdateHub>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let setting = AppSettings::get(cx).updates.check;
    let ask = hub.read(cx).notices.should_ask_about_updates(setting);
    if ask {
        let hub_weak = hub.downgrade();
        ws.modal_layer().update(cx, |layer, cx| {
            let prompt = cx.new(|cx| FirstRunPrompt {
                hub: hub_weak,
                focus: cx.focus_handle(),
            });
            layer.show_modal(prompt, window, cx)
        });
    } else {
        hub.update(cx, |h, cx| h.automatic_check(cx));
    }
    // After an abnormal previous exit: one quiet note, nothing sent.
    let paths = lector_app::health::HealthPaths::default();
    let previous = lector_app::health::current_session_id()
        .and_then(|id| lector_app::health::previous_session_abnormal(&paths, &id));
    if let Some(record) = previous {
        if hub.read(cx).notices.needs_crash_note(&record.id) {
            hub.update(cx, |h, _| {
                h.persist(|n| n.acknowledged_crash = Some(record.id.clone()));
            });
            ws.show_action_toast(
                crate::health_model::ABNORMAL_EXIT_NOTE,
                crate::health_model::ABNORMAL_EXIT_BUTTON,
                std::time::Duration::from_secs(12),
                show_crash_log,
                cx,
            );
        }
    }
    // The state store could not be used normally (ADR 0004): say what this process loses.
    let core = lector_ui::commands_bridge::core(cx);
    let store = core.store();
    if let Some(note) = crate::health_model::state_store_note(&store.mode(), store.recovered_from())
    {
        ws.show_action_toast_level(
            note,
            lector_ui::views::workspace::ToastLevel::Warning,
            crate::health_model::STATE_STORE_BUTTON,
            std::time::Duration::from_secs(20),
            |_, _, _| {},
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use lector_theme::{Appearance, ThemeRegistry};
    use lector_ui::theme::{ThemeMode, ThemeState};

    fn window_with_workspace<'a>(
        cx: &'a mut TestAppContext,
        dir: &std::path::Path,
    ) -> (Entity<Workspace>, &'a mut gpui::VisualTestContext) {
        let mut settings = lector_config::Settings::default();
        settings.lsp.enabled = false;
        settings.session.restore = false;
        let project = lector_app::Project::open(dir, settings).expect("project");
        cx.add_window_view(|window, cx| Workspace::new(project, window, cx))
    }

    /// The answer to "Check for Updates" appears in the window that asked, whichever window has
    /// focus when the check finishes; other windows stay quiet.
    #[gpui::test]
    fn manual_result_goes_to_the_asking_window(cx: &mut TestAppContext) {
        std::env::set_var("LECTOR_NO_NOTICES", "1");
        cx.update(|cx: &mut App| {
            ThemeState::init(cx, ThemeRegistry::new(), ThemeMode::Dark, Appearance::Dark);
            cx.set_global(lector_ui::code_view::EditorSettings::default());
            cx.set_global(AppSettings {
                settings: std::sync::Arc::new(lector_config::Settings::default()),
                warnings: Default::default(),
            });
        });
        let hub = cx.new(|_| UpdateHub {
            notices: Notices::default(),
            store: None,
            update_state: None,
            status: None,
            checking: false,
            askers: Vec::new(),
            startup_done: true,
        });
        cx.update(|cx| install(cx, hub.clone()));
        let dir = tempfile::tempdir().unwrap();
        let mut base_b = cx.clone();
        let (ws_a, cx_a) = window_with_workspace(cx, dir.path());
        let asker = cx_a.update(|window, _| window.window_handle());
        let (ws_b, cx_b) = window_with_workspace(&mut base_b, dir.path());
        // Window B is the newest (focused) one; A asked.
        let shows = |ws: &Entity<Workspace>, cx: &mut gpui::VisualTestContext| {
            ws.read_with(cx, |ws, cx| {
                ws.modal_layer()
                    .read(cx)
                    .active_modal::<UpdateDialog>()
                    .is_some()
            })
        };
        hub.update(cx_b, |_, cx| {
            cx.emit(HubEvent::ManualResult(
                Box::new(UpdateStatus::UpToDate),
                asker,
            ))
        });
        cx_b.run_until_parked();
        assert!(!shows(&ws_b, cx_b), "the focused window did not ask");
        assert!(shows(&ws_a, cx_a), "the asking window shows the answer");
    }
}
