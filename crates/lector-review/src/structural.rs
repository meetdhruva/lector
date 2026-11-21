//! Structural (syntax-tree) diff in the style of difftastic.
//!
//! Instead of comparing lines, the two syntax trees are compared. Reformatting
//! (line breaks, indentation of braces, spacing) produces no change at all;
//! only tokens whose structure or content differs are marked.
//!
//! # Algorithm
//!
//! Both trees are turned into lists and atoms: a node with children is a
//! *list* (its first/last children are its *delimiters* when they are brackets),
//! a leaf, string or comment is an *atom*. Every node gets a structural hash.
//!
//! 1. **Trim**: at every level, a common prefix and suffix of identical
//!    sibling subtrees are skipped; when exactly one node remains on each side
//!    and both are lists of the same kind, the comparison descends into them.
//! 2. **Shortest path**: what remains is the shortest path in the graph of
//!    states `(position in old tree, position in new tree, stack of entered
//!    lists)`, found with Dijkstra. Edges: skip identical subtrees (cheap), enter
//!    two lists of the same kind together (cheap), replace a string/comment
//!    (medium), or treat an atom / list opening as removed or added (expensive).
//!    A small penalty discourages matching nodes at different nesting depths.
//! 3. **Marks**: consecutive removed/added nodes between matched ones are
//!    `Changed` when both sides have some and `Removed`/`Added` otherwise.
//!    Novel subtrees that exist unchanged on the other side (and are not
//!    trivial) are `Moved`.
//!
//! # Scaling (v0.7)
//!
//! The shortest-path search is quadratic in the size of what it compares, so it
//! only ever sees small pieces:
//!
//! * **Anchors first.** At every nesting level the sibling subtrees of both
//!   sides are compared by their structural hash with a linear diff
//!   (histogram, via `lector_diff`). Hash-identical siblings are settled at
//!   once, however many there are; only the runs between them (the changed
//!   regions) go on, aligned to the smallest enclosing syntax nodes. The work
//!   therefore scales with the size of the change, not of the file.
//! * **Pairing.** A changed run with as many siblings on one side as on the
//!   other is paired position by position (lists of the same kind are entered
//!   and compared recursively, the rest goes to the search).
//! * **Budgets per region.** Every search has its own state cap
//!   ([`Budget::max_region_states`]); the whole diff has a state and a time
//!   cap. A region that exceeds a cap is *skipped*: it is reported in
//!   [`StructuralDiff::skipped`] and its marks come from the line diff refined
//!   with the word diff, so nothing is hidden and nothing is worse than the
//!   plain line diff. Only when no region could be solved at all (or the trees
//!   are too big) the result is `None`.
//! * **Cancellation** through a [`CancelToken`] (`None` result).

use std::collections::{BinaryHeap, HashMap};
use std::hash::{BuildHasherDefault, Hasher};
use std::ops::Range;
use std::time::{Duration, Instant};

use lector_base::CancelToken;
use lector_diff::{diff_lines, refine_inline, DiffOptions};
use lector_syntax::{Syntax, TextInput, TreeNode};

/// Limits of a structural diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Maximum number of graph states expanded, summed over all regions.
    pub max_states: usize,
    /// Maximum number of graph states one region may expand before it is skipped.
    pub max_region_states: usize,
    /// Maximum wall-clock time.
    pub max_time: Duration,
    /// Trees with more nodes than this are refused outright.
    pub max_nodes: usize,
}

/// Why a region was left to the line + word diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The region alone exceeded [`Budget::max_region_states`].
    RegionBudget,
    /// The state budget of the whole diff ran out.
    TotalBudget,
    /// The time budget of the whole diff ran out.
    Time,
}

/// A changed region the structural search did not solve. Its marks are those
/// of the line diff refined with the word diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRegion {
    /// Byte range in the old text (empty at the insertion point when the region only exists in the new text).
    pub old: Range<usize>,
    /// Byte range in the new text.
    pub new: Range<usize>,
    /// Why it was skipped.
    pub reason: SkipReason,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_states: 400_000,
            max_region_states: 60_000,
            max_time: Duration::from_millis(750),
            max_nodes: 1_000_000,
        }
    }
}

/// How a range of text changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Mark {
    /// Replaced by different content (on both sides).
    Changed,
    /// Only in the new text.
    Added,
    /// Only in the old text.
    Removed,
    /// Present on both sides but at a different place.
    Moved,
}

/// A marked byte range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marked {
    /// Byte range in the text of its side.
    pub range: Range<usize>,
    /// What happened to it.
    pub mark: Mark,
}

/// Result of [`diff`]: changed byte ranges on each side, sorted and disjoint.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StructuralDiff {
    /// Ranges of the old text (`Removed`, `Changed`, `Moved`).
    pub old: Vec<Marked>,
    /// Ranges of the new text (`Added`, `Changed`, `Moved`).
    pub new: Vec<Marked>,
    /// Graph states expanded (a measure of the work done).
    pub states: usize,
    /// Regions that fell back to the line + word diff; empty for a fully structural result.
    pub skipped: Vec<SkippedRegion>,
}

impl StructuralDiff {
    /// True when the trees are structurally identical.
    pub fn is_empty(&self) -> bool {
        self.old.is_empty() && self.new.is_empty()
    }
}

const NONE: u32 = u32::MAX;

