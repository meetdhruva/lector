//! A generic, copy-on-write B+tree that maintains *summaries* of its items.
//!
//! This is a small, self-contained take on the `SumTree` from the Zed editor.
//! Every leaf stores a run of [`Item`]s together with each item's
//! [`Summary`]; every internal node stores the summaries of its children.
//! The summary of a node is the "sum" (via [`Summary::add_summary`]) of all
//! the items below it, in order.
//!
//! A [`Dimension`] is a value that can be *accumulated* from summaries — for
//! example a byte offset, a line/column [`Point`](crate::Point), or a count of
//! some item property. A [`Cursor`] walks the tree tracking one dimension and
//! can [`seek`](Cursor::seek) to any target in `O(log n)` by skipping whole
//! subtrees whose summarised extent lies before the target.
//!
//! Dimensions compose: `(D1, D2)` and `(D1, D2, D3)` are dimensions whenever
//! their components are, and a cursor over `(D1, D2)` can seek by a `D1`
//! target while reporting both coordinates. This is how conversions such as
//! *byte offset → point* are done in a single descent.
//!
//! Nodes are reference counted ([`Arc`]); cloning a tree is `O(1)` and edits
//! copy only the path from the root to the changed leaf. Trees are therefore
//! cheap immutable snapshots that can be shared across threads.
//!
//! The structure is intentionally generic: Lector uses it for rope chunks,
//! and later for folds, decorations, diff hunks and blame ranges.
//!
//! # Example
//! ```
//! use lector_text::sum_tree::{Bias, Dimension, Item, SumTree, Summary};
//!
//! #[derive(Clone, Debug)]
//! struct Run(usize);
//!
//! #[derive(Clone, Debug, Default)]
//! struct RunSummary { total: usize, count: usize }
//!
//! impl Summary for RunSummary {
//!     fn add_summary(&mut self, other: &Self) {
//!         self.total += other.total;
//!         self.count += other.count;
//!     }
//! }
//!
//! impl Item for Run {
//!     type Summary = RunSummary;
//!     fn summary(&self) -> RunSummary { RunSummary { total: self.0, count: 1 } }
//! }
//!
//! #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
//! struct Total(usize);
//! impl<'a> Dimension<'a, RunSummary> for Total {
//!     fn add_summary(&mut self, s: &'a RunSummary) { self.0 += s.total; }
//! }
//!
//! let tree: SumTree<Run> = (1..=100).map(Run).collect();
//! assert_eq!(tree.summary().count, 100);
//! let mut cursor = tree.cursor::<Total>();
//! cursor.seek(&Total(10), Bias::Right); // 1+2+3+4 = 10, so the next run is 5
//! assert_eq!(cursor.item().map(|r| r.0), Some(5));
//! assert_eq!(*cursor.start(), Total(10));
//! ```

use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;

use arrayvec::ArrayVec;
use smallvec::SmallVec;

/// Half the maximum fan-out of a node. Nodes hold between `TREE_BASE` and
/// `2 * TREE_BASE` children (the root and the right spine may hold fewer).
#[cfg(not(test))]
pub const TREE_BASE: usize = 6;
/// Half the maximum fan-out of a node (small in tests to exercise deep trees).
#[cfg(test)]
pub const TREE_BASE: usize = 2;

const MAX_CHILDREN: usize = 2 * TREE_BASE;

/// An element stored in a [`SumTree`].
pub trait Item: Clone {
    /// The summary type describing this item (and runs of items).
    type Summary: Summary;
    /// Computes this item's summary. Called once when the item enters a tree.
    fn summary(&self) -> Self::Summary;
}

/// An associative "sum" of items. `Default` must be the identity element.
pub trait Summary: Clone + Default + fmt::Debug {
    /// Appends `other` (which describes items *after* `self`) to `self`.
    fn add_summary(&mut self, other: &Self);
}

/// A quantity that can be accumulated from summaries while walking a tree.
///
/// Implementations must be monotonic with respect to the order of items for
/// seeking to be meaningful.
pub trait Dimension<'a, S: Summary>: Clone + Default {
    /// Advances this dimension past the items described by `summary`.
    fn add_summary(&mut self, summary: &'a S);

    /// Builds the dimension spanned by `summary`.
    fn from_summary(summary: &'a S) -> Self {
        let mut dimension = Self::default();
        dimension.add_summary(summary);
        dimension
    }
}

/// Something a [`Cursor`] tracking dimension `D` can seek to.
///
/// Every `D: Dimension + Ord` is a seek target for itself, and `D1` is a seek
/// target for cursors tracking `(D1, D2)` or `(D1, D2, D3)` (only the first
/// component is compared).
pub trait SeekTarget<'a, S: Summary, D: Dimension<'a, S>> {
    /// Compares the target with a cursor position.
    fn cmp(&self, cursor_location: &D) -> Ordering;
}

impl<'a, S: Summary, D: Dimension<'a, S> + Ord> SeekTarget<'a, S, D> for D {
    fn cmp(&self, cursor_location: &D) -> Ordering {
        Ord::cmp(self, cursor_location)
    }
}

impl<'a, S, D1, D2> SeekTarget<'a, S, (D1, D2)> for D1
where
    S: Summary,
    D1: SeekTarget<'a, S, D1> + Dimension<'a, S>,
    D2: Dimension<'a, S>,
{
    fn cmp(&self, cursor_location: &(D1, D2)) -> Ordering {
        SeekTarget::cmp(self, &cursor_location.0)
    }
}

impl<'a, S, D1, D2, D3> SeekTarget<'a, S, (D1, D2, D3)> for D1
where
    S: Summary,
    D1: SeekTarget<'a, S, D1> + Dimension<'a, S>,
    D2: Dimension<'a, S>,
    D3: Dimension<'a, S>,
{
    fn cmp(&self, cursor_location: &(D1, D2, D3)) -> Ordering {
        SeekTarget::cmp(self, &cursor_location.0)
    }
}

