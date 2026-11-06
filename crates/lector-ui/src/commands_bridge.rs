//! Bridge between GPUI and the headless command API (`lector_app::commands`).
//!
//! * [`init`] stores the process-wide [`AppCore`] as a GPUI global and, for every
//!   new [`Workspace`], registers a handler for each GPUI action that has a
//!   registry command ([`CommandSpec::ui_action`](lector_app::CommandSpec)): the
//!   action then runs through [`CommandRegistry::invoke`](lector_app::CommandRegistry::invoke),
//!   so writes triggered from menus, keys and the palette are journaled.
//! * [`command_context`] builds a [`CommandContext`] from a workspace (project,
//!   active file, selections); extension crates add focus (changeset, hunk) with
//!   [`register_focus_provider`].
//! * [`invoke`] runs a command on the scheduler and executes its [`UiRequest`]s
//!   in the workspace (open path, open changeset, toast, refresh); failures toast.
//! * [`palette_entries`] lists registry commands for the command palette, with
//!   availability reasons.

use std::rc::Rc;

use gpui::{App, Context, Global, Window};
use lector_app::commands::{Availability, CommandContext, Focus, Outcome, Source, UiRequest};
use lector_app::AppCore;
use lector_git::Changeset;
use serde_json::Value;

use crate::views::Workspace;

/// The application core as a GPUI global.
pub struct CoreGlobal(pub AppCore);

impl Global for CoreGlobal {}

type FocusProvider = Rc<dyn Fn(&Workspace, &mut Focus, &App)>;
type ChangesetOpener = Rc<dyn Fn(&mut Workspace, Changeset, &mut Window, &mut Context<Workspace>)>;

#[derive(Default)]
struct Hooks {
    focus: Vec<FocusProvider>,
    open_changeset: Option<ChangesetOpener>,
}

impl Global for Hooks {}

/// Installs the core (if not set yet) and the action bridges. Called by [`crate::init`].
pub fn init(cx: &mut App) {
    if cx.try_global::<CoreGlobal>().is_none() {
        cx.set_global(CoreGlobal(AppCore::global()));
    }
    if cx.try_global::<Hooks>().is_none() {
        cx.set_global(Hooks::default());
    }
    cx.observe_new(|ws: &mut Workspace, _window, cx| {
        install_action_bridges(ws, cx);
    })
    .detach();
}

/// The application core (the global one when [`init`] has not run, e.g. in tests).
pub fn core(cx: &App) -> AppCore {
    cx.try_global::<CoreGlobal>()
        .map(|g| g.0.clone())
        .unwrap_or_else(AppCore::global)
}

/// Recent projects (database-backed; replaces `lector_config::RecentProjects`).
pub fn recent_projects() -> lector_app::db::DbRecentProjects {
    AppCore::global().db().recent_projects()
}

/// Window sessions (database-backed; replaces `lector_config::SessionStore`).
pub fn session_store() -> lector_app::db::DbSessionStore {
    AppCore::global().db().session_store()
}

/// Adds a contributor to [`Focus`] (e.g. lector-ui-git fills the changeset of the active diff).
pub fn register_focus_provider(cx: &mut App, f: impl Fn(&Workspace, &mut Focus, &App) + 'static) {
    cx.default_global::<Hooks>().focus.push(Rc::new(f));
}

/// Sets how [`UiRequest::OpenChangeset`] is executed (lector-ui-git opens a review tab).
pub fn set_changeset_opener(
    cx: &mut App,
    f: impl Fn(&mut Workspace, Changeset, &mut Window, &mut Context<Workspace>) + 'static,
) {
    cx.default_global::<Hooks>().open_changeset = Some(Rc::new(f));
}

/// The command context of a workspace window: its project (None without a
/// folder), the active file and its selections, plus registered focus providers.
pub fn command_context(ws: &Workspace, cx: &App) -> CommandContext {
    let project = ws.has_folder().then(|| ws.project().clone());
    let mut cx_ = CommandContext::new(core(cx), project, Source::Ui);
    cx_.focus.path = ws.active_path(cx);
    if let Some(view) = ws.active_code_view(cx) {
        let point = |p: crate::code_view::selection::Pos| lector_text::Point {
            row: p.row,
            column: p.col as u32,
        };
        cx_.focus.selections = view
            .read(cx)
            .selections()
            .iter()
            .map(|s| {
                let (a, b) = (point(s.anchor), point(s.head));
                if a <= b {
                    a..b
                } else {
                    b..a
                }
            })
            .collect();
    }
    if let Some(hooks) = cx.try_global::<Hooks>() {
        for f in &hooks.focus {
            f(ws, &mut cx_.focus, cx);
        }
    }
    cx_
}

