//! The minimal text interface tree-sitter logic needs, implemented for plain
//! strings and for `lector_text::Text`.

use std::borrow::Cow;
use std::ops::Range;

pub use lector_text::Point;
use lector_text::{Rope, Text};

/// Read-only text access used by every operation in this crate.
///
/// Implemented for `str`, `String` and `lector_text::Text`, so public APIs
/// can be called with `&Text` (the contract) or `&str` (tests, tools).
pub trait TextInput {
    /// Length in bytes.
    fn len(&self) -> usize;
    /// True when the text is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Chunks covering `range`, in order. Chunk boundaries are char boundaries.
    fn chunks_in_range(&self, range: Range<usize>) -> Box<dyn Iterator<Item = &str> + '_>;
    /// The text of `range` (borrowed when contiguous).
    fn text_for_range(&self, range: Range<usize>) -> Cow<'_, str>;
    /// Row/byte-column of `offset`.
    fn offset_to_point(&self, offset: usize) -> Point;
}

impl TextInput for str {
    fn len(&self) -> usize {
        str::len(self)
    }

    fn chunks_in_range(&self, range: Range<usize>) -> Box<dyn Iterator<Item = &str> + '_> {
        let end = range.end.min(self.len());
        let start = range.start.min(end);
        Box::new(std::iter::once(self.get(start..end).unwrap_or("")))
    }

    fn text_for_range(&self, range: Range<usize>) -> Cow<'_, str> {
        let end = range.end.min(self.len());
        let start = range.start.min(end);
        match self.get(start..end) {
            Some(s) => Cow::Borrowed(s),
            None => Cow::Owned(String::from_utf8_lossy(&self.as_bytes()[start..end]).into_owned()),
        }
    }

    fn offset_to_point(&self, offset: usize) -> Point {
        let offset = offset.min(self.len());
        let bytes = &self.as_bytes()[..offset];
        let row = bytes.iter().filter(|b| **b == b'\n').count();
        let line_start = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        Point {
            row: row as u32,
            column: (offset - line_start) as u32,
        }
    }
}

impl TextInput for String {
    fn len(&self) -> usize {
        String::len(self)
    }
    fn chunks_in_range(&self, range: Range<usize>) -> Box<dyn Iterator<Item = &str> + '_> {
        self.as_str().chunks_in_range(range)
    }
    fn text_for_range(&self, range: Range<usize>) -> Cow<'_, str> {
        self.as_str().text_for_range(range)
    }
    fn offset_to_point(&self, offset: usize) -> Point {
        self.as_str().offset_to_point(offset)
    }
}

impl TextInput for Text {
    fn len(&self) -> usize {
        Text::len(self)
    }
    fn chunks_in_range(&self, range: Range<usize>) -> Box<dyn Iterator<Item = &str> + '_> {
        Text::chunks_in_range(self, range)
    }
    fn text_for_range(&self, range: Range<usize>) -> Cow<'_, str> {
        Text::text_for_range(self, range)
    }
    fn offset_to_point(&self, offset: usize) -> Point {
        Text::offset_to_point(self, offset)
    }
}

impl TextInput for Rope {
    fn len(&self) -> usize {
        Rope::len(self)
    }
    fn chunks_in_range(&self, range: Range<usize>) -> Box<dyn Iterator<Item = &str> + '_> {
        Box::new(Rope::chunks_in_range(self, range))
    }
    fn text_for_range(&self, range: Range<usize>) -> Cow<'_, str> {
        Cow::Owned(Rope::text_for_range(self, range))
    }
    fn offset_to_point(&self, offset: usize) -> Point {
        Rope::offset_to_point(self, offset)
    }
}

/// Sequential chunk reader for tree-sitter's input callback. Reuses the chunk
/// iterator while reads are contiguous.
pub(crate) struct ChunkReader<'a, T: TextInput + ?Sized> {
    text: &'a T,
    len: usize,
    next_offset: usize,
    iter: Option<Box<dyn Iterator<Item = &'a str> + 'a>>,
}

impl<'a, T: TextInput + ?Sized> ChunkReader<'a, T> {
    pub(crate) fn new(text: &'a T) -> Self {
        ChunkReader {
            text,
            len: text.len(),
            next_offset: 0,
            iter: None,
        }
    }

    /// The chunk starting at `offset` (empty at end of text).
    pub(crate) fn read(&mut self, offset: usize) -> &'a [u8] {
        if offset >= self.len {
            return &[];
        }
        if self.next_offset != offset {
            self.iter = None;
        }
        if self.iter.is_none() {
            self.iter = Some(self.text.chunks_in_range(offset..self.len));
            self.next_offset = offset;
        }
        if let Some(iter) = self.iter.as_mut() {
            for chunk in iter.by_ref() {
                if !chunk.is_empty() {
                    self.next_offset += chunk.len();
                    return chunk.as_bytes();
                }
            }
        }
        self.iter = None;
        &[]
    }
}
