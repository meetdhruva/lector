//! An immutable-snapshot rope built on [`SumTree`].

use std::fmt;
use std::ops::Range;

use arrayvec::ArrayString;
use rayon::prelude::*;

use crate::point::{CharOffset, OffsetUtf16, Point, PointUtf16};
use crate::sum_tree::{Bias, Cursor, Dimension, Item, SumTree, Summary};

/// Maximum bytes in one rope chunk.
#[cfg(not(test))]
pub(crate) const MAX_CHUNK: usize = 128;
/// Maximum bytes in one rope chunk (tiny in unit tests to exercise chunk
/// boundaries; must stay >= 5 so a 4-byte char plus "\r\n" handling fits).
#[cfg(test)]
pub(crate) const MAX_CHUNK: usize = 16;
const MIN_CHUNK: usize = MAX_CHUNK / 2;

/// Inputs larger than this compute chunk summaries in parallel.
const PARALLEL_THRESHOLD: usize = 1 << 20;

/// Aggregate metrics of a run of text. This is the [`Summary`] of a rope
/// chunk; [`Rope::summary`] describes the whole text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TextSummary {
    /// Length in bytes.
    pub len: usize,
    /// Number of Unicode scalar values.
    pub chars: usize,
    /// Length in UTF-16 code units.
    pub len_utf16: usize,
    /// Extent: number of `'\n'`s as `row`, and bytes after the last `'\n'` as `column`.
    pub lines: Point,
    /// The row with the most chars (the first such row on ties).
    pub longest_row: u32,
    /// Number of chars in [`longest_row`](Self::longest_row), excluding the newline.
    pub longest_row_chars: u32,
    /// Chars before the first `'\n'`.
    pub first_line_chars: u32,
    /// Chars after the last `'\n'`.
    pub last_line_chars: u32,
    /// UTF-16 code units after the last `'\n'`.
    pub last_line_len_utf16: u32,
}

fn count_chars(bytes: &[u8]) -> usize {
    // Every byte that is not a UTF-8 continuation byte starts a char.
    bytes.iter().filter(|&&b| (b as i8) >= -0x40).count()
}