// Edge costs.
const COST_UNCHANGED: u32 = 1;
const COST_ENTER_BOTH: u32 = 1;
const COST_REPLACE_ATOM: u32 = 150;
const COST_NOVEL: u32 = 300;
const COST_DEPTH: u32 = 40;
const MOVE_MIN_LEAVES: u32 = 3;
const MOVE_MIN_BYTES: usize = 12;
/// A run of paired lists with more leaves than this is paired without trying the search first.
const PAIR_FIRST_LEAVES: u32 = 24;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Plain,
    Comment,
    Text,
}

/// Lists and atoms of one syntax tree. Node 0 is a virtual root whose only
/// child is the real root.
struct Tree {
    hash: Vec<u64>,
    /// Hash of (kind, delimiters): lists with equal keys can be entered together.
    key: Vec<u64>,
    parent: Vec<u32>,
    first: Vec<u32>,
    next: Vec<u32>,
    depth: Vec<u32>,
    is_list: Vec<bool>,
    class: Vec<Class>,
    range: Vec<Range<usize>>,
    open: Vec<Option<Range<usize>>>,
    close: Vec<Option<Range<usize>>>,
    leaves: Vec<u32>,
}

fn mix(h: u64, v: u64) -> u64 {
    (h.rotate_left(5) ^ v).wrapping_mul(0x517c_c1b7_2722_0a95)
}

fn hash_str(s: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in s.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    h ^ (h >> 29)
}

fn is_open(s: &str) -> bool {
    !s.is_empty() && s.len() <= 2 && s.chars().all(|c| "([{<".contains(c))
}

fn is_close(s: &str) -> bool {
    !s.is_empty() && s.len() <= 2 && s.chars().all(|c| ")]}>".contains(c))
}

fn class_of(kind: &str) -> Class {
    if kind.contains("comment") {
        Class::Comment
    } else if (kind.contains("string") || kind == "char_literal" || kind.contains("regex"))
        && !kind.contains("interpolation")
        && !kind.contains("template_substitution")
    {
        Class::Text
    } else {
        Class::Plain
    }
}

impl Tree {
    fn build<T: TextInput + ?Sized>(syntax: &Syntax, text: &T, max_nodes: usize) -> Option<Tree> {
        let flat = syntax.flatten(max_nodes)?;
        let full = text.text_for_range(0..text.len());
        let src: &str = &full;
        let slice = |r: &Range<usize>| src.get(r.clone()).unwrap_or("");
        let n = flat.len() + 1;
        let mut t = Tree {
            hash: vec![0; n],
            key: vec![0; n],
            parent: vec![NONE; n],
            first: vec![NONE; n],
            next: vec![NONE; n],
            depth: vec![0; n],
            is_list: vec![true; n],
            class: vec![Class::Plain; n],
            range: vec![0..0; n],
            open: vec![None; n],
            close: vec![None; n],
            leaves: vec![0; n],
        };
        // Which flat nodes take part: not below an atom, and non-empty (leaves).
        let mut live = vec![false; flat.len()];
        let mut atom = vec![false; flat.len()];
        for (i, f) in flat.iter().enumerate() {
            let parent_ok = f
                .parent
                .is_none_or(|p| live[p as usize] && !atom[p as usize]);
            let is_atom = f.child_count == 0 || class_of(f.kind) != Class::Plain;
            atom[i] = is_atom;
            live[i] = parent_ok && !(f.child_count == 0 && f.range.is_empty());
        }
        let map = |i: usize| i as u32 + 1;
        // Children per node (live only), in order.
        let mut children: Vec<Vec<u32>> = vec![Vec::new(); n];
        for (i, f) in flat.iter().enumerate() {
            if !live[i] {
                continue;
            }
            let p = f.parent.map_or(0, |p| p + 1);
            children[p as usize].push(map(i));
        }
        for (i, f) in flat.iter().enumerate() {
            let id = map(i) as usize;
            t.range[id] = f.range.clone();
            t.is_list[id] = !atom[i];
            t.class[id] = class_of(f.kind);
            t.parent[id] = f.parent.map_or(0, |p| p + 1);
        }
        t.range[0] = 0..src.len();
        // Delimiters.
        let flat_of = |id: u32| -> &TreeNode<'_> { &flat[id as usize - 1] };
        for id in 1..n {
            if !t.is_list[id] || !live[id - 1] {
                continue;
            }
            let ch = &mut children[id];
            if ch.len() >= 2 {
                let (a, z) = (flat_of(ch[0]), flat_of(ch[ch.len() - 1]));
                if a.child_count == 0
                    && z.child_count == 0
                    && !a.named
                    && !z.named
                    && is_open(slice(&a.range))
                    && is_close(slice(&z.range))
                {
                    t.open[id] = Some(a.range.clone());
                    t.close[id] = Some(z.range.clone());
                    ch.pop();
                    ch.remove(0);
                }
            }
        }
        // Links and depth (ids are in pre-order: parents precede children).
        for (id, slot) in children.iter_mut().enumerate().take(n) {
            let kids = std::mem::take(slot);
            let mut prev = NONE;
            for &c in &kids {
                if prev == NONE {
                    t.first[id] = c;
                } else {
                    t.next[prev as usize] = c;
                }
                prev = c;
            }
            for &c in &kids {
                t.parent[c as usize] = id as u32;
            }
        }
        for id in 1..n {
            let p = t.parent[id];
            // Delimiter tokens and dead nodes keep depth 0 (never visited).
            t.depth[id] = if p == NONE {
                0
            } else {
                t.depth[p as usize] + 1
            };
        }
        // Hashes bottom-up (children have larger ids than their parent).
        for id in (0..n).rev() {
            if id != 0 && !live[id - 1] {
                continue;
            }
            if id != 0 && !t.is_list[id] {
                t.hash[id] = hash_str(slice(&t.range[id]));
                t.key[id] = t.hash[id];
                t.leaves[id] = 1;
                continue;
            }
            let (kind_id, open_h, close_h) = if id == 0 {
                (u16::MAX as u64, 0, 0)
            } else {
                (
                    flat[id - 1].kind_id as u64,
                    t.open[id].as_ref().map_or(0, |r| hash_str(slice(r))),
                    t.close[id].as_ref().map_or(0, |r| hash_str(slice(r))),
                )
            };
            t.key[id] = mix(mix(mix(0x1234, kind_id), open_h), close_h);
            let mut h = t.key[id];
            let mut leaves = 0;
            let mut c = t.first[id];
            let mut count = 0u64;
            while c != NONE {
                h = mix(h, t.hash[c as usize]);
                leaves += t.leaves[c as usize];
                count += 1;
                c = t.next[c as usize];
            }
            t.hash[id] = mix(h, count);
            t.leaves[id] = leaves + u32::from(t.open[id].is_some()) * 2;
        }
        Some(t)
    }

