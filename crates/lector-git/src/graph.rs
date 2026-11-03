//! Lane assignment for a commit-graph column (like `git log --graph`).
//!
//! # Model
//!
//! Rows correspond 1:1 to the input commits (in display order, children before
//! parents as returned by [`crate::Repo::log`]). Each row has a node drawn in
//! column [`GraphRow::lane`]. [`GraphRow::edges`] are the line segments drawn
//! from this row's center down to the next row's center: `from_lane` is a column
//! in this row, `to_lane` a column in the next row. Lines arriving at a node from
//! above are the previous row's edges whose `to_lane` equals this row's `lane`.
//!
//! Lanes are compacted: when a lane ends, the lanes to its right shift left,
//! which shows up as edges with `from_lane != to_lane`.
//!
//! Parents that are not part of the input (e.g. cut off by a limit) keep their
//! lane open until the last row.

use std::collections::HashMap;

use crate::{CommitInfo, Oid};

/// What an edge represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum EdgeKind {
    /// A lane continuing downwards: either a lane passing this row, or the
    /// node's line to its first parent in its own lane.
    Straight,
    /// From a merge node to the lane of one of its second-or-later parents.
    MergeIn,
    /// From the node to another lane that already expects its first parent:
    /// this commit's branch forked off there (its own lane ends).
    BranchOut,
}

/// One segment between this row and the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GraphEdge {
    /// Column in this row.
    pub from_lane: u16,
    /// Column in the next row.
    pub to_lane: u16,
    /// Edge kind.
    pub kind: EdgeKind,
}

/// Graph layout for one commit row.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GraphRow {
    /// Column of this commit's node.
    pub lane: u16,
    /// Number of columns in use in this row (max of lanes at the node and below it).
    pub lanes: u16,
    /// Segments from this row to the next, sorted by `(from_lane, to_lane)`.
    pub edges: Vec<GraphEdge>,
}

fn lane_u16(index: usize) -> u16 {
    u16::try_from(index).unwrap_or(u16::MAX)
}