/// A registry command as shown in the palette.
#[derive(Debug, Clone)]
pub struct PaletteEntry {
    /// Command id.
    pub id: &'static str,
    /// "Category: Title".
    pub label: String,
    /// The bound GPUI action, if any (the palette merges it with that action's row).
    pub ui_action: Option<&'static str>,
    /// Why it cannot run now.
    pub unavailable: Option<String>,
}

/// Registry commands for the palette: those bound to a GPUI action (to annotate
/// that row with availability) and those runnable without arguments.
pub fn palette_entries(ctx: &CommandContext) -> Vec<PaletteEntry> {
    ctx.core
        .registry()
        .list()
        .into_iter()
        .filter_map(|c| {
            let spec = c.spec();
            let needs_args = spec
                .args_schema
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|r| !r.is_empty());
            if needs_args && spec.ui_action.is_none() {
                return None;
            }
            let unavailable = match c.available(ctx) {
                Availability::Available => None,
                Availability::Unavailable(why) => Some(why),
            };
            Some(PaletteEntry {
                id: spec.id,
                label: format!("{}: {}", spec.category, spec.title),
                ui_action: spec.ui_action,
                unavailable,
            })
        })
        .collect()
}

/// Runs a registry command for the workspace in the background, then executes
/// its UI requests (and toasts its summary when `toast_summary`). Errors toast.
pub fn invoke(
    ws: &mut Workspace,
    id: &str,
    args: Value,
    toast_summary: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let ctx = command_context(ws, cx);
    let registry = ctx.core.registry().clone();
    let job = registry.invoke_async(id, args, ctx).into_future();
    let id = id.to_string();
    cx.spawn_in(window, async move |this, cx| {
        let result = job.await;
        let _ = this.update_in(cx, |ws, window, cx| match result {
            Some(Ok(outcome)) => apply_outcome(ws, outcome, toast_summary, window, cx),
            Some(Err(err)) => ws.show_toast(err.to_string(), cx),
            None => tracing::warn!("command `{id}` was cancelled"),
        });
    })
    .detach();
}

/// Executes an outcome's UI requests in the workspace.
pub fn apply_outcome(
    ws: &mut Workspace,
    outcome: Outcome,
    toast_summary: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let mut toasted = false;
    for req in outcome.ui {
        match req {
            UiRequest::OpenPath { path, point } => {
                ws.open_path_at(
                    path,
                    point.map(|p| p.row + 1),
                    point.map(|p| p.column + 1),
                    window,
                    cx,
                );
            }
            UiRequest::OpenChangeset(cs) => {
                let opener = cx
                    .try_global::<Hooks>()
                    .and_then(|h| h.open_changeset.clone());
                match opener {
                    Some(open) => open(ws, cs, window, cx),
                    None => ws.show_toast("Git views are not available", cx),
                }
            }
            UiRequest::Toast(msg) => {
                ws.show_toast(msg, cx);
                toasted = true;
            }
            UiRequest::Refresh => {
                if let Some(repo) = ws.project().repo() {
                    core(cx).git().repo_for(repo).refresh_and_notify();
                }
            }
        }
    }
    if toast_summary && !toasted && !outcome.summary.is_empty() {
        ws.show_toast(outcome.summary, cx);
    }
}

/// Routes every GPUI action that has a registry command through [`invoke`].
fn install_action_bridges(ws: &mut Workspace, cx: &mut Context<Workspace>) {
    let core = core(cx);
    for command in core.registry().list() {
        let spec = command.spec();
        let Some(action_name) = spec.ui_action else {
            continue;
        };
        let Ok(action) = cx.build_action(action_name, None) else {
            tracing::warn!("command `{}` names unknown action `{action_name}`", spec.id);
            continue;
        };
        let id = spec.id;
        let core = core.clone();
        ws.register_boxed_action(action, move |ws, window, cx| {
            let ctx = command_context(ws, cx);
            match core.registry().get(id).map(|c| c.available(&ctx)) {
                Some(Availability::Available) => invoke(ws, id, Value::Null, true, window, cx),
                Some(Availability::Unavailable(why)) => ws.show_toast(why, cx),
                None => cx.propagate(),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lector_app::CoreOptions;

    #[test]
    fn palette_lists_argument_free_commands_with_reasons() {
        let core = AppCore::open(CoreOptions::for_tests());
        let ctx = CommandContext::new(core, None, Source::Ui);
        let entries = palette_entries(&ctx);
        // Commands that need arguments (and have no action) are not listed.
        assert!(!entries.iter().any(|e| e.id == "search.project"));
        let changes = entries
            .iter()
            .find(|e| e.id == "git.list_changes")
            .expect("listed");
        assert_eq!(changes.label, "Git: List Changes");
        assert_eq!(changes.unavailable.as_deref(), Some("No folder is open"));
        let list = entries.iter().find(|e| e.id == "commands.list").unwrap();
        assert_eq!(list.unavailable, None);
    }
}