/// A summary is trivially a dimension of itself.
impl<'a, S: Summary> Dimension<'a, S> for S {
    fn add_summary(&mut self, summary: &'a S) {
        Summary::add_summary(self, summary);
    }
}

/// The unit dimension, for cursors that only need to iterate.
impl<'a, S: Summary> Dimension<'a, S> for () {
    fn add_summary(&mut self, _: &'a S) {}
}

impl<'a, S: Summary, D1: Dimension<'a, S>, D2: Dimension<'a, S>> Dimension<'a, S> for (D1, D2) {
    fn add_summary(&mut self, summary: &'a S) {
        self.0.add_summary(summary);
        self.1.add_summary(summary);
    }
}

impl<'a, S: Summary, D1: Dimension<'a, S>, D2: Dimension<'a, S>, D3: Dimension<'a, S>>
    Dimension<'a, S> for (D1, D2, D3)
{
    fn add_summary(&mut self, summary: &'a S) {
        self.0.add_summary(summary);
        self.1.add_summary(summary);
        self.2.add_summary(summary);
    }
}

/// A seek target that lies after every item (used by [`Cursor::suffix`]).
struct End;

impl<'a, S: Summary, D: Dimension<'a, S>> SeekTarget<'a, S, D> for End {
    fn cmp(&self, _: &D) -> Ordering {
        Ordering::Greater
    }
}

/// Which side to prefer when a seek target falls exactly on an item boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Bias {
    /// Prefer the item that *ends* at the target.
    #[default]
    Left,
    /// Prefer the item that *starts* at the target.
    Right,
}

impl Bias {
    /// The opposite bias.
    pub fn invert(self) -> Self {
        match self {
            Bias::Left => Bias::Right,
            Bias::Right => Bias::Left,
        }
    }
}

/// A copy-on-write B+tree of items with cached summaries. See the
/// [module docs](self).
pub struct SumTree<T: Item>(Arc<Node<T>>);

enum Node<T: Item> {
    Internal {
        height: u8,
        summary: T::Summary,
        child_summaries: ArrayVec<T::Summary, MAX_CHILDREN>,
        child_trees: ArrayVec<SumTree<T>, MAX_CHILDREN>,
    },
    Leaf {
        summary: T::Summary,
        items: ArrayVec<T, MAX_CHILDREN>,
        item_summaries: ArrayVec<T::Summary, MAX_CHILDREN>,
    },
}

impl<T: Item> Clone for SumTree<T> {
    fn clone(&self) -> Self {
        SumTree(Arc::clone(&self.0))
    }
}

impl<T: Item> Clone for Node<T> {
    fn clone(&self) -> Self {
        match self {
            Node::Internal {
                height,
                summary,
                child_summaries,
                child_trees,
            } => Node::Internal {
                height: *height,
                summary: summary.clone(),
                child_summaries: child_summaries.clone(),
                child_trees: child_trees.clone(),
            },
            Node::Leaf {
                summary,
                items,
                item_summaries,
            } => Node::Leaf {
                summary: summary.clone(),
                items: items.clone(),
                item_summaries: item_summaries.clone(),
            },
        }
    }
}

impl<T: Item> Node<T> {
    fn summary(&self) -> &T::Summary {
        match self {
            Node::Internal { summary, .. } | Node::Leaf { summary, .. } => summary,
        }
    }

    fn height(&self) -> u8 {
        match self {
            Node::Internal { height, .. } => *height,
            Node::Leaf { .. } => 0,
        }
    }

    fn child_trees(&self) -> &[SumTree<T>] {
        match self {
            Node::Internal { child_trees, .. } => child_trees,
            Node::Leaf { .. } => &[],
        }
    }

    fn child_count(&self) -> usize {
        match self {
            Node::Internal { child_trees, .. } => child_trees.len(),
            Node::Leaf { items, .. } => items.len(),
        }
    }

    fn is_underflowing(&self) -> bool {
        self.child_count() < TREE_BASE
    }
}

fn sum<'a, S: Summary + 'a>(summaries: impl IntoIterator<Item = &'a S>) -> S {
    let mut total = S::default();
    for summary in summaries {
        total.add_summary(summary);
    }
    total
}

impl<T: Item> Default for SumTree<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Item + fmt::Debug> fmt::Debug for SumTree<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T: Item> SumTree<T> {
    /// Creates an empty tree.
    pub fn new() -> Self {
        SumTree(Arc::new(Node::Leaf {
            summary: T::Summary::default(),
            items: ArrayVec::new(),
            item_summaries: ArrayVec::new(),
        }))
    }

    /// Creates a tree holding a single item.
    pub fn from_item(item: T) -> Self {
        let summary = item.summary();
        let mut items = ArrayVec::new();
        items.push(item);
        let mut item_summaries = ArrayVec::new();
        item_summaries.push(summary.clone());
        SumTree(Arc::new(Node::Leaf {
            summary,
            items,
            item_summaries,
        }))
    }

    /// Builds a balanced tree from items in order, in `O(n)`.
    #[allow(clippy::should_implement_trait)] // also available via `FromIterator`
    pub fn from_iter(iter: impl IntoIterator<Item = T>) -> Self {
        Self::from_items_and_summaries(iter.into_iter().map(|item| {
            let summary = item.summary();
            (item, summary)
        }))
    }

    /// Builds a balanced tree from items whose summaries were computed by the
    /// caller (e.g. in parallel). Each summary must equal `item.summary()`.
    pub fn from_items_and_summaries(iter: impl IntoIterator<Item = (T, T::Summary)>) -> Self {
        let mut iter = iter.into_iter().peekable();
        let mut nodes: Vec<SumTree<T>> = Vec::new();
        while iter.peek().is_some() {
            let mut items = ArrayVec::new();
            let mut item_summaries = ArrayVec::new();
            for (item, summary) in iter.by_ref().take(MAX_CHILDREN) {
                items.push(item);
                item_summaries.push(summary);
            }
            nodes.push(Self::leaf_node(items, item_summaries));
        }
        Self::from_leaf_nodes(nodes)
    }

