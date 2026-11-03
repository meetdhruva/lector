//! Read-only memory-mapped text for very large files.
//!
//! # Soundness of the mapping
//!
//! `memmap2::Mmap::map` is `unsafe` because the OS does not stop other
//! processes from modifying or truncating the file while it is mapped:
//!
//! * **Concurrent modification** changes bytes under a `&[u8]`. Lector only
//!   ever reads the mapping as plain bytes and *never* relies on its content
//!   being valid UTF-8 — every slice handed out as `&str` is validated first
//!   (lossily replacing invalid sequences), and every index is bounds-checked
//!   against the length captured at open time. A concurrent writer can
//!   therefore produce garbled text, but not out-of-bounds reads or invalid
//!   `str`s.
//! * **Truncation** of the file below the mapped length makes accesses to
//!   the vanished pages raise `SIGBUS` (Unix) / an access violation
//!   (Windows). This cannot be prevented without signal handling, so it is a
//!   documented limitation: mapped text is only used for files above a large
//!   size threshold (where copying would be prohibitive), callers should
//!   watch the file and re-open it on change, and [`MappedText::is_stale`]
//!   lets them detect that the file changed since it was opened. This is
//!   the same trade-off made by tools such as ripgrep.
//!
//! # Line index
//!
//! The index stores, for every [`BLOCK`]-byte block of the file, the row
//! containing the block's first byte (`u32`) and the start offset of that
//! row (`u64`): 12 bytes per 8 KiB, i.e. ~0.15% of the file size. It is
//! built in parallel with `memchr` at open time (a 1 GB file is scanned in
//! well under a second on warm caches). Conversions binary-search or index
//! the blocks and then scan at most one block (plus, for very long lines,
//! the row itself).

use std::borrow::Cow;
use std::fs::File;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use memmap2::{Mmap, MmapOptions};
use rayon::prelude::*;

use crate::point::{Point, PointUtf16};
use crate::sum_tree::Bias;

/// Size of one line-index block in bytes.
pub const BLOCK: usize = 8192;

/// Read-only, memory-mapped text. Offsets are byte offsets into the file.
///
/// Content is treated as UTF-8; invalid sequences are replaced with U+FFFD
/// *per slice* when text is requested (so the byte length of returned text
/// may differ from the requested range when the file is not valid UTF-8).
/// Line semantics match [`Rope`](crate::Rope): rows are separated by `'\n'`,
/// and [`line_range`](Self::line_range) excludes `"\n"` / `"\r\n"`.
pub struct MappedText {
    path: PathBuf,
    map: Option<Mmap>,
    len: usize,
    stamp: (u64, Option<SystemTime>),
    newlines: u64,
    /// Row containing the first byte of each block.
    block_rows: Vec<u32>,
    /// Start offset of that row.
    block_row_starts: Vec<u64>,
    version: u64,
}

impl std::fmt::Debug for MappedText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedText")
            .field("path", &self.path)
            .field("len", &self.len)
            .field("lines", &self.line_count())
            .field("version", &self.version)
            .finish()
    }
}

