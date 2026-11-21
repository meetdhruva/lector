//! Editing a document through its buffer: transactions, history, undo and edit bookkeeping.

use std::collections::HashMap;
use std::sync::Arc;

use lector_buffer::{Buffer, BufferOptions, TextEdit, Transaction};
use lector_text::Text;

use crate::documents::derive::{self, DocDiagnostic};

use super::{
    history_since, BufferView, DocError, DocEvent, DocHandle, DocState, EditOrigin, EditResult, Fx,
    HistoryEntry, HISTORY_LEN,
};

impl DocHandle {
    /// Runs a buffer operation as `origin`. `since` is the caller's last seen
    /// seq: edits made by others after it are returned in
    /// [`EditResult::catch_up`] (use `u64::MAX` to skip). Fails when read-only
    /// or not editable.
    pub fn edit_from<R>(
        &self,
        origin: EditOrigin,
        since: u64,
        f: impl FnOnce(&mut Buffer) -> (R, Transaction),
    ) -> Result<EditResult<R>, DocError> {
        let mut fx = Fx::default();
        let result = {
            let mut st = self.shared.state.lock();
            if st.read_only {
                return Err(DocError::ReadOnly);
            }
            let (catch_up, gap) = if since == u64::MAX {
                (Vec::new(), false)
            } else {
                history_since(&st, since)
            };
            let Some(buffer) = st.buffer.as_mut() else {
                return Err(DocError::NotEditable);
            };
            let (value, tx) = f(buffer);
            let tx = Arc::new(tx);
            if !tx.deltas.is_empty() {
                self.record_edit(&mut st, origin, tx.clone(), true, &mut fx);
            }
            self.check_dirty(&mut st, &mut fx);
            let view = st
                .buffer
                .as_ref()
                .map(|b| BufferView::of(b, st.seq))
                .ok_or(DocError::NotEditable)?;
            EditResult {
                value,
                transaction: tx,
                catch_up,
                gap,
                view,
            }
        };
        self.run(fx);
        Ok(result)
    }

    /// [`DocHandle::edit_from`] for programmatic edits.
    pub fn edit<R>(
        &self,
        f: impl FnOnce(&mut Buffer) -> (R, Transaction),
    ) -> Result<(R, Arc<Transaction>), DocError> {
        self.edit_from(EditOrigin::EXTERNAL, u64::MAX, f)
            .map(|r| (r.value, r.transaction))
    }

    /// Runs a buffer operation that does not change the text (selections,
    /// options, transactions grouping, copy). Works while read-only.
    pub fn with_buffer_mut<R>(&self, f: impl FnOnce(&mut Buffer) -> R) -> Option<(R, BufferView)> {
        let mut fx = Fx::default();
        let out = {
            let mut st = self.shared.state.lock();
            let seq = st.seq;
            let buffer = st.buffer.as_mut()?;
            let before = buffer.version();
            let r = f(buffer);
            debug_assert_eq!(before, buffer.version(), "with_buffer_mut changed the text");
            let view = BufferView::of(buffer, seq);
            self.check_dirty(&mut st, &mut fx);
            (r, view)
        };
        self.run(fx);
        Some(out)
    }

    /// Applies edits (offsets into the current text) as one undo step.
    pub fn apply_edits(&self, edits: Vec<TextEdit>) -> Result<Arc<Transaction>, DocError> {
        self.apply_edits_as(EditOrigin::EXTERNAL, None, edits)
    }

    /// Applies edits only if the text is still at `version`.
    pub fn apply_edits_at(
        &self,
        version: u64,
        edits: Vec<TextEdit>,
    ) -> Result<Arc<Transaction>, DocError> {
        self.apply_edits_as(EditOrigin::EXTERNAL, Some(version), edits)
    }

    /// Applies edits as `origin`, optionally only at `expected` version.
    pub fn apply_edits_as(
        &self,
        origin: EditOrigin,
        expected: Option<u64>,
        edits: Vec<TextEdit>,
    ) -> Result<Arc<Transaction>, DocError> {
        let r = self.edit_from(origin, u64::MAX, move |b| {
            if let Some(expected) = expected {
                if b.version() != expected {
                    return (Err(b.version()), Transaction::default());
                }
            }
            (Ok(()), b.apply_edits(edits))
        })?;
        match r.value {
            Ok(()) => Ok(r.transaction),
            Err(actual) => Err(DocError::VersionMismatch {
                expected: expected.unwrap_or_default(),
                actual,
            }),
        }
    }

    /// Replaces the whole text (one undo step; carets kept near their place).
    pub fn replace_all(&self, text: &str) -> Result<Arc<Transaction>, DocError> {
        self.edit(|b| ((), b.replace_all(text))).map(|(_, tx)| tx)
    }

    /// Undo.
    pub fn undo(&self) -> Result<Arc<Transaction>, DocError> {
        self.edit(|b| ((), b.undo())).map(|(_, tx)| tx)
    }

    /// Redo.
    pub fn redo(&self) -> Result<Arc<Transaction>, DocError> {
        self.edit(|b| ((), b.redo())).map(|(_, tx)| tx)
    }

    /// Replaces the buffer options.
    pub fn set_options(&self, options: BufferOptions) {
        let _ = self.with_buffer_mut(|b| b.set_options(options));
    }

    /// Locks or unlocks edits.
    pub fn set_read_only(&self, read_only: bool) {
        let changed = {
            let mut st = self.shared.state.lock();
            std::mem::replace(&mut st.read_only, read_only) != read_only
        };
        if changed {
            self.shared
                .subs
                .emit(vec![DocEvent::ReadOnlyChanged { read_only }]);
        }
    }

    // ----- internals: edit bookkeeping ---------------------------------

    pub(super) fn record_edit(
        &self,
        st: &mut DocState,
        origin: EditOrigin,
        tx: Arc<Transaction>,
        reparse: bool,
        fx: &mut Fx,
    ) {
        let Some(buffer) = &st.buffer else { return };
        let rope = buffer.text().clone();
        let version = buffer.version();
        st.seq += 1;
        let seq = st.seq;
        st.history.push_back(HistoryEntry {
            seq,
            origin,
            version,
            text: rope.clone(),
            transaction: tx.clone(),
        });
        while st.history.len() > HISTORY_LEN {
            st.history.pop_front();
        }
        st.live = st.live.with_text(Text::from(rope), None);
        st.highlights = (version, HashMap::new());
        if !st.diagnostics.is_empty() {
            st.diagnostics = Arc::new(
                st.diagnostics
                    .iter()
                    .map(|d| DocDiagnostic {
                        range: derive::map_range(&d.range, &tx.deltas),
                        ..d.clone()
                    })
                    .collect(),
            );
            fx.events.push(DocEvent::DiagnosticsChanged);
        }
        if reparse {
            st.pending_deltas.extend_from_slice(&tx.deltas);
            self.schedule_reparse(st, fx);
        }
        fx.events.push(DocEvent::Edited {
            origin,
            seq,
            version,
            transaction: tx,
        });
        let delays = self.delays();
        self.lsp_job(st, delays.lsp, fx);
    }

    pub(super) fn check_dirty(&self, st: &mut DocState, fx: &mut Fx) {
        let dirty = st.dirty();
        if dirty != st.was_dirty {
            st.was_dirty = dirty;
            fx.events.push(DocEvent::DirtyChanged { dirty });
        }
    }
}
