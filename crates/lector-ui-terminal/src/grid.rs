//! Geometry and the per-row render cache.
//!
//! * [`grid_size`]: cell size + bounds → columns × rows for the core's `resize`.
//! * [`RowCache`]: one entry per viewport row holding whatever the renderer derived from a
//!   [`SnapshotRow`] (shaped text, merged backgrounds). The core hands back unchanged rows as
//!   the *same* `Arc`, so an entry is reused while `Arc::ptr_eq` holds and the style
//!   generation (theme, font) is unchanged; only new rows (the damaged ones) are re-derived.
//! * [`merge_backgrounds`]: adjacent cells with the same background become one rectangle.

use std::sync::Arc;

use lector_terminal::{Damage, SnapshotRow};

/// Inner padding of the terminal area, in px (left/right and top/bottom).
pub const PADDING_X: f32 = 6.0;
/// Top/bottom padding.
pub const PADDING_Y: f32 = 4.0;

/// Columns × rows that fit `width` × `height` px with `cell_width` × `cell_height` cells
/// (after padding). Never less than 2 × 1.
pub fn grid_size(width: f32, height: f32, cell_width: f32, cell_height: f32) -> (u16, u16) {
    let fit = |avail: f32, cell: f32, min: u16| -> u16 {
        if !(cell.is_finite() && cell > 0.0 && avail.is_finite()) {
            return min;
        }
        let n = (avail / cell).floor();
        if n < f32::from(min) {
            min
        } else {
            n.min(f32::from(u16::MAX)) as u16
        }
    };
    (
        fit(width - 2.0 * PADDING_X, cell_width, 2),
        fit(height - 2.0 * PADDING_Y, cell_height, 1),
    )
}

/// The viewport cell under a point relative to the terminal's top-left corner, and whether
/// it is in the right half of the cell. Clamped to the grid.
pub fn cell_at(
    x: f32,
    y: f32,
    cell_width: f32,
    cell_height: f32,
    cols: u16,
    rows: u16,
) -> (u32, u32, bool) {
    let fx = ((x - PADDING_X) / cell_width.max(1.0)).max(0.0);
    let fy = ((y - PADDING_Y) / cell_height.max(1.0)).max(0.0);
    let col = (fx.floor() as u32).min(u32::from(cols.saturating_sub(1)));
    let row = (fy.floor() as u32).min(u32::from(rows.saturating_sub(1)));
    let right = fx - fx.floor() >= 0.5 || fx.floor() as u32 > col;
    (row, col, right)
}

struct Entry<T> {
    row: Arc<SnapshotRow>,
    generation: u64,
    value: T,
}

/// Per-row cache of derived render data (see the module docs).
pub struct RowCache<T> {
    entries: Vec<Option<Entry<T>>>,
    /// Rows derived during the last [`RowCache::sync`] (for tests and tracing).
    pub last_shaped: usize,
}

impl<T> Default for RowCache<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            last_shaped: 0,
        }
    }
}

impl<T> RowCache<T> {
    /// Brings the cache in line with `rows`: entries whose row is not the same `Arc` (or
    /// whose generation differs) are derived again with `derive`. Returns how many were.
    /// `damage` is advisory: an undamaged row is still re-derived if its `Arc` changed.
    pub fn sync(
        &mut self,
        rows: &[Arc<SnapshotRow>],
        damage: &Damage,
        generation: u64,
        mut derive: impl FnMut(usize, &SnapshotRow) -> T,
    ) -> usize {
        let _ = damage;
        self.entries.resize_with(rows.len(), || None);
        let mut shaped = 0;
        for (i, row) in rows.iter().enumerate() {
            let fresh = matches!(
                &self.entries[i],
                Some(e) if e.generation == generation && Arc::ptr_eq(&e.row, row)
            );
            if !fresh {
                self.entries[i] = Some(Entry {
                    row: row.clone(),
                    generation,
                    value: derive(i, row),
                });
                shaped += 1;
            }
        }
        self.last_shaped = shaped;
        shaped
    }

    /// The derived value of viewport row `i`.
    pub fn get(&self, i: usize) -> Option<&T> {
        self.entries.get(i)?.as_ref().map(|e| &e.value)
    }