/// Compute graph rows for `commits` (deterministic; O(rows × lanes)).
pub fn graph(commits: &[CommitInfo]) -> Vec<GraphRow> {
    // Each lane holds the commit it expects next. Compact at the start of each row.
    let mut lanes: Vec<Oid> = Vec::new();
    let mut rows = Vec::with_capacity(commits.len());
    // Position lookup to avoid scanning; rebuilt on compaction.
    let mut position: HashMap<Oid, usize> = HashMap::new();

    for commit in commits {
        // 1. Find or allocate the node's lane.
        let node = match position.get(&commit.oid) {
            Some(&lane) => lane,
            None => {
                lanes.push(commit.oid);
                position.insert(commit.oid, lanes.len() - 1);
                lanes.len() - 1
            }
        };
        // `next[i]` = what lane `i` expects after this row (None = lane ends).
        let mut next: Vec<Option<Oid>> = lanes.iter().copied().map(Some).collect();
        next[node] = None;
        position.remove(&commit.oid);

        // Pending edges in pre-compaction coordinates: (from, to_index_in_next, kind)
        let mut pending: Vec<(usize, usize, EdgeKind)> = Vec::new();

        for (i, parent) in commit.parents.iter().enumerate() {
            let existing = next.iter().position(|expected| *expected == Some(*parent));
            match (i, existing) {
                (0, Some(lane)) => pending.push((node, lane, EdgeKind::BranchOut)),
                (0, None) => {
                    next[node] = Some(*parent);
                    pending.push((node, node, EdgeKind::Straight));
                }
                (_, Some(lane)) => pending.push((node, lane, EdgeKind::MergeIn)),
                (_, None) => {
                    next.push(Some(*parent));
                    pending.push((node, next.len() - 1, EdgeKind::MergeIn));
                }
            }
        }
        // Pass-through lanes.
        for (i, expected) in lanes.iter().enumerate() {
            if i != node && next.get(i).copied().flatten() == Some(*expected) {
                pending.push((i, i, EdgeKind::Straight));
            }
        }

        // Compact: map indices of `next` to positions after removing ended lanes.
        let mut map = vec![0usize; next.len()];
        let mut compacted = Vec::with_capacity(next.len());
        for (i, expected) in next.iter().enumerate() {
            map[i] = compacted.len();
            if let Some(oid) = expected {
                compacted.push(*oid);
            }
        }
        let mut edges: Vec<GraphEdge> = pending
            .into_iter()
            .map(|(from, to, kind)| GraphEdge {
                from_lane: lane_u16(from),
                to_lane: lane_u16(map[to]),
                kind,
            })
            .collect();
        edges.sort_by_key(|e| (e.from_lane, e.to_lane));
        edges.dedup();

        let width = lanes.len().max(compacted.len());
        rows.push(GraphRow {
            lane: lane_u16(node),
            lanes: lane_u16(width),
            edges,
        });

        lanes = compacted;
        position.clear();
        for (i, oid) in lanes.iter().enumerate() {
            position.entry(*oid).or_insert(i);
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Signature;

    fn oid(n: u8) -> Oid {
        let mut bytes = [0u8; 20];
        bytes[19] = n;
        bytes[0] = 1;
        Oid::from_bytes(&bytes).unwrap()
    }

    fn c(n: u8, parents: &[u8]) -> CommitInfo {
        CommitInfo {
            oid: oid(n),
            parents: parents.iter().map(|p| oid(*p)).collect(),
            author: Signature::default(),
            committer: Signature::default(),
            summary: String::new(),
            refs: Vec::new(),
        }
    }

    fn e(from: u16, to: u16, kind: EdgeKind) -> GraphEdge {
        GraphEdge {
            from_lane: from,
            to_lane: to,
            kind,
        }
    }

    use EdgeKind::*;

    #[test]
    fn linear() {
        let rows = graph(&[c(3, &[2]), c(2, &[1]), c(1, &[])]);
        assert_eq!(rows.len(), 3);
        for row in &rows[..2] {
            assert_eq!(row.lane, 0);
            assert_eq!(row.lanes, 1);
            assert_eq!(row.edges, vec![e(0, 0, Straight)]);
        }
        assert_eq!(rows[2].lane, 0);
        assert!(rows[2].edges.is_empty());
    }

    #[test]
    fn merge_and_fork() {
        // 4 = merge(3, 2); 3 -> 1; 2 -> 1; 1 root
        let rows = graph(&[c(4, &[3, 2]), c(3, &[1]), c(2, &[1]), c(1, &[])]);
        assert_eq!(rows[0].lane, 0);
        assert_eq!(rows[0].edges, vec![e(0, 0, Straight), e(0, 1, MergeIn)]);
        assert_eq!(rows[0].lanes, 2);
        assert_eq!(rows[1].lane, 0);
        assert_eq!(rows[1].edges, vec![e(0, 0, Straight), e(1, 1, Straight)]);
        assert_eq!(rows[2].lane, 1);
        // 2's parent 1 is already expected in lane 0: branch out, lane 1 ends.
        assert_eq!(rows[2].edges, vec![e(0, 0, Straight), e(1, 0, BranchOut)]);
        assert_eq!(rows[3].lane, 0);
        assert!(rows[3].edges.is_empty());
    }

    #[test]
    fn octopus() {
        let rows = graph(&[
            c(5, &[4, 3, 2]),
            c(4, &[1]),
            c(3, &[1]),
            c(2, &[1]),
            c(1, &[]),
        ]);
        assert_eq!(
            rows[0].edges,
            vec![e(0, 0, Straight), e(0, 1, MergeIn), e(0, 2, MergeIn)]
        );
        assert_eq!(rows[0].lanes, 3);
        assert_eq!(rows[2].lane, 1);
        assert_eq!(
            rows[2].edges,
            vec![e(0, 0, Straight), e(1, 0, BranchOut), e(2, 1, Straight)]
        );
        assert_eq!(rows[3].lane, 1);
        assert_eq!(rows[3].edges, vec![e(0, 0, Straight), e(1, 0, BranchOut)]);
        assert_eq!(rows[4].lane, 0);
    }

    #[test]
    fn multiple_heads() {
        // Two branch tips 3 and 2 sharing parent 1, plus an unrelated root 9.
        let rows = graph(&[c(3, &[1]), c(2, &[1]), c(9, &[]), c(1, &[])]);
        assert_eq!(rows[0].lane, 0);
        assert_eq!(rows[1].lane, 1);
        assert_eq!(rows[1].edges, vec![e(0, 0, Straight), e(1, 0, BranchOut)]);
        assert_eq!(rows[2].lane, 1);
        assert_eq!(rows[2].edges, vec![e(0, 0, Straight)]);
        assert_eq!(rows[3].lane, 0);
    }

    #[test]
    fn lanes_compact_after_end() {
        // 5 merge(4, 3); 4 -> 2; 3 -> 2 ; separate head 6 -> 2 first.
        let rows = graph(&[c(6, &[2]), c(5, &[4, 3]), c(4, &[2]), c(3, &[2]), c(2, &[])]);
        // 6 in lane 0 expecting 2; 5 in new lane 1 with parents 4 (lane 1) and 3 (lane 2)
        assert_eq!(rows[1].lane, 1);
        assert_eq!(
            rows[1].edges,
            vec![e(0, 0, Straight), e(1, 1, Straight), e(1, 2, MergeIn)]
        );
        // 4 -> 2 already in lane 0: branch out, lane 1 ends, lane 2 shifts to 1
        assert_eq!(rows[2].lane, 1);
        assert_eq!(
            rows[2].edges,
            vec![e(0, 0, Straight), e(1, 0, BranchOut), e(2, 1, Straight)]
        );
        assert_eq!(rows[3].lane, 1);
        assert_eq!(rows[4].lane, 0);
        assert!(rows.iter().all(|r| r.lane < r.lanes));
    }

    #[test]
    fn missing_parents_keep_lanes_open() {
        let rows = graph(&[c(3, &[2]), c(5, &[4])]);
        assert_eq!(rows[1].lane, 1);
        assert_eq!(rows[1].edges, vec![e(0, 0, Straight), e(1, 1, Straight)]);
    }

    #[test]
    fn deterministic() {
        let commits = [
            c(5, &[4, 3, 2]),
            c(4, &[1]),
            c(3, &[1]),
            c(2, &[1]),
            c(1, &[]),
        ];
        assert_eq!(graph(&commits), graph(&commits));
    }

    proptest::proptest! {
        /// Random DAGs in topological order: rows are well-formed and every edge
        /// lands on the lane where the target commit (or an open lane) appears.
        #[test]
        fn random_dags_are_consistent(
            parent_choices in proptest::collection::vec(proptest::collection::vec(0usize..1000, 0..4), 1..60)
        ) {
            let n = parent_choices.len();
            // Commit i (displayed at row i) may only have parents with larger index.
            let commits: Vec<CommitInfo> = (0..n)
                .map(|i| {
                    let mut parents: Vec<u8> = Vec::new();
                    for choice in &parent_choices[i] {
                        if i + 1 < n {
                            let p = (i + 1 + choice % (n - i - 1)) as u8;
                            if !parents.contains(&p) {
                                parents.push(p);
                            }
                        }
                    }
                    c(i as u8, &parents)
                })
                .collect();
            let rows = graph(&commits);
            proptest::prop_assert_eq!(rows.len(), n);
            for (i, row) in rows.iter().enumerate() {
                proptest::prop_assert!(row.lane < row.lanes);
                for edge in &row.edges {
                    proptest::prop_assert!(edge.from_lane < row.lanes);
                }
                if let Some(next) = rows.get(i + 1) {
                    for edge in &row.edges {
                        proptest::prop_assert!(edge.to_lane < next.lanes);
                    }
                    // Lines arriving at the next node come from rows that expect it.
                    let arriving = row.edges.iter().filter(|e| e.to_lane == next.lane).count();
                    let has_child = commits[..=i].iter().any(|c| c.parents.contains(&commits[i + 1].oid));
                    proptest::prop_assert_eq!(arriving > 0, has_child, "row {}", i + 1);
                }
                // Exactly one edge per parent leaves the node.
                let leaving = row.edges.iter().filter(|e| e.from_lane == row.lane).count();
                proptest::prop_assert_eq!(leaving, commits[i].parents.len());
            }
        }
    }
}