fn count_utf16(bytes: &[u8]) -> usize {
    // Chars of 4 bytes (lead byte >= 0xF0) need a surrogate pair.
    count_chars(bytes) + bytes.iter().filter(|&&b| b >= 0xF0).count()
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl TextSummary {
    /// Computes the summary of `text`.
    pub fn from_text(text: &str) -> Self {
        let bytes = text.as_bytes();
        let mut summary = TextSummary {
            len: bytes.len(),
            ..Default::default()
        };
        let mut row = 0u32;
        let mut line_start = 0;
        for newline in memchr::memchr_iter(b'\n', bytes) {
            let line_chars = to_u32(count_chars(&bytes[line_start..newline]));
            if row == 0 {
                summary.first_line_chars = line_chars;
            }
            if line_chars > summary.longest_row_chars {
                summary.longest_row = row;
                summary.longest_row_chars = line_chars;
            }
            row += 1;
            line_start = newline + 1;
        }
        let last_line = &bytes[line_start..];
        let last_chars = to_u32(count_chars(last_line));
        if row == 0 {
            summary.first_line_chars = last_chars;
        }
        if last_chars > summary.longest_row_chars {
            summary.longest_row = row;
            summary.longest_row_chars = last_chars;
        }
        summary.last_line_chars = last_chars;
        summary.last_line_len_utf16 = to_u32(count_utf16(last_line));
        summary.lines = Point::new(row, to_u32(last_line.len()));
        summary.chars = count_chars(bytes);
        summary.len_utf16 = summary.chars + bytes.iter().filter(|&&b| b >= 0xF0).count();
        summary
    }

    /// The extent in UTF-16 coordinates.
    pub fn lines_utf16(&self) -> PointUtf16 {
        PointUtf16::new(self.lines.row, self.last_line_len_utf16)
    }
}

impl From<&str> for TextSummary {
    fn from(text: &str) -> Self {
        TextSummary::from_text(text)
    }
}

impl Summary for TextSummary {
    fn add_summary(&mut self, other: &Self) {
        let joined_chars = self.last_line_chars.saturating_add(other.first_line_chars);
        if joined_chars > self.longest_row_chars {
            self.longest_row = self.lines.row;
            self.longest_row_chars = joined_chars;
        }
        if other.longest_row_chars > self.longest_row_chars {
            self.longest_row = self.lines.row.saturating_add(other.longest_row);
            self.longest_row_chars = other.longest_row_chars;
        }
        if self.lines.row == 0 {
            self.first_line_chars = self.first_line_chars.saturating_add(other.first_line_chars);
        }
        if other.lines.row == 0 {
            self.last_line_chars = self.last_line_chars.saturating_add(other.first_line_chars);
            self.last_line_len_utf16 = self
                .last_line_len_utf16
                .saturating_add(other.last_line_len_utf16);
        } else {
            self.last_line_chars = other.last_line_chars;
            self.last_line_len_utf16 = other.last_line_len_utf16;
        }
        self.len += other.len;
        self.chars += other.chars;
        self.len_utf16 += other.len_utf16;
        self.lines += other.lines;
    }
}

impl<'a> Dimension<'a, TextSummary> for usize {
    fn add_summary(&mut self, summary: &'a TextSummary) {
        *self += summary.len;
    }
}

impl<'a> Dimension<'a, TextSummary> for Point {
    fn add_summary(&mut self, summary: &'a TextSummary) {
        *self += summary.lines;
    }
}

impl<'a> Dimension<'a, TextSummary> for PointUtf16 {
    fn add_summary(&mut self, summary: &'a TextSummary) {
        *self += summary.lines_utf16();
    }
}

impl<'a> Dimension<'a, TextSummary> for CharOffset {
    fn add_summary(&mut self, summary: &'a TextSummary) {
        self.0 += summary.chars;
    }
}

impl<'a> Dimension<'a, TextSummary> for OffsetUtf16 {
    fn add_summary(&mut self, summary: &'a TextSummary) {
        self.0 += summary.len_utf16;
    }
}

/// Largest char boundary `<= index` (clamped to `text.len()`).
pub(crate) fn floor_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    let mut index = index;
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Stretches of old text up to this many bytes are copied while batching edits
/// (one big push is far cheaper than a tree operation each); longer ones are
/// shared with the old rope.
const COPY_GAP: usize = 512;
/// The copy buffer of a batch is pushed into the new rope once this large.
const FLUSH_AT: usize = 16 * 1024;
/// A batch of edits rebuilds the touched leaves of the chunk tree (instead of
/// splicing edit by edit) when `edits * LEAF_REBUILD_BYTES >= text length`:
/// per edit, splicing costs about as much as walking this many bytes' worth of
/// leaves.
const LEAF_REBUILD_BYTES: usize = 256 * 1024;

/// Moves `cursor` forward to the chunk containing `offset` and returns
/// `offset` clipped down to a char boundary (`len` when past the last chunk).
fn skip_to(cursor: &mut Cursor<'_, Chunk, usize>, offset: usize, len: usize) -> usize {
    cursor.seek_forward(&offset, Bias::Right);
    match cursor.item() {
        Some(chunk) => {
            let chunk_start = *cursor.start();
            chunk_start + floor_char_boundary(chunk.as_str(), offset - chunk_start)
        }
        None => len,
    }
}

/// Pushes the gathered text into the rope being built.
fn flush_pending(out: &mut Rope, pending: &mut String) {
    if !pending.is_empty() {
        out.append(Rope::from_str(pending));
        pending.clear();
    }
}

/// Appends the old text `from..to` (`from` already on a char boundary, both
/// at least the cursor's position) to the rope being built: short stretches
/// are copied into `pending`, long ones are shared subtrees of the old rope
/// (after flushing `pending` so order is kept). Leaves `cursor` on the chunk
/// containing `to` and returns `to` clipped down to a char boundary.
fn copy_gap(
    cursor: &mut Cursor<'_, Chunk, usize>,
    out: &mut Rope,
    pending: &mut String,
    from: usize,
    to: usize,
    len: usize,
) -> usize {
    cursor.seek_forward(&from, Bias::Right);
    if to - from > COPY_GAP {
        // Longer than any chunk: head piece, shared middle, tail piece.
        let Some(chunk) = cursor.item() else {
            return len;
        };
        pending.push_str(&chunk.as_str()[from - *cursor.start()..]);
        flush_pending(out, pending);
        cursor.next();
        out.append(Rope::from_tree(cursor.slice(&to, Bias::Right)));
        return match cursor.item() {
            Some(chunk) => {
                let chunk_start = *cursor.start();
                let local_end = floor_char_boundary(chunk.as_str(), to - chunk_start);
                pending.push_str(&chunk.as_str()[..local_end]);
                chunk_start + local_end
            }
            None => len,
        };
    }
    let mut from = from;
    loop {
        let Some(chunk) = cursor.item() else {
            return len;
        };
        let chunk_start = *cursor.start();
        let text = chunk.as_str();
        let local_start = from - chunk_start;
        if to <= chunk_start + text.len() {
            let local_end = floor_char_boundary(text, to - chunk_start).max(local_start);
            pending.push_str(&text[local_start..local_end]);
            return chunk_start + local_end;
        }
        pending.push_str(&text[local_start..]);
        from = chunk_start + text.len();
        cursor.next();
        if pending.len() >= FLUSH_AT {
            flush_pending(out, pending);
        }
    }
}

/// Splits `text` into chunks like [`split_chunks`], but a short tail is
/// rebalanced with the chunk before it so no tiny chunk is left behind.
fn dense_chunks(text: &str) -> Vec<&str> {
    let mut pieces: Vec<&str> = split_chunks(text).collect();
    if pieces.len() >= 2 && pieces[pieces.len() - 1].len() < MIN_CHUNK {
        let last = pieces.pop().map_or(0, str::len);
        let prev = pieces.pop().map_or(0, str::len);
        let tail = &text[text.len() - prev - last..];
        if tail.len() <= MAX_CHUNK {
            pieces.push(tail);
        } else {
            let mut cut = floor_char_boundary(tail, tail.len() / 2);
            if cut == 0 {
                cut = ceil_char_boundary(tail, tail.len() / 2);
            }
            let bytes = tail.as_bytes();
            if cut > 1 && bytes[cut - 1] == b'\r' && bytes[cut] == b'\n' {
                cut -= 1;
            }
            pieces.push(&tail[..cut]);
            pieces.push(&tail[cut..]);
        }
    }
    pieces
}

/// Smallest char boundary `>= index` (clamped to `text.len()`).
pub(crate) fn ceil_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    let mut index = index;
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// A leaf of the rope: a short inline string. Chunks never split a UTF-8
/// char and avoid splitting `"\r\n"`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chunk(ArrayString<MAX_CHUNK>);

impl Chunk {
    fn new(text: &str) -> Self {
        let mut chunk = ArrayString::new();
        // `text` is at most MAX_CHUNK bytes by construction.
        let _ = chunk.try_push_str(text);
        Chunk(chunk)
    }

    /// The chunk's text.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    fn offset_to_point(&self, offset: usize) -> Point {
        let text = self.as_str();
        let offset = floor_char_boundary(text, offset);
        let bytes = &text.as_bytes()[..offset];
        let rows = memchr::memchr_iter(b'\n', bytes).count();
        let column = match memchr::memrchr(b'\n', bytes) {
            Some(newline) => offset - newline - 1,
            None => offset,
        };
        Point::new(to_u32(rows), to_u32(column))
    }

    fn offset_to_point_utf16(&self, offset: usize) -> PointUtf16 {
        let text = self.as_str();
        let offset = floor_char_boundary(text, offset);
        let bytes = &text.as_bytes()[..offset];
        let rows = memchr::memchr_iter(b'\n', bytes).count();
        let line_start = memchr::memrchr(b'\n', bytes).map_or(0, |newline| newline + 1);
        PointUtf16::new(to_u32(rows), to_u32(count_utf16(&bytes[line_start..])))
    }

    /// Byte offset where relative row `row` starts, or `None` if the chunk
    /// has fewer rows.
    fn row_start(&self, row: u32) -> Option<usize> {
        if row == 0 {
            return Some(0);
        }
        memchr::memchr_iter(b'\n', self.0.as_bytes())
            .nth(row as usize - 1)
            .map(|newline| newline + 1)
    }

    fn row_end(&self, row_start: usize) -> usize {
        memchr::memchr(b'\n', &self.0.as_bytes()[row_start..])
            .map_or(self.0.len(), |newline| row_start + newline)
    }

    fn point_to_offset(&self, point: Point) -> usize {
        let Some(row_start) = self.row_start(point.row) else {
            return self.0.len();
        };
        let row_end = self.row_end(row_start);
        let offset = row_start.saturating_add(point.column as usize).min(row_end);
        floor_char_boundary(self.as_str(), offset)
    }

    fn point_utf16_to_offset(&self, point: PointUtf16) -> usize {
        let Some(row_start) = self.row_start(point.row) else {
            return self.0.len();
        };
        let row_end = self.row_end(row_start);
        let mut units = 0usize;
        let target = point.column as usize;
        for (index, ch) in self.as_str()[row_start..row_end].char_indices() {
            if units >= target {
                return row_start + index;
            }
            units += ch.len_utf16();
            if units > target {
                // Target falls inside a surrogate pair: clip to the char start.
                return row_start + index;
            }
        }
        row_end
    }
}