    fn leaf_node(
        items: ArrayVec<T, MAX_CHILDREN>,
        item_summaries: ArrayVec<T::Summary, MAX_CHILDREN>,
    ) -> SumTree<T> {
        let summary = sum(item_summaries.iter());
        SumTree(Arc::new(Node::Leaf {
            summary,
            items,
            item_summaries,
        }))
    }

    /// Splits `items` (with their summaries) into leaf nodes of near-equal
    /// size (never a tiny trailing leaf), for [`from_leaf_nodes`](Self::from_leaf_nodes).
    pub(crate) fn balanced_leaf_nodes(items: Vec<(T, T::Summary)>) -> Vec<SumTree<T>> {
        let total = items.len();
        let leaves = total.div_ceil(MAX_CHILDREN);
        let mut nodes = Vec::with_capacity(leaves);
        let mut iter = items.into_iter();
        for k in 0..leaves {
            // Spread the remainder over the first leaves.
            let take = total / leaves + usize::from(k < total % leaves);
            let mut leaf_items = ArrayVec::new();
            let mut leaf_summaries = ArrayVec::new();
            for (item, summary) in iter.by_ref().take(take) {
                leaf_items.push(item);
                leaf_summaries.push(summary);
            }
            nodes.push(Self::leaf_node(leaf_items, leaf_summaries));
        }
        nodes
    }

    /// Builds a balanced tree over `nodes`, which must all be leaf nodes (as
    /// yielded by [`leaves`](Self::leaves) or [`balanced_leaf_nodes`](Self::balanced_leaf_nodes)).
    pub(crate) fn from_leaf_nodes(mut nodes: Vec<SumTree<T>>) -> SumTree<T> {
        let mut height = 0u8;
        while nodes.len() > 1 {
            height += 1;
            let mut parents = Vec::with_capacity(nodes.len().div_ceil(MAX_CHILDREN));
            let mut children = nodes.into_iter().peekable();
            while children.peek().is_some() {
                let child_trees: ArrayVec<SumTree<T>, MAX_CHILDREN> =
                    children.by_ref().take(MAX_CHILDREN).collect();
                let child_summaries: ArrayVec<T::Summary, MAX_CHILDREN> = child_trees
                    .iter()
                    .map(|tree| tree.0.summary().clone())
                    .collect();
                let summary = sum(child_summaries.iter());
                parents.push(SumTree(Arc::new(Node::Internal {
                    height,
                    summary,
                    child_summaries,
                    child_trees,
                })));
            }
            nodes = parents;
        }
        nodes.pop().unwrap_or_default()
    }