    fn children(&self, n: u32) -> Vec<u32> {
        let mut v = Vec::new();
        let mut c = self.first[n as usize];
        while c != NONE {
            v.push(c);
            c = self.next[c as usize];
        }
        v
    }
}

// ---------------------------------------------------------------------------
// Shortest path

#[derive(Default)]
struct IdHasher(u64);
impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = mix(self.0, b as u64);
        }
    }
    fn write_u32(&mut self, i: u32) {
        self.0 = mix(self.0, i as u64);
    }
}
type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<IdHasher>>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Edge {
    Start,
    Unchanged,
    EnterBoth(u32, u32),
    PopBoth(u32, u32),
    Replace(u32, u32),
    NovelAtomL(u32),
    NovelAtomR(u32),
    EnterNovelL(u32),
    EnterNovelR(u32),
    PopNovelL(u32),
    PopNovelR(u32),
}

struct Vertex {
    lc: u32,
    rc: u32,
    stack: u32,
    dist: u32,
    prev: u32,
    edge: Edge,
}

const ST_BOTH: u8 = 1;
const ST_LEFT: u8 = 2;
const ST_RIGHT: u8 = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Whole,
    Open,
    Close,
}

/// Why a search stopped early.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// The caller cancelled: the whole diff is abandoned.
    Cancelled,
    /// A region (or everything that follows) is left to the line + word diff.
    Skip(SkipReason),
}

struct Solver<'a> {
    l: &'a Tree,
    r: &'a Tree,
    budget: &'a Budget,
    cancel: &'a CancelToken,
    started: Instant,
    states: usize,
    /// `states` when the current search began.
    region_base: usize,
    /// Searches (and atom pairs) that completed.
    solved: usize,
    skipped: Vec<SkippedRegion>,
    /// Set once a whole-diff budget ran out: later regions are skipped at once.
    exhausted: Option<SkipReason>,
    last_l: u32,
    last_r: u32,
    /// Items to mark: (is_left, node, part, mark).
    items: Vec<(bool, u32, Part, Mark)>,
    novel_l: Vec<bool>,
    novel_r: Vec<bool>,
}

fn at(n: u32) -> u32 {
    n << 1
}
fn end(n: u32) -> u32 {
    (n << 1) | 1
}