impl Item for Chunk {
    type Summary = TextSummary;
    fn summary(&self) -> TextSummary {
        TextSummary::from_text(self.as_str())
    }
}

/// Splits `text` into pieces of at most [`MAX_CHUNK`] bytes on char
/// boundaries, avoiding splits between `'\r'` and `'\n'`.
fn split_chunks(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut end = rest.len().min(MAX_CHUNK);
        if end < rest.len() {
            end = floor_char_boundary(rest, end);
            let bytes = rest.as_bytes();
            if end > 1 && bytes[end - 1] == b'\r' && bytes[end] == b'\n' {
                end -= 1;
            }
        }
        let (piece, tail) = rest.split_at(end);
        rest = tail;
        Some(piece)
    })
}

/// A persistent rope: a balanced tree of small string chunks with cached
/// [`TextSummary`]s, giving `O(log n)` offset/point conversions and slicing.
///
/// Cloning is `O(1)`. Edits ([`replace`](Rope::replace), [`push`](Rope::push),
/// [`append`](Rope::append)) share unchanged subtrees with earlier clones.
///
/// # Line semantics
/// * Rows are separated by `'\n'`; [`line_count`](Rope::line_count) is the
///   number of `'\n'` plus one (`""` has 1 line, `"a\n"` has 2).
/// * [`line_range`](Rope::line_range) / [`line_len`](Rope::line_len) exclude
///   the `'\n'` **and** a `'\r'` immediately before it (CRLF files read as
///   expected). A lone `'\r'` at the very end of the text is kept.
/// * [`Point`] columns are raw byte offsets within the row, so the `'\r'` of
///   a CRLF line is addressable; columns past the end of a row clip to the
///   position of its `'\n'` (or the end of text).
/// * Offsets and points that fall inside a multi-byte char are clipped to
///   the start of that char.
#[derive(Clone)]
pub struct Rope {
    chunks: SumTree<Chunk>,
    version: u64,
}

impl Default for Rope {
    fn default() -> Self {
        Self::new()
    }
}

impl Rope {
    /// An empty rope.
    pub fn new() -> Self {
        Rope {
            chunks: SumTree::new(),
            version: crate::next_version(),
        }
    }

    /// Builds a rope from a string. Large inputs are summarised in parallel.
    #[allow(clippy::should_implement_trait)] // `FromStr` is implemented too
    pub fn from_str(text: &str) -> Self {
        let chunks = if text.len() >= PARALLEL_THRESHOLD {
            let pieces: Vec<&str> = split_chunks(text).collect();
            let items: Vec<(Chunk, TextSummary)> = pieces
                .par_iter()
                .map(|piece| (Chunk::new(piece), TextSummary::from_text(piece)))
                .collect();
            SumTree::from_items_and_summaries(items)
        } else {
            SumTree::from_iter(split_chunks(text).map(Chunk::new))
        };
        Rope {
            chunks,
            version: crate::next_version(),
        }
    }

    fn from_tree(chunks: SumTree<Chunk>) -> Self {
        Rope {
            chunks,
            version: crate::next_version(),
        }
    }

    /// A process-unique id of this content snapshot. Clones share it; every
    /// mutation assigns a fresh one.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The underlying chunk tree (for building custom cursors).
    pub fn chunk_tree(&self) -> &SumTree<Chunk> {
        &self.chunks
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.chunks.summary().len
    }

    /// Whether the rope is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of lines: `'\n'` count + 1.
    pub fn line_count(&self) -> u32 {
        self.chunks.summary().lines.row.saturating_add(1)
    }

    /// Summary of the whole text.
    pub fn summary(&self) -> TextSummary {
        *self.chunks.summary()
    }

    /// The point at the end of the text.
    pub fn max_point(&self) -> Point {
        self.chunks.summary().lines
    }

    /// The UTF-16 point at the end of the text.
    pub fn max_point_utf16(&self) -> PointUtf16 {
        self.chunks.summary().lines_utf16()
    }

    /// Summary of the text in `range` (clipped to char boundaries).
    pub fn text_summary_for_range(&self, range: Range<usize>) -> TextSummary {
        let mut summary = TextSummary::default();
        for chunk in self.chunks_in_range(range) {
            Summary::add_summary(&mut summary, &TextSummary::from_text(chunk));
        }
        summary
    }

    /// The byte at `offset`, if any.
    pub fn byte_at(&self, offset: usize) -> Option<u8> {
        if offset >= self.len() {
            return None;
        }
        let mut cursor = self.chunks.cursor::<usize>();
        cursor.seek(&offset, Bias::Right);
        let chunk = cursor.item()?;
        chunk.0.as_bytes().get(offset - *cursor.start()).copied()
    }

    /// Byte range of `row`, excluding its line terminator (`"\n"` or
    /// `"\r\n"`). Rows past the end yield `len()..len()`.
    pub fn line_range(&self, row: u32) -> Range<usize> {
        let max = self.max_point();
        if row > max.row {
            return self.len()..self.len();
        }
        let start = self.point_to_offset(Point::new(row, 0));
        if row == max.row {
            return start..self.len();
        }
        let mut end = self.point_to_offset(Point::new(row + 1, 0)) - 1;
        if end > start && self.byte_at(end - 1) == Some(b'\r') {
            end -= 1;
        }
        start..end
    }

    /// Length of `row` in bytes, excluding its line terminator.
    pub fn line_len(&self, row: u32) -> u32 {
        to_u32(self.line_range(row).len())
    }

    /// Converts a byte offset to a point (offsets are clamped to `len()` and
    /// clipped to a char boundary).
    pub fn offset_to_point(&self, offset: usize) -> Point {
        let offset = offset.min(self.len());
        let mut cursor = self.chunks.cursor::<(usize, Point)>();
        cursor.seek(&offset, Bias::Left);
        match cursor.item() {
            Some(chunk) => {
                let (start_offset, start_point) = *cursor.start();
                start_point + chunk.offset_to_point(offset - start_offset)
            }
            None => cursor.start().1,
        }
    }

    /// Converts a point to a byte offset. Columns past the end of a row clip
    /// to the row's `'\n'`; rows past the end clip to `len()`.
    pub fn point_to_offset(&self, point: Point) -> usize {
        if point >= self.max_point() {
            return self.len();
        }
        let mut cursor = self.chunks.cursor::<(Point, usize)>();
        cursor.seek(&point, Bias::Left);
        match cursor.item() {
            Some(chunk) => {
                let (start_point, start_offset) = *cursor.start();
                start_offset + chunk.point_to_offset(point - start_point)
            }
            None => cursor.start().1,
        }
    }

