//! The [`Buffer`]: text, selections, history and the plumbing every
//! operation goes through.

use std::collections::HashSet;
use std::time::Duration;

use lector_text::{Bias, Rope};
use parking_lot::Mutex;

use crate::engine::{build_deltas, build_rope, Change, EditKind, Mapper, NewSel, Plan, Pos, Side};
use crate::history::{History, Step};
use crate::selection::{normalize, Selection};
use crate::text_util::{detect_eol, Eol};
use crate::{BufferOptions, EditDelta, TextEdit, Transaction};

#[derive(Default)]
struct Caches {
    /// `(text version, eol)` of the last line terminator detection.
    eol: Option<(u64, Option<Eol>)>,
    /// Text of the last copy/cut of empty selections (whole-line clip).
    line_clip: Option<String>,
}

/// A running "add next occurrence" command that started from a caret: its
/// query matches whole words only, for as long as the text and the newest
/// selection stay as the session left them.
#[derive(Clone, Debug)]
pub(crate) struct OccurrenceSession {
    pub version: u64,
    pub query: String,
}

/// A closing bracket or quote that Lector inserted while typing the opener.
/// `start..end` is the text typed between the two; the closer sits at `end`.
/// Typing the closer with a caret at `end` skips over it; the entry is dropped
/// as soon as no caret is inside `start..=end` or the closer changes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AutoCloser {
    pub start: usize,
    pub end: usize,
    pub ch: char,
}

/// An editable text buffer with multi-cursor selections and undo history.
///
/// `Clone` is cheap (the rope is shared); the clone has an independent history.
pub struct Buffer {
    pub(crate) text: Rope,
    pub(crate) options: BufferOptions,
    pub(crate) sels: Vec<Selection>,
    next_id: u64,
    pub(crate) newest: u64,
    pub(crate) history: History,
    saved_version: u64,
    caches: Mutex<Caches>,
    pub(crate) occurrence: Option<OccurrenceSession>,
    /// Auto-inserted closers still eligible for type-over, sorted by `end`.
    pub(crate) closers: Vec<AutoCloser>,
}

impl Clone for Buffer {
    fn clone(&self) -> Buffer {
        let c = self.caches.lock();
        Buffer {
            text: self.text.clone(),
            options: self.options.clone(),
            sels: self.sels.clone(),
            next_id: self.next_id,
            newest: self.newest,
            history: self.history.clone(),
            saved_version: self.saved_version,
            caches: Mutex::new(Caches {
                eol: c.eol,
                line_clip: c.line_clip.clone(),
            }),
            occurrence: self.occurrence.clone(),
            closers: self.closers.clone(),
        }
    }
}

impl std::fmt::Debug for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Buffer")
            .field("len", &self.text.len())
            .field("selections", &self.sels)
            .field("dirty", &self.is_dirty())
            .finish()
    }
}

impl Buffer {
    /// A buffer over `text` with one caret at offset 0.
    pub fn new(text: Rope, options: BufferOptions) -> Buffer {
        let saved_version = text.version();
        Buffer {
            text,
            options,
            sels: vec![Selection::caret(1, 0)],
            next_id: 2,
            newest: 1,
            history: History::new(),
            saved_version,
            caches: Mutex::new(Caches::default()),
            occurrence: None,
            closers: Vec::new(),
        }
    }

    /// The current text snapshot.
    pub fn text(&self) -> &Rope {
        &self.text
    }

    /// A process-unique id of the current content (the rope version): changes
    /// on every edit and returns to an earlier value when undo restores that
    /// earlier snapshot.
    pub fn version(&self) -> u64 {
        self.text.version()
    }

    /// True when the content differs from the last [`Buffer::mark_saved`] (or load).
    pub fn is_dirty(&self) -> bool {
        self.text.version() != self.saved_version
    }

    /// Records the current content as the saved state.
    pub fn mark_saved(&mut self) {
        self.saved_version = self.text.version();
    }

    /// The editing options.
    pub fn options(&self) -> &BufferOptions {
        &self.options
    }

