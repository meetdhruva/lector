//! Credential prompts from git / ssh in the GUI (askpass, v0.7).
//!
//! One [`AskpassServer`] per app process, started the first time a network operation runs
//! (nothing listens before that). Its handler runs on a connection thread: it posts the
//! prompt to the GPUI thread and blocks until the dialog answers, at most
//! [`PROMPT_TIMEOUT`]. On the GPUI thread prompts are queued and shown one at a time as a
//! [`PromptDialog`] in the window whose operation is running, naming the operation and the
//! remote (credentials stripped from the URL). Closing the dialog in any way other than
//! answering — Escape, the window closing, the operation being cancelled or ending — answers
//! Cancel. Answers are passed to git as a [`lector_process::askpass::Secret`] (zeroed after
//! use), never logged and never stored by Lector.
//!
//! The helper program put into `GIT_ASKPASS` / `SSH_ASKPASS` is the server's generated shim
//! (Unix) or hard link (Windows), see `lector_process::askpass`; the environment goes to
//! [`SyncHost::set_askpass_env`] so only git network commands get it.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

use futures::StreamExt as _;
use gpui::{AnyWindowHandle, App, AppContext as _, Entity, Global, WeakEntity};
use lector_app::commands::git_sync::SyncHost;
use lector_process::askpass::{AskpassServer, PromptReply, PromptRequest};
use lector_ui::views::Workspace;

use crate::sync_dialogs::PromptDialog;
use crate::sync_model::prompt_spec;

/// How long git may wait for an answer before the prompt is cancelled.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

struct Pending {
    request: PromptRequest,
    reply: Sender<PromptReply>,
}

/// Told whether a prompt was answered (`true`) or cancelled (`false`).
pub type OnPrompt = std::rc::Rc<dyn Fn(bool, &mut App)>;

/// An operation that may prompt (registered while it runs).
#[derive(Clone)]
pub struct PromptContext {
    /// Operation id (unique per [`begin_operation`]).
    pub id: u64,
    /// Window to show the prompt in.
    pub window: AnyWindowHandle,
    /// Its workspace.
    pub workspace: WeakEntity<Workspace>,
    /// "Push main → origin/main".
    pub label: String,
    /// Remote URL, credentials redacted.
    pub remote: Option<String>,
    /// Told whether a prompt was answered (`true`) or cancelled (`false`).
    pub on_prompt: OnPrompt,
}

#[derive(Default)]
struct AskpassHost {
    server: Option<AskpassServer>,
    start_error: Option<String>,
    runtime_dir: Option<PathBuf>,
    queue: VecDeque<Pending>,
    showing: Option<(WeakEntity<PromptDialog>, u64)>,
    ops: Vec<PromptContext>,
    next_id: u64,
}

impl Global for AskpassHost {}

/// Uses `dir` (instead of Lector's data directory) for the askpass socket (tests).
pub fn set_runtime_dir(dir: PathBuf, cx: &mut App) {
    cx.default_global::<AskpassHost>().runtime_dir = Some(dir);
}

