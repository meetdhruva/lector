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
//! The work is bounded by a [`Budget`] (states and time); over budget the
//! result is `None` and the caller falls back to the line diff.

use std::collections::{BinaryHeap, HashMap};
use std::hash::{BuildHasherDefault, Hasher};
use std::ops::Range;
use std::time::{Duration, Instant};

use lector_syntax::{Syntax, TextInput, TreeNode};

/// Limits of a structural diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Maximum number of graph states expanded.
    pub max_states: usize,
    /// Maximum wall-clock time.
    pub max_time: Duration,
    /// Trees with more nodes than this are refused outright.
    pub max_nodes: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_states: 400_000,
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

struct Solver<'a> {
    l: &'a Tree,
    r: &'a Tree,
    budget: &'a Budget,
    started: Instant,
    states: usize,
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

    /// Compares sibling lists `ls` / `rs` (children of `pl` / `pr`), trimming
    /// identical prefixes and suffixes. `None` when over budget.
    fn solve_level(&mut self, pl: u32, ls: &[u32], pr: u32, rs: &[u32]) -> Option<()> {
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
            return Some(());
        }
        if let ([x], [y]) = (lm, rm) {
            let (xi, yi) = (*x as usize, *y as usize);
            if self.l.is_list[xi] && self.r.is_list[yi] && self.l.key[xi] == self.r.key[yi] {
                let (cx, cy) = (self.l.children(*x), self.r.children(*y));
                return self.solve_level(*x, &cx, *y, &cy);
            }
        }
        self.dijkstra(pl, lm, pr, rm)
    }

    fn dijkstra(&mut self, pl: u32, lm: &[u32], pr: u32, rm: &[u32]) -> Option<()> {
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
            if self.states > self.budget.max_states
                || (self.states % 256 == 0 && self.started.elapsed() > self.budget.max_time)
            {
                return None;
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
            return None;
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
        Some(())
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

/// Structural diff of two parsed texts; `None` when over `budget` (the caller
/// should fall back to the line diff). `old_text`/`new_text` must be the texts
/// the syntaxes were parsed from.
pub fn diff<A: TextInput + ?Sized, B: TextInput + ?Sized>(
    old_syntax: &Syntax,
    old_text: &A,
    new_syntax: &Syntax,
    new_text: &B,
    budget: &Budget,
) -> Option<StructuralDiff> {
    let started = Instant::now();
    let l = Tree::build(old_syntax, old_text, budget.max_nodes)?;
    let r = Tree::build(new_syntax, new_text, budget.max_nodes)?;
    if l.hash[0] == r.hash[0] {
        return Some(StructuralDiff::default());
    }
    let mut solver = Solver {
        l: &l,
        r: &r,
        budget,
        started,
        states: 0,
        last_l: NONE,
        last_r: NONE,
        items: Vec::new(),
        novel_l: vec![false; l.hash.len()],
        novel_r: vec![false; r.hash.len()],
    };
    let (ls, rs) = (l.children(0), r.children(0));
    solver.solve_level(0, &ls, 0, &rs)?;

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
    Some(StructuralDiff {
        old: merge(old_marks, &old_full),
        new: merge(new_marks, &new_full),
        states: solver.states,
    })
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
mod tests {
    use super::*;
    use lector_syntax::LanguageRegistry;
    use proptest::prelude::*;

    fn run(lang: &str, old: &str, new: &str) -> Option<StructuralDiff> {
        let reg = LanguageRegistry::new();
        let l = reg.by_name(lang).unwrap();
        let (so, sn) = (
            Syntax::parse(&l, old).unwrap(),
            Syntax::parse(&l, new).unwrap(),
        );
        diff(&so, old, &sn, new, &Budget::default())
    }

    fn texts<'a>(src: &'a str, marks: &[Marked], mark: Mark) -> Vec<&'a str> {
        marks
            .iter()
            .filter(|m| m.mark == mark)
            .map(|m| &src[m.range.clone()])
            .collect()
    }

    #[test]
    fn identical_and_reformatted_code_has_no_changes() {
        let a = "fn add(a: i32, b: i32) -> i32 { let c = a + b; c * 2 }\n";
        let b = "fn add(\n    a: i32,\n    b: i32,\n) -> i32 {\n    let c = a + b;\n    c * 2\n}\n";
        assert!(run("rust", a, a).unwrap().is_empty());
        let d = run("rust", a, b).unwrap();
        // Only the trailing comma is new.
        assert_eq!(texts(b, &d.new, Mark::Added), vec![","], "{d:?}");
        assert!(d.old.is_empty(), "{d:?}");
    }

    #[test]
    fn rust_literal_change() {
        let (a, b) = ("fn f() { g(1, 2) }\n", "fn f() { g(1, 3) }\n");
        let d = run("rust", a, b).unwrap();
        assert_eq!(texts(a, &d.old, Mark::Changed), vec!["2"]);
        assert_eq!(texts(b, &d.new, Mark::Changed), vec!["3"]);
        assert_eq!(d.old.len(), 1);
    }

    #[test]
    fn rust_added_and_removed_statements() {
        let a = "fn f() {\n    one();\n    two();\n}\n";
        let b = "fn f() {\n    one();\n    inserted();\n    two();\n}\n";
        let d = run("rust", a, b).unwrap();
        assert!(d.old.is_empty(), "{d:?}");
        let added = texts(b, &d.new, Mark::Added);
        assert_eq!(added.len(), 1, "{d:?}");
        assert!(added[0].contains("inserted"), "{added:?}");
        let r = run("rust", b, a).unwrap();
        assert!(r.new.is_empty());
        assert_eq!(texts(b, &r.old, Mark::Removed).len(), 1);
    }

    #[test]
    fn moved_function() {
        let first = "fn first() { println!(\"first one\"); }\n";
        let second = "fn second(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n    let diff = a - b;\n    if sum > diff { sum * 2 } else { diff * 3 }\n}\n";
        let third = "fn third(items: &[u8]) -> usize {\n    let mut n = 0;\n    for i in items {\n        if *i > 3 { n += 1; }\n    }\n    n\n}\n";
        let a = format!("{first}{second}{third}");
        let b = format!("{second}{third}{first}");
        let d = run("rust", &a, &b).unwrap();
        assert!(!d.is_empty());
        assert!(
            d.old.iter().chain(&d.new).all(|m| m.mark == Mark::Moved),
            "{d:?}"
        );
        assert_eq!(texts(&a, &d.old, Mark::Moved), vec![first.trim_end()]);
        assert_eq!(texts(&b, &d.new, Mark::Moved), vec![first.trim_end()]);
    }

    #[test]
    fn renamed_identifier() {
        let a = "fn compute() { let total = 1; total }\n";
        let b = "fn compute() { let sum = 1; sum }\n";
        let d = run("rust", a, b).unwrap();
        assert_eq!(texts(a, &d.old, Mark::Changed), vec!["total", "total"]);
        assert_eq!(texts(b, &d.new, Mark::Changed), vec!["sum", "sum"]);
    }

    #[test]
    fn string_and_comment_replacement() {
        let a = "fn f() {\n    // old comment text\n    log(\"hello world\");\n}\n";
        let b = "fn f() {\n    // new comment text\n    log(\"hello there\");\n}\n";
        let d = run("rust", a, b).unwrap();
        let changed = texts(a, &d.old, Mark::Changed);
        assert_eq!(changed.len(), 2, "{d:?}");
        assert!(changed[0].contains("old comment") && changed[1].contains("hello world"));
    }

    #[test]
    fn wrapped_expression() {
        let a = "fn f() { compute(a, b) }\n";
        let b = "fn f() { Some(compute(a, b)) }\n";
        let d = run("rust", a, b).unwrap();
        assert!(d.old.is_empty(), "{d:?}");
        // `Some(` and `)` are new; `compute(a, b)` is untouched.
        let added = texts(b, &d.new, Mark::Added);
        assert!(added.iter().all(|t| !t.contains("compute")), "{added:?}");
        assert!(!added.is_empty());
    }

    #[test]
    fn typescript() {
        let a = "export function area(w: number, h: number): number {\n  return w * h;\n}\n";
        let b = "export function area(w: number, h: number, scale: number): number {\n  return w * h * scale;\n}\n";
        let d = run("typescript", a, b).unwrap();
        assert!(d.old.is_empty(), "{d:?}");
        let added = texts(b, &d.new, Mark::Added);
        assert!(added.iter().any(|t| t.contains("scale")), "{added:?}");
        let same = "const x = {a: 1, b: [1,2,3]};\n";
        let spaced = "const x = {\n  a: 1,\n  b: [1, 2, 3],\n};\n";
        let d = run("typescript", same, spaced).unwrap();
        assert!(d.old.is_empty());
    }

    #[test]
    fn python() {
        let a = "def f(x):\n    if x:\n        return 1\n    return 2\n";
        let b = "def f(x):\n    if x:\n        return 10\n    return 2\n";
        let d = run("python", a, b).unwrap();
        assert_eq!(texts(a, &d.old, Mark::Changed), vec!["1"]);
        assert_eq!(texts(b, &d.new, Mark::Changed), vec!["10"]);
        // Dedenting a statement out of the `if` is a structural change.
        let c = "def f(x):\n    if x:\n        return 1\n    return 2\n    return 3\n";
        let d = run("python", a, c).unwrap();
        assert!(!d.is_empty());
    }

    #[test]
    fn go_and_json() {
        let a = "package main\n\nfunc main() {\n\tfmt.Println(\"a\", 1)\n}\n";
        let b = "package main\n\nfunc main() {\n\tfmt.Println(\"a\", 2)\n}\n";
        let d = run("go", a, b).unwrap();
        assert_eq!(texts(a, &d.old, Mark::Changed), vec!["1"]);
        let ja = "{\"a\": 1, \"b\": [1, 2]}";
        let jb = "{\n  \"a\": 1,\n  \"b\": [1, 2, 3]\n}";
        let d = run("json", ja, jb).unwrap();
        assert!(d.old.is_empty(), "{d:?}");
        assert!(
            texts(jb, &d.new, Mark::Added)
                .iter()
                .any(|t| t.contains('3')),
            "{d:?}"
        );
    }

    #[test]
    fn empty_sides() {
        let d = run("rust", "", "fn a() {}\n").unwrap();
        assert!(d.old.is_empty());
        assert_eq!(texts("fn a() {}\n", &d.new, Mark::Added).len(), 1);
        let d = run("rust", "fn a() {}\n", "").unwrap();
        assert!(d.new.is_empty());
        assert!(!d.old.is_empty());
    }

    #[test]
    fn budget_exceeded_returns_none() {
        let old: String = (0..12)
            .map(|i| format!("fn f{i}() {{ a{i}(); b({i}); }}\n"))
            .collect();
        let new: String = (0..12)
            .map(|i| format!("fn g{i}() {{ c{i}(); d({}); }}\n", i + 1))
            .collect();
        let reg = LanguageRegistry::new();
        let l = reg.by_name("rust").unwrap();
        let (so, sn) = (
            Syntax::parse(&l, &old[..]).unwrap(),
            Syntax::parse(&l, &new[..]).unwrap(),
        );
        let tiny = Budget {
            max_states: 20,
            ..Budget::default()
        };
        assert!(diff(&so, &old[..], &sn, &new[..], &tiny).is_none());
        let nodes = Budget {
            max_nodes: 10,
            ..Budget::default()
        };
        assert!(diff(&so, &old[..], &sn, &new[..], &nodes).is_none());
        let generous = Budget {
            max_states: 50_000_000,
            max_time: Duration::from_secs(120),
            ..Budget::default()
        };
        assert!(diff(&so, &old[..], &sn, &new[..], &generous).is_some());
    }

    proptest! {
        /// Marks are in-bounds, sorted, disjoint; identical inputs give no marks; swapping
        /// the sides swaps added/removed.
        #[test]
        fn marks_are_well_formed(
            a in proptest::collection::vec(0u8..6, 0..12),
            b in proptest::collection::vec(0u8..6, 0..12),
        ) {
            let stmt = |n: u8| match n {
                0 => "let a = 1;",
                1 => "let b = foo(2, 3);",
                2 => "if x { y(); }",
                3 => "return bar;",
                4 => "baz.qux(1);",
                _ => "let c = [1, 2];",
            };
            let mk = |v: &[u8]| format!("fn f() {{\n{}\n}}\n", v.iter().map(|n| stmt(*n)).collect::<Vec<_>>().join("\n"));
            let (old, new) = (mk(&a), mk(&b));
            let d = run("rust", &old, &new).unwrap();
            for (marks, src) in [(&d.old, &old), (&d.new, &new)] {
                let mut prev = 0;
                for m in marks {
                    prop_assert!(m.range.start >= prev && m.range.start < m.range.end && m.range.end <= src.len());
                    prev = m.range.end;
                }
            }
            if a == b {
                prop_assert!(d.is_empty());
            }
            let r = run("rust", &new, &old).unwrap();
            let count = |marks: &[Marked], k: Mark| marks.iter().filter(|m| m.mark == k).count();
            prop_assert_eq!(count(&d.old, Mark::Removed) > 0, count(&r.new, Mark::Added) > 0);
        }
    }
}
