//! [`Text`]: one read API over ropes and memory-mapped files.

use std::borrow::Cow;
use std::fmt;
use std::io;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use crate::mapped::MappedText;
use crate::point::{Point, PointUtf16};
use crate::rope::Rope;
use crate::sum_tree::Bias;

/// An immutable text snapshot: either an in-memory [`Rope`] or a
/// memory-mapped file. Cloning is `O(1)`; `Text` is `Send + Sync`.
///
/// All offsets are byte offsets; see [`Rope`] for line semantics (shared by
/// both variants). For [`Text::Mapped`], text accessors decode lossily, so
/// the returned strings may differ in byte length from the requested range
/// when the file contains invalid UTF-8.
#[derive(Clone)]
pub enum Text {
    /// In-memory text.
    Rope(Rope),
    /// A memory-mapped (read-only, large) file.
    Mapped(Arc<MappedText>),
}

impl Default for Text {
    fn default() -> Self {
        Text::Rope(Rope::new())
    }
}

impl fmt::Debug for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Text::Rope(rope) => f
                .debug_struct("Text::Rope")
                .field("len", &rope.len())
                .field("version", &rope.version())
                .finish(),
            Text::Mapped(mapped) => fmt::Debug::fmt(mapped, f),
        }
    }
}

impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.chunks() {
            f.write_str(chunk)?;
        }
        Ok(())
    }
}

impl From<Rope> for Text {
    fn from(rope: Rope) -> Self {
        Text::Rope(rope)
    }
}

impl From<&str> for Text {
    fn from(text: &str) -> Self {
        Text::from_str(text)
    }
}

impl From<String> for Text {
    fn from(text: String) -> Self {
        Text::from_str(&text)
    }
}

impl std::str::FromStr for Text {
    type Err = std::convert::Infallible;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Ok(Text::from_str(text))
    }
}

impl Text {
    /// An in-memory text.
    #[allow(clippy::should_implement_trait)] // `FromStr` is implemented too
    pub fn from_str(text: &str) -> Text {
        Text::Rope(Rope::from_str(text))
    }

    /// Memory-maps the file at `path` (see [`MappedText`]).
    pub fn open_mapped(path: &Path) -> io::Result<Text> {
        Ok(Text::Mapped(Arc::new(MappedText::open(path)?)))
    }

    /// A process-unique id of this content snapshot (from a global atomic
    /// counter). Clones keep the version; [`replace`](Self::replace) and
    /// every new text get a fresh one.
    pub fn version(&self) -> u64 {
        match self {
            Text::Rope(rope) => rope.version(),
            Text::Mapped(mapped) => mapped.version(),
        }
    }

    /// Whether this is a memory-mapped text.
    pub fn is_mapped(&self) -> bool {
        matches!(self, Text::Mapped(_))
    }

    /// The rope, if in memory.
    pub fn as_rope(&self) -> Option<&Rope> {
        match self {
            Text::Rope(rope) => Some(rope),
            Text::Mapped(_) => None,
        }
    }

    /// The mapping, if memory-mapped.
    pub fn as_mapped(&self) -> Option<&Arc<MappedText>> {
        match self {
            Text::Rope(_) => None,
            Text::Mapped(mapped) => Some(mapped),
        }
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        match self {
            Text::Rope(rope) => rope.len(),
            Text::Mapped(mapped) => mapped.len(),
        }
    }

    /// Whether the text is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of lines (`'\n'` count + 1).
    pub fn line_count(&self) -> u32 {
        match self {
            Text::Rope(rope) => rope.line_count(),
            Text::Mapped(mapped) => mapped.line_count(),
        }
    }

    /// The point at the end of the text.
    pub fn max_point(&self) -> Point {
        match self {
            Text::Rope(rope) => rope.max_point(),
            Text::Mapped(mapped) => mapped.max_point(),
        }
    }

    /// Byte range of `row`, excluding `"\n"` / `"\r\n"`.
    pub fn line_range(&self, row: u32) -> Range<usize> {
        match self {
            Text::Rope(rope) => rope.line_range(row),
            Text::Mapped(mapped) => mapped.line_range(row),
        }
    }

    /// Length of `row` in bytes, excluding its terminator.
    pub fn line_len(&self, row: u32) -> u32 {
        match self {
            Text::Rope(rope) => rope.line_len(row),
            Text::Mapped(mapped) => mapped.line_len(row),
        }
    }

    /// Converts a byte offset to a point.
    pub fn offset_to_point(&self, offset: usize) -> Point {
        match self {
            Text::Rope(rope) => rope.offset_to_point(offset),
            Text::Mapped(mapped) => mapped.offset_to_point(offset),
        }
    }

    /// Converts a point to a byte offset (clipping).
    pub fn point_to_offset(&self, point: Point) -> usize {
        match self {
            Text::Rope(rope) => rope.point_to_offset(point),
            Text::Mapped(mapped) => mapped.point_to_offset(point),
        }
    }

    /// Converts a byte offset to a UTF-16 point.
    pub fn offset_to_point_utf16(&self, offset: usize) -> PointUtf16 {
        match self {
            Text::Rope(rope) => rope.offset_to_point_utf16(offset),
            Text::Mapped(mapped) => mapped.offset_to_point_utf16(offset),
        }
    }