    /// Converts a byte offset to a UTF-16 point.
    pub fn offset_to_point_utf16(&self, offset: usize) -> PointUtf16 {
        let offset = offset.min(self.len());
        let mut cursor = self.chunks.cursor::<(usize, PointUtf16)>();
        cursor.seek(&offset, Bias::Left);
        match cursor.item() {
            Some(chunk) => {
                let (start_offset, start_point) = *cursor.start();
                start_point + chunk.offset_to_point_utf16(offset - start_offset)
            }
            None => cursor.start().1,
        }
    }

    /// Converts a UTF-16 point to a byte offset (clipping like
    /// [`point_to_offset`](Self::point_to_offset); a column inside a
    /// surrogate pair clips to the start of that char).
    pub fn point_utf16_to_offset(&self, point: PointUtf16) -> usize {
        if point >= self.max_point_utf16() {
            return self.len();
        }
        let mut cursor = self.chunks.cursor::<(PointUtf16, usize)>();
        cursor.seek(&point, Bias::Left);
        match cursor.item() {
            Some(chunk) => {
                let (start_point, start_offset) = *cursor.start();
                start_offset + chunk.point_utf16_to_offset(point - start_point)
            }
            None => cursor.start().1,
        }
    }

    /// Converts a point to a UTF-16 point.
    pub fn point_to_point_utf16(&self, point: Point) -> PointUtf16 {
        self.offset_to_point_utf16(self.point_to_offset(point))
    }

    /// Converts a UTF-16 point to a point.
    pub fn point_utf16_to_point(&self, point: PointUtf16) -> Point {
        self.offset_to_point(self.point_utf16_to_offset(point))
    }

    /// Converts a byte offset to a char offset.
    pub fn offset_to_char_offset(&self, offset: usize) -> CharOffset {
        let offset = offset.min(self.len());
        let mut cursor = self.chunks.cursor::<(usize, CharOffset)>();
        cursor.seek(&offset, Bias::Left);
        match cursor.item() {
            Some(chunk) => {
                let (start_offset, start_chars) = *cursor.start();
                let text = chunk.as_str();
                let local = floor_char_boundary(text, offset - start_offset);
                CharOffset(start_chars.0 + count_chars(&text.as_bytes()[..local]))
            }
            None => cursor.start().1,
        }
    }

    /// Converts a char offset to a byte offset (clamped to `len()`).
    pub fn char_offset_to_offset(&self, chars: CharOffset) -> usize {
        let mut cursor = self.chunks.cursor::<(CharOffset, usize)>();
        cursor.seek(&chars, Bias::Left);
        match cursor.item() {
            Some(chunk) => {
                let (start_chars, start_offset) = *cursor.start();
                let skip = chars.0.saturating_sub(start_chars.0);
                let local = chunk
                    .as_str()
                    .char_indices()
                    .nth(skip)
                    .map_or(chunk.0.len(), |(i, _)| i);
                start_offset + local
            }
            None => cursor.start().1,
        }
    }

    /// Clips `offset` to a char boundary, moving left or right per `bias`.
    pub fn clip_offset(&self, offset: usize, bias: Bias) -> usize {
        if offset >= self.len() {
            return self.len();
        }
        let mut cursor = self.chunks.cursor::<usize>();
        cursor.seek(&offset, Bias::Right);
        let Some(chunk) = cursor.item() else {
            return self.len();
        };
        let start = *cursor.start();
        let local = offset - start;
        start
            + match bias {
                Bias::Left => floor_char_boundary(chunk.as_str(), local),
                Bias::Right => ceil_char_boundary(chunk.as_str(), local),
            }
    }

    /// Clips a point to a valid position (row and column in range, on a char
    /// boundary).
    pub fn clip_point(&self, point: Point, bias: Bias) -> Point {
        let max = self.max_point();
        if point.row > max.row {
            return max;
        }
        let row_start = self.point_to_offset(Point::new(point.row, 0));
        let row_end = if point.row == max.row {
            self.len()
        } else {
            self.point_to_offset(Point::new(point.row + 1, 0)) - 1
        };
        let raw = row_start.saturating_add(point.column as usize).min(row_end);
        self.offset_to_point(self.clip_offset(raw, bias))
    }

