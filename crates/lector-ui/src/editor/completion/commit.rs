//! Applying a completion commit to the code view, including finishing a
//! pending acceptance ahead of a view command ([`finish_before_command`]).

use gpui::{App, Context, Entity, WeakEntity};
use lector_lsp::{CompletionCommit, CompletionSession};

use super::{CompletionController, FLAG_ACCEPTING, FLAG_OPEN, FLAG_PENDING, POPUP_KEY};
use crate::code_view::{edit, CodeView};

/// What finishing an acceptance ahead of a view command leaves for the view to apply.
pub(super) struct EarlyFinish {
    /// The item as the session stands; `None` when the text no longer fits it.
    pub(super) commit: Option<CompletionCommit>,
    /// The text held back meanwhile, in the order it was typed.
    pub(super) typed: Vec<String>,
}

/// The commit of `item` against the text and carets `view` has right now. `None` (logged)
/// when the text moved so that the commit no longer fits.
pub(super) fn commit_at(
    session: &CompletionSession,
    view: &CodeView,
    item: usize,
    cx: &App,
) -> Option<CompletionCommit> {
    let rope = view.buffer()?.read(cx).text().clone();
    let carets: Vec<usize> = view
        .selections()
        .iter()
        .map(|s| edit::pos_to_offset(&rope, s.head))
        .collect();
    match session.commit(&rope, &carets, view.primary_index(), item) {
        Ok(commit) => Some(commit),
        Err(err) => {
            tracing::debug!(target: "lector_ui::lsp", "completion not applied: {err}");
            None
        }
    }
}

/// Hides the popup and drops the key context flags of a session.
pub(super) fn clear_popup_state(v: &mut CodeView, cx: &mut Context<CodeView>) {
    v.set_popup(POPUP_KEY, None, cx);
    v.set_context_flag(FLAG_OPEN, false, cx);
    v.set_context_flag(FLAG_PENDING, false, cx);
    v.set_context_flag(FLAG_ACCEPTING, false, cx);
}

/// The view's command hook: a command, a click or a composition is about to run while an
/// acceptance waits for the server. Finishes the acceptance first, synchronously, on `view`
/// itself (it is borrowed by the command, so the controller cannot update it): commits the
/// item as it stands (one undo step), types the held-back text (the next one), and only
/// then lets the command run against that buffer. The late resolve answer is dropped.
///
/// No signature help is requested for this commit: the command may already have moved on.
pub(super) fn finish_before_command(
    controller: &WeakEntity<CompletionController>,
    view: &mut CodeView,
    cx: &mut Context<CodeView>,
) {
    let Some(controller) = controller.upgrade() else {
        clear_popup_state(view, cx);
        return;
    };
    let early = controller.update(cx, |c, cx| c.take_pending_for_command(view, cx));
    clear_popup_state(view, cx);
    let Some(early) = early else {
        return;
    };
    if let Some(commit) = &early.commit {
        apply_commit_in(view, commit, cx);
    }
    for chunk in &early.typed {
        view.insert_typed(chunk, cx);
    }
    // The command that follows measures the text we just changed.
    view.sync_source_with_buffer(cx);
    // The edits' own change events must not read as a caret move.
    let state = view.buffer().map(|b| {
        let rope = b.read(cx).text().clone();
        let caret = edit::pos_to_offset(&rope, view.selection().head);
        (rope.version(), caret)
    });
    controller.update(cx, |c, cx| {
        c.last_key = state;
        cx.notify();
    });
}

/// Applies a commit to the view as one undo step: the edits (main and additional) in
/// one `apply_edits`, then every caret. Returns whether the buffer took it.
pub(super) fn apply_commit(
    code_view: &Entity<CodeView>,
    commit: &CompletionCommit,
    cx: &mut App,
) -> bool {
    code_view.update(cx, |v, cx| apply_commit_in(v, commit, cx))
}

/// [`apply_commit`] on a view that is already borrowed.
fn apply_commit_in(
    v: &mut CodeView,
    commit: &CompletionCommit,
    cx: &mut Context<CodeView>,
) -> bool {
    let edits: Vec<lector_buffer::TextEdit> = commit
        .edits
        .iter()
        .map(|e| lector_buffer::TextEdit {
            range: e.range.clone(),
            new_text: e.new_text.clone(),
        })
        .collect();
    let carets = commit.carets.clone();
    let primary = v.primary_index();
    v.edit(cx, move |b| {
        let tx = b.apply_edits(edits);
        let n = carets.len() as u64;
        // Same id scheme as the view's own conversion: the largest id is the primary caret.
        let sels = carets
            .iter()
            .enumerate()
            .map(|(ix, c)| lector_buffer::Selection {
                id: if ix == primary { n + 1 } else { ix as u64 + 1 },
                anchor: *c,
                head: *c,
                goal: None,
            })
            .collect();
        b.set_selections(sels);
        ((), tx)
    })
    .is_some()
}
