//! Undo/redo history built from rope snapshots.
//!
//! Each step keeps the text and selections before and after the step. Rope
//! clones share structure, so a step costs `O(edit size + log n)` memory, not
//! `O(file size)`; consecutive steps share the middle snapshot. The history is
//! bounded by [`MAX_STEPS`] and by [`MAX_COST`] (total stored selections and
//! deltas), whichever is hit first; the oldest steps are dropped.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use lector_text::Rope;

use crate::engine::EditKind;
use crate::selection::Selection;
use crate::EditDelta;

/// Maximum number of undo steps kept.
pub(crate) const MAX_STEPS: usize = 10_000;
/// Maximum total of stored selections and deltas over all steps.
pub(crate) const MAX_COST: usize = 1_000_000;

/// A coalesced typing run stops growing once its cost reaches this (about
/// 5000 keystrokes with one caret), so one step never holds unbounded data.
const MAX_STEP_COST: usize = 20_000;

#[derive(Clone)]
pub(crate) struct Step {
    pub before_text: Rope,
    pub before_sels: Vec<Selection>,
    pub before_newest: u64,
    pub after_text: Rope,
    pub after_sels: Vec<Selection>,
    pub after_newest: u64,
    /// Sequentially valid deltas that turn `before_text` into `after_text`.
    pub deltas: Vec<EditDelta>,
}

impl Step {
    fn cost(&self) -> usize {
        // A delta also stands for the rope chunk it dirtied (~128 bytes).
        self.before_sels.len() + self.after_sels.len() + self.deltas.len() * 4 + 1
    }
}

#[derive(Clone)]
struct LastEdit {
    kind: EditKind,
    at: Instant,
    version: u64,
    sels: Vec<(usize, usize)>,
}

fn key(sels: &[Selection]) -> Vec<(usize, usize)> {
    sels.iter().map(|s| (s.anchor, s.head)).collect()
}

#[derive(Clone)]
pub(crate) struct History {
    undo: VecDeque<Step>,
    redo: Vec<Step>,
    cost: usize,
    txn_depth: u32,
    txn_open: bool,
    last: Option<LastEdit>,
    pub window: Duration,
}

impl History {
    pub(crate) fn new() -> History {
        History {
            undo: VecDeque::new(),
            redo: Vec::new(),
            cost: 0,
            txn_depth: 0,
            txn_open: false,
            last: None,
            window: Duration::from_secs(1),
        }
    }

    pub(crate) fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub(crate) fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub(crate) fn undo_len(&self) -> usize {
        self.undo.len()
    }

    /// Stop merging the next edit into the previous step.
    pub(crate) fn break_coalescing(&mut self) {
        self.last = None;
    }

    /// Selections moved without a text change as part of typing (typing over a
    /// closer): keep the typing run going from the new positions.
    pub(crate) fn retarget(&mut self, sels: &[Selection]) {
        if let Some(l) = self.last.as_mut() {
            l.sels = key(sels);
            if let Some(top) = self.undo.back_mut() {
                let old = top.cost();
                top.after_sels = sels.to_vec();
                let new = top.cost();
                self.cost = self.cost + new - old;
            }
        }
    }

    pub(crate) fn begin(&mut self) {
        self.txn_depth += 1;
        self.last = None;
    }

    pub(crate) fn end(&mut self) {
        if self.txn_depth > 0 {
            self.txn_depth -= 1;
            if self.txn_depth == 0 {
                self.txn_open = false;
            }
        }
        self.last = None;
    }

    /// Closes any explicit transaction (undo/redo inside one ends it).
    pub(crate) fn close_all(&mut self) {
        self.txn_depth = 0;
        self.txn_open = false;
        self.last = None;
    }

    pub(crate) fn record(&mut self, step: Step, kind: EditKind) {
        let now = Instant::now();
        for s in self.redo.drain(..) {
            self.cost -= s.cost();
        }
        if self.txn_depth > 0 {
            if self.txn_open && self.extend_top(&step) {
                return;
            }
            self.txn_open = true;
            self.last = None;
            self.push(step);
            return;
        }
        let can_merge = kind.coalescible()
            && !self.window.is_zero()
            && self
                .undo
                .back()
                .is_some_and(|top| top.cost() < MAX_STEP_COST)
            && self.last.as_ref().is_some_and(|l| {
                l.kind == kind
                    && now.duration_since(l.at) <= self.window
                    && l.version == step.before_text.version()
                    && l.sels == key(&step.before_sels)
            });
        let version = step.after_text.version();
        let sels = key(&step.after_sels);
        if !(can_merge && self.extend_top(&step)) {
            self.push(step);
        }
        self.last = Some(LastEdit {
            kind,
            at: now,
            version,
            sels,
        });
    }

    fn extend_top(&mut self, step: &Step) -> bool {
        let Some(top) = self.undo.back_mut() else {
            return false;
        };
        let old_cost = top.cost();
        top.after_text = step.after_text.clone();
        top.after_sels = step.after_sels.clone();
        top.after_newest = step.after_newest;
        top.deltas.extend(step.deltas.iter().copied());
        let new_cost = top.cost();
        self.cost = self.cost + new_cost - old_cost;
        self.evict();
        true
    }

    fn push(&mut self, step: Step) {
        self.cost += step.cost();
        self.undo.push_back(step);
        self.evict();
    }

    fn evict(&mut self) {
        while self.undo.len() > MAX_STEPS || (self.cost > MAX_COST && self.undo.len() > 1) {
            if let Some(s) = self.undo.pop_front() {
                self.cost -= s.cost();
            }
        }
    }

    pub(crate) fn pop_undo(&mut self) -> Option<Step> {
        let s = self.undo.pop_back()?;
        self.cost -= s.cost();
        Some(s)
    }

    pub(crate) fn push_redo(&mut self, s: Step) {
        self.cost += s.cost();
        self.redo.push(s);
    }

    pub(crate) fn pop_redo(&mut self) -> Option<Step> {
        let s = self.redo.pop()?;
        self.cost -= s.cost();
        Some(s)
    }

    pub(crate) fn push_undo_keep_redo(&mut self, s: Step) {
        self.push(s);
    }
}