    /// Iterates over the text in `range` as string slices, in order. Range
    /// ends are clamped to `len()` and clipped down to char boundaries.
    pub fn chunks_in_range(&self, range: Range<usize>) -> Chunks<'_> {
        Chunks::new(self, range, false)
    }

    /// Iterates over the text in `range` as string slices, last to first.
    pub fn reversed_chunks_in_range(&self, range: Range<usize>) -> Chunks<'_> {
        Chunks::new(self, range, true)
    }

    /// Iterates over the whole text as string slices.
    pub fn chunks(&self) -> Chunks<'_> {
        self.chunks_in_range(0..self.len())
    }

    /// The text of `range` (clipped as in
    /// [`chunks_in_range`](Self::chunks_in_range)) as a `String`.
    pub fn text_for_range(&self, range: Range<usize>) -> String {
        let mut text = String::with_capacity(range.len().min(self.len()));
        for chunk in self.chunks_in_range(range) {
            text.push_str(chunk);
        }
        text
    }

    /// Chars starting at `offset`.
    pub fn chars_at(&self, offset: usize) -> impl Iterator<Item = char> + '_ {
        self.chunks_in_range(offset..self.len())
            .flat_map(str::chars)
    }

    /// Chars before `offset`, nearest first.
    pub fn reversed_chars_at(&self, offset: usize) -> impl Iterator<Item = char> + '_ {
        self.reversed_chunks_in_range(0..offset)
            .flat_map(|chunk| chunk.chars().rev())
    }

    /// Chars of the whole text.
    pub fn chars(&self) -> impl Iterator<Item = char> + '_ {
        self.chars_at(0)
    }

    /// A new rope holding `range` of this one (shares unchanged subtrees).
    /// Range ends are clamped and clipped down to char boundaries.
    pub fn slice(&self, range: Range<usize>) -> Rope {
        let start = range.start.min(self.len());
        let end = range.end.clamp(start, self.len());
        let mut result = Rope::new();
        if start == end {
            return result;
        }
        let mut cursor = self.chunks.cursor::<usize>();
        cursor.seek(&start, Bias::Right);
        if let Some(chunk) = cursor.item() {
            let chunk_start = *cursor.start();
            let text = chunk.as_str();
            let local_start = floor_char_boundary(text, start - chunk_start);
            let local_end = floor_char_boundary(text, end.min(cursor.end()) - chunk_start);
            result.push(&text[local_start..local_end]);
        }
        if end > cursor.end() {
            cursor.next();
            result.append(Rope::from_tree(cursor.slice(&end, Bias::Right)));
            if let Some(chunk) = cursor.item() {
                let text = chunk.as_str();
                let local_end = floor_char_boundary(text, end - *cursor.start());
                result.push(&text[..local_end]);
            }
        }
        result
    }

    /// Appends `text`.
    pub fn push(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut rest = text;
        self.chunks.update_last(|last| {
            let space = MAX_CHUNK - last.0.len();
            if rest.len() <= space {
                last.0.push_str(rest);
                rest = "";
            } else if last.0.len() < MIN_CHUNK {
                let mut end = floor_char_boundary(rest, space);
                let bytes = rest.as_bytes();
                if end > 1 && bytes[end - 1] == b'\r' && bytes.get(end) == Some(&b'\n') {
                    end -= 1;
                }
                last.0.push_str(&rest[..end]);
                rest = &rest[end..];
            }
        });
        if !rest.is_empty() {
            if rest.len() >= PARALLEL_THRESHOLD {
                self.chunks.append(Rope::from_str(rest).chunks);
            } else {
                self.chunks.extend(split_chunks(rest).map(Chunk::new));
            }
        }
        self.version = crate::next_version();
    }

    /// Appends another rope (sharing its subtrees).
    pub fn append(&mut self, other: Rope) {
        if other.is_empty() {
            return;
        }
        self.version = crate::next_version();
        if let (Some(last), Some(first)) = (self.chunks.last(), other.chunks.first()) {
            let (last_len, first_len) = (last.0.len(), first.0.len());
            if last_len + first_len <= MAX_CHUNK && (last_len < MIN_CHUNK || first_len < MIN_CHUNK)
            {
                let first = first.clone();
                self.chunks
                    .update_last(|last| last.0.push_str(first.as_str()));
                let mut cursor = other.chunks.cursor::<()>();
                cursor.next();
                cursor.next();
                let rest = cursor.suffix();
                self.chunks.append(rest);
                return;
            }
        }
        self.chunks.append(other.chunks);
    }

    /// Replaces `range` with `text` (range ends are clamped and clipped down
    /// to char boundaries). `O(log n + text.len())`.
    pub fn replace(&mut self, range: Range<usize>, text: &str) {
        let start = self.clip_offset(range.start.min(self.len()), Bias::Left);
        let end = self.clip_offset(range.end.clamp(start, self.len()), Bias::Left);
        let mut result = self.slice(0..start);
        result.push(text);
        result.append(self.slice(end..self.len()));
        *self = result;
        self.version = crate::next_version();
    }

    /// Applies a batch of replacements in one pass.
    ///
    /// `edits` are `(range, replacement)` pairs in the coordinates of the text
    /// *before* the call, sorted by position and non-overlapping (this is what
    /// multi-cursor editing, replace-all and LSP edit lists produce). The new
    /// rope is assembled left to right from slices of the old one with a
    /// single forward cursor, so the cost is `O(k log n + inserted bytes)`
    /// with a small constant instead of `k` independent [`replace`](Self::replace)
    /// calls; long untouched stretches are shared with the old rope and short
    /// ones are copied. Range ends are clamped and clipped down to char
    /// boundaries like in `replace`; an edit that starts before the end of the
    /// previous one is moved to start there, so malformed input cannot panic
    /// (but is not meaningful). An empty list leaves the rope (and its
    /// version) unchanged.
    pub fn apply_edits<S: AsRef<str>>(&mut self, edits: &[(Range<usize>, S)]) {
        if edits.is_empty() {
            return;
        }
        let len = self.len();
        // Many edits spread over a big text: rebuilding the touched leaves of
        // the tree and sharing all others beats splicing edit by edit.
        let chunks = if len > 0 && edits.len() >= 8 && edits.len() * LEAF_REBUILD_BYTES >= len {
            self.apply_edits_by_leaves(edits)
        } else {
            self.apply_edits_by_splicing(edits)
        };
        self.chunks = chunks;
        self.version = crate::next_version();
    }

    /// Edit by edit with one forward cursor: shares long untouched stretches
    /// and copies short ones. `O(k log n)`.
    fn apply_edits_by_splicing<S: AsRef<str>>(
        &self,
        edits: &[(Range<usize>, S)],
    ) -> SumTree<Chunk> {
        let len = self.len();
        let mut out = Rope::new();
        let mut pending = String::new();
        let mut cursor = self.chunks.cursor::<usize>();
        let mut pos = 0usize;
        for (range, text) in edits {
            let start = range.start.clamp(pos, len);
            let end = range.end.clamp(start, len);
            let start = copy_gap(&mut cursor, &mut out, &mut pending, pos, start, len);
            pending.push_str(text.as_ref());
            if pending.len() >= FLUSH_AT {
                flush_pending(&mut out, &mut pending);
            }
            // Clip the end of the replaced range to a char boundary.
            pos = if end > start {
                skip_to(&mut cursor, end, len)
            } else {
                start
            };
        }
        copy_gap(&mut cursor, &mut out, &mut pending, pos, len, len);
        flush_pending(&mut out, &mut pending);
        out.chunks
    }

    /// Rebuilds only the part of the chunk tree that edits touch: chunks an
    /// edit reaches (a run of adjacent ones is re-chunked as one text) are
    /// replaced, every other chunk keeps its stored summary, leaves without
    /// edits are shared as they are, and the upper levels are assembled in one
    /// bottom-up pass. `O(leaves + edited bytes)` with a tiny per-leaf constant.
    fn apply_edits_by_leaves<S: AsRef<str>>(&self, edits: &[(Range<usize>, S)]) -> SumTree<Chunk> {
        let len = self.len();
        let normalized = |i: usize, pos: usize| {
            let (range, text) = &edits[i];
            let start = range.start.clamp(pos, len);
            (start, range.end.clamp(start, len), text.as_ref())
        };
        let mut nodes: Vec<SumTree<Chunk>> = Vec::new();
        // Chunks of the open group of touched leaves (as they will be stored).
        let mut group: Vec<(Chunk, TextSummary)> = Vec::new();
        let mut in_group = false;
        // The open run of chunks an edit reaches: their old text and the edits.
        let mut run = String::new();
        let mut run_edits: Vec<(usize, usize, &str)> = Vec::new();
        let mut run_start = 0usize;
        let mut run_open = false;
        let mut reach = 0usize;
        let mut pos = 0usize;
        let mut next = 0usize;
        let mut offset = 0usize;
        let mut leaves = self.chunks.leaves().peekable();
        while let Some(leaf) = leaves.next() {
            let (chunks, summaries) = leaf.leaf_parts().unwrap_or_default();
            let leaf_end = offset + leaf.summary().len;
            let is_last_leaf = leaves.peek().is_none();
            let starts_here =
                next < edits.len() && (is_last_leaf || normalized(next, pos).0 < leaf_end);
            if !in_group && !starts_here {
                nodes.push(leaf.clone());
                offset = leaf_end;
                continue;
            }
            in_group = true;
            for (k, (chunk, summary)) in chunks.iter().zip(summaries).enumerate() {
                let chunk_end = offset + summary.len;
                let is_last = is_last_leaf && k + 1 == chunks.len();
                let starts = next < edits.len() && (is_last || normalized(next, pos).0 < chunk_end);
                if !run_open && !starts {
                    group.push((chunk.clone(), *summary));
                    offset = chunk_end;
                    continue;
                }
                if !run_open {
                    run_open = true;
                    run_start = offset;
                    run.clear();
                    run_edits.clear();
                    reach = 0;
                }
                run.push_str(chunk.as_str());
                while next < edits.len() {
                    let (start, end, text) = normalized(next, pos);
                    if start >= chunk_end && !is_last {
                        break;
                    }
                    run_edits.push((start, end, text));
                    reach = reach.max(end);
                    pos = end;
                    next += 1;
                }
                offset = chunk_end;
                if reach > chunk_end && !is_last {
                    continue; // an edit reaches into the next chunk
                }
                run_open = false;
                let mut new_text = String::with_capacity(run.len() + 16);
                let mut done = 0usize;
                for &(start, end, text) in &run_edits {
                    let start =
                        floor_char_boundary(&run, start.saturating_sub(run_start)).max(done);
                    let end = floor_char_boundary(&run, end.saturating_sub(run_start)).max(start);
                    new_text.push_str(&run[done..start]);
                    new_text.push_str(text);
                    done = end;
                }
                new_text.push_str(&run[done..]);
                group.extend(
                    dense_chunks(&new_text)
                        .into_iter()
                        .map(|piece| (Chunk::new(piece), TextSummary::from_text(piece))),
                );
            }
            if !run_open {
                in_group = false;
                nodes.extend(SumTree::balanced_leaf_nodes(std::mem::take(&mut group)));
            }
        }
        SumTree::from_leaf_nodes(nodes)
    }

    /// Iterates over the lines in `rows` (without terminators).
    pub fn lines(&self, rows: Range<u32>) -> impl Iterator<Item = String> + '_ {
        rows.map(move |row| self.text_for_range(self.line_range(row)))
    }
}

