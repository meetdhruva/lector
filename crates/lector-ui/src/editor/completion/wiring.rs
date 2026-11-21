//! Wiring of completion: one [`CompletionController`] per file tab (the
//! registry, [`init`], [`controller_for`]) and the workspace actions that
//! route completion and signature-help keys to it.

use std::collections::HashMap;

use gpui::{prelude::*, App, Context, Entity, EntityId, Global, Window};

use super::{CompletionController, EnterOutcome};
use crate::actions::completion as act;
use crate::actions::editor::{
    ClearSelection, MoveDown, MoveUp, Newline, PageDown, PageUp, Tab, TriggerParameterHints,
    TriggerSuggest,
};
use crate::actions::signature as sig_act;
use crate::editor::signature_help::SignatureController;
use crate::editor::FileView;
use crate::views::Workspace;

#[derive(Default)]
struct Registry {
    by_view: HashMap<EntityId, Entity<CompletionController>>,
}

impl Global for Registry {}

/// The completion controller of a file tab.
pub fn controller_for(view: &Entity<FileView>, cx: &App) -> Option<Entity<CompletionController>> {
    cx.try_global::<Registry>()?
        .by_view
        .get(&view.entity_id())
        .cloned()
}

/// Installs completion and signature help for every file tab and the
/// workspace-level action handlers (called by `lsp::init`).
pub fn init(cx: &mut App) {
    if !cx.has_global::<Registry>() {
        cx.set_global(Registry::default());
    }
    cx.observe_new(|_: &mut FileView, _, cx: &mut Context<FileView>| {
        let view = cx.entity();
        // Once the tab is fully built.
        cx.defer(move |cx| attach(&view, cx));
    })
    .detach();
    cx.observe_new(|ws: &mut Workspace, window, _| {
        if window.is_some() {
            register_actions(ws);
        }
    })
    .detach();
}

fn attach(view: &Entity<FileView>, cx: &mut App) {
    let (code_view, model) = {
        let v = view.read(cx);
        (v.code_view().clone(), v.model().clone())
    };
    let signature = {
        let (cv, m) = (code_view.downgrade(), model.downgrade());
        cx.new(|cx| SignatureController::new(cv, m, cx))
    };
    let controller = cx.new(|cx| CompletionController::new(&code_view, &model, signature, cx));
    let id = view.entity_id();
    cx.update_global::<Registry, _>(|r, _| {
        r.by_view.insert(id, controller);
    });
    cx.observe_release(view, move |_, cx| {
        cx.update_global::<Registry, _>(|r, _| {
            r.by_view.remove(&id);
        });
    })
    .detach();
}

/// Runs `f` on the completion controller of the active file tab. `f` returns
/// whether it handled the key; otherwise `fallback` (what the key does without
/// a popup) is dispatched.
fn with_popup(
    ws: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    f: impl FnOnce(&mut CompletionController, &mut Context<CompletionController>) -> bool,
    fallback: Box<dyn gpui::Action>,
) {
    let handled = ws
        .active_file_view(cx)
        .and_then(|view| controller_for(&view, cx))
        .is_some_and(|c| c.update(cx, f));
    if !handled {
        window.dispatch_action(fallback, cx);
    }
}

fn register_actions(ws: &mut Workspace) {
    ws.register_action(|ws, _: &act::SelectNext, window, cx| {
        with_popup(
            ws,
            window,
            cx,
            |c, cx| c.select_next(cx),
            Box::new(MoveDown),
        );
    })
    .register_action(|ws, _: &act::SelectPrev, window, cx| {
        with_popup(
            ws,
            window,
            cx,
            |c, cx| c.select_previous(cx),
            Box::new(MoveUp),
        );
    })
    .register_action(|ws, _: &act::PageDown, window, cx| {
        with_popup(ws, window, cx, |c, cx| c.page_down(cx), Box::new(PageDown));
    })
    .register_action(|ws, _: &act::PageUp, window, cx| {
        with_popup(ws, window, cx, |c, cx| c.page_up(cx), Box::new(PageUp));
    })
    .register_action(|ws, _: &act::Accept, window, cx| {
        with_popup(ws, window, cx, |c, cx| c.accept(None, cx), Box::new(Tab));
    })
    .register_action(|ws, _: &act::AcceptOnEnter, window, cx| {
        let outcome = ws
            .active_file_view(cx)
            .and_then(|view| controller_for(&view, cx))
            .map(|c| c.update(cx, |c, cx| c.accept_on_enter(cx)))
            .unwrap_or(EnterOutcome::NotOpen);
        if outcome != EnterOutcome::Accepted {
            window.dispatch_action(Box::new(Newline), cx);
        }
    })
    .register_action(|ws, _: &act::Dismiss, window, cx| {
        with_popup(
            ws,
            window,
            cx,
            |c, cx| c.dismiss(cx),
            Box::new(ClearSelection),
        );
    })
    .register_action(|ws, _: &TriggerSuggest, _window, cx| {
        let Some(controller) = ws
            .active_file_view(cx)
            .and_then(|view| controller_for(&view, cx))
        else {
            cx.propagate();
            return;
        };
        if let Err(message) = controller.update(cx, |c, cx| c.trigger_manual(cx)) {
            if !message.is_empty() {
                ws.show_toast(message, cx);
            }
        }
    })
    .register_action(|ws, _: &TriggerParameterHints, _window, cx| {
        let Some(controller) = ws
            .active_file_view(cx)
            .and_then(|view| controller_for(&view, cx))
        else {
            cx.propagate();
            return;
        };
        let signature = controller.read(cx).signature().clone();
        if let Err(message) = signature.update(cx, |s, cx| s.trigger_manual(cx)) {
            ws.show_toast(message, cx);
        }
    })
    .register_action(|ws, _: &sig_act::NextSignature, window, cx| {
        with_signature(
            ws,
            window,
            cx,
            |s, cx| s.next_signature(cx),
            Box::new(MoveDown),
        );
    })
    .register_action(|ws, _: &sig_act::PrevSignature, window, cx| {
        with_signature(
            ws,
            window,
            cx,
            |s, cx| s.previous_signature(cx),
            Box::new(MoveUp),
        );
    })
    .register_action(|ws, _: &sig_act::Dismiss, window, cx| {
        with_signature(
            ws,
            window,
            cx,
            |s, cx| s.dismiss(cx),
            Box::new(ClearSelection),
        );
    });
}

fn with_signature(
    ws: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    f: impl FnOnce(&mut SignatureController, &mut Context<SignatureController>) -> bool,
    fallback: Box<dyn gpui::Action>,
) {
    let handled = ws
        .active_file_view(cx)
        .and_then(|view| controller_for(&view, cx))
        .map(|c| c.read(cx).signature().clone())
        .is_some_and(|s| s.update(cx, f));
    if !handled {
        window.dispatch_action(fallback, cx);
    }
}
