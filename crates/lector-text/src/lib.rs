//! Text storage and coordinates for Lector.
//!
//! * [`sum_tree`] — a generic copy-on-write B+tree with summaries and
//!   seekable cursors (Zed-style), reusable for any ordered annotated data.
//! * [`Rope`] — an immutable-snapshot rope over `SumTree<Chunk>` with
//!   `O(log n)` conversions between byte offsets, [`Point`]s (row, byte
//!   column), [`PointUtf16`]s (row, UTF-16 column) and char offsets.
//! * [`MappedText`] — a read-only memory-mapped file with a compact line
//!   index, for files too large to copy into memory.
//! * [`Text`] — one cheap-to-clone, `Send + Sync` read API over both.
//!
//! Conventions: byte offsets are `usize`; rows are 0-based `u32`; rows are
//! separated by `'\n'` and line ranges exclude `"\n"` / `"\r\n"`.
//!
//! Line semantics (the same for [`Rope`] and [`MappedText`]): `"a\nb"` and
//! `"a\n"` have 2 lines, `""` has 1. [`Point`] columns are raw bytes, so a
//! `'\r'` before the `'\n'` is addressable even though line ranges exclude it.
//! Out-of-range columns clip to the row's `'\n'`, rows past the end clip to
//! `len()`, and offsets or points inside a multi-byte char clip to the char
//! start ([`Bias::Left`]). Strings returned by [`Text::Mapped`] are decoded
//! lossily, so their byte length can differ from the requested range when the
//! file is not valid UTF-8.
//!
//! [`Text::version`] is unique per distinct content snapshot (a process-wide
//! counter): clones keep it, every edit assigns a new one. Caches and
//! background results are keyed by it.
//!
//! This is the only Lector crate that uses `unsafe` (to memory-map files);
//! see the [`MappedText`] module docs for why that is acceptable.

#![deny(missing_docs)]

mod mapped;
mod point;
mod rope;
pub mod sum_tree;
mod text;

use std::sync::atomic::{AtomicU64, Ordering};

pub use mapped::{MappedText, BLOCK as MAPPED_INDEX_BLOCK};
pub use point::{CharOffset, OffsetUtf16, Point, PointUtf16};
pub use rope::{Chunk, Chunks, Rope, TextSummary};
pub use sum_tree::Bias;
pub use text::Text;

/// Returns a fresh, process-unique snapshot version.
pub(crate) fn next_version() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}