    /// The leaf nodes (subtrees of height 0) in order.
    pub(crate) fn leaves(&self) -> Leaves<'_, T> {
        Leaves {
            stack: vec![(self, 0)],
        }
    }

    /// The items and their summaries of a leaf node (`None` for an internal node).
    pub(crate) fn leaf_parts(&self) -> Option<(&[T], &[T::Summary])> {
        match &*self.0 {
            Node::Leaf {
                items,
                item_summaries,
                ..
            } => Some((items, item_summaries)),
            Node::Internal { .. } => None,
        }
    }

    /// The summary of all items in the tree.
    pub fn summary(&self) -> &T::Summary {
        self.0.summary()
    }

    /// The extent of the whole tree measured in dimension `D`.
    pub fn extent<'a, D: Dimension<'a, T::Summary>>(&'a self) -> D {
        D::from_summary(self.summary())
    }

    /// Whether the tree has no items.
    pub fn is_empty(&self) -> bool {
        match &*self.0 {
            Node::Internal { .. } => false,
            Node::Leaf { items, .. } => items.is_empty(),
        }
    }

    /// The height of the tree (0 for a single leaf).
    pub fn height(&self) -> u8 {
        self.0.height()
    }

    /// The first item, if any.
    pub fn first(&self) -> Option<&T> {
        let mut node = &*self.0;
        loop {
            match node {
                Node::Internal { child_trees, .. } => node = &child_trees.first()?.0,
                Node::Leaf { items, .. } => return items.first(),
            }
        }
    }

    /// The last item, if any.
    pub fn last(&self) -> Option<&T> {
        let mut node = &*self.0;
        loop {
            match node {
                Node::Internal { child_trees, .. } => node = &child_trees.last()?.0,
                Node::Leaf { items, .. } => return items.last(),
            }
        }
    }

    /// Iterates over all items in order.
    pub fn iter(&self) -> Iter<'_, T> {
        Iter {
            cursor: self.cursor::<()>(),
            started: false,
        }
    }

    /// Collects all items into a `Vec`.
    pub fn items(&self) -> Vec<T> {
        self.iter().cloned().collect()
    }

    /// Creates a cursor tracking dimension `D`, positioned before the first item.
    pub fn cursor<'a, D: Dimension<'a, T::Summary>>(&'a self) -> Cursor<'a, T, D> {
        Cursor::new(self)
    }

    /// Appends one item. `O(log n)`.
    pub fn push(&mut self, item: T) {
        self.append(SumTree::from_item(item));
    }

    /// Appends all items of `iter`.
    pub fn extend(&mut self, iter: impl IntoIterator<Item = T>) {
        self.append(SumTree::from_iter(iter));
    }

    /// Mutates the last item in place, updating summaries along the path.
    /// Does nothing on an empty tree. Returns whether an item was updated.
    pub fn update_last(&mut self, f: impl FnOnce(&mut T)) -> bool {
        if self.is_empty() {
            return false;
        }
        self.update_last_recursive(f);
        true
    }

    fn update_last_recursive(&mut self, f: impl FnOnce(&mut T)) {
        match Arc::make_mut(&mut self.0) {
            Node::Internal {
                summary,
                child_summaries,
                child_trees,
                ..
            } => {
                if let (Some(last_tree), Some(last_summary)) =
                    (child_trees.last_mut(), child_summaries.last_mut())
                {
                    last_tree.update_last_recursive(f);
                    *last_summary = last_tree.0.summary().clone();
                }
                *summary = sum(child_summaries.iter());
            }
            Node::Leaf {
                summary,
                items,
                item_summaries,
            } => {
                if let (Some(item), Some(item_summary)) =
                    (items.last_mut(), item_summaries.last_mut())
                {
                    f(item);
                    *item_summary = item.summary();
                }
                *summary = sum(item_summaries.iter());
            }
        }
    }

    /// Appends all items of `other` after the items of `self`. `O(log n)`.
    pub fn append(&mut self, other: SumTree<T>) {
        if self.is_empty() {
            *self = other;
        } else if !other.is_empty() {
            if self.height() < other.height() {
                for tree in other.0.child_trees() {
                    self.append(tree.clone());
                }
            } else if let Some(split_tree) = self.push_tree_recursive(other) {
                *self = Self::from_child_trees(self.clone(), split_tree);
            }
        }
    }

    fn from_child_trees(left: SumTree<T>, right: SumTree<T>) -> Self {
        let height = left.height() + 1;
        let mut child_summaries = ArrayVec::new();
        child_summaries.push(left.summary().clone());
        child_summaries.push(right.summary().clone());
        let mut child_trees = ArrayVec::new();
        child_trees.push(left);
        child_trees.push(right);
        let summary = sum(child_summaries.iter());
        SumTree(Arc::new(Node::Internal {
            height,
            summary,
            child_summaries,
            child_trees,
        }))
    }

    /// Appends `other` (whose height is at most ours) into this tree. Returns a
    /// sibling tree of the same height if this node overflowed and split.
    fn push_tree_recursive(&mut self, other: SumTree<T>) -> Option<SumTree<T>> {
        match Arc::make_mut(&mut self.0) {
            Node::Internal {
                height,
                summary,
                child_summaries,
                child_trees,
            } => {
                let other_node = Arc::clone(&other.0);
                Summary::add_summary(summary, other_node.summary());

                let height_delta = *height - other_node.height();
                let mut summaries_to_append: ArrayVec<T::Summary, MAX_CHILDREN> = ArrayVec::new();
                let mut trees_to_append: ArrayVec<SumTree<T>, MAX_CHILDREN> = ArrayVec::new();
                if height_delta == 0 {
                    if let Node::Internal {
                        child_summaries,
                        child_trees,
                        ..
                    } = &*other_node
                    {
                        summaries_to_append.extend(child_summaries.iter().cloned());
                        trees_to_append.extend(child_trees.iter().cloned());
                    }
                } else if height_delta == 1 && !other_node.is_underflowing() {
                    summaries_to_append.push(other_node.summary().clone());
                    trees_to_append.push(other);
                } else if let (Some(last_tree), Some(last_summary)) =
                    (child_trees.last_mut(), child_summaries.last_mut())
                {
                    let split = last_tree.push_tree_recursive(other);
                    *last_summary = last_tree.summary().clone();
                    if let Some(split_tree) = split {
                        summaries_to_append.push(split_tree.summary().clone());
                        trees_to_append.push(split_tree);
                    }
                }

                let child_count = child_trees.len() + trees_to_append.len();
                if child_count > MAX_CHILDREN {
                    let midpoint = child_count.div_ceil(2);
                    let mut all_summaries = child_summaries
                        .iter()
                        .chain(summaries_to_append.iter())
                        .cloned();
                    let left_summaries: ArrayVec<_, MAX_CHILDREN> =
                        all_summaries.by_ref().take(midpoint).collect();
                    let right_summaries: ArrayVec<_, MAX_CHILDREN> = all_summaries.collect();
                    let mut all_trees = child_trees.iter().chain(trees_to_append.iter()).cloned();
                    let left_trees: ArrayVec<_, MAX_CHILDREN> =
                        all_trees.by_ref().take(midpoint).collect();
                    let right_trees: ArrayVec<_, MAX_CHILDREN> = all_trees.collect();

                    *summary = sum(left_summaries.iter());
                    *child_summaries = left_summaries;
                    *child_trees = left_trees;
                    Some(SumTree(Arc::new(Node::Internal {
                        height: *height,
                        summary: sum(right_summaries.iter()),
                        child_summaries: right_summaries,
                        child_trees: right_trees,
                    })))
                } else {
                    child_summaries.extend(summaries_to_append);
                    child_trees.extend(trees_to_append);
                    None
                }
            }
            Node::Leaf {
                summary,
                items,
                item_summaries,
            } => {
                let Node::Leaf {
                    items: other_items,
                    item_summaries: other_summaries,
                    ..
                } = &*other.0
                else {
                    // Heights are equal (both 0), so `other` is a leaf too.
                    return None;
                };
                let child_count = items.len() + other_items.len();
                if child_count > MAX_CHILDREN {
                    let midpoint = child_count.div_ceil(2);
                    let mut all_items = items.iter().chain(other_items.iter()).cloned();
                    let left_items: ArrayVec<_, MAX_CHILDREN> =
                        all_items.by_ref().take(midpoint).collect();
                    let right_items: ArrayVec<_, MAX_CHILDREN> = all_items.collect();
                    let mut all_summaries =
                        item_summaries.iter().chain(other_summaries.iter()).cloned();
                    let left_summaries: ArrayVec<_, MAX_CHILDREN> =
                        all_summaries.by_ref().take(midpoint).collect();
                    let right_summaries: ArrayVec<_, MAX_CHILDREN> = all_summaries.collect();

                    *items = left_items;
                    *summary = sum(left_summaries.iter());
                    *item_summaries = left_summaries;
                    Some(SumTree(Arc::new(Node::Leaf {
                        summary: sum(right_summaries.iter()),
                        items: right_items,
                        item_summaries: right_summaries,
                    })))
                } else {
                    Summary::add_summary(summary, other.summary());
                    items.extend(other_items.iter().cloned());
                    item_summaries.extend(other_summaries.iter().cloned());
                    None
                }
            }
        }
    }
}

impl<T: Item> FromIterator<T> for SumTree<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        SumTree::from_iter(iter)
    }
}

impl<T: Item> Extend<T> for SumTree<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        SumTree::extend(self, iter);
    }
}