    /// Replaces the editing options.
    pub fn set_options(&mut self, o: BufferOptions) {
        self.options = o;
    }

    /// The selections: sorted, non-overlapping, never empty.
    pub fn selections(&self) -> &[Selection] {
        &self.sels
    }

    /// The most recently added selection (the "primary" one).
    pub fn newest_selection(&self) -> Selection {
        self.sels
            .iter()
            .find(|s| s.id == self.newest)
            .or_else(|| self.sels.last())
            .copied()
            .unwrap_or_else(|| Selection::caret(self.newest, 0))
    }

    /// True when [`Buffer::undo`] would do something.
    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    /// True when [`Buffer::redo`] would do something.
    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// Number of undo steps currently stored.
    pub fn undo_depth(&self) -> usize {
        self.history.undo_len()
    }

    /// Typing coalescing window (default 1 s; zero disables coalescing).
    pub fn set_coalesce_window(&mut self, window: Duration) {
        self.history.window = window;
    }

    /// Ends the current typing run so the next edit starts a new undo step.
    pub fn break_undo_group(&mut self) {
        self.history.break_coalescing();
    }

    /// The text covered by the selections, joined by the document's line ending.
    pub fn selected_text(&self) -> String {
        let eol = self.eol().as_str();
        let mut out = String::new();
        let mut first = true;
        for s in &self.sels {
            if s.is_empty() {
                continue;
            }
            if !first {
                out.push_str(eol);
            }
            first = false;
            out.push_str(&self.text.text_for_range(s.range()));
        }
        out
    }

    /// The document's line terminator: that of its first line break, else `\n`.
    pub(crate) fn eol(&self) -> Eol {
        self.eol_detected().unwrap_or(Eol::Lf)
    }

    /// The first line terminator of the text, `None` when it has no line break.
    pub(crate) fn eol_detected(&self) -> Option<Eol> {
        let v = self.text.version();
        let mut c = self.caches.lock();
        if let Some((cv, e)) = c.eol {
            if cv == v {
                return e;
            }
        }
        let e = detect_eol(&self.text);
        c.eol = Some((v, e));
        e
    }

    pub(crate) fn set_line_clip(&self, clip: Option<String>) {
        self.caches.lock().line_clip = clip;
    }

    pub(crate) fn line_clip(&self) -> Option<String> {
        self.caches.lock().line_clip.clone()
    }

    pub(crate) fn new_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Replaces the selections. Offsets are clipped to grapheme boundaries,
    /// sorted and merged; ids that are 0 or duplicated are reassigned; the
    /// last selection given becomes the newest.
    pub fn set_selections(&mut self, mut sels: Vec<Selection>) {
        self.history.break_coalescing();
        if sels.is_empty() {
            sels.push(Selection::caret(0, 0));
        }
        let mut used = HashSet::with_capacity(sels.len());
        for s in &mut sels {
            if s.id == 0 || !used.insert(s.id) {
                s.id = self.new_id();
                used.insert(s.id);
            }
            self.next_id = self.next_id.max(s.id + 1);
        }
        let newest = sels.last().map_or(1, |s| s.id);
        self.newest = newest;
        self.install_selections(sels);
    }

    /// Normalizes `sels` against the current text and installs them.
    pub(crate) fn install_selections(&mut self, sels: Vec<Selection>) {
        let mut n = normalize(&self.text, sels, self.newest);
        if n.is_empty() {
            n.push(Selection::caret(self.newest, 0));
        }
        if !n.iter().any(|s| s.id == self.newest) {
            self.newest = n.last().map_or(self.newest, |s| s.id);
        }
        self.sels = n;
        self.prune_closers();
    }

    /// Forgets auto-inserted closers that no caret is inside of any more.
    fn prune_closers(&mut self) {
        if self.closers.is_empty() {
            return;
        }
        let sels = &self.sels;
        // Heads of sorted, disjoint selections are sorted too.
        self.closers.retain(|c| {
            let first = sels.partition_point(|s| s.head < c.start);
            sels.get(first).is_some_and(|s| s.head <= c.end)
        });
    }