impl Solver<'_> {
    fn after(&self, left: bool, n: u32) -> u32 {
        let (t, last) = if left {
            (self.l, self.last_l)
        } else {
            (self.r, self.last_r)
        };
        if n == last || t.next[n as usize] == NONE {
            end(t.parent[n as usize])
        } else {
            at(t.next[n as usize])
        }
    }

    fn first_child(&self, left: bool, n: u32) -> u32 {
        let t = if left { self.l } else { self.r };
        match t.first[n as usize] {
            NONE => end(n),
            c => at(c),
        }
    }

    fn cursor_depth(t: &Tree, c: u32) -> u32 {
        let n = c >> 1;
        if c & 1 == 0 {
            t.depth[n as usize]
        } else {
            t.depth[n as usize] + 1
        }
    }

    /// Compares sibling lists `ls` / `rs` (children of `pl` / `pr`): trims
    /// identical prefixes and suffixes, anchors the rest on identical
    /// siblings and solves the runs between the anchors one by one.
    fn solve_level(&mut self, pl: u32, ls: &[u32], pr: u32, rs: &[u32]) -> Result<(), Stop> {
        if self.cancel.is_cancelled() {
            return Err(Stop::Cancelled);
        }
        let max = ls.len().min(rs.len());
        let mut pre = 0;
        while pre < max && self.l.hash[ls[pre] as usize] == self.r.hash[rs[pre] as usize] {
            pre += 1;
        }
        let mut suf = 0;
        while suf < max - pre
            && self.l.hash[ls[ls.len() - 1 - suf] as usize]
                == self.r.hash[rs[rs.len() - 1 - suf] as usize]
        {
            suf += 1;
        }
        let (lm, rm) = (&ls[pre..ls.len() - suf], &rs[pre..rs.len() - suf]);
        if lm.is_empty() && rm.is_empty() {
            return Ok(());
        }
        if let ([x], [y]) = (lm, rm) {
            let (xi, yi) = (*x as usize, *y as usize);
            if self.l.is_list[xi] && self.r.is_list[yi] && self.l.key[xi] == self.r.key[yi] {
                let (cx, cy) = (self.l.children(*x), self.r.children(*y));
                return self.solve_level(*x, &cx, *y, &cy);
            }
        }
        let hunks = if lm.len() > 1 && rm.len() > 1 {
            let lh: Vec<u64> = lm.iter().map(|&n| self.l.hash[n as usize]).collect();
            let rh: Vec<u64> = rm.iter().map(|&n| self.r.hash[n as usize]).collect();
            sibling_hunks(&lh, &rh)
        } else {
            vec![(0..lm.len(), 0..rm.len())]
        };
        for (a, b) in hunks {
            let before_l = (pre + a.start).checked_sub(1).map(|k| ls[k]);
            let before_r = (pre + b.start).checked_sub(1).map(|k| rs[k]);
            self.solve_run(pl, &lm[a], before_l, pr, &rm[b], before_r)?;
        }
        Ok(())
    }

    /// Solves one run of siblings that has no identical sibling on the other
    /// side; a budget overrun skips just this run.
    fn solve_run(
        &mut self,
        pl: u32,
        lm: &[u32],
        before_l: Option<u32>,
        pr: u32,
        rm: &[u32],
        before_r: Option<u32>,
    ) -> Result<(), Stop> {
        // Pairing is an alternative to the search for runs whose siblings
        // correspond one to one: same number, all lists of the same kind
        // (e.g. a row of changed functions).
        let pairable = !lm.is_empty()
            && lm.len() == rm.len()
            && lm.iter().zip(rm).all(|(&x, &y)| {
                self.l.is_list[x as usize]
                    && self.r.is_list[y as usize]
                    && self.l.key[x as usize] == self.r.key[y as usize]
            });
        let size: u32 = lm
            .iter()
            .map(|&n| self.l.leaves[n as usize])
            .chain(rm.iter().map(|&n| self.r.leaves[n as usize]))
            .sum();
        let result = if let Some(reason) = self.exhausted {
            Err(Stop::Skip(reason))
        } else if pairable && size > PAIR_FIRST_LEAVES {
            self.solve_pairs(pl, lm, before_l, pr, rm, before_r)
        } else {
            match self.dijkstra(pl, lm, pr, rm) {
                Err(Stop::Skip(SkipReason::RegionBudget)) if pairable => {
                    self.solve_pairs(pl, lm, before_l, pr, rm, before_r)
                }
                other => other,
            }
        };
        match result {
            Err(Stop::Skip(reason)) => {
                self.skip_region(pl, lm, before_l, pr, rm, before_r, reason);
                Ok(())
            }
            other => other,
        }
    }

    /// Pairs the siblings of a run position by position.
    fn solve_pairs(
        &mut self,
        pl: u32,
        lm: &[u32],
        before_l: Option<u32>,
        pr: u32,
        rm: &[u32],
        before_r: Option<u32>,
    ) -> Result<(), Stop> {
        for (k, (&x, &y)) in lm.iter().zip(rm).enumerate() {
            let (bl, br) = (
                k.checked_sub(1).map(|k| lm[k]).or(before_l),
                k.checked_sub(1).map(|k| rm[k]).or(before_r),
            );
            self.solve_pair(pl, x, bl, pr, y, br)?;
        }
        Ok(())
    }

    /// One sibling against one sibling (inside a paired run).
    fn solve_pair(
        &mut self,
        pl: u32,
        x: u32,
        before_l: Option<u32>,
        pr: u32,
        y: u32,
        before_r: Option<u32>,
    ) -> Result<(), Stop> {
        let (xi, yi) = (x as usize, y as usize);
        if self.l.is_list[xi] == self.r.is_list[yi] && self.l.hash[xi] == self.r.hash[yi] {
            return Ok(());
        }
        if self.l.is_list[xi] && self.r.is_list[yi] && self.l.key[xi] == self.r.key[yi] {
            let (cx, cy) = (self.l.children(x), self.r.children(y));
            return self.solve_level(x, &cx, y, &cy);
        }
        // Atoms, or lists of different kinds: a search of its own (a pair
        // that exhausts the budget is skipped on its own).
        match self.exhausted {
            Some(reason) => {
                self.skip_region(pl, &[x], before_l, pr, &[y], before_r, reason);
                Ok(())
            }
            None => match self.dijkstra(pl, &[x], pr, &[y]) {
                Err(Stop::Skip(reason)) => {
                    self.skip_region(pl, &[x], before_l, pr, &[y], before_r, reason);
                    Ok(())
                }
                other => other,
            },
        }
    }

    /// Records a skipped region (and stops all further searching when a
    /// whole-diff budget is what ran out).
    #[allow(clippy::too_many_arguments)]
    fn skip_region(
        &mut self,
        pl: u32,
        lm: &[u32],
        before_l: Option<u32>,
        pr: u32,
        rm: &[u32],
        before_r: Option<u32>,
        reason: SkipReason,
    ) {
        if reason != SkipReason::RegionBudget {
            self.exhausted = Some(reason);
        }
        let span = |t: &Tree, parent: u32, nodes: &[u32], before: Option<u32>| -> Range<usize> {
            match (nodes.first(), nodes.last()) {
                (Some(&a), Some(&z)) => t.range[a as usize].start..t.range[z as usize].end,
                _ => {
                    let at = match before {
                        Some(b) => t.range[b as usize].end,
                        None => t.open[parent as usize]
                            .as_ref()
                            .map_or(t.range[parent as usize].start, |o| o.end),
                    };
                    at..at
                }
            }
        };
        let region = SkippedRegion {
            old: span(self.l, pl, lm, before_l),
            new: span(self.r, pr, rm, before_r),
            reason,
        };
        self.skipped.push(region);
    }

    fn dijkstra(&mut self, pl: u32, lm: &[u32], pr: u32, rm: &[u32]) -> Result<(), Stop> {
        self.region_base = self.states;
        self.last_l = lm.last().copied().unwrap_or(NONE);
        self.last_r = rm.last().copied().unwrap_or(NONE);
        let start_l = lm.first().map_or(end(pl), |&n| at(n));
        let start_r = rm.first().map_or(end(pr), |&n| at(n));
        let goal = (end(pl), end(pr), 0u32);

        let mut stacks: Vec<(u32, u8)> = vec![(0, 0)];
        let mut stack_ix: FastMap<(u32, u8), u32> = FastMap::default();
        let mut verts: Vec<Vertex> = vec![Vertex {
            lc: start_l,
            rc: start_r,
            stack: 0,
            dist: 0,
            prev: NONE,
            edge: Edge::Start,
        }];
        let mut seen: FastMap<(u32, u32, u32), u32> = FastMap::default();
        seen.insert((start_l, start_r, 0), 0);
        let mut heap: BinaryHeap<std::cmp::Reverse<(u32, u32)>> = BinaryHeap::new();
        heap.push(std::cmp::Reverse((0, 0)));
        let mut done: Vec<bool> = vec![false];

        let mut found = NONE;
        while let Some(std::cmp::Reverse((dist, vi))) = heap.pop() {
            let v = &verts[vi as usize];
            if done[vi as usize] || dist > v.dist {
                continue;
            }
            done[vi as usize] = true;
            let (lc, rc, st) = (v.lc, v.rc, v.stack);
            if (lc, rc, st) == goal {
                found = vi;
                break;
            }
            self.states += 1;
            if self.states > self.budget.max_states {
                return Err(Stop::Skip(SkipReason::TotalBudget));
            }
            if self.states - self.region_base > self.budget.max_region_states {
                return Err(Stop::Skip(SkipReason::RegionBudget));
            }
            if self.states % 256 == 0 {
                if self.cancel.is_cancelled() {
                    return Err(Stop::Cancelled);
                }
                if self.started.elapsed() > self.budget.max_time {
                    return Err(Stop::Skip(SkipReason::Time));
                }
            }
            let top = stacks[st as usize].1;
            let mut moves: Vec<(u32, u32, u32, u32, Edge)> = Vec::with_capacity(6); // (cost, lc, rc, stack, edge)
            let push_stack = |stacks: &mut Vec<(u32, u8)>,
                              ix: &mut FastMap<(u32, u8), u32>,
                              parent: u32,
                              kind: u8|
             -> u32 {
                *ix.entry((parent, kind)).or_insert_with(|| {
                    stacks.push((parent, kind));
                    stacks.len() as u32 - 1
                })
            };
            let (l_at, r_at) = (lc & 1 == 0, rc & 1 == 0);
            let (ln, rn) = (lc >> 1, rc >> 1);
            let parent_stack = stacks[st as usize].0;

            // Pops.
            if !l_at && !r_at && top == ST_BOTH {
                moves.push((
                    0,
                    self.after(true, ln),
                    self.after(false, rn),
                    parent_stack,
                    Edge::PopBoth(ln, rn),
                ));
            }
            if !l_at && top == ST_LEFT {
                moves.push((
                    0,
                    self.after(true, ln),
                    rc,
                    parent_stack,
                    Edge::PopNovelL(ln),
                ));
            }
            if !r_at && top == ST_RIGHT {
                moves.push((
                    0,
                    lc,
                    self.after(false, rn),
                    parent_stack,
                    Edge::PopNovelR(rn),
                ));
            }

            let novel_l = |me: &Self,
                           moves: &mut Vec<(u32, u32, u32, u32, Edge)>,
                           stacks: &mut Vec<(u32, u8)>,
                           ix: &mut FastMap<(u32, u8), u32>| {
                if me.l.is_list[ln as usize] {
                    let s = push_stack(stacks, ix, st, ST_LEFT);
                    moves.push((
                        COST_NOVEL,
                        me.first_child(true, ln),
                        rc,
                        s,
                        Edge::EnterNovelL(ln),
                    ));
                } else {
                    moves.push((COST_NOVEL, me.after(true, ln), rc, st, Edge::NovelAtomL(ln)));
                }
            };
            let novel_r = |me: &Self,
                           moves: &mut Vec<(u32, u32, u32, u32, Edge)>,
                           stacks: &mut Vec<(u32, u8)>,
                           ix: &mut FastMap<(u32, u8), u32>| {
                if me.r.is_list[rn as usize] {
                    let s = push_stack(stacks, ix, st, ST_RIGHT);
                    moves.push((
                        COST_NOVEL,
                        lc,
                        me.first_child(false, rn),
                        s,
                        Edge::EnterNovelR(rn),
                    ));
                } else {
                    moves.push((
                        COST_NOVEL,
                        lc,
                        me.after(false, rn),
                        st,
                        Edge::NovelAtomR(rn),
                    ));
                }
            };

            match (l_at, r_at) {
                (true, true) => {
                    let (xi, yi) = (ln as usize, rn as usize);
                    if self.l.hash[xi] == self.r.hash[yi]
                        && self.l.is_list[xi] == self.r.is_list[yi]
                    {
                        let dd = Self::cursor_depth(self.l, lc)
                            .abs_diff(Self::cursor_depth(self.r, rc))
                            .min(4);
                        moves.push((
                            COST_UNCHANGED + COST_DEPTH * dd,
                            self.after(true, ln),
                            self.after(false, rn),
                            st,
                            Edge::Unchanged,
                        ));
                    } else {
                        if self.l.is_list[xi]
                            && self.r.is_list[yi]
                            && self.l.key[xi] == self.r.key[yi]
                        {
                            let s = push_stack(&mut stacks, &mut stack_ix, st, ST_BOTH);
                            moves.push((
                                COST_ENTER_BOTH,
                                self.first_child(true, ln),
                                self.first_child(false, rn),
                                s,
                                Edge::EnterBoth(ln, rn),
                            ));
                        }
                        if !self.l.is_list[xi]
                            && !self.r.is_list[yi]
                            && self.l.class[xi] != Class::Plain
                            && self.l.class[xi] == self.r.class[yi]
                        {
                            moves.push((
                                COST_REPLACE_ATOM,
                                self.after(true, ln),
                                self.after(false, rn),
                                st,
                                Edge::Replace(ln, rn),
                            ));
                        }
                        novel_l(self, &mut moves, &mut stacks, &mut stack_ix);
                        novel_r(self, &mut moves, &mut stacks, &mut stack_ix);
                    }
                }
                (true, false) => novel_l(self, &mut moves, &mut stacks, &mut stack_ix),
                (false, true) => novel_r(self, &mut moves, &mut stacks, &mut stack_ix),
                (false, false) => {}
            }

            for (cost, nlc, nrc, nst, edge) in moves {
                let nd = dist + cost;
                let key = (nlc, nrc, nst);
                match seen.get(&key) {
                    Some(&ix) => {
                        let ex = &mut verts[ix as usize];
                        if nd < ex.dist {
                            ex.dist = nd;
                            ex.prev = vi;
                            ex.edge = edge;
                            heap.push(std::cmp::Reverse((nd, ix)));
                        }
                    }
                    None => {
                        let ix = verts.len() as u32;
                        verts.push(Vertex {
                            lc: nlc,
                            rc: nrc,
                            stack: nst,
                            dist: nd,
                            prev: vi,
                            edge,
                        });
                        done.push(false);
                        seen.insert(key, ix);
                        heap.push(std::cmp::Reverse((nd, ix)));
                    }
                }
            }
        }
        if found == NONE {
            // Unreachable goal cannot happen for well-formed trees.
            return Err(Stop::Skip(SkipReason::RegionBudget));
        }
        // Reconstruct the path.
        let mut path = Vec::new();
        let mut cur = found;
        while cur != NONE {
            path.push(verts[cur as usize].edge);
            cur = verts[cur as usize].prev;
        }
        path.reverse();
        self.collect_marks(&path);
        self.solved += 1;
        Ok(())
    }

    fn collect_marks(&mut self, path: &[Edge]) {
        let mut run_l: Vec<(u32, Part)> = Vec::new();
        let mut run_r: Vec<(u32, Part)> = Vec::new();
        fn flush(me: &mut Solver<'_>, run_l: &mut Vec<(u32, Part)>, run_r: &mut Vec<(u32, Part)>) {
            let both = !run_l.is_empty() && !run_r.is_empty();
            for (n, p) in run_l.drain(..) {
                me.items
                    .push((true, n, p, if both { Mark::Changed } else { Mark::Removed }));
            }
            for (n, p) in run_r.drain(..) {
                me.items
                    .push((false, n, p, if both { Mark::Changed } else { Mark::Added }));
            }
        }
        for e in path {
            match *e {
                Edge::Start => {}
                Edge::Unchanged | Edge::EnterBoth(..) | Edge::PopBoth(..) => {
                    flush(self, &mut run_l, &mut run_r)
                }
                Edge::Replace(l, r) => {
                    run_l.push((l, Part::Whole));
                    run_r.push((r, Part::Whole));
                }
                Edge::NovelAtomL(n) => {
                    self.novel_l[n as usize] = true;
                    run_l.push((n, Part::Whole));
                }
                Edge::NovelAtomR(n) => {
                    self.novel_r[n as usize] = true;
                    run_r.push((n, Part::Whole));
                }
                Edge::EnterNovelL(n) => {
                    self.novel_l[n as usize] = true;
                    run_l.push((n, Part::Open));
                }
                Edge::EnterNovelR(n) => {
                    self.novel_r[n as usize] = true;
                    run_r.push((n, Part::Open));
                }
                Edge::PopNovelL(n) => run_l.push((n, Part::Close)),
                Edge::PopNovelR(n) => run_r.push((n, Part::Close)),
            }
        }
        flush(self, &mut run_l, &mut run_r);
    }
}