impl<'a, T: Item> IntoIterator for &'a SumTree<T> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator over the items of a [`SumTree`], created by [`SumTree::iter`].
pub struct Iter<'a, T: Item> {
    cursor: Cursor<'a, T, ()>,
    started: bool,
}

impl<'a, T: Item> Iterator for Iter<'a, T> {
    type Item = &'a T;
    fn next(&mut self) -> Option<&'a T> {
        if self.started && self.cursor.item().is_none() {
            return None;
        }
        self.started = true;
        self.cursor.next();
        self.cursor.item()
    }
}

/// Iterator over the leaf nodes of a tree, created by [`SumTree::leaves`].
pub(crate) struct Leaves<'a, T: Item> {
    stack: Vec<(&'a SumTree<T>, usize)>,
}

impl<'a, T: Item> Iterator for Leaves<'a, T> {
    type Item = &'a SumTree<T>;

    fn next(&mut self) -> Option<&'a SumTree<T>> {
        loop {
            let (tree, index) = self.stack.last_mut()?;
            let tree: &'a SumTree<T> = tree;
            match &*tree.0 {
                Node::Leaf { .. } => {
                    self.stack.pop();
                    return Some(tree);
                }
                Node::Internal { child_trees, .. } => match child_trees.get(*index) {
                    Some(child) => {
                        *index += 1;
                        self.stack.push((child, 0));
                    }
                    None => {
                        self.stack.pop();
                    }
                },
            }
        }
    }
}

struct StackEntry<'a, T: Item, D> {
    tree: &'a SumTree<T>,
    /// Index of the child (internal node) or item (leaf) the cursor is in.
    index: usize,
    /// The dimension at the start of this node.
    position: D,
}

/// Collects items skipped by a seek (used to build slices).
trait SeekAggregate<'a, T: Item> {
    fn push_item(&mut self, item: &'a T, summary: &'a T::Summary);
    fn push_tree(&mut self, tree: &'a SumTree<T>);
}

impl<'a, T: Item> SeekAggregate<'a, T> for () {
    fn push_item(&mut self, _: &'a T, _: &'a T::Summary) {}
    fn push_tree(&mut self, _: &'a SumTree<T>) {}
}

struct SliceAggregate<T: Item> {
    tree: SumTree<T>,
    /// Items with their already known summaries (never recomputed).
    pending: Vec<(T, T::Summary)>,
}

impl<T: Item> SliceAggregate<T> {
    fn new() -> Self {
        SliceAggregate {
            tree: SumTree::new(),
            pending: Vec::new(),
        }
    }

    fn flush(&mut self) {
        if !self.pending.is_empty() {
            let items = std::mem::take(&mut self.pending);
            self.tree.append(SumTree::from_items_and_summaries(items));
        }
    }

    fn finish(mut self) -> SumTree<T> {
        self.flush();
        self.tree
    }
}

impl<'a, T: Item> SeekAggregate<'a, T> for SliceAggregate<T> {
    fn push_item(&mut self, item: &'a T, summary: &'a T::Summary) {
        self.pending.push((item.clone(), summary.clone()));
    }
    fn push_tree(&mut self, tree: &'a SumTree<T>) {
        self.flush();
        self.tree.append(tree.clone());
    }
}

/// A position in a [`SumTree`] tracking dimension `D`.
///
/// A cursor is either *before the start* (freshly created; `item()` is
/// `None`), *on an item*, or *at the end* (`item()` is `None`, `start()` is
/// the extent of the whole tree). [`next`](Cursor::next) and
/// [`prev`](Cursor::prev) move between these states; seeking is `O(log n)`.
pub struct Cursor<'a, T: Item, D> {
    tree: &'a SumTree<T>,
    stack: SmallVec<[StackEntry<'a, T, D>; 16]>,
    position: D,
    at_end: bool,
}

impl<'a, T: Item, D: Dimension<'a, T::Summary>> Cursor<'a, T, D> {
    fn new(tree: &'a SumTree<T>) -> Self {
        Cursor {
            tree,
            stack: SmallVec::new(),
            position: D::default(),
            at_end: false,
        }
    }

    /// Returns the cursor to the "before the start" state.
    pub fn reset(&mut self) {
        self.stack.clear();
        self.position = D::default();
        self.at_end = false;
    }

    /// The dimension at the start of the current item (or the total extent
    /// when at the end, or zero before the start).
    pub fn start(&self) -> &D {
        &self.position
    }

    /// The dimension at the end of the current item (equal to
    /// [`start`](Self::start) when there is no current item).
    pub fn end(&self) -> D {
        let mut end = self.position.clone();
        if let Some(summary) = self.item_summary() {
            end.add_summary(summary);
        }
        end
    }

