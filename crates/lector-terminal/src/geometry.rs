//! Sizes and coordinates shared by every part of the API.
//!
//! **Grid coordinates.** A [`GridPoint`] addresses one cell of the whole buffer
//! (scrollback + screen): `line` 0 is the top line of the live screen, positive
//! lines go down to `rows - 1`, negative lines are scrollback (`-1` is the
//! newest scrollback line). These coordinates do not change when the user scrolls
//! the view; they shift by one each time output scrolls a line into scrollback.
//!
//! **Viewport rows.** A [`Snapshot`](crate::Snapshot) holds the rows currently
//! shown, indexed `0..rows`. Viewport row `r` is grid line
//! `r as i32 - display_offset as i32`; use [`viewport_to_grid`] /
//! [`grid_to_viewport`] to convert.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};

/// Size of a terminal: character cells plus (optional) pixel size of one cell.
///
/// Pixel sizes are only used to answer `CSI 14 t`-style size queries; pass 0
/// when unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TermSize {
    /// Number of columns (at least 2 is enforced internally).
    pub cols: u16,
    /// Number of visible rows (at least 1 is enforced internally).
    pub rows: u16,
    /// Width of one cell in pixels (0 if unknown).
    pub cell_width: u16,
    /// Height of one cell in pixels (0 if unknown).
    pub cell_height: u16,
}

impl TermSize {
    /// A size of `cols` x `rows` cells with unknown pixel dimensions.
    pub const fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            cell_width: 0,
            cell_height: 0,
        }
    }

    /// The size clamped to the minimum the emulator supports (2 x 1).
    pub fn clamped(self) -> Self {
        Self {
            cols: self.cols.max(2),
            rows: self.rows.max(1),
            ..self
        }
    }
}

impl Default for TermSize {
    fn default() -> Self {
        Self::new(80, 24)
    }
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }

    fn screen_lines(&self) -> usize {
        self.rows as usize
    }

    fn columns(&self) -> usize {
        self.cols as usize
    }
}

/// One cell of the buffer in grid coordinates (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GridPoint {
    /// Line: 0 = top of the live screen, negative = scrollback.
    pub line: i32,
    /// Column, 0-based.
    pub col: u32,
}

impl GridPoint {
    /// Creates a point.
    pub const fn new(line: i32, col: u32) -> Self {
        Self { line, col }
    }

    pub(crate) fn to_alac(self) -> Point {
        Point::new(Line(self.line), Column(self.col as usize))
    }

    pub(crate) fn from_alac(p: Point) -> Self {
        Self {
            line: p.line.0,
            col: u32::try_from(p.column.0).unwrap_or(u32::MAX),
        }
    }
}

/// Which half of a cell the pointer is on; selections include a cell only when
/// the pointer crosses its middle, like every desktop terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Side {
    /// Left half of the cell.
    #[default]
    Left,
    /// Right half of the cell.
    Right,
}

impl Side {
    pub(crate) fn to_alac(self) -> alacritty_terminal::index::Side {
        match self {
            Side::Left => alacritty_terminal::index::Side::Left,
            Side::Right => alacritty_terminal::index::Side::Right,
        }
    }
}

/// Direction for searches and command-mark navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Towards older output (up).
    Previous,
    /// Towards newer output (down).
    Next,
}

/// How to scroll the view over scrollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScrollAmount {
    /// Scroll by this many lines; positive scrolls up into history.
    Lines(i32),
    /// One page up (towards history).
    PageUp,
    /// One page down.
    PageDown,
    /// Oldest scrollback line.
    Top,
    /// Live screen (no scrollback shown).
    Bottom,
}

/// Converts viewport row `row` to a grid line for the given display offset.
pub fn viewport_to_grid(row: u32, col: u32, display_offset: u32) -> GridPoint {
    let line = i64::from(row) - i64::from(display_offset);
    GridPoint {
        line: line.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        col,
    }
}

/// Converts a grid point to a viewport row, or `None` when it is not visible.
pub fn grid_to_viewport(point: GridPoint, display_offset: u32, rows: u16) -> Option<u32> {
    let row = i64::from(point.line) + i64::from(display_offset);
    (0..i64::from(rows)).contains(&row).then_some(row as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_round_trip() {
        let p = viewport_to_grid(3, 7, 10);
        assert_eq!(p, GridPoint::new(-7, 7));
        assert_eq!(grid_to_viewport(p, 10, 24), Some(3));
        assert_eq!(grid_to_viewport(GridPoint::new(-11, 0), 10, 24), None);
        assert_eq!(grid_to_viewport(GridPoint::new(14, 0), 10, 24), None);
    }

    #[test]
    fn clamped_size() {
        assert_eq!(TermSize::new(0, 0).clamped(), TermSize::new(2, 1));
    }
}