fn to_u32(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Number of UTF-16 code units in `bytes`, where each byte that is not part
/// of a valid UTF-8 sequence counts as one unit (so counts are additive over
/// any split at a [`MappedText::clip_offset`] boundary).
fn utf16_len_lossy(bytes: &[u8]) -> usize {
    bytes
        .utf8_chunks()
        .map(|chunk| {
            let valid = chunk.valid().as_bytes();
            let units = valid.iter().filter(|&&b| (b as i8) >= -0x40).count()
                + valid.iter().filter(|&&b| b >= 0xF0).count();
            units + chunk.invalid().len()
        })
        .sum()
}

impl MappedText {
    /// Memory-maps `path` and builds its line index.
    pub fn open(path: &Path) -> io::Result<MappedText> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        if metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path is a directory",
            ));
        }
        let file_len = metadata.len();
        let len = usize::try_from(file_len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file too large to map"))?;
        let map = if len == 0 {
            None
        } else {
            // SAFETY: see the module docs. The mapping is read-only, its length
            // is fixed to the size observed above, and all accesses are bounds
            // checked and treat the bytes as untrusted (validated before use as
            // `str`). Concurrent truncation can still fault; that is a documented
            // limitation that callers mitigate by re-opening on change.
            let map = unsafe { MmapOptions::new().len(len).map(&file)? };
            #[cfg(unix)]
            let _ = map.advise(memmap2::Advice::Sequential);
            Some(map)
        };
        let mut text = MappedText {
            path: path.to_path_buf(),
            map,
            len,
            stamp: (file_len, metadata.modified().ok()),
            newlines: 0,
            block_rows: Vec::new(),
            block_row_starts: Vec::new(),
            version: crate::next_version(),
        };
        text.build_index();
        #[cfg(unix)]
        if let Some(map) = &text.map {
            let _ = map.advise(memmap2::Advice::Normal);
        }
        Ok(text)
    }

    fn build_index(&mut self) {
        let bytes = self.bytes_all();
        let per_block: Vec<(u32, u32)> = bytes
            .par_chunks(BLOCK)
            .with_min_len(32)
            .map(|block| {
                let count = memchr::memchr_iter(b'\n', block).count() as u32;
                let last = memchr::memrchr(b'\n', block).map_or(0, |i| i as u32 + 1);
                (count, last)
            })
            .collect();
        let mut rows = Vec::with_capacity(per_block.len());
        let mut starts = Vec::with_capacity(per_block.len());
        let mut row = 0u64;
        let mut row_start = 0u64;
        for (index, (count, last)) in per_block.into_iter().enumerate() {
            rows.push(to_u32(row));
            starts.push(row_start);
            row += u64::from(count);
            if last != 0 {
                row_start = (index * BLOCK) as u64 + u64::from(last);
            }
        }
        self.newlines = row;
        self.block_rows = rows;
        self.block_row_starts = starts;
    }

    fn bytes_all(&self) -> &[u8] {
        match &self.map {
            Some(map) => &map[..self.len.min(map.len())],
            None => &[],
        }
    }

    /// The mapped file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A process-unique id assigned when the file was opened.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Whether the file's size or modification time changed since it was
    /// opened (or it can no longer be read). Callers should re-open stale
    /// files; reading a file that shrank can fault (see module docs).
    pub fn is_stale(&self) -> bool {
        match std::fs::metadata(&self.path) {
            Ok(metadata) => (metadata.len(), metadata.modified().ok()) != self.stamp,
            Err(_) => true,
        }
    }

    /// Length in bytes (as of opening).
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the file is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Approximate heap memory used by the line index, in bytes.
    pub fn index_memory(&self) -> usize {
        self.block_rows.capacity() * 4 + self.block_row_starts.capacity() * 8
    }

    /// Raw bytes of `range` (clamped to the file length).
    pub fn bytes(&self, range: Range<usize>) -> &[u8] {
        let bytes = self.bytes_all();
        let start = range.start.min(bytes.len());
        let end = range.end.clamp(start, bytes.len());
        &bytes[start..end]
    }

    /// Number of lines: `'\n'` count + 1 (saturating at `u32::MAX`).
    pub fn line_count(&self) -> u32 {
        to_u32(self.newlines.saturating_add(1))
    }

    /// The point at the end of the text.
    pub fn max_point(&self) -> Point {
        self.offset_to_point(self.len)
    }

    /// Start offset of `row`, if it exists.
    fn row_start(&self, row: u32) -> Option<usize> {
        if u64::from(row) > self.newlines {
            return None;
        }
        if row == 0 {
            return Some(0);
        }
        let block = self
            .block_rows
            .partition_point(|&r| r <= row)
            .checked_sub(1)?;
        if self.block_rows[block] == row {
            return Some(self.block_row_starts[block] as usize);
        }
        let skip = (row - self.block_rows[block]) as usize;
        let from = block * BLOCK;
        memchr::memchr_iter(b'\n', &self.bytes_all()[from..])
            .nth(skip - 1)
            .map(|newline| from + newline + 1)
    }

    /// Offset of the `'\n'` ending `row` (or `len()` for the last row).
    fn row_end(&self, row: u32, row_start: usize) -> usize {
        let next_block = self.block_rows.partition_point(|&r| r <= row);
        let from = row_start.max(next_block.saturating_sub(1) * BLOCK);
        let bytes = self.bytes_all();
        if from >= bytes.len() {
            return bytes.len();
        }
        memchr::memchr(b'\n', &bytes[from..]).map_or(bytes.len(), |newline| from + newline)
    }

    /// Byte range of `row` excluding `"\n"` / `"\r\n"`; rows past the end
    /// yield `len()..len()`.
    pub fn line_range(&self, row: u32) -> Range<usize> {
        let Some(start) = self.row_start(row) else {
            return self.len..self.len;
        };
        let mut end = self.row_end(row, start);
        let bytes = self.bytes_all();
        if end < bytes.len() && end > start && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        start..end
    }

    /// Length of `row` in bytes, excluding its terminator.
    pub fn line_len(&self, row: u32) -> u32 {
        to_u32(self.line_range(row).len() as u64)
    }

    /// The text of `row` (lossy UTF-8), without terminator.
    pub fn line(&self, row: u32) -> Cow<'_, str> {
        String::from_utf8_lossy(self.bytes(self.line_range(row)))
    }

    /// Converts a byte offset to a point (clamped, clipped to a char boundary).
    pub fn offset_to_point(&self, offset: usize) -> Point {
        let offset = self.clip_offset(offset, Bias::Left);
        if self.block_rows.is_empty() {
            return Point::ZERO;
        }
        let block = (offset / BLOCK).min(self.block_rows.len() - 1);
        let block_start = block * BLOCK;
        let scanned = &self.bytes_all()[block_start..offset];
        let rows = memchr::memchr_iter(b'\n', scanned).count() as u64;
        let row = to_u32(u64::from(self.block_rows[block]) + rows);
        let column = match memchr::memrchr(b'\n', scanned) {
            Some(newline) => offset - (block_start + newline + 1),
            None => offset - self.block_row_starts[block] as usize,
        };
        Point::new(row, to_u32(column as u64))
    }

    /// Converts a point to a byte offset; columns past the end of a row clip
    /// to its `'\n'`, rows past the end clip to `len()`.
    pub fn point_to_offset(&self, point: Point) -> usize {
        let Some(start) = self.row_start(point.row) else {
            return self.len;
        };
        let end = self.row_end(point.row, start);
        let offset = start.saturating_add(point.column as usize).min(end);
        self.clip_offset(offset, Bias::Left)
    }

    /// Converts a byte offset to a UTF-16 point (each byte of an invalid
    /// UTF-8 sequence counts as one code unit).
    pub fn offset_to_point_utf16(&self, offset: usize) -> PointUtf16 {
        let offset = self.clip_offset(offset, Bias::Left);
        let point = self.offset_to_point(offset);
        let line_start = offset - point.column as usize;
        let column = utf16_len_lossy(&self.bytes_all()[line_start..offset]);
        PointUtf16::new(point.row, to_u32(column as u64))
    }

    /// Converts a UTF-16 point to a byte offset (clipping like
    /// [`point_to_offset`](Self::point_to_offset)).
    pub fn point_utf16_to_offset(&self, point: PointUtf16) -> usize {
        let Some(start) = self.row_start(point.row) else {
            return self.len;
        };
        let end = self.row_end(point.row, start);
        let target = point.column as usize;
        let mut units = 0usize;
        let mut offset = start;
        for chunk in self.bytes_all()[start..end].utf8_chunks() {
            for ch in chunk.valid().chars() {
                if units + ch.len_utf16() > target {
                    return offset;
                }
                units += ch.len_utf16();
                offset += ch.len_utf8();
            }
            for _ in chunk.invalid() {
                if units + 1 > target {
                    return offset;
                }
                units += 1;
                offset += 1;
            }
        }
        end
    }

    /// Clips `offset` to a char boundary. Bytes that are not part of a valid
    /// UTF-8 sequence are their own boundaries.
    pub fn clip_offset(&self, offset: usize, bias: Bias) -> usize {
        let bytes = self.bytes_all();
        if offset >= bytes.len() {
            return bytes.len();
        }
        if (bytes[offset] as i8) >= -0x40 {
            return offset;
        }
        for back in 1..=3 {
            let Some(lead_at) = offset.checked_sub(back) else {
                break;
            };
            let lead = bytes[lead_at];
            if (lead as i8) < -0x40 {
                continue; // another continuation byte
            }
            let width = match lead {
                0xC2..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF4 => 4,
                _ => return offset,
            };
            let end = lead_at + width;
            if width > back
                && end <= bytes.len()
                && std::str::from_utf8(&bytes[lead_at..end]).is_ok()
            {
                return match bias {
                    Bias::Left => lead_at,
                    Bias::Right => end,
                };
            }
            return offset;
        }
        offset
    }

    /// The text of `range` (clipped down to char boundaries), lossily decoded.
    pub fn text_for_range(&self, range: Range<usize>) -> Cow<'_, str> {
        let start = self.clip_offset(range.start, Bias::Left);
        let end = self.clip_offset(range.end.max(start), Bias::Left);
        String::from_utf8_lossy(&self.bytes_all()[start..end])
    }

    /// Iterates over `range` as string slices borrowed from the mapping;
    /// each invalid UTF-8 sequence yields `"\u{FFFD}"`.
    pub fn chunks_in_range(&self, range: Range<usize>) -> impl Iterator<Item = &str> + '_ {
        let start = self.clip_offset(range.start, Bias::Left);
        let end = self.clip_offset(range.end.max(start), Bias::Left);
        self.bytes_all()[start..end]
            .utf8_chunks()
            .flat_map(|chunk| {
                let replacement = (!chunk.invalid().is_empty()).then_some("\u{FFFD}");
                Some(chunk.valid())
                    .filter(|valid| !valid.is_empty())
                    .into_iter()
                    .chain(replacement)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rope;
    use std::io::Write;

    fn mapped(bytes: &[u8]) -> (tempfile::NamedTempFile, MappedText) {
        let mut file = tempfile::NamedTempFile::new().expect("tempfile");
        file.write_all(bytes).expect("write");
        file.flush().expect("flush");
        let text = MappedText::open(file.path()).expect("open");
        (file, text)
    }

    /// Compares every query against a rope built from the same (valid) text.
    fn check_against_rope(text: &str) {
        let (_file, mapped) = mapped(text.as_bytes());
        let rope = Rope::from(text);
        assert_eq!(mapped.len(), rope.len());
        assert_eq!(mapped.line_count(), rope.line_count());
        assert_eq!(mapped.max_point(), rope.max_point());
        for row in 0..rope.line_count() + 2 {
            assert_eq!(mapped.line_range(row), rope.line_range(row), "row {row}");
            for column in [0, 1, 3, 7, 1000] {
                let point = Point::new(row, column);
                assert_eq!(
                    mapped.point_to_offset(point),
                    rope.point_to_offset(point),
                    "{point:?}"
                );
                let point = PointUtf16::new(row, column);
                assert_eq!(
                    mapped.point_utf16_to_offset(point),
                    rope.point_utf16_to_offset(point)
                );
            }
        }
        let step = (text.len() / 2000).max(1);
        for offset in (0..=text.len() + 1).step_by(step) {
            assert_eq!(
                mapped.offset_to_point(offset),
                rope.offset_to_point(offset),
                "{offset}"
            );
            assert_eq!(
                mapped.offset_to_point_utf16(offset),
                rope.offset_to_point_utf16(offset)
            );
            assert_eq!(
                mapped.clip_offset(offset, Bias::Left),
                rope.clip_offset(offset, Bias::Left)
            );
            assert_eq!(
                mapped.clip_offset(offset, Bias::Right),
                rope.clip_offset(offset, Bias::Right)
            );
        }
        assert_eq!(
            mapped.chunks_in_range(0..text.len()).collect::<String>(),
            text
        );
        assert_eq!(
            mapped.text_for_range(3..text.len() / 2),
            rope.text_for_range(3..text.len() / 2)
        );
    }

    #[test]
    fn empty_file() {
        let (_file, text) = mapped(b"");
        assert_eq!(text.len(), 0);
        assert_eq!(text.line_count(), 1);
        assert_eq!(text.line_range(0), 0..0);
        assert_eq!(text.line(0), "");
        assert_eq!(text.offset_to_point(10), Point::ZERO);
        assert_eq!(text.point_to_offset(Point::new(3, 3)), 0);
        assert_eq!(text.chunks_in_range(0..10).count(), 0);
        check_against_rope("");
    }

    #[test]
    fn small_files() {
        check_against_rope("a");
        check_against_rope("\n");
        check_against_rope("no trailing newline\nlast");
        check_against_rope("crlf\r\nline two\r\n\r\nend\r");
        check_against_rope("unicode é€😀\nx😀y\n");
    }

    #[test]
    fn multi_block_files() {
        // Short lines, long lines spanning many blocks, CRLF across block edges.
        let mut text = String::new();
        for i in 0..5000 {
            text.push_str(&format!("line {i} with some text é\r\n"));
        }
        text.push_str(&"x".repeat(BLOCK * 3 + 17));
        text.push('\n');
        text.push_str(&"😀".repeat(BLOCK));
        text.push_str("\n\n\ntail");
        check_against_rope(&text);
        let (_file, mapped) = mapped(text.as_bytes());
        assert_eq!(mapped.line(4999), "line 4999 with some text é");
        assert!(mapped.index_memory() * 50 < text.len());
        assert!(!mapped.is_stale());
    }

    #[test]
    fn invalid_utf8_is_lossy() {
        let bytes = b"ok\xff\xfe line\n\xe2\x82 cut\nfine\n";
        let (_file, text) = mapped(bytes);
        assert_eq!(text.line_count(), 4);
        assert_eq!(text.line(0), "ok\u{FFFD}\u{FFFD} line");
        assert_eq!(text.line(1), "\u{FFFD} cut");
        assert_eq!(text.line(2), "fine");
        // Invalid bytes are their own boundaries.
        assert_eq!(text.clip_offset(3, Bias::Left), 3);
        assert_eq!(text.clip_offset(12, Bias::Left), 12);
        assert_eq!(
            text.chunks_in_range(0..bytes.len()).collect::<String>(),
            String::from_utf8_lossy(bytes)
        );
        // UTF-16 column counts one unit per replacement char.
        assert_eq!(text.offset_to_point_utf16(4), PointUtf16::new(0, 4));
        assert_eq!(text.point_utf16_to_offset(PointUtf16::new(0, 4)), 4);
        assert_eq!(text.point_utf16_to_offset(PointUtf16::new(0, 3)), 3);
    }

    #[test]
    fn detects_staleness() {
        let (mut file, text) = mapped(b"hello\n");
        assert!(!text.is_stale());
        file.write_all(b"more\n").expect("write");
        file.flush().expect("flush");
        assert!(text.is_stale());
        // The mapping still serves the original length safely.
        assert_eq!(text.line(0), "hello");
    }
}