    /// Whether a closer `ch` that Lector inserted sits right at `offset` and
    /// the caret is still inside the pair.
    pub(crate) fn has_auto_closer(&self, offset: usize, ch: char) -> bool {
        let first = self.closers.partition_point(|c| c.end < offset);
        self.closers[first..]
            .iter()
            .take_while(|c| c.end == offset)
            .any(|c| c.ch == ch)
    }

    /// Replaces the selections with a single caret at `offset`.
    pub fn set_cursor(&mut self, offset: usize) {
        let id = self.new_id();
        self.set_selections(vec![Selection::caret(id, offset)]);
    }

    /// Applies a plan: text, deltas, selections, history.
    pub(crate) fn apply_plan(&mut self, plan: Plan) -> Transaction {
        self.apply_plan_closing(plan, Vec::new())
    }

    /// Like [`Buffer::apply_plan`]; `new_closers` are auto-inserted closing
    /// characters, given as the position (in the new text) of the closer.
    pub(crate) fn apply_plan_closing(
        &mut self,
        plan: Plan,
        new_closers: Vec<(Pos, char)>,
    ) -> Transaction {
        let Plan {
            changes,
            sels,
            kind,
        } = plan;
        // Replacing text by itself changes nothing: no new version, no undo
        // step; the selections still move as an edit would move them.
        if changes.iter().all(|c| is_noop_change(&self.text, c)) {
            let resolved =
                Self::resolve_sels(&mut self.next_id, &Mapper::new(&changes, &self.text), &sels);
            if kind == EditKind::Typing {
                self.install_selections(resolved);
                self.history.retarget(&self.sels);
            } else {
                self.history.break_coalescing();
                self.install_selections(resolved);
            }
            return Transaction::default();
        }
        debug_assert!(changes.windows(2).all(|w| w[0].end <= w[1].start));
        let old = self.text.clone();
        let new = build_rope(&old, &changes);
        let deltas = build_deltas(&old, &changes);
        let mapper = Mapper::new(&changes, &new);
        let resolved = Self::resolve_sels(&mut self.next_id, &mapper, &sels);
        self.closers = self.mapped_closers(&mapper, &new, &new_closers);
        let before_sels = std::mem::take(&mut self.sels);
        let before_newest = self.newest;
        self.text = new;
        self.install_selections(resolved);
        self.history.record(
            Step {
                before_text: old,
                before_sels,
                before_newest,
                after_text: self.text.clone(),
                after_sels: self.sels.clone(),
                after_newest: self.newest,
                deltas: deltas.clone(),
            },
            kind,
        );
        Transaction { deltas }
    }

    /// The tracked closers mapped through `mapper` plus `added` ones; entries
    /// whose closing character is no longer there are dropped.
    fn mapped_closers(
        &self,
        mapper: &Mapper<'_>,
        new: &Rope,
        added: &[(Pos, char)],
    ) -> Vec<AutoCloser> {
        let mut out: Vec<AutoCloser> = self
            .closers
            .iter()
            .map(|c| AutoCloser {
                start: mapper.resolve(Pos::Old(c.start, Side::Before)),
                end: mapper.resolve(Pos::Old(c.end, Side::After)),
                ch: c.ch,
            })
            .collect();
        for &(pos, ch) in added {
            let at = mapper.resolve(pos);
            out.push(AutoCloser {
                start: at,
                end: at,
                ch,
            });
        }
        out.retain(|c| c.start <= c.end && new.chars_at(c.end).next() == Some(c.ch));
        out.sort_by_key(|c| c.end);
        out
    }

    fn resolve_sels(next_id: &mut u64, mapper: &Mapper<'_>, specs: &[NewSel]) -> Vec<Selection> {
        let mut out = Vec::with_capacity(specs.len());
        for spec in specs {
            let id = match spec.id {
                Some(id) => id,
                None => {
                    let id = *next_id;
                    *next_id += 1;
                    id
                }
            };
            out.push(Selection {
                id,
                anchor: mapper.resolve(spec.anchor),
                head: mapper.resolve(spec.head),
                goal: spec.goal,
            });
        }
        out
    }