/// Structural diff of two parsed texts; `None` when the trees are too big,
/// when no region could be solved within `budget` (the caller should fall back
/// to the line diff) or when `cancel` fired. Regions that exceed a budget on
/// their own are reported in [`StructuralDiff::skipped`] and marked from the
/// line + word diff. `old_text`/`new_text` must be the texts the syntaxes were
/// parsed from.
pub fn diff_cancellable<A: TextInput + ?Sized, B: TextInput + ?Sized>(
    old_syntax: &Syntax,
    old_text: &A,
    new_syntax: &Syntax,
    new_text: &B,
    budget: &Budget,
    cancel: &CancelToken,
) -> Option<StructuralDiff> {
    let started = Instant::now();
    let l = Tree::build(old_syntax, old_text, budget.max_nodes)?;
    if cancel.is_cancelled() {
        return None;
    }
    let r = Tree::build(new_syntax, new_text, budget.max_nodes)?;
    if l.hash[0] == r.hash[0] {
        return Some(StructuralDiff::default());
    }
    let mut solver = Solver {
        l: &l,
        r: &r,
        budget,
        cancel,
        started,
        states: 0,
        region_base: 0,
        solved: 0,
        skipped: Vec::new(),
        exhausted: None,
        last_l: NONE,
        last_r: NONE,
        items: Vec::new(),
        novel_l: vec![false; l.hash.len()],
        novel_r: vec![false; r.hash.len()],
    };
    let (ls, rs) = (l.children(0), r.children(0));
    match solver.solve_level(0, &ls, 0, &rs) {
        Ok(()) => {}
        Err(_) => return None,
    }
    if !solver.skipped.is_empty() && solver.solved == 0 {
        return None;
    }

    // Moves: maximal fully-novel subtrees that exist on both sides.
    let fully = |t: &Tree, novel: &[bool]| -> Vec<bool> {
        let mut f = vec![false; t.hash.len()];
        for id in (1..t.hash.len()).rev() {
            if !novel[id] {
                continue;
            }
            if !t.is_list[id] {
                f[id] = true;
                continue;
            }
            let mut all = true;
            let mut c = t.first[id];
            while c != NONE {
                all &= f[c as usize];
                c = t.next[c as usize];
            }
            f[id] = all;
        }
        f
    };
    let (fl, fr) = (fully(&l, &solver.novel_l), fully(&r, &solver.novel_r));
    let roots = |t: &Tree, f: &[bool]| -> Vec<u32> {
        (1..t.hash.len() as u32)
            .filter(|&n| {
                let p = t.parent[n as usize];
                f[n as usize]
                    && (p == 0 || p == NONE || !f[p as usize])
                    && t.leaves[n as usize] >= MOVE_MIN_LEAVES
                    && t.range[n as usize].len() >= MOVE_MIN_BYTES
            })
            .collect()
    };
    let (mut roots_l, roots_r) = (roots(&l, &fl), roots(&r, &fr));
    roots_l.sort_by_key(|&n| std::cmp::Reverse(l.leaves[n as usize]));
    let mut by_hash: HashMap<u64, Vec<u32>> = HashMap::new();
    for &n in &roots_r {
        by_hash.entry(r.hash[n as usize]).or_default().push(n);
    }
    let mut moved_l = vec![false; l.hash.len()];
    let mut moved_r = vec![false; r.hash.len()];
    for &n in &roots_l {
        if let Some(m) = by_hash.get_mut(&l.hash[n as usize]).and_then(|v| v.pop()) {
            moved_l[n as usize] = true;
            moved_r[m as usize] = true;
        }
    }
    let under_moved = |t: &Tree, moved: &[bool], mut n: u32| -> bool {
        while n != 0 && n != NONE {
            if moved[n as usize] {
                return true;
            }
            n = t.parent[n as usize];
        }
        false
    };

    let old_full = old_text.text_for_range(0..old_text.len());
    let new_full = new_text.text_for_range(0..new_text.len());
    let mut old_marks: Vec<Marked> = Vec::new();
    let mut new_marks: Vec<Marked> = Vec::new();
    let part_range = |t: &Tree, n: u32, p: Part| -> Option<Range<usize>> {
        match p {
            Part::Whole => Some(t.range[n as usize].clone()),
            Part::Open => t.open[n as usize].clone(),
            Part::Close => t.close[n as usize].clone(),
        }
    };
    for &(left, n, part, mark) in &solver.items {
        let (t, moved, marks) = if left {
            (&l, &moved_l, &mut old_marks)
        } else {
            (&r, &moved_r, &mut new_marks)
        };
        if under_moved(t, moved, n) {
            continue;
        }
        if let Some(range) = part_range(t, n, part) {
            marks.push(Marked { range, mark });
        }
    }
    for (t, moved, marks) in [
        (&l, &moved_l, &mut old_marks),
        (&r, &moved_r, &mut new_marks),
    ] {
        for n in 1..t.hash.len() as u32 {
            if moved[n as usize] {
                marks.push(Marked {
                    range: t.range[n as usize].clone(),
                    mark: Mark::Moved,
                });
            }
        }
    }
    for region in &solver.skipped {
        let (o, n) = fallback_marks(&old_full, &new_full, region);
        old_marks.extend(o);
        new_marks.extend(n);
    }
    Some(StructuralDiff {
        old: merge(old_marks, &old_full),
        new: merge(new_marks, &new_full),
        states: solver.states,
        skipped: solver.skipped,
    })
}

