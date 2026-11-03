//! Text coordinates.

use std::cmp::Ordering;
use std::ops::{Add, AddAssign, Sub};

/// A 0-based `(row, column)` position where `column` counts **bytes** (UTF-8).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Point {
    /// 0-based line number.
    pub row: u32,
    /// 0-based byte offset within the line.
    pub column: u32,
}

/// A 0-based `(row, column)` position where `column` counts **UTF-16 code
/// units** (the LSP default encoding).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PointUtf16 {
    /// 0-based line number.
    pub row: u32,
    /// 0-based UTF-16 code unit offset within the line.
    pub column: u32,
}

macro_rules! impl_point {
    ($name:ident) => {
        impl $name {
            /// The origin `(0, 0)`.
            pub const ZERO: Self = Self { row: 0, column: 0 };

            /// Creates a point.
            pub const fn new(row: u32, column: u32) -> Self {
                Self { row, column }
            }

            /// Whether this is `(0, 0)`.
            pub fn is_zero(&self) -> bool {
                self.row == 0 && self.column == 0
            }
        }

        impl Ord for $name {
            fn cmp(&self, other: &Self) -> Ordering {
                self.row
                    .cmp(&other.row)
                    .then(self.column.cmp(&other.column))
            }
        }

        impl PartialOrd for $name {
            fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                Some(self.cmp(other))
            }
        }

        /// Extent addition: `a + b` is the position reached by advancing
        /// from `a` over a text whose extent is `b`.
        impl Add for $name {
            type Output = Self;
            fn add(self, other: Self) -> Self {
                if other.row == 0 {
                    Self::new(self.row, self.column.saturating_add(other.column))
                } else {
                    Self::new(self.row.saturating_add(other.row), other.column)
                }
            }
        }

        impl AddAssign for $name {
            fn add_assign(&mut self, other: Self) {
                *self = *self + other;
            }
        }

        /// Extent subtraction (inverse of `+` for `self >= other`); saturates.
        impl Sub for $name {
            type Output = Self;
            fn sub(self, other: Self) -> Self {
                if self <= other {
                    Self::ZERO
                } else if self.row == other.row {
                    Self::new(0, self.column - other.column)
                } else {
                    Self::new(self.row - other.row, self.column)
                }
            }
        }

        impl From<(u32, u32)> for $name {
            fn from((row, column): (u32, u32)) -> Self {
                Self::new(row, column)
            }
        }
    };
}

impl_point!(Point);
impl_point!(PointUtf16);

/// An offset counted in Unicode scalar values (`char`s). Used as a rope
/// dimension.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CharOffset(pub usize);

/// An offset counted in UTF-16 code units. Used as a rope dimension.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OffsetUtf16(pub usize);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_arithmetic() {
        let a = Point::new(1, 4);
        assert_eq!(a + Point::new(0, 3), Point::new(1, 7));
        assert_eq!(a + Point::new(2, 3), Point::new(3, 3));
        assert_eq!(Point::new(3, 3) - a, Point::new(2, 3));
        assert_eq!(Point::new(1, 7) - a, Point::new(0, 3));
        assert_eq!(a - Point::new(5, 0), Point::ZERO);
        assert!(Point::new(0, 100) < Point::new(1, 0));
    }
}