impl fmt::Display for Rope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.chunks() {
            f.write_str(chunk)?;
        }
        Ok(())
    }
}

impl fmt::Debug for Rope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.to_string(), f)
    }
}

impl PartialEq for Rope {
    fn eq(&self, other: &Self) -> bool {
        if self.len() != other.len() {
            return false;
        }
        let mut left = self.chunks().flat_map(|c| c.as_bytes().iter().copied());
        let mut right = other.chunks().flat_map(|c| c.as_bytes().iter().copied());
        loop {
            match (left.next(), right.next()) {
                (None, None) => return true,
                (a, b) if a != b => return false,
                _ => {}
            }
        }
    }
}

impl Eq for Rope {}

impl From<&str> for Rope {
    fn from(text: &str) -> Self {
        Rope::from_str(text)
    }
}

impl From<String> for Rope {
    fn from(text: String) -> Self {
        Rope::from_str(&text)
    }
}

impl std::str::FromStr for Rope {
    type Err = std::convert::Infallible;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Ok(Rope::from_str(text))
    }
}

/// Iterator over string slices of a [`Rope`], created by
/// [`Rope::chunks_in_range`] and friends.
pub struct Chunks<'a> {
    cursor: Cursor<'a, Chunk, usize>,
    range: Range<usize>,
    reversed: bool,
}

impl<'a> Chunks<'a> {
    fn new(rope: &'a Rope, range: Range<usize>, reversed: bool) -> Self {
        let start = range.start.min(rope.len());
        let end = range.end.clamp(start, rope.len());
        let mut cursor = rope.chunks.cursor::<usize>();
        if reversed {
            cursor.seek(&end, Bias::Left);
        } else {
            cursor.seek(&start, Bias::Right);
        }
        Chunks {
            cursor,
            range: start..end,
            reversed,
        }
    }

    /// The byte offset of the next slice to be yielded (start of the
    /// remaining range when iterating forward, its end when reversed).
    pub fn offset(&self) -> usize {
        if self.reversed {
            self.range.end
        } else {
            self.range.start
        }
    }
}