/// [`diff_cancellable`] without cancellation.
pub fn diff<A: TextInput + ?Sized, B: TextInput + ?Sized>(
    old_syntax: &Syntax,
    old_text: &A,
    new_syntax: &Syntax,
    new_text: &B,
    budget: &Budget,
) -> Option<StructuralDiff> {
    diff_cancellable(
        old_syntax,
        old_text,
        new_syntax,
        new_text,
        budget,
        &CancelToken::new(),
    )
}

/// Hunks (index ranges in `lh` / `rh`) of a linear diff of two hash sequences;
/// everything outside the hunks is identical.
fn sibling_hunks(lh: &[u64], rh: &[u64]) -> Vec<(Range<usize>, Range<usize>)> {
    use std::fmt::Write;
    let encode = |h: &[u64]| {
        let mut s = String::with_capacity(h.len() * 17);
        for x in h {
            let _ = writeln!(s, "{x:016x}");
        }
        s
    };
    let d = diff_lines(&encode(lh), &encode(rh), &DiffOptions::default());
    d.hunks
        .iter()
        .map(|h| {
            (
                (h.old.start as usize).min(lh.len())..(h.old.end as usize).min(lh.len()),
                (h.new.start as usize).min(rh.len())..(h.new.end as usize).min(rh.len()),
            )
        })
        .filter(|(a, b)| !(a.is_empty() && b.is_empty()))
        .collect()
}

