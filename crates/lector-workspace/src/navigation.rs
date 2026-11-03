//! Back/forward navigation history.

use std::path::PathBuf;

use lector_text::Point;

const MAX_ENTRIES: usize = 100;
const NEAR_ROWS: u32 = 3;

/// A position in a file.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Location {
    /// Absolute path.
    pub path: PathBuf,
    /// 0-based row / byte column.
    pub point: Point,
}

impl Location {
    /// Same file and within a few rows.
    fn is_near(&self, other: &Location) -> bool {
        self.path == other.path && self.point.row.abs_diff(other.point.row) <= NEAR_ROWS
    }
}

/// Browser-style navigation history of jumps (go to definition, picker, ...).
///
/// `push` records the place you are leaving; `back(current)` returns the previous place and
/// remembers `current` for `forward`.
#[derive(Clone, Debug, Default)]
pub struct NavHistory {
    back: Vec<Location>,
    forward: Vec<Location>,
}

impl NavHistory {
    /// Empty history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `loc` (the position being left). Clears the forward stack. A location in the
    /// same file within 3 rows of the newest entry replaces it instead of adding a new one.
    pub fn push(&mut self, loc: Location) {
        self.forward.clear();
        match self.back.last_mut() {
            Some(last) if last.is_near(&loc) => *last = loc,
            _ => {
                self.back.push(loc);
                if self.back.len() > MAX_ENTRIES {
                    self.back.remove(0);
                }
            }
        }
    }

    /// Goes back from `current`; `None` when there is nowhere (distinct) to go.
    pub fn back(&mut self, current: Location) -> Option<Location> {
        while let Some(target) = self.back.pop() {
            if target.is_near(&current) {
                continue;
            }
            self.forward.push(current);
            return Some(target);
        }
        None
    }

    /// Goes forward from `current`.
    pub fn forward(&mut self, current: Location) -> Option<Location> {
        while let Some(target) = self.forward.pop() {
            if target.is_near(&current) {
                continue;
            }
            self.back.push(current);
            if self.back.len() > MAX_ENTRIES {
                self.back.remove(0);
            }
            return Some(target);
        }
        None
    }

    /// Whether `back` can (probably) go somewhere.
    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    /// Whether `forward` can (probably) go somewhere.
    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// Forgets everything.
    pub fn clear(&mut self) {
        self.back.clear();
        self.forward.clear();
    }
}