    /// Specs that keep every selection where it is (mapped through the changes).
    pub(crate) fn identity_sels(&self) -> Vec<NewSel> {
        self.sels
            .iter()
            .map(|s| {
                let (a, h) = side_pair(s);
                NewSel {
                    id: Some(s.id),
                    anchor: Pos::Old(s.anchor, a),
                    head: Pos::Old(s.head, h),
                    goal: None,
                }
            })
            .collect()
    }

    // ---- transactions & history -------------------------------------------

    /// Starts grouping every following edit into one undo step. Nestable.
    pub fn begin_transaction(&mut self) {
        self.history.begin();
    }

    /// Ends the innermost [`Buffer::begin_transaction`] group.
    pub fn end_transaction(&mut self) {
        self.history.end();
    }

    /// Undoes the last step, restoring text and selections.
    pub fn undo(&mut self) -> Transaction {
        self.history.close_all();
        let Some(step) = self.history.pop_undo() else {
            return Transaction::default();
        };
        let deltas = inverse_deltas(&step.deltas);
        self.closers.clear();
        self.text = step.before_text.clone();
        self.sels = step.before_sels.clone();
        self.newest = step.before_newest;
        self.history.push_redo(step);
        Transaction { deltas }
    }

    /// Re-applies the last undone step.
    pub fn redo(&mut self) -> Transaction {
        self.history.close_all();
        let Some(step) = self.history.pop_redo() else {
            return Transaction::default();
        };
        let deltas = step.deltas.clone();
        self.closers.clear();
        self.text = step.after_text.clone();
        self.sels = step.after_sels.clone();
        self.newest = step.after_newest;
        self.history.push_undo_keep_redo(step);
        Transaction { deltas }
    }

    // ---- external edits ------------------------------------------------------

    /// Applies edits from outside (LSP formatting, rename, code actions) as one
    /// undo step. Offsets refer to the current text. Edits are clipped to char
    /// boundaries, sorted, shrunk to their minimal differing part; no-ops and
    /// edits overlapping an earlier one are dropped. Selections are mapped
    /// through the edits.
    pub fn apply_edits(&mut self, edits: Vec<TextEdit>) -> Transaction {
        let len = self.text.len();
        let mut list: Vec<(usize, usize, String)> = edits
            .into_iter()
            .map(|e| {
                let start = self.text.clip_offset(e.range.start.min(len), Bias::Left);
                let end = self
                    .text
                    .clip_offset(e.range.end.clamp(e.range.start.min(len), len), Bias::Left)
                    .max(start);
                (start, end, e.new_text)
            })
            .collect();
        list.sort_by_key(|(s, e, _)| (*s, *e));
        let mut changes: Vec<Change> = Vec::with_capacity(list.len());
        let mut floor = 0;
        for (start, end, text) in list {
            if start < floor {
                continue;
            }
            let old_text = self.text.text_for_range(start..end);
            let (p, s) = common_affixes(&old_text, &text);
            let change = Change {
                start: start + p,
                end: end - s,
                text: text[p..text.len() - s].to_string(),
            };
            floor = end;
            if change.start == change.end && change.text.is_empty() {
                continue;
            }
            changes.push(change);
        }
        if changes.is_empty() {
            return Transaction::default();
        }
        let sels = self.identity_sels();
        self.apply_plan(Plan {
            changes,
            sels,
            kind: EditKind::Other,
        })
    }