/// Marks of a skipped region from the line diff of its two texts, refined
/// with the word diff inside changed line pairs.
fn fallback_marks(
    old_src: &str,
    new_src: &str,
    region: &SkippedRegion,
) -> (Vec<Marked>, Vec<Marked>) {
    let o = old_src.get(region.old.clone()).unwrap_or("");
    let n = new_src.get(region.new.clone()).unwrap_or("");
    let d = diff_lines(o, n, &DiffOptions::default());
    let starts = |s: &str| -> Vec<usize> {
        let mut v = vec![0];
        v.extend(s.match_indices('\n').map(|(i, _)| i + 1));
        v
    };
    let (os, ns) = (starts(o), starts(n));
    // Byte range of a line range, without the final line break.
    let span = |s: &str, st: &[usize], lines: &Range<u32>| -> Range<usize> {
        let a = st.get(lines.start as usize).copied().unwrap_or(s.len());
        let mut z = st.get(lines.end as usize).copied().unwrap_or(s.len());
        if z > a && s.as_bytes().get(z - 1) == Some(&b'\n') {
            z -= 1;
        }
        a.min(z)..z
    };
    let (mut old_marks, mut new_marks) = (Vec::new(), Vec::new());
    for h in &d.hunks {
        let (or, nr) = (span(o, &os, &h.old), span(n, &ns, &h.new));
        let (ot, nt) = (&o[or.clone()], &n[nr.clone()]);
        let blank = |t: &str| t.chars().all(char::is_whitespace);
        match (blank(ot), blank(nt)) {
            (false, false) => {
                let ic = refine_inline(ot, nt);
                for r in ic.old {
                    old_marks.push(Marked {
                        range: region.old.start + or.start + r.start
                            ..region.old.start + or.start + r.end,
                        mark: Mark::Changed,
                    });
                }
                for r in ic.new {
                    new_marks.push(Marked {
                        range: region.new.start + nr.start + r.start
                            ..region.new.start + nr.start + r.end,
                        mark: Mark::Changed,
                    });
                }
            }
            (false, true) => old_marks.push(Marked {
                range: region.old.start + or.start..region.old.start + or.end,
                mark: Mark::Removed,
            }),
            (true, false) => new_marks.push(Marked {
                range: region.new.start + nr.start..region.new.start + nr.end,
                mark: Mark::Added,
            }),
            (true, true) => {}
        }
    }
    (old_marks, new_marks)
}

/// Sorts marks and merges neighbours with the same mark separated only by whitespace.
fn merge(mut marks: Vec<Marked>, src: &str) -> Vec<Marked> {
    marks.sort_by(|a, b| {
        a.range
            .start
            .cmp(&b.range.start)
            .then(b.range.end.cmp(&a.range.end))
    });
    let mut out: Vec<Marked> = Vec::new();
    for m in marks {
        if let Some(last) = out.last_mut() {
            if m.range.start < last.range.end {
                // Nested or overlapping: keep the outer mark, extend if needed.
                if m.range.end > last.range.end && m.mark == last.mark {
                    last.range.end = m.range.end;
                }
                continue;
            }
            if last.mark == m.mark
                && src
                    .get(last.range.end..m.range.start)
                    .is_some_and(|gap| gap.chars().all(char::is_whitespace))
            {
                last.range.end = m.range.end;
                continue;
            }
        }
        out.push(m);
    }
    out
}

#[cfg(test)]
mod tests;