    /// The current item.
    pub fn item(&self) -> Option<&'a T> {
        let entry = self.stack.last()?;
        match &*entry.tree.0 {
            Node::Leaf { items, .. } => items.get(entry.index),
            Node::Internal { .. } => None,
        }
    }

    /// The summary of the current item.
    pub fn item_summary(&self) -> Option<&'a T::Summary> {
        let entry = self.stack.last()?;
        match &*entry.tree.0 {
            Node::Leaf { item_summaries, .. } => item_summaries.get(entry.index),
            Node::Internal { .. } => None,
        }
    }

    /// Whether the cursor is past the last item.
    pub fn is_at_end(&self) -> bool {
        self.at_end
    }

    /// Peeks at the item after the current one without moving.
    pub fn next_item(&self) -> Option<&'a T> {
        let mut probe = self.clone_shallow();
        probe.next();
        probe.item()
    }

    /// Peeks at the item before the current one without moving.
    pub fn prev_item(&self) -> Option<&'a T> {
        let mut probe = self.clone_shallow();
        probe.prev();
        probe.item()
    }

    fn clone_shallow(&self) -> Self {
        Cursor {
            tree: self.tree,
            stack: self
                .stack
                .iter()
                .map(|entry| StackEntry {
                    tree: entry.tree,
                    index: entry.index,
                    position: entry.position.clone(),
                })
                .collect(),
            position: self.position.clone(),
            at_end: self.at_end,
        }
    }

    /// Moves to the next item (from "before the start" this is the first
    /// item). At the end this is a no-op.
    pub fn next(&mut self) {
        if self.at_end {
            return;
        }
        if self.stack.is_empty() {
            self.descend_leftmost(self.tree, D::default());
            return;
        }
        if let Some(summary) = self.item_summary() {
            self.position.add_summary(summary);
        }
        // Advance within the leaf, or climb until a node has a next child.
        while let Some(entry) = self.stack.last_mut() {
            entry.index += 1;
            let tree: &'a SumTree<T> = entry.tree;
            let index = entry.index;
            if index < tree.0.child_count() {
                if let Node::Internal { child_trees, .. } = &*tree.0 {
                    let position = self.position.clone();
                    self.descend_leftmost(&child_trees[index], position);
                }
                if self.item().is_some() {
                    return;
                }
                // An empty subtree (only possible for an empty root): keep climbing.
                continue;
            }
            self.stack.pop();
        }
        self.at_end = true;
    }

    /// Descends from `tree` (which starts at `position`) to its first item.
    fn descend_leftmost(&mut self, mut tree: &'a SumTree<T>, position: D) {
        self.position = position;
        loop {
            self.stack.push(StackEntry {
                tree,
                index: 0,
                position: self.position.clone(),
            });
            match &*tree.0 {
                Node::Internal { child_trees, .. } => tree = &child_trees[0],
                Node::Leaf { items, .. } => {
                    if items.is_empty() {
                        // Empty tree.
                        self.stack.clear();
                        self.at_end = true;
                    }
                    return;
                }
            }
        }
    }

    /// Descends from `tree` (which starts at `position`) to its last item.
    fn descend_rightmost(&mut self, mut tree: &'a SumTree<T>, mut position: D) {
        loop {
            match &*tree.0 {
                Node::Internal {
                    child_trees,
                    child_summaries,
                    ..
                } => {
                    let index = child_trees.len() - 1;
                    self.stack.push(StackEntry {
                        tree,
                        index,
                        position: position.clone(),
                    });
                    for summary in &child_summaries[..index] {
                        position.add_summary(summary);
                    }
                    tree = &child_trees[index];
                }
                Node::Leaf {
                    items,
                    item_summaries,
                    ..
                } => {
                    if items.is_empty() {
                        self.reset();
                        return;
                    }
                    let index = items.len() - 1;
                    self.stack.push(StackEntry {
                        tree,
                        index,
                        position: position.clone(),
                    });
                    for summary in &item_summaries[..index] {
                        position.add_summary(summary);
                    }
                    self.position = position;
                    return;
                }
            }
        }
    }

    /// Moves to the previous item. From the first item this moves to the
    /// "before the start" state; from the end it moves to the last item.
    pub fn prev(&mut self) {
        if self.at_end {
            self.at_end = false;
            self.stack.clear();
            self.descend_rightmost(self.tree, D::default());
            return;
        }
        if self.stack.is_empty() {
            return;
        }
        loop {
            let Some(entry) = self.stack.last_mut() else {
                self.reset();
                return;
            };
            if entry.index == 0 {
                self.stack.pop();
                continue;
            }
            entry.index -= 1;
            let index = entry.index;
            let tree: &'a SumTree<T> = entry.tree;
            let mut position = entry.position.clone();
            match &*tree.0 {
                Node::Leaf { item_summaries, .. } => {
                    for summary in &item_summaries[..index] {
                        position.add_summary(summary);
                    }
                    self.position = position;
                }
                Node::Internal {
                    child_trees,
                    child_summaries,
                    ..
                } => {
                    for summary in &child_summaries[..index] {
                        position.add_summary(summary);
                    }
                    let child = &child_trees[index];
                    self.descend_rightmost(child, position);
                }
            }
            return;
        }
    }

    /// Seeks from the root to the item containing `target`. With
    /// [`Bias::Left`] the cursor lands on the first item whose end is `>=`
    /// target; with [`Bias::Right`] on the first item whose end is `>` target
    /// (so a target on a boundary selects the item *starting* there).
    ///
    /// Returns whether the cursor's [`start`](Self::start) equals `target`.
    pub fn seek<Target>(&mut self, target: &Target, bias: Bias) -> bool
    where
        Target: SeekTarget<'a, T::Summary, D> + ?Sized,
    {
        self.reset();
        self.seek_internal(target, bias, &mut ())
    }

    /// Like [`seek`](Self::seek) but only moves forward from the current
    /// position (cheaper for monotonic sequences of seeks).
    pub fn seek_forward<Target>(&mut self, target: &Target, bias: Bias) -> bool
    where
        Target: SeekTarget<'a, T::Summary, D> + ?Sized,
    {
        self.seek_internal(target, bias, &mut ())
    }

    /// Moves forward to `target` (as [`seek_forward`](Self::seek_forward))
    /// and returns a tree of all items passed over, i.e. from the current
    /// item up to (excluding) the item the cursor lands on. `O(log n)` plus
    /// the number of partially covered leaves.
    pub fn slice<Target>(&mut self, target: &Target, bias: Bias) -> SumTree<T>
    where
        Target: SeekTarget<'a, T::Summary, D> + ?Sized,
    {
        let mut aggregate = SliceAggregate::new();
        self.seek_internal(target, bias, &mut aggregate);
        aggregate.finish()
    }

    /// Returns a tree of the current item and everything after it, moving
    /// the cursor to the end.
    pub fn suffix(&mut self) -> SumTree<T> {
        let mut aggregate = SliceAggregate::new();
        self.seek_internal(&End, Bias::Right, &mut aggregate);
        aggregate.finish()
    }

    fn seek_internal<Target>(
        &mut self,
        target: &Target,
        bias: Bias,
        aggregate: &mut impl SeekAggregate<'a, T>,
    ) -> bool
    where
        Target: SeekTarget<'a, T::Summary, D> + ?Sized,
    {
        let skip = |end: &D| match target.cmp(end) {
            Ordering::Greater => true,
            Ordering::Equal => bias == Bias::Right,
            Ordering::Less => false,
        };

        if self.at_end {
            return target.cmp(&self.position) == Ordering::Equal;
        }
        if self.stack.is_empty() {
            self.position = D::default();
            self.stack.push(StackEntry {
                tree: self.tree,
                index: 0,
                position: D::default(),
            });
        }

        // `descending` is true when the entry on top of the stack has not yet
        // considered the child/item at `index`; false when we just climbed
        // back up from that child and must move past it.
        let mut descending = true;
        while let Some(entry) = self.stack.last_mut() {
            if !descending {
                entry.index += 1;
            }
            let tree: &'a SumTree<T> = entry.tree;
            match &*tree.0 {
                Node::Internal {
                    child_summaries,
                    child_trees,
                    ..
                } => {
                    let mut descend_into = None;
                    while entry.index < child_trees.len() {
                        let mut child_end = self.position.clone();
                        child_end.add_summary(&child_summaries[entry.index]);
                        if skip(&child_end) {
                            aggregate.push_tree(&child_trees[entry.index]);
                            self.position = child_end;
                            entry.index += 1;
                        } else {
                            descend_into = Some(&child_trees[entry.index]);
                            break;
                        }
                    }
                    if let Some(child) = descend_into {
                        let position = self.position.clone();
                        self.stack.push(StackEntry {
                            tree: child,
                            index: 0,
                            position,
                        });
                        descending = true;
                    } else {
                        self.stack.pop();
                        descending = false;
                    }
                }
                Node::Leaf {
                    items,
                    item_summaries,
                    ..
                } => {
                    while entry.index < items.len() {
                        let mut item_end = self.position.clone();
                        item_end.add_summary(&item_summaries[entry.index]);
                        if skip(&item_end) {
                            aggregate.push_item(&items[entry.index], &item_summaries[entry.index]);
                            self.position = item_end;
                            entry.index += 1;
                        } else {
                            return target.cmp(&self.position) == Ordering::Equal;
                        }
                    }
                    self.stack.pop();
                    descending = false;
                }
            }
        }
        self.at_end = true;
        target.cmp(&self.position) == Ordering::Equal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct N(u32);

    #[derive(Clone, Debug, Default, PartialEq)]
    struct NSummary {
        count: usize,
        sum: u64,
        max: u32,
    }

    impl Summary for NSummary {
        fn add_summary(&mut self, other: &Self) {
            self.count += other.count;
            self.sum += other.sum;
            self.max = self.max.max(other.max);
        }
    }

    impl Item for N {
        type Summary = NSummary;
        fn summary(&self) -> NSummary {
            NSummary {
                count: 1,
                sum: self.0 as u64,
                max: self.0,
            }
        }
    }

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
    struct Count(usize);
    impl<'a> Dimension<'a, NSummary> for Count {
        fn add_summary(&mut self, s: &'a NSummary) {
            self.0 += s.count;
        }
    }

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
    struct Sum(u64);
    impl<'a> Dimension<'a, NSummary> for Sum {
        fn add_summary(&mut self, s: &'a NSummary) {
            self.0 += s.sum;
        }
    }

    fn check_invariants<T: Item>(tree: &SumTree<T>, is_root: bool) -> usize
    where
        T::Summary: PartialEq,
    {
        match &*tree.0 {
            Node::Internal {
                height,
                summary,
                child_summaries,
                child_trees,
            } => {
                assert!(child_trees.len() >= if is_root { 2 } else { 1 });
                assert_eq!(child_summaries.len(), child_trees.len());
                let mut total = 0;
                for (child, child_summary) in child_trees.iter().zip(child_summaries) {
                    assert_eq!(child.height() + 1, *height);
                    assert_eq!(child.summary(), child_summary);
                    total += check_invariants(child, false);
                }
                assert_eq!(&sum(child_summaries.iter()), summary);
                total
            }
            Node::Leaf {
                summary,
                items,
                item_summaries,
            } => {
                assert_eq!(items.len(), item_summaries.len());
                for (item, item_summary) in items.iter().zip(item_summaries) {
                    assert_eq!(&item.summary(), item_summary);
                }
                assert_eq!(&sum(item_summaries.iter()), summary);
                items.len()
            }
        }
    }

    #[test]
    fn empty_tree() {
        let tree = SumTree::<N>::new();
        assert!(tree.is_empty());
        assert_eq!(tree.iter().count(), 0);
        let mut cursor = tree.cursor::<Count>();
        cursor.next();
        assert!(cursor.item().is_none());
        assert!(cursor.is_at_end());
        cursor.prev();
        assert!(cursor.item().is_none());
        let mut cursor = tree.cursor::<Count>();
        cursor.seek(&Count(3), Bias::Right);
        assert!(cursor.item().is_none());
        assert_eq!(cursor.slice(&Count(1), Bias::Left).iter().count(), 0);
    }

    #[test]
    fn push_append_and_iterate() {
        for n in 0..200u32 {
            let mut tree = SumTree::new();
            for i in 0..n {
                tree.push(N(i));
            }
            check_invariants(&tree, true);
            let items: Vec<u32> = tree.iter().map(|n| n.0).collect();
            assert_eq!(items, (0..n).collect::<Vec<_>>());

            let built: SumTree<N> = (0..n).map(N).collect();
            check_invariants(&built, true);
            assert_eq!(built.summary(), tree.summary());

            for split in [0, n / 3, n / 2, n] {
                let mut left: SumTree<N> = (0..split).map(N).collect();
                let right: SumTree<N> = (split..n).map(N).collect();
                left.append(right);
                check_invariants(&left, true);
                assert_eq!(left.items(), (0..n).map(N).collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn leaves_and_leaf_level_rebuild() {
        for n in [0u32, 1, 2, 3, 5, 17, 100, 333] {
            let tree: SumTree<N> = (0..n).map(N).collect();
            // The leaves in order hold exactly the items of the tree.
            let leaves: Vec<&SumTree<N>> = tree.leaves().collect();
            let items: Vec<u32> = leaves
                .iter()
                .flat_map(|l| l.leaf_parts().map_or(&[][..], |p| p.0).iter().map(|n| n.0))
                .collect();
            assert_eq!(items, (0..n).collect::<Vec<_>>());
            // Reassembling shared leaves gives a valid tree with the same content.
            let rebuilt = SumTree::from_leaf_nodes(leaves.into_iter().cloned().collect());
            check_invariants(&rebuilt, true);
            assert_eq!(rebuilt.items(), tree.items());
            // Balanced leaf distribution never leaves a tiny trailing leaf.
            let pairs: Vec<(N, NSummary)> = (0..n).map(|i| (N(i), N(i).summary())).collect();
            let nodes = SumTree::balanced_leaf_nodes(pairs);
            if nodes.len() > 1 {
                let sizes: Vec<usize> = nodes
                    .iter()
                    .map(|l| l.leaf_parts().map_or(0, |p| p.0.len()))
                    .collect();
                assert!(sizes.iter().all(|&s| s >= MAX_CHILDREN / 2), "{sizes:?}");
            }
            let balanced = SumTree::from_leaf_nodes(nodes);
            check_invariants(&balanced, true);
            assert_eq!(balanced.items(), tree.items());
        }
    }

    #[test]
    fn cursor_navigation() {
        let tree: SumTree<N> = (1..=50).map(N).collect();
        let mut cursor = tree.cursor::<(Count, Sum)>();
        assert!(cursor.item().is_none());
        let mut expected_sum = 0;
        for i in 1..=50u32 {
            cursor.next();
            assert_eq!(cursor.item(), Some(&N(i)));
            assert_eq!(cursor.start().0, Count(i as usize - 1));
            assert_eq!(cursor.start().1, Sum(expected_sum));
            expected_sum += i as u64;
            assert_eq!(cursor.end().1, Sum(expected_sum));
        }
        cursor.next();
        assert!(cursor.item().is_none());
        assert_eq!(cursor.start().0, Count(50));
        for i in (1..=50u32).rev() {
            cursor.prev();
            assert_eq!(cursor.item(), Some(&N(i)), "prev to {i}");
            assert_eq!(cursor.start().0, Count(i as usize - 1));
            expected_sum -= i as u64;
            assert_eq!(cursor.start().1, Sum(expected_sum));
        }
        cursor.prev();
        assert!(cursor.item().is_none());
        cursor.next();
        assert_eq!(cursor.item(), Some(&N(1)));
    }

    #[test]
    fn seek_bias_and_slices() {
        let tree: SumTree<N> = (0..100).map(N).collect();
        for target in 0..=100usize {
            let mut cursor = tree.cursor::<Count>();
            let found = cursor.seek(&Count(target), Bias::Right);
            assert!(found);
            assert_eq!(
                cursor.item().map(|n| n.0 as usize),
                (target < 100).then_some(target)
            );

            let mut cursor = tree.cursor::<Count>();
            cursor.seek(&Count(target), Bias::Left);
            let expected = if target == 0 {
                Some(0)
            } else {
                Some(target - 1)
            };
            assert_eq!(cursor.item().map(|n| n.0 as usize), expected);

            // Slicing [a, target) for every a.
            for start in (0..=target).step_by(7) {
                let mut cursor = tree.cursor::<Count>();
                cursor.seek(&Count(start), Bias::Right);
                let slice = cursor.slice(&Count(target), Bias::Right);
                check_invariants(&slice, true);
                let items: Vec<usize> = slice.iter().map(|n| n.0 as usize).collect();
                assert_eq!(items, (start..target).collect::<Vec<_>>());
                let suffix = cursor.suffix();
                let items: Vec<usize> = suffix.iter().map(|n| n.0 as usize).collect();
                assert_eq!(items, (target..100).collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn seek_by_secondary_dimension_via_tuple() {
        let tree: SumTree<N> = (1..=20).map(N).collect();
        let mut cursor = tree.cursor::<(Sum, Count)>();
        // Sums: 1,3,6,10,15,... Seeking Sum(10) with Right lands on item 5.
        cursor.seek(&Sum(10), Bias::Right);
        assert_eq!(cursor.item(), Some(&N(5)));
        assert_eq!(cursor.start().1, Count(4));
        cursor.seek_forward(&Sum(12), Bias::Right);
        assert_eq!(cursor.item(), Some(&N(5)));
        cursor.seek_forward(&Sum(1000), Bias::Right);
        assert!(cursor.item().is_none());
        assert_eq!(cursor.start().0, Sum(210));
    }

    #[test]
    fn update_last_and_clone_is_persistent() {
        let mut tree: SumTree<N> = (0..40).map(N).collect();
        let snapshot = tree.clone();
        assert!(tree.update_last(|n| n.0 = 1000));
        check_invariants(&tree, true);
        assert_eq!(tree.summary().max, 1000);
        assert_eq!(snapshot.summary().max, 39);
        assert_eq!(snapshot.last(), Some(&N(39)));
        assert_eq!(tree.first(), Some(&N(0)));
    }

    #[test]
    fn random_appends_keep_invariants() {
        // Deterministic xorshift to avoid flaky tests.
        let mut state = 0x2545F4914F6CDD1Du64;
        let mut rand = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n.max(1)
        };
        for _ in 0..50 {
            let mut tree = SumTree::new();
            let mut model = Vec::new();
            for _ in 0..rand(20) {
                let len = rand(60) as u32;
                let base = model.len() as u32;
                let part: SumTree<N> = (base..base + len).map(N).collect();
                model.extend((base..base + len).map(N));
                tree.append(part);
                check_invariants(&tree, true);
            }
            assert_eq!(tree.items(), model);
            // Height stays logarithmic.
            let n = model.len().max(2) as f64;
            assert!((tree.height() as f64) <= 2.0 * n.log2() + 2.0);
        }
    }
}