    /// Number of rows held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drops everything (e.g. when hidden for a long time).
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Merges per-cell backgrounds (`None` = default, not painted) into `(start_col, end_col,
/// color)` spans, end exclusive.
pub fn merge_backgrounds<C: PartialEq + Copy>(cells: &[Option<C>]) -> Vec<(u32, u32, C)> {
    let mut out: Vec<(u32, u32, C)> = Vec::new();
    for (i, bg) in cells.iter().enumerate() {
        let Some(c) = bg else { continue };
        let i = i as u32;
        match out.last_mut() {
            Some((_, end, last)) if *end == i && *last == *c => *end = i + 1,
            _ => out.push((i, i + 1, *c)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lector_terminal::{Emulator, EmulatorConfig, TermSize};

    #[test]
    fn cell_size_to_grid() {
        // 8 x 16 cells in 812 x 400: (812 - 12) / 8 = 100 cols, (400 - 8) / 16 = 24.5 → 24.
        assert_eq!(grid_size(812., 400., 8., 16.), (100, 24));
        assert_eq!(grid_size(5., 5., 8., 16.), (2, 1), "minimum size");
        assert_eq!(grid_size(800., 400., 0., 16.), (2, 24), "degenerate cell");
        assert_eq!(grid_size(f32::NAN, 400., 8., 16.).0, 2);
        assert_eq!(grid_size(1e9, 1e9, 1., 1.), (u16::MAX, u16::MAX));
        // Fractional cells (HiDPI fonts).
        assert_eq!(
            grid_size(12. + 7.8 * 80. + 0.5, 8. + 17.2 * 10. + 0.5, 7.8, 17.2),
            (80, 10)
        );
    }

    #[test]
    fn point_to_cell() {
        assert_eq!(cell_at(6., 4., 8., 16., 80, 24), (0, 0, false));
        assert_eq!(
            cell_at(6. + 8. * 3. + 5., 4. + 16. * 2. + 1., 8., 16., 80, 24),
            (2, 3, true)
        );
        assert_eq!(cell_at(-50., -50., 8., 16., 80, 24), (0, 0, false));
        assert_eq!(cell_at(1e5, 1e5, 8., 16., 80, 24), (23, 79, true));
    }

    #[test]
    fn row_cache_reuses_undamaged_rows() {
        let mut em = Emulator::new(EmulatorConfig {
            size: TermSize::new(20, 5),
            ..EmulatorConfig::default()
        });
        em.feed(b"one\r\ntwo\r\nthree");
        let s1 = em.snapshot();
        let mut cache = RowCache::<String>::default();
        let mut derived = Vec::new();
        let n = cache.sync(&s1.rows, &s1.damage, 0, |i, r| {
            derived.push(i);
            r.text().trim_end().to_string()
        });
        assert_eq!(n, 5);
        assert_eq!(cache.get(1).map(String::as_str), Some("two"));
        // Change only row 1: only it is derived again.
        em.feed(b"\x1b[2;1Hxyz");
        let s2 = em.snapshot();
        assert!(s2.damage.contains(1));
        let mut again = Vec::new();
        let n = cache.sync(&s2.rows, &s2.damage, 0, |i, r| {
            again.push(i);
            r.text().trim_end().to_string()
        });
        // Row 1 (the text) and the row the cursor left; never the untouched row 0.
        let damaged: Vec<usize> = (0..5).filter(|r| s2.damage.contains(*r as u32)).collect();
        assert!(again.contains(&1) && !again.contains(&0), "{again:?}");
        assert_eq!(again, damaged, "damage {:?}", s2.damage);
        assert_eq!(n, again.len());
        assert_eq!(cache.get(1).map(String::as_str), Some("xyz"));
        assert_eq!(cache.get(0).map(String::as_str), Some("one"));
        // Nothing changed: nothing derived but (at most) the cursor's row, which the
        // emulator always reports damaged.
        let s3 = em.snapshot();
        let mut third = Vec::new();
        cache.sync(&s3.rows, &s3.damage, 0, |i, r| {
            third.push(i);
            r.text().trim_end().to_string()
        });
        assert!(
            third.len() <= 1 && third.iter().all(|r| *r as u32 == s3.cursor.row),
            "{third:?}"
        );
        // A new style generation (theme / font change) re-derives everything.
        assert_eq!(cache.sync(&s3.rows, &s3.damage, 1, |_, r| r.text()), 5);
        // Resize: the cache follows the row count.
        em.resize(TermSize::new(20, 3));
        let s4 = em.snapshot();
        cache.sync(&s4.rows, &s4.damage, 1, |_, r| r.text());
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn backgrounds_merge() {
        let r = Some(1u8);
        let g = Some(2u8);
        assert_eq!(
            merge_backgrounds(&[r, r, None, g, g, g, r]),
            vec![(0, 2, 1), (3, 6, 2), (6, 7, 1)]
        );
        assert!(merge_backgrounds::<u8>(&[None, None]).is_empty());
    }
}
