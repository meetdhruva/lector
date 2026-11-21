//! Disk side of a document: saving, reload, external change detection and conflicts.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lector_base::CancelToken;
use lector_buffer::save::{LineEnding as SaveLineEnding, SaveOptions};

use crate::documents::policy;
use crate::LineEnding;

use super::{
    canonical_key, is_editable_document, load_from_disk, new_state, BeforeSaveHook, DiskChange,
    DiskConflict, DocError, DocEvent, DocHandle, DocState, EditOrigin, Fx, Loaded, SaveOutcome,
    SaveRequest, Slot, Structure,
};

impl DocHandle {
    /// Saves (blocking): before-save hooks, cleanup edits, encoding/line
    /// endings kept, "changed on disk" check (unless `force`), atomic write.
    pub fn save(&self, req: SaveRequest) -> Result<SaveOutcome, DocError> {
        let _serial = self.shared.save_lock.lock();
        let (same_path, target) = {
            let st = self.shared.state.lock();
            if st.buffer.is_none() {
                return Err(DocError::NotEditable);
            }
            match &req.path {
                Some(p) if *p != st.path => (false, p.clone()),
                _ => (true, st.path.clone()),
            }
        };
        // 1. Hooks (format on save, ...).
        if req.run_hooks {
            let hooks: Vec<Arc<dyn BeforeSaveHook>> = self
                .shared
                .store
                .upgrade()
                .map(|s| s.hooks.read().iter().map(|(_, h)| h.clone()).collect())
                .unwrap_or_default();
            for hook in hooks {
                let snap = self.snapshot();
                match hook.before_save(&snap, &CancelToken::new()) {
                    Ok(edits) if !edits.is_empty() => {
                        match self.apply_edits_as(EditOrigin::SAVE, Some(snap.version), edits) {
                            Ok(_) | Err(DocError::VersionMismatch { .. }) => {}
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(_) => {}
                    Err(err) => tracing::warn!("before-save hook failed: {err:#}"),
                }
            }
        }
        // 2. Cleanup edits, encode.
        let mut fx = Fx::default();
        let (bytes, version, expected, dirty, deleted) = {
            let mut st = self.shared.state.lock();
            let line_ending = st.line_ending();
            let encoding = st.disk.encoding;
            let Some(buffer) = st.buffer.as_mut() else {
                return Err(DocError::NotEditable);
            };
            let edits = policy::cleanup_edits(
                buffer.text(),
                line_ending,
                req.trim_trailing_whitespace,
                req.ensure_final_newline,
            );
            if !edits.is_empty() {
                let tx = Arc::new(buffer.apply_edits(edits));
                if !tx.deltas.is_empty() {
                    self.record_edit(&mut st, EditOrigin::SAVE, tx, true, &mut fx);
                }
            }
            let opts = SaveOptions {
                encoding,
                line_ending: match line_ending {
                    LineEnding::CrLf => Some(SaveLineEnding::CrLf),
                    LineEnding::Lf => Some(SaveLineEnding::Lf),
                    _ => None,
                },
                trim_trailing_whitespace: false,
                ensure_final_newline: false,
            };
            let Some(buffer) = st.buffer.as_ref() else {
                return Err(DocError::NotEditable);
            };
            let bytes = lector_buffer::save::encode(buffer.text(), &opts);
            (
                bytes,
                buffer.version(),
                st.disk.stamp,
                st.dirty(),
                st.disk.deleted,
            )
        };
        self.run(fx);
        // A clean buffer of a deleted file is still written: Save recreates it.
        if same_path && !dirty && !req.force && !deleted && expected.is_some() {
            return Ok(SaveOutcome::Unchanged);
        }
        // 3. Conflict check + atomic write.
        if same_path && !req.force {
            if let Some(expected) = expected {
                if let Ok(now) = lector_fs::stamp(&target) {
                    if now != expected {
                        return Err(DocError::ChangedOnDisk { path: target });
                    }
                }
            }
        }
        match &req.confine {
            Some(confinement) => confinement
                .write(&target)?
                .write(&bytes, lector_fs::CreateDirs::No)?,
            None => lector_fs::write_atomic(&target, &bytes)?,
        }
        let new_stamp = lector_fs::stamp(&target).ok();
        // 4. Bookkeeping.
        let mut fx = Fx::default();
        {
            let mut st = self.shared.state.lock();
            st.disk.stamp = new_stamp;
            st.disk.conflict = None;
            st.disk.deleted = false;
            st.save_count += 1;
            if let Some(b) = st.buffer.as_mut() {
                if b.version() == version {
                    b.mark_saved();
                }
            }
            st.gutter_base = None;
            st.gutter.value = None;
            st.gutter.scheduled = None;
            st.blame.value = None;
            st.blame.scheduled = None;
            self.check_dirty(&mut st, &mut fx);
            if !same_path {
                self.move_to(&mut st, target, &mut fx);
            }
            fx.events.push(DocEvent::Saved { version });
            fx.events.push(DocEvent::DiskStateChanged);
        }
        self.run(fx);
        Ok(SaveOutcome::Saved)
    }

    fn move_to(&self, st: &mut DocState, path: PathBuf, fx: &mut Fx) {
        let old = std::mem::replace(&mut st.path, path.clone());
        if let Some(lsp) = st.project.lsp() {
            lsp.close_document(&old);
        }
        st.lsp_synced = None;
        let new_key = canonical_key(&path);
        if let Some(store) = self.shared.store.upgrade() {
            let mut docs = store.docs.lock();
            if docs.get(&st.key).is_some_and(|w| w.ptr_eq(&self.shared.me)) {
                docs.remove(&st.key);
            }
            docs.insert(new_key.clone(), self.shared.me.clone());
        }
        st.key = new_key;
        let rel = st.project.relative_path(&path);
        let languages = st.project.languages().clone();
        let doc = st.live.with_path(path.clone(), rel, &languages);
        let version = st.version();
        st.parse_gen += 1;
        st.parse_running = false;
        st.pending_deltas.clear();
        st.parsed = doc.syntax().cloned().map(|s| (s, version));
        st.structure = Slot {
            value: Some((
                version,
                Structure {
                    document: doc.clone(),
                    outline: doc.outline(),
                    folds: doc.fold_ranges(),
                },
            )),
            ..Slot::default()
        };
        st.live = doc;
        st.highlights = (version, HashMap::new());
        st.disk.stamp = lector_fs::stamp(&path).ok();
        self.lsp_job(st, Duration::ZERO, fx);
        fx.events.push(DocEvent::PathChanged { old, new: path });
    }

    /// The file changed on disk (watcher): same stamp → nothing (our own
    /// save); gone → deleted state; clean → reloaded in place (carets kept);
    /// dirty → a pending [`DiskConflict`]. Blocking (reads the file).
    pub fn check_disk(&self) -> Result<DiskChange, DocError> {
        self.reload_inner(false)
    }

    /// Discards unsaved changes and reloads from disk (Revert / "Reload" in
    /// a conflict). Blocking.
    pub fn reload(&self) -> Result<DiskChange, DocError> {
        self.reload_inner(true)
    }

    /// Keeps the buffer after an external change: the next save overwrites.
    pub fn keep_mine(&self) {
        let changed = {
            let mut st = self.shared.state.lock();
            match st.disk.conflict.take() {
                Some(conflict) => {
                    st.disk.stamp = conflict.stamp;
                    true
                }
                None => false,
            }
        };
        if changed {
            self.shared.subs.emit(vec![DocEvent::DiskStateChanged]);
        }
    }

    fn set_deleted(&self, deleted: bool) {
        let changed = {
            let mut st = self.shared.state.lock();
            std::mem::replace(&mut st.disk.deleted, deleted) != deleted
        };
        if changed {
            self.shared.subs.emit(vec![DocEvent::DiskStateChanged]);
        }
    }

    fn reload_inner(&self, discard: bool) -> Result<DiskChange, DocError> {
        let (project, path, known) = {
            let st = self.shared.state.lock();
            (st.project.clone(), st.path.clone(), st.disk.stamp)
        };
        match lector_fs::stamp(&path) {
            Ok(now) if !discard && known == Some(now) => {
                self.set_deleted(false);
                return Ok(DiskChange::Unchanged);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.set_deleted(true);
                return Ok(DiskChange::Deleted);
            }
            _ => {}
        }
        let loaded = load_from_disk(&project, &path, None)?;
        let mut fx = Fx::default();
        let change = {
            let mut st = self.shared.state.lock();
            if st.disk.deleted {
                st.disk.deleted = false;
                fx.events.push(DocEvent::DiskStateChanged);
            }
            self.apply_disk_version(&mut st, loaded, discard, &mut fx)
        };
        self.run(fx);
        Ok(change)
    }

    fn apply_disk_version(
        &self,
        st: &mut DocState,
        loaded: Loaded,
        discard: bool,
        fx: &mut Fx,
    ) -> DiskChange {
        let editable_now = is_editable_document(&loaded.doc);
        if st.buffer.is_none() || !editable_now {
            // Not editable (or became binary / huge): replace the snapshot.
            if st.dirty() && !discard {
                st.disk.conflict = Some(DiskConflict {
                    disk: loaded.doc,
                    stamp: loaded.stamp,
                });
                fx.events.push(DocEvent::DiskStateChanged);
                return DiskChange::Conflict;
            }
            let options = st
                .buffer
                .as_ref()
                .map(|b| b.options().clone())
                .unwrap_or_else(|| policy::buffer_options(&loaded.doc, &st.project.settings()));
            let path = st.path.clone();
            let key = st.key.clone();
            let project = st.project.clone();
            let mut fresh = new_state(&path, key, project, loaded, options);
            fresh.read_only = st.read_only;
            fresh.seq = st.seq + 1;
            // No history across a replacement: followers resynchronize.
            fresh.history.clear();
            fresh.parse_gen = st.parse_gen + 1;
            fresh.gutter_enabled = st.gutter_enabled;
            *st = fresh;
            fx.events.push(DocEvent::Replaced);
            fx.events.push(DocEvent::DiskStateChanged);
            return DiskChange::Reloaded;
        }
        let dirty = st.dirty();
        if dirty && !discard {
            st.disk.conflict = Some(DiskConflict {
                disk: loaded.doc,
                stamp: loaded.stamp,
            });
            fx.events.push(DocEvent::DiskStateChanged);
            return DiskChange::Conflict;
        }
        let Some(new_text) = loaded.doc.text().map(|t| t.to_string()) else {
            return DiskChange::Unchanged;
        };
        st.disk.conflict = None;
        st.disk.stamp = loaded.stamp;
        st.disk.encoding = loaded.encoding;
        let Some(buffer) = st.buffer.as_mut() else {
            return DiskChange::Unchanged;
        };
        let same = buffer.text().len() == new_text.len() && buffer.text().to_string() == new_text;
        if !same {
            let tx = Arc::new(buffer.replace_all(&new_text));
            if !tx.deltas.is_empty() {
                self.record_edit(st, EditOrigin::RELOAD, tx, false, fx);
            }
            // Adopt the full parse of the disk version.
            let version = st.version();
            st.parse_gen += 1;
            st.parse_running = false;
            st.pending_deltas.clear();
            st.parsed = loaded.doc.syntax().cloned().map(|s| (s, version));
            if let Some(text) = st.live.text().cloned() {
                st.live = loaded.doc.with_text(text, loaded.doc.syntax().cloned());
            }
            st.structure.value = Some((
                version,
                Structure {
                    document: loaded.doc.clone(),
                    outline: loaded.doc.outline(),
                    folds: loaded.doc.fold_ranges(),
                },
            ));
            st.structure.scheduled = None;
            st.structure.waiting_syntax = false;
            fx.events.push(DocEvent::Reloaded { version });
        }
        if let Some(b) = st.buffer.as_mut() {
            b.mark_saved();
        }
        st.gutter_base = None;
        st.blame.value = None;
        self.check_dirty(st, fx);
        fx.events.push(DocEvent::DiskStateChanged);
        DiskChange::Reloaded
    }
}