impl<'a> Iterator for Chunks<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        loop {
            if self.range.is_empty() {
                return None;
            }
            let chunk = self.cursor.item()?;
            let text = chunk.as_str();
            let chunk_start = *self.cursor.start();
            let local_start =
                floor_char_boundary(text, self.range.start.saturating_sub(chunk_start));
            let local_end = floor_char_boundary(text, self.range.end - chunk_start);
            if self.reversed {
                self.range.end = chunk_start.max(self.range.start);
                self.cursor.prev();
            } else {
                self.range.start = chunk_start + text.len();
                self.cursor.next();
            }
            if local_start < local_end {
                return Some(&text[local_start..local_end]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Reference implementation of the line/point semantics on a `String`.
    struct Model<'a>(&'a str);

    impl Model<'_> {
        fn floor(&self, offset: usize) -> usize {
            floor_char_boundary(self.0, offset.min(self.0.len()))
        }
        fn line_starts(&self) -> Vec<usize> {
            std::iter::once(0)
                .chain(self.0.match_indices('\n').map(|(i, _)| i + 1))
                .collect()
        }
        fn offset_to_point(&self, offset: usize) -> Point {
            let offset = self.floor(offset);
            let prefix = &self.0[..offset];
            let row = prefix.matches('\n').count() as u32;
            let column = prefix.rfind('\n').map_or(offset, |i| offset - i - 1) as u32;
            Point::new(row, column)
        }
        fn offset_to_point_utf16(&self, offset: usize) -> PointUtf16 {
            let offset = self.floor(offset);
            let prefix = &self.0[..offset];
            let row = prefix.matches('\n').count() as u32;
            let line = &prefix[prefix.rfind('\n').map_or(0, |i| i + 1)..];
            PointUtf16::new(row, line.encode_utf16().count() as u32)
        }
        fn raw_line(&self, row: u32) -> Option<Range<usize>> {
            let starts = self.line_starts();
            let start = *starts.get(row as usize)?;
            let end = starts.get(row as usize + 1).map_or(self.0.len(), |s| s - 1);
            Some(start..end)
        }
        fn point_to_offset(&self, point: Point) -> usize {
            match self.raw_line(point.row) {
                None => self.0.len(),
                Some(line) => self.floor((line.start + point.column as usize).min(line.end)),
            }
        }
        fn point_utf16_to_offset(&self, point: PointUtf16) -> usize {
            match self.raw_line(point.row) {
                None => self.0.len(),
                Some(line) => {
                    let mut units = 0;
                    for (i, ch) in self.0[line.clone()].char_indices() {
                        if units + ch.len_utf16() > point.column as usize {
                            return line.start + i;
                        }
                        units += ch.len_utf16();
                    }
                    line.end
                }
            }
        }
        fn line_range(&self, row: u32) -> Range<usize> {
            let starts = self.line_starts();
            match self.raw_line(row) {
                None => self.0.len()..self.0.len(),
                Some(mut line) => {
                    let has_newline = (row as usize + 1) < starts.len();
                    if has_newline
                        && line.end > line.start
                        && self.0.as_bytes()[line.end - 1] == b'\r'
                    {
                        line.end -= 1;
                    }
                    line
                }
            }
        }
        fn summary(&self) -> (usize, usize, usize, u32, u32) {
            let mut longest = (0u32, 0u32);
            for (row, line) in self.0.split('\n').enumerate() {
                let chars = line.chars().count() as u32;
                if chars > longest.1 {
                    longest = (row as u32, chars);
                }
            }
            (
                self.0.len(),
                self.0.chars().count(),
                self.0.encode_utf16().count(),
                longest.0,
                longest.1,
            )
        }
    }

    /// Cheap consistency checks for big texts: content, summary, line index.
    fn check_light(rope: &Rope, text: &str) {
        assert_eq!(rope.to_string(), text);
        assert_eq!(rope.summary(), TextSummary::from_text(text));
        assert_eq!(rope.line_count() as usize, text.matches('\n').count() + 1);
        let model = Model(text);
        for offset in (0..=text.len()).step_by(97) {
            assert_eq!(rope.offset_to_point(offset), model.offset_to_point(offset));
        }
        for row in (0..rope.line_count()).step_by(31) {
            assert_eq!(
                rope.point_to_offset(Point::new(row, 0)),
                model.line_starts()[row as usize]
            );
        }
    }

    fn check_rope(rope: &Rope, text: &str) {
        let model = Model(text);
        assert_eq!(rope.to_string(), text);
        assert_eq!(rope.len(), text.len());
        assert_eq!(rope.line_count() as usize, text.matches('\n').count() + 1);
        let summary = rope.summary();
        assert_eq!(
            (
                summary.len,
                summary.chars,
                summary.len_utf16,
                summary.longest_row,
                summary.longest_row_chars
            ),
            model.summary()
        );
        assert_eq!(rope.max_point(), model.offset_to_point(text.len()));
        for offset in 0..=text.len() + 1 {
            let point = rope.offset_to_point(offset);
            assert_eq!(
                point,
                model.offset_to_point(offset),
                "offset_to_point({offset})"
            );
            assert_eq!(
                rope.offset_to_point_utf16(offset),
                model.offset_to_point_utf16(offset)
            );
            let clipped = model.floor(offset);
            assert_eq!(rope.clip_offset(offset, Bias::Left), clipped);
            assert_eq!(rope.point_to_offset(point), clipped);
            assert_eq!(
                rope.point_utf16_to_offset(rope.offset_to_point_utf16(offset)),
                clipped
            );
            assert_eq!(
                rope.offset_to_char_offset(offset).0,
                text[..clipped].chars().count(),
            );
            assert_eq!(
                rope.char_offset_to_offset(rope.offset_to_char_offset(offset)),
                clipped
            );
            let right = rope.clip_offset(offset, Bias::Right);
            assert!(right >= offset.min(text.len()) && text.is_char_boundary(right));
            assert_eq!(rope.chars_at(offset).collect::<String>(), text[clipped..]);
            assert_eq!(
                rope.reversed_chars_at(offset).collect::<String>(),
                text[..clipped].chars().rev().collect::<String>()
            );
        }
        let rows = rope.line_count() + 1;
        for row in 0..rows {
            assert_eq!(
                rope.line_range(row),
                model.line_range(row),
                "line_range({row})"
            );
            for column in [0, 1, 2, 3, 5, 100] {
                let point = Point::new(row, column);
                assert_eq!(
                    rope.point_to_offset(point),
                    model.point_to_offset(point),
                    "{point:?}"
                );
                let point = PointUtf16::new(row, column);
                assert_eq!(
                    rope.point_utf16_to_offset(point),
                    model.point_utf16_to_offset(point)
                );
            }
        }
    }

    #[test]
    fn basics() {
        assert_eq!(Rope::new().line_count(), 1);
        assert_eq!(Rope::from("a\nb").line_count(), 2);
        assert_eq!(Rope::from("a\n").line_count(), 2);
        let rope = Rope::from("ab\r\ncd\r\n\r\nx\r");
        assert_eq!(rope.line_range(0), 0..2);
        assert_eq!(rope.line_range(1), 4..6);
        assert_eq!(rope.line_range(2), 8..8);
        assert_eq!(rope.line_range(3), 10..12);
        assert_eq!(rope.line_len(3), 2);
        for text in [
            "",
            "a",
            "\n",
            "hello\nworld\n",
            "αβγ\n😀x\r\nyz\n\n€€€€€€€€€€€€€€€€€€€€€€\r\n",
            "line one is fairly long, to span chunks\r\nshort\n\u{10FFFF}\u{10FFFF}\n",
        ] {
            check_rope(&Rope::from(text), text);
        }
    }

    #[test]
    fn chunks_respect_char_and_crlf_boundaries() {
        let text = "ab\r\n".repeat(50) + &"é😀".repeat(40);
        let rope = Rope::from(text.as_str());
        let mut previous_ended_with_cr = false;
        for chunk in rope.chunk_tree().iter() {
            let chunk = chunk.as_str();
            assert!(chunk.len() <= MAX_CHUNK);
            assert!(!(previous_ended_with_cr && chunk.starts_with('\n')));
            previous_ended_with_cr = chunk.ends_with('\r');
        }
        check_rope(&rope, &text);
    }

    #[test]
    fn large_parallel_build() {
        let line = "fn main() { println!(\"héllo wörld\"); }\r\n";
        let text = line.repeat(PARALLEL_THRESHOLD / line.len() + 100);
        let rope = Rope::from(text.as_str());
        assert_eq!(rope.len(), text.len());
        assert_eq!(rope.summary(), TextSummary::from_text(&text));
        let row = 12345;
        let range = rope.line_range(row);
        assert_eq!(&text[range], line.trim_end());
        assert_eq!(rope.slice(100..5000).to_string(), text[100..5000]);
    }

    #[test]
    fn versions() {
        let mut rope = Rope::from("abc");
        let clone = rope.clone();
        assert_eq!(rope.version(), clone.version());
        rope.replace(1..2, "x");
        assert_ne!(rope.version(), clone.version());
        assert_eq!(rope.to_string(), "axc");
        assert_eq!(clone.to_string(), "abc");
    }

    fn text_strategy() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                Just("a".to_string()),
                Just("xyz".to_string()),
                Just("\n".to_string()),
                Just("\r\n".to_string()),
                Just("\r".to_string()),
                Just("é".to_string()),
                Just("€".to_string()),
                Just("😀".to_string()),
                "[a-z ]{0,20}",
            ],
            0..40,
        )
        .prop_map(|parts| parts.concat())
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

        #[test]
        fn rope_matches_string_model(text in text_strategy()) {
            let rope = Rope::from(text.as_str());
            check_rope(&rope, &text);
            // Building by pushes must give the same text and summary.
            let mut pushed = Rope::new();
            for piece in text.split_inclusive(['\n', 'z']) {
                pushed.push(piece);
            }
            prop_assert_eq!(pushed.summary(), rope.summary());
            prop_assert_eq!(&pushed, &rope);
        }

        #[test]
        fn slices_and_ranges(text in text_strategy(), a in 0usize..300, b in 0usize..300) {
            let rope = Rope::from(text.as_str());
            let (start, end) = (a.min(b).min(text.len()), a.max(b).min(text.len()));
            let model = Model(&text);
            let (s, e) = (model.floor(start), model.floor(end));
            prop_assert_eq!(rope.slice(start..end).to_string(), &text[s..e]);
            prop_assert_eq!(rope.text_for_range(start..end), &text[s..e]);
            let reversed: Vec<&str> = rope.reversed_chunks_in_range(start..end).collect();
            prop_assert_eq!(reversed.into_iter().rev().collect::<String>(), &text[s..e]);
            prop_assert_eq!(rope.text_summary_for_range(start..end), TextSummary::from_text(&text[s..e]));
        }

        #[test]
        fn replace_matches_model(
            text in text_strategy(),
            edits in proptest::collection::vec((0usize..300, 0usize..300, text_strategy()), 1..6),
        ) {
            let mut rope = Rope::from(text.as_str());
            let mut model = text.clone();
            for (a, b, insert) in edits {
                let (start, end) = (a.min(b).min(model.len()), a.max(b).min(model.len()));
                let (s, e) = (floor_char_boundary(&model, start), floor_char_boundary(&model, end));
                rope.replace(start..end, &insert);
                model.replace_range(s..e, &insert);
                prop_assert_eq!(rope.to_string(), model.as_str());
                prop_assert_eq!(rope.summary(), TextSummary::from_text(&model));
            }
            check_rope(&rope, &model);
        }

        /// Batched edits equal the same edits applied to a `String` (both the
        /// copy path for short gaps and the shared-subtree path for long ones).
        #[test]
        fn apply_edits_matches_model(
            text in text_strategy(),
            repeat in 1usize..14,
            cuts in proptest::collection::vec(0usize..8000, 0..24),
            inserts in proptest::collection::vec(text_strategy(), 12),
        ) {
            let text = text.repeat(repeat);
            let mut cuts = cuts;
            cuts.sort_unstable();
            let edits: Vec<(Range<usize>, &str)> = cuts
                .chunks_exact(2)
                .zip(&inserts)
                .map(|(c, t)| (c[0]..c[1], t.as_str()))
                .collect();
            let mut rope = Rope::from(text.as_str());
            let before = rope.clone();
            // The public entry point and both strategies on their own.
            let by_splicing = Rope { chunks: rope.apply_edits_by_splicing(&edits), version: 0 };
            let by_leaves = if text.is_empty() {
                None
            } else {
                Some(Rope { chunks: rope.apply_edits_by_leaves(&edits), version: 0 })
            };
            rope.apply_edits(&edits);
            let mut model = text.clone();
            // offsets always refer to the original text
            for (r, t) in edits.iter().rev() {
                let s = floor_char_boundary(&text, r.start.min(text.len()));
                let e = floor_char_boundary(&text, r.end.min(text.len()));
                model.replace_range(s..e, t);
            }
            for candidate in [Some(&rope), Some(&by_splicing), by_leaves.as_ref()].into_iter().flatten() {
                prop_assert_eq!(candidate.to_string(), model.as_str());
                prop_assert_eq!(candidate.summary(), TextSummary::from_text(&model));
                check_light(candidate, &model);
                // no tiny chunk is left behind by a rebuilt tail
                let sizes: Vec<usize> = candidate.chunk_tree().iter().map(|c| c.as_str().len()).collect();
                prop_assert!(sizes.iter().all(|&n| n > 0), "empty chunk in {sizes:?}");
            }
            // the old snapshot is untouched
            prop_assert_eq!(before.to_string(), text);
        }
    }

    #[test]
    fn apply_edits_basics() {
        let mut r = Rope::from("hello world");
        let v = r.version();
        r.apply_edits::<&str>(&[]);
        assert_eq!(r.version(), v, "no edits, no new version");
        r.apply_edits(&[(0..5, "HELLO"), (6..11, "there"), (11..11, "!")]);
        assert_eq!(r.to_string(), "HELLO there!");
        assert_ne!(r.version(), v);
        // inserts at the same offset keep their order; pure deletion; clamping
        let mut r = Rope::from("abc");
        r.apply_edits(&[(1..1, "x"), (1..1, "y"), (2..99, "")]);
        assert_eq!(r.to_string(), "axyb");
        let mut r = Rope::from("");
        r.apply_edits(&[(0..0, "new")]);
        assert_eq!(r.to_string(), "new");
        // malformed (overlapping / unsorted) input never panics and keeps text sane
        let mut r = Rope::from("0123456789");
        r.apply_edits(&[(4..8, "A"), (2..6, "B")]);
        assert_eq!(r.to_string(), "0123AB89");
        // char boundaries: an edit inside a multi-byte char is clipped down
        let mut r = Rope::from("a\u{e9}b");
        r.apply_edits(&[(2..3, "X")]);
        assert_eq!(r.to_string(), "aXb");
    }

    #[test]
    fn apply_edits_shares_long_gaps_and_stays_balanced() {
        let text: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
        let mut r = Rope::from(text.as_str());
        let edits: Vec<(Range<usize>, &str)> = (0..500)
            .map(|i| {
                let at = i * 700;
                (at..at, "x")
            })
            .collect();
        r.apply_edits(&edits);
        let mut model = text.clone();
        for (rg, t) in edits.iter().rev() {
            let e = floor_char_boundary(&model, rg.start);
            model.insert_str(e, t);
        }
        check_light(&r, &model);
        assert_eq!(r.to_string(), model);
        // later edits still work on the result
        r.apply_edits(&[(0..1, "L"), (r.len()..r.len(), "end")]);
        assert!(r.to_string().starts_with("Lline 0") && r.to_string().ends_with("end"));
    }
}