    /// Converts a UTF-16 point to a byte offset (clipping).
    pub fn point_utf16_to_offset(&self, point: PointUtf16) -> usize {
        match self {
            Text::Rope(rope) => rope.point_utf16_to_offset(point),
            Text::Mapped(mapped) => mapped.point_utf16_to_offset(point),
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

    /// Clips `offset` to a char boundary.
    pub fn clip_offset(&self, offset: usize, bias: Bias) -> usize {
        match self {
            Text::Rope(rope) => rope.clip_offset(offset, bias),
            Text::Mapped(mapped) => mapped.clip_offset(offset, bias),
        }
    }

    /// The text of `range` (ends clamped and clipped down to char
    /// boundaries). Borrows when the range lies within one rope chunk or is
    /// valid UTF-8 in a mapping; allocates otherwise.
    pub fn text_for_range(&self, range: Range<usize>) -> Cow<'_, str> {
        match self {
            Text::Rope(rope) => {
                let mut chunks = rope.chunks_in_range(range);
                let Some(first) = chunks.next() else {
                    return Cow::Borrowed("");
                };
                match chunks.next() {
                    None => Cow::Borrowed(first),
                    Some(second) => {
                        let mut text = String::with_capacity(first.len() + second.len() * 2);
                        text.push_str(first);
                        text.push_str(second);
                        text.extend(chunks);
                        Cow::Owned(text)
                    }
                }
            }
            Text::Mapped(mapped) => mapped.text_for_range(range),
        }
    }

    /// The text of `row` without its terminator.
    pub fn line(&self, row: u32) -> Cow<'_, str> {
        self.text_for_range(self.line_range(row))
    }

    /// The lines in `rows` (clamped to existing rows), without terminators.
    pub fn lines(&self, rows: Range<u32>) -> impl Iterator<Item = Cow<'_, str>> {
        let end = rows.end.min(self.line_count());
        (rows.start.min(end)..end).map(move |row| self.line(row))
    }

    /// Iterates over the text in `range` as string slices.
    pub fn chunks_in_range(&self, range: Range<usize>) -> Box<dyn Iterator<Item = &str> + '_> {
        match self {
            Text::Rope(rope) => Box::new(rope.chunks_in_range(range)),
            Text::Mapped(mapped) => Box::new(mapped.chunks_in_range(range)),
        }
    }

    /// Iterates over the whole text as string slices.
    pub fn chunks(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        self.chunks_in_range(0..self.len())
    }

    /// Chars starting at `offset`.
    pub fn chars_at(&self, offset: usize) -> impl Iterator<Item = char> + '_ {
        self.chunks_in_range(offset..self.len())
            .flat_map(str::chars)
    }

    /// Replaces `range` with `new_text`, assigning a new version. A mapped
    /// text is first converted to an in-memory rope (lossily) — expensive
    /// for huge files.
    pub fn replace(&mut self, range: Range<usize>, new_text: &str) {
        match self {
            Text::Rope(rope) => rope.replace(range, new_text),
            Text::Mapped(mapped) => {
                let mut rope = Rope::from_str(&mapped.text_for_range(0..mapped.len()));
                rope.replace(range, new_text);
                *self = Text::Rope(rope);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn text_is_send_sync_and_versions_are_unique() {
        assert_send_sync::<Text>();
        let a = Text::from_str("x");
        let b = Text::from_str("x");
        assert_ne!(a.version(), b.version());
        assert_eq!(a.clone().version(), a.version());
        let mut c = a.clone();
        c.replace(0..1, "y");
        assert_ne!(c.version(), a.version());
        assert_eq!(c.to_string(), "y");
        assert_eq!(a.to_string(), "x");
    }

    #[test]
    fn rope_and_mapped_agree() {
        let content = "first\r\nsecond é\nthird 😀\n\nlast";
        let mut file = tempfile::NamedTempFile::new().expect("tempfile");
        file.write_all(content.as_bytes()).expect("write");
        file.flush().expect("flush");
        let mapped = Text::open_mapped(file.path()).expect("open");
        let rope = Text::from_str(content);
        assert!(mapped.is_mapped() && !rope.is_mapped());
        for text in [&mapped, &rope] {
            assert_eq!(text.line_count(), 5);
            let lines: Vec<_> = text.lines(0..10).collect();
            assert_eq!(lines, ["first", "second é", "third 😀", "", "last"]);
            assert_eq!(text.line(1), "second é");
            assert_eq!(text.text_for_range(2..12), &content[2..12]);
            assert_eq!(text.chunks().collect::<String>(), content);
            assert_eq!(text.offset_to_point(9), Point::new(1, 2));
            assert_eq!(text.point_to_offset(Point::new(2, 100)), 27);
            assert_eq!(
                text.offset_to_point_utf16(content.len()),
                PointUtf16::new(4, 4)
            );
            assert_eq!(text.chars_at(23).next(), Some('😀'));
        }
        let mut edited = mapped.clone();
        edited.replace(0..5, "1st");
        assert!(!edited.is_mapped());
        assert_eq!(edited.line(0), "1st");
    }

    #[test]
    fn text_for_range_borrows_within_a_chunk() {
        let text = Text::from_str("hello world");
        assert!(matches!(text.text_for_range(0..5), Cow::Borrowed("hello")));
    }
}