/// Starts the askpass server once per process and installs its environment on the
/// [`SyncHost`]. Failing to start is not fatal: git then fails with an authentication error
/// where it would have prompted, and the failure dialog says so.
pub fn ensure_started(cx: &mut App) {
    let host = cx.default_global::<AskpassHost>();
    if host.server.is_some() || host.start_error.is_some() {
        return;
    }
    let dir = host
        .runtime_dir
        .clone()
        .unwrap_or_else(lector_config::paths::data_dir);
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<Pending>();
    let handler = move |request: PromptRequest| -> PromptReply {
        let (reply, answer) = std::sync::mpsc::channel();
        if tx.unbounded_send(Pending { request, reply }).is_err() {
            return PromptReply::Cancel;
        }
        // Dropped sender (dialog closed, window gone) or timeout: cancel.
        answer
            .recv_timeout(PROMPT_TIMEOUT)
            .unwrap_or(PromptReply::Cancel)
    };
    let started = AskpassServer::start(&dir, handler).and_then(|server| {
        let exe = std::env::current_exe().map_err(lector_process::askpass::AskpassError::Io)?;
        let helper = server
            .helper_program(&exe)
            .map_err(lector_process::askpass::AskpassError::Io)?;
        Ok((server.env(&helper), server))
    });
    match started {
        Ok((env, server)) => {
            let core = lector_ui::commands_bridge::core(cx);
            SyncHost::of(&core).set_askpass_env(env);
            cx.default_global::<AskpassHost>().server = Some(server);
            cx.spawn(async move |cx| {
                while let Some(p) = rx.next().await {
                    if cx.update(|cx| enqueue(p, cx)).is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        Err(err) => {
            tracing::warn!("askpass: {err}; git cannot ask for credentials in this session");
            cx.default_global::<AskpassHost>().start_error = Some(err.to_string());
        }
    }
}

/// Stops the server and removes its private directory (socket, helper program). Called
/// when the app quits; prompts still waiting are cancelled.
pub fn shutdown(cx: &mut App) {
    cancel_pending(cx);
    if let Some(host) = cx.try_global::<AskpassHost>() {
        if host.server.is_some() {
            if let Some(server) = cx.global_mut::<AskpassHost>().server.take() {
                server.shutdown();
            }
        }
    }
}

/// Why credential prompts are unavailable, if the server could not start.
pub fn start_error(cx: &App) -> Option<String> {
    cx.try_global::<AskpassHost>()
        .and_then(|h| h.start_error.clone())
}

/// Registers a running operation that may prompt; returns its id for [`end_operation`].
pub fn begin_operation(mut ctx: PromptContext, cx: &mut App) -> u64 {
    let host = cx.default_global::<AskpassHost>();
    host.next_id += 1;
    ctx.id = host.next_id;
    let id = ctx.id;
    host.ops.push(ctx);
    id
}

/// The operation ended: its prompts (if any are still up) are cancelled.
pub fn end_operation(id: u64, cx: &mut App) {
    let host = cx.default_global::<AskpassHost>();
    host.ops.retain(|o| o.id != id);
    let showing_for_it = host.showing.as_ref().is_some_and(|(_, op)| *op == id);
    if host.ops.is_empty() || showing_for_it {
        cancel_pending(cx);
    }
}

/// Answers Cancel to every queued prompt and closes the one shown (the user cancelled the
/// operation, or it ended).
pub fn cancel_pending(cx: &mut App) {
    if cx.try_global::<AskpassHost>().is_none() {
        return;
    }
    let showing = showing(cx);
    let host = cx.global_mut::<AskpassHost>();
    for p in host.queue.drain(..) {
        let _ = p.reply.send(PromptReply::Cancel);
    }
    if let Some(dialog) = showing {
        dialog.update(cx, |d, cx| d.cancel(cx));
    }
}

/// Number of prompts waiting behind the visible one (tests).
pub fn queued(cx: &App) -> usize {
    cx.try_global::<AskpassHost>().map_or(0, |h| h.queue.len())
}

/// The visible prompt dialog (tests).
pub fn showing(cx: &App) -> Option<Entity<PromptDialog>> {
    cx.try_global::<AskpassHost>()?
        .showing
        .as_ref()
        .and_then(|(d, _)| d.upgrade())
        .filter(|d| !d.read(cx).is_finished())
}

fn enqueue(p: Pending, cx: &mut App) {
    cx.default_global::<AskpassHost>().queue.push_back(p);
    // Outside any window update (the dialog opens in a window).
    cx.defer(show_next);
}

/// Shows a prompt directly (the askpass server calls this through its queue; tests call it
/// with their own reply channel).
pub fn show_prompt(request: PromptRequest, reply: Sender<PromptReply>, cx: &mut App) {
    enqueue(Pending { request, reply }, cx);
}

/// Shows the next queued prompt once the visible one is answered or gone.
pub fn advance(cx: &mut App) {
    show_next(cx);
}

fn show_next(cx: &mut App) {
    loop {
        let busy = cx
            .default_global::<AskpassHost>()
            .showing
            .as_ref()
            .and_then(|(d, _)| d.upgrade())
            .is_some_and(|d| !d.read(cx).is_finished());
        if busy {
            return;
        }
        let host = cx.global_mut::<AskpassHost>();
        host.showing = None;
        let Some(pending) = host.queue.pop_front() else {
            return;
        };
        let ctx = host.ops.last().cloned().or_else(|| fallback_context(cx));
        let Some(ctx) = ctx else {
            let _ = pending.reply.send(PromptReply::Cancel);
            continue;
        };
        let Some(ws) = ctx.workspace.upgrade() else {
            let _ = pending.reply.send(PromptReply::Cancel);
            continue;
        };
        let spec = prompt_spec(&pending.request.text, pending.request.kind);
        let op_id = ctx.id;
        let label = ctx.label.clone();
        let remote = ctx.remote.clone();
        let on_prompt = ctx.on_prompt.clone();
        let reply = pending.reply.clone();
        let shown = ctx.window.update(cx, move |_, window, cx| {
            let dialog = cx.new(|cx| PromptDialog::new(spec, label, remote, reply, cx));
            cx.observe_release(&dialog, move |d, cx| {
                let answered = d.answered();
                let on_prompt = on_prompt.clone();
                cx.defer(move |cx| {
                    on_prompt(answered, cx);
                    if let Some(host) = cx.try_global::<AskpassHost>() {
                        if host
                            .showing
                            .as_ref()
                            .is_some_and(|(d, _)| d.upgrade().is_none())
                        {
                            cx.global_mut::<AskpassHost>().showing = None;
                        }
                    }
                    show_next(cx);
                });
            })
            .detach();
            let layer = ws.read(cx).modal_layer().clone();
            let d = dialog.clone();
            layer.update(cx, |layer, cx| layer.show_modal(d, window, cx));
            dialog.downgrade()
        });
        match shown {
            Ok(dialog) => {
                cx.global_mut::<AskpassHost>().showing = Some((dialog, op_id));
                return;
            }
            Err(_) => {
                let _ = pending.reply.send(PromptReply::Cancel);
            }
        }
    }
}

/// Without a registered operation (a prompt from a git process Lector did not start through
/// the sync UI): the first window with a git store.
fn fallback_context(cx: &App) -> Option<PromptContext> {
    let controller = crate::sync::any_controller(cx)?;
    let c = controller.read(cx);
    Some(PromptContext {
        id: 0,
        window: c.window_handle(),
        workspace: c.workspace(cx)?,
        label: "git".into(),
        remote: None,
        on_prompt: std::rc::Rc::new(|_, _| {}),
    })
}