    /// Replaces the whole text (file reload) as one undo step, keeping each
    /// caret at its offset when the change is elsewhere and at the same
    /// line/column otherwise.
    pub fn replace_all(&mut self, text: &str) -> Transaction {
        let old = self.text.to_string();
        if old == text {
            return Transaction::default();
        }
        let (mut p, mut s) = common_affixes(&old, text);
        // Never split a CRLF pair: keep both bytes inside the changed region.
        if p > 0 && old.as_bytes()[p - 1] == b'\r' && old.as_bytes().get(p) == Some(&b'\n') {
            p -= 1;
        }
        if s > 0 && p + s <= old.len() && old.as_bytes()[old.len() - s] == b'\n' {
            let before = old.len() - s;
            if before > 0 && old.as_bytes()[before - 1] == b'\r' {
                s -= 1;
            }
        }
        let change = Change {
            start: p,
            end: old.len() - s,
            text: text[p..text.len() - s].to_string(),
        };
        let delta = text.len() as isize - old.len() as isize;
        let old_rope = self.text.clone();
        let (changed_start, changed_end) = (change.start, change.end);
        let specs: Vec<NewSel> = self
            .sels
            .iter()
            .map(|s| {
                let map = |off: usize| -> Pos {
                    if off <= changed_start {
                        Pos::Old(off, Side::Before)
                    } else if off >= changed_end {
                        Pos::Old((off as isize + delta).max(0) as usize, Side::Before)
                    } else {
                        let row = crate::text_util::row_of(&old_rope, off);
                        Pos::RowCol(row, crate::text_util::col_chars(&old_rope, off))
                    }
                };
                NewSel {
                    id: Some(s.id),
                    anchor: map(s.anchor),
                    head: map(s.head),
                    goal: s.goal,
                }
            })
            .collect();
        // `Pos::Old` was pre-shifted above, so resolve against an identity mapper.
        let new = build_rope(&self.text, std::slice::from_ref(&change));
        let deltas = build_deltas(&self.text, std::slice::from_ref(&change));
        let identity: [Change; 0] = [];
        let resolved = Self::resolve_sels(&mut self.next_id, &Mapper::new(&identity, &new), &specs);
        let before_sels = std::mem::take(&mut self.sels);
        let before_newest = self.newest;
        let old_text = std::mem::replace(&mut self.text, new);
        self.closers.clear();
        self.install_selections(resolved);
        self.history.record(
            Step {
                before_text: old_text,
                before_sels,
                before_newest,
                after_text: self.text.clone(),
                after_sels: self.sels.clone(),
                after_newest: self.newest,
                deltas: deltas.clone(),
            },
            EditKind::Other,
        );
        Transaction { deltas }
    }
}

/// True when applying `change` would leave the text as it is.
fn is_noop_change(old: &Rope, change: &Change) -> bool {
    if change.start == change.end {
        return change.text.is_empty();
    }
    change.text.len() == change.end - change.start
        && old
            .chunks_in_range(change.start..change.end)
            .flat_map(str::bytes)
            .eq(change.text.bytes())
}

/// Sticky sides for the (anchor, head) of a selection: the earlier end stays
/// before insertions at its offset, the later end moves after them.
pub(crate) fn side_pair(s: &Selection) -> (Side, Side) {
    if s.is_empty() {
        (Side::After, Side::After)
    } else if s.reversed() {
        (Side::After, Side::Before)
    } else {
        (Side::Before, Side::After)
    }
}

fn inverse_deltas(deltas: &[EditDelta]) -> Vec<EditDelta> {
    deltas
        .iter()
        .rev()
        .map(|d| EditDelta {
            start_byte: d.start_byte,
            old_end_byte: d.new_end_byte,
            new_end_byte: d.old_end_byte,
            start: d.start,
            old_end: d.new_end,
            new_end: d.old_end,
        })
        .collect()
}

/// Lengths (bytes, on char boundaries) of the common prefix and suffix of `a`
/// and `b`; together they never exceed the shorter string.
pub(crate) fn common_affixes(a: &str, b: &str) -> (usize, usize) {
    let mut p = 0;
    for ((i, ca), cb) in a.char_indices().zip(b.chars()) {
        if ca != cb {
            p = i;
            return finish_suffix(a, b, p);
        }
        p = i + ca.len_utf8();
    }
    finish_suffix(a, b, p)
}

fn finish_suffix(a: &str, b: &str, p: usize) -> (usize, usize) {
    let max = a.len().min(b.len()) - p.min(a.len().min(b.len()));
    let mut s = 0;
    for (ca, cb) in a.chars().rev().zip(b.chars().rev()) {
        if ca != cb || s + ca.len_utf8() > max {
            break;
        }
        s += ca.len_utf8();
    }
    (p, s)
}
