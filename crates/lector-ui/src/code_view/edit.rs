//! Editing support for the code view.
//!
//! * [`EditBuffer`]: a GPUI entity around a `lector_buffer::Buffer`, shared by
//!   every [`super::CodeView`] showing the same text. Mutations go through
//!   [`EditBuffer::edit`], which emits [`EditBufferEvent::Edited`] with the
//!   transaction so other views can map their carets and owners can update
//!   derived state (syntax, gutter, LSP).
//! * [`RopeSource`]: a minimal [`CodeSource`] over a rope, used by views that
//!   manage their own source (tests, scratch buffers).
//! * Conversions between view positions ([`Pos`]), buffer offsets and the
//!   UTF-16 offsets used by the platform input handler (IME).

use std::ops::Range;
use std::sync::Arc;

use gpui::{Context, EntityId, EventEmitter, SharedString};
use lector_app::documents::{BufferView, DocHandle, EditOrigin, HistoryEntry};
use lector_buffer::{Buffer, EditDelta, Transaction};
use lector_text::sum_tree::Bias;
use lector_text::{OffsetUtf16, Point, Rope};

use super::selection::{Pos, Selection};
use super::source::{CodeSource, LexicalHighlighter, SpanStyle};
use super::HighlightSpan;

/// Events emitted by an [`EditBuffer`].
#[derive(Clone)]
pub enum EditBufferEvent {
    /// The text changed. `origin` is the view that made the edit (`None`:
    /// programmatic, e.g. reload, LSP edits, or another front-end / agent
    /// editing the shared document).
    Edited {
        /// Entity id of the editing view.
        origin: Option<EntityId>,
        /// What changed (in edit order).
        transaction: Arc<Transaction>,
    },
}

/// Read access to the buffer state (an owned `Buffer`, or the mirror of a
/// shared document at the last seq this front-end processed).
pub enum BufferRef<'a> {
    /// A private buffer.
    Owned(&'a Buffer),
    /// A shared document's buffer, as last synchronized.
    Mirror(&'a BufferView),
}

impl BufferRef<'_> {
    /// Text version.
    pub fn version(&self) -> u64 {
        match self {
            BufferRef::Owned(b) => b.version(),
            BufferRef::Mirror(v) => v.version,
        }
    }

    /// Current text.
    pub fn text(&self) -> &Rope {
        match self {
            BufferRef::Owned(b) => b.text(),
            BufferRef::Mirror(v) => &v.text,
        }
    }

    /// Selections as left by the last buffer operation.
    pub fn selections(&self) -> &[lector_buffer::Selection] {
        match self {
            BufferRef::Owned(b) => b.selections(),
            BufferRef::Mirror(v) => &v.selections,
        }
    }

    /// Options.
    pub fn options(&self) -> &lector_buffer::BufferOptions {
        match self {
            BufferRef::Owned(b) => b.options(),
            BufferRef::Mirror(v) => &v.options,
        }
    }

    /// Unsaved changes.
    pub fn is_dirty(&self) -> bool {
        match self {
            BufferRef::Owned(b) => b.is_dirty(),
            BufferRef::Mirror(v) => v.dirty,
        }
    }

    /// Undo available.
    pub fn can_undo(&self) -> bool {
        match self {
            BufferRef::Owned(b) => b.can_undo(),
            BufferRef::Mirror(v) => v.can_undo,
        }
    }

    /// Redo available.
    pub fn can_redo(&self) -> bool {
        match self {
            BufferRef::Owned(b) => b.can_redo(),
            BufferRef::Mirror(v) => v.can_redo,
        }
    }
}

enum Storage {
    Owned(Box<Buffer>),
    Shared {
        handle: DocHandle,
        origin: EditOrigin,
        view: BufferView,
    },
}

/// A text buffer shared by the views editing it: either private (scratch
/// buffers) or a mirror of a `lector_app` document (file tabs), which other
/// windows, agents and commands may edit too.
pub struct EditBuffer {
    storage: Storage,
}

impl EventEmitter<EditBufferEvent> for EditBuffer {}

impl EditBuffer {
    /// Wraps a private `buffer`.
    pub fn new(buffer: Buffer) -> Self {
        EditBuffer {
            storage: Storage::Owned(Box::new(buffer)),
        }
    }

    /// Mirrors the shared document `handle` (`None`: not editable).
    pub fn shared(handle: DocHandle) -> Option<Self> {
        let view = handle.buffer_view()?;
        Some(EditBuffer {
            storage: Storage::Shared {
                handle,
                origin: EditOrigin::new(),
                view,
            },
        })
    }

    /// The shared document, if this buffer mirrors one.
    pub fn handle(&self) -> Option<&DocHandle> {
        match &self.storage {
            Storage::Owned(_) => None,
            Storage::Shared { handle, .. } => Some(handle),
        }
    }

    /// The origin tag of this front-end's edits (shared buffers).
    pub fn origin(&self) -> Option<EditOrigin> {
        match &self.storage {
            Storage::Owned(_) => None,
            Storage::Shared { origin, .. } => Some(*origin),
        }
    }

    /// The buffer (read access).
    pub fn buffer(&self) -> BufferRef<'_> {
        match &self.storage {
            Storage::Owned(b) => BufferRef::Owned(b),
            Storage::Shared { view, .. } => BufferRef::Mirror(view),
        }
    }

    /// Current text.
    pub fn text(&self) -> &Rope {
        match &self.storage {
            Storage::Owned(b) => b.text(),
            Storage::Shared { view, .. } => &view.text,
        }
    }

    /// Whether the text differs from the last saved state.
    pub fn is_dirty(&self) -> bool {
        self.buffer().is_dirty()
    }

    /// Runs `f` on the buffer. When `selections` is given, the buffer's
    /// selections are replaced first (a view brings its carets). Emits
    /// [`EditBufferEvent::Edited`] when the returned transaction is not empty
    /// (after edits other front-ends made meanwhile). `None` when the shared
    /// document refused the edit (read-only).
    pub fn edit<R>(
        &mut self,
        origin: Option<EntityId>,
        selections: Option<Vec<lector_buffer::Selection>>,
        f: impl FnOnce(&mut Buffer) -> (R, Transaction),
        cx: &mut Context<Self>,
    ) -> Option<R> {
        let run = move |buffer: &mut Buffer| {
            if let Some(sels) = selections {
                // Re-installing identical carets would end the current typing run
                // (`set_selections` breaks undo coalescing), making every keystroke
                // its own undo step.
                if !same_selections(buffer, &sels) {
                    buffer.set_selections(sels);
                }
            }
            f(buffer)
        };
        match &mut self.storage {
            Storage::Owned(buffer) => {
                let (result, transaction) = run(buffer);
                if !transaction.deltas.is_empty() {
                    cx.emit(EditBufferEvent::Edited {
                        origin,
                        transaction: Arc::new(transaction),
                    });
                    cx.notify();
                }
                Some(result)
            }
            Storage::Shared {
                handle,
                origin: me,
                view,
            } => {
                let r = handle.edit_from(*me, view.seq, run).ok()?;
                let before = std::mem::replace(view, r.view);
                let me = *me;
                let after = view.text.clone();
                Self::emit_foreign(&before.text, &after, &r.catch_up, r.gap, me, cx);
                if !r.transaction.deltas.is_empty() {
                    cx.emit(EditBufferEvent::Edited {
                        origin,
                        transaction: r.transaction,
                    });
                }
                cx.notify();
                Some(r.value)
            }
        }
    }

    /// Applies external edits (LSP formatting, rename, code actions) as one
    /// undo step. Offsets refer to the current text. Empty when nothing
    /// changed or the shared document refused (read-only).
    pub fn apply_edits(
        &mut self,
        edits: Vec<lector_buffer::TextEdit>,
        cx: &mut Context<Self>,
    ) -> Arc<Transaction> {
        let transaction = match &mut self.storage {
            Storage::Owned(buffer) => Arc::new(buffer.apply_edits(edits)),
            Storage::Shared {
                handle,
                origin,
                view,
            } => match handle.edit_from(*origin, view.seq, |b| ((), b.apply_edits(edits))) {
                Ok(r) => {
                    let before = std::mem::replace(view, r.view);
                    let me = *origin;
                    let after = view.text.clone();
                    Self::emit_foreign(&before.text, &after, &r.catch_up, r.gap, me, cx);
                    r.transaction
                }
                Err(_) => Arc::new(Transaction::default()),
            },
        };
        if !transaction.deltas.is_empty() {
            cx.emit(EditBufferEvent::Edited {
                origin: None,
                transaction: transaction.clone(),
            });
        }
        cx.notify();
        transaction
    }

    /// Runs an operation that does not change the text (selections, copy,
    /// options, undo grouping). Shared buffers catch up with other
    /// front-ends' edits first.
    pub fn update_buffer<R>(
        &mut self,
        f: impl FnOnce(&mut Buffer) -> R,
        cx: &mut Context<Self>,
    ) -> R {
        self.catch_up(cx);
        match &mut self.storage {
            Storage::Owned(buffer) => f(buffer),
            Storage::Shared { handle, view, .. } => {
                let mut f = Some(f);
                let mut out = None;
                let res = handle.with_buffer_mut(|b| {
                    if let Some(f) = f.take() {
                        out = Some(f(b));
                    }
                });
                if let Some(((), fresh)) = res {
                    if fresh.seq == view.seq {
                        *view = fresh;
                    } else {
                        // Another edit slipped in: the next catch-up brings the text.
                        view.selections = fresh.selections;
                        view.options = fresh.options;
                    }
                }
                if let Some(f) = f.take() {
                    // No longer editable: run on a scratch copy of the mirror.
                    let mut scratch = Buffer::new(view.text.clone(), view.options.clone());
                    return f(&mut scratch);
                }
                match out {
                    Some(r) => r,
                    None => unreachable!("the closure ran"),
                }
            }
        }
    }

    /// Shared buffers: emits the edits other front-ends (agents, other
    /// windows, reloads, save cleanups) made since this mirror was last
    /// updated, and refreshes the dirty / undo state.
    pub fn catch_up(&mut self, cx: &mut Context<Self>) {
        let Storage::Shared {
            handle,
            origin,
            view,
        } = &mut self.storage
        else {
            return;
        };
        let Some(up) = handle.catch_up(view.seq) else {
            return;
        };
        let changed = up.view.seq != view.seq
            || up.view.dirty != view.dirty
            || up.view.can_undo != view.can_undo
            || up.view.can_redo != view.can_redo;
        let before = std::mem::replace(view, up.view);
        let me = *origin;
        let after = view.text.clone();
        Self::emit_foreign(&before.text, &after, &up.entries, up.gap, me, cx);
        if changed {
            cx.notify();
        }
    }

    fn emit_foreign(
        before: &Rope,
        after: &Rope,
        entries: &[HistoryEntry],
        gap: bool,
        me: EditOrigin,
        cx: &mut Context<Self>,
    ) {
        if gap {
            // History did not reach back far enough: one whole-text replacement.
            cx.emit(EditBufferEvent::Edited {
                origin: None,
                transaction: Arc::new(Transaction {
                    deltas: vec![EditDelta {
                        start_byte: 0,
                        old_end_byte: before.len(),
                        new_end_byte: after.len(),
                        start: Point::new(0, 0),
                        old_end: before.max_point(),
                        new_end: after.max_point(),
                    }],
                }),
            });
            return;
        }
        for entry in entries.iter().filter(|e| e.origin != me) {
            cx.emit(EditBufferEvent::Edited {
                origin: None,
                transaction: entry.transaction.clone(),
            });
        }
    }
}

/// Byte offset of `pos` in `text`.
pub fn pos_to_offset(text: &Rope, pos: Pos) -> usize {
    text.point_to_offset(Point::new(pos.row, pos.col as u32))
}

/// View position of byte `offset` in `text`.
pub fn offset_to_pos(text: &Rope, offset: usize) -> Pos {
    let p = text.offset_to_point(offset);
    Pos::new(p.row, p.column as usize)
}

/// Converts view selections to buffer selections. The primary selection gets
/// the largest id (the buffer treats the newest caret as primary).
pub fn to_buffer_selections(
    text: &Rope,
    selections: &[Selection],
    primary: usize,
) -> Vec<lector_buffer::Selection> {
    let n = selections.len() as u64;
    selections
        .iter()
        .enumerate()
        .map(|(ix, s)| lector_buffer::Selection {
            id: if ix == primary { n + 1 } else { ix as u64 + 1 },
            anchor: pos_to_offset(text, s.anchor),
            head: pos_to_offset(text, s.head),
            goal: None,
        })
        .collect()
}

/// Whether `sels` are the buffer's current selections (same ranges, same newest).
fn same_selections(buffer: &Buffer, sels: &[lector_buffer::Selection]) -> bool {
    let current = buffer.selections();
    if current.len() != sels.len() {
        return false;
    }
    // `set_selections` makes the last given selection the newest.
    let newest = buffer.newest_selection();
    sels.last()
        .is_some_and(|s| s.anchor == newest.anchor && s.head == newest.head)
        && current
            .iter()
            .zip(sels)
            .all(|(a, b)| a.anchor == b.anchor && a.head == b.head)
}

/// Converts buffer selections back to view selections; returns them with the
/// index of the primary (newest) one.
pub fn from_buffer_selections(
    text: &Rope,
    selections: &[lector_buffer::Selection],
) -> (Vec<Selection>, usize) {
    let mut primary = 0;
    let mut best = 0;
    let out = selections
        .iter()
        .enumerate()
        .map(|(ix, s)| {
            if s.id >= best {
                best = s.id;
                primary = ix;
            }
            Selection {
                anchor: offset_to_pos(text, s.anchor),
                head: offset_to_pos(text, s.head),
                goal_column: None,
            }
        })
        .collect();
    (out, primary)
}

fn point_of(pos: Pos) -> Point {
    Point::new(pos.row, pos.col as u32)
}

/// Maps `pos` through one edit (`Bias::Right`: a position at the edit start
/// moves after inserted text only when it was after the replaced range).
pub fn map_pos(pos: Pos, delta: &EditDelta) -> Pos {
    let p = point_of(pos);
    if p < delta.start {
        return pos;
    }
    if p < delta.old_end {
        // Inside the replaced range: collapse to the end of the new text.
        return Pos::new(delta.new_end.row, delta.new_end.column as usize);
    }
    if p.row == delta.old_end.row {
        let col = delta.new_end.column + (p.column - delta.old_end.column);
        Pos::new(delta.new_end.row, col as usize)
    } else {
        let row = p.row - delta.old_end.row + delta.new_end.row;
        Pos::new(row, pos.col)
    }
}

/// Maps a position through all edits of a transaction (applied in order).
pub fn map_pos_through(pos: Pos, deltas: &[EditDelta]) -> Pos {
    deltas.iter().fold(pos, map_pos)
}

/// Maps a row through a transaction. Rows inside a replaced range map to
/// the row where the replacement ends.
pub fn map_row_through(row: u32, deltas: &[EditDelta]) -> u32 {
    map_pos_through(Pos::new(row, 0), deltas).row
}

/// UTF-16 offset of byte `offset` in `text` (for the platform input handler).
pub fn offset_to_utf16(text: &Rope, offset: usize) -> usize {
    let offset = text.clip_offset(offset.min(text.len()), Bias::Left);
    let mut cursor = text.chunk_tree().cursor::<(usize, OffsetUtf16)>();
    cursor.seek(&offset, Bias::Left);
    let (start, start_utf16) = *cursor.start();
    let partial = match cursor.item() {
        Some(chunk) => {
            let s = chunk.as_str();
            let end = (offset - start).min(s.len());
            s[..end].encode_utf16().count()
        }
        None => 0,
    };
    start_utf16.0 + partial
}

/// Byte offset of UTF-16 offset `utf16` in `text` (clipped to a char start).
pub fn utf16_to_offset(text: &Rope, utf16: usize) -> usize {
    let mut cursor = text.chunk_tree().cursor::<(OffsetUtf16, usize)>();
    cursor.seek(&OffsetUtf16(utf16), Bias::Left);
    let (start_utf16, start) = *cursor.start();
    match cursor.item() {
        Some(chunk) => {
            let mut remaining = utf16.saturating_sub(start_utf16.0);
            let mut bytes = 0;
            for c in chunk.as_str().chars() {
                let n = c.len_utf16();
                if remaining < n {
                    break;
                }
                remaining -= n;
                bytes += c.len_utf8();
            }
            start + bytes
        }
        None => text.len(),
    }
}

/// Byte range of a UTF-16 range.
pub fn range_from_utf16(text: &Rope, range: &Range<usize>) -> Range<usize> {
    utf16_to_offset(text, range.start)..utf16_to_offset(text, range.end)
}

/// UTF-16 range of a byte range.
pub fn range_to_utf16(text: &Rope, range: &Range<usize>) -> Range<usize> {
    offset_to_utf16(text, range.start)..offset_to_utf16(text, range.end)
}

/// A minimal [`CodeSource`] over a rope (lexical highlighting by file name).
pub struct RopeSource {
    text: Rope,
    language: Option<SharedString>,
    highlighter: Option<LexicalHighlighter>,
}

impl RopeSource {
    /// Source over `text`; `file_name` picks a lexical highlighter.
    pub fn new(text: Rope, file_name: Option<&str>) -> Self {
        let (language, highlighter) = match file_name.and_then(LexicalHighlighter::for_file_name) {
            Some((lang, h)) => (Some(SharedString::from(lang)), Some(h)),
            None => (None, None),
        };
        RopeSource {
            text,
            language,
            highlighter,
        }
    }
}

impl CodeSource for RopeSource {
    fn line_count(&self) -> u32 {
        self.text.line_count().max(1)
    }

    fn line(&self, row: u32) -> SharedString {
        if row >= self.text.line_count() {
            return SharedString::default();
        }
        let range = self.text.line_range(row);
        self.text.text_for_range(range).into()
    }

    fn version(&self) -> u64 {
        self.text.version()
    }

    fn max_line_len(&self) -> Option<usize> {
        let s = self.text.summary();
        Some(self.text.line_len(s.longest_row) as usize)
    }

    fn highlights(&self, rows: Range<u32>) -> Vec<HighlightSpan> {
        let Some(h) = &self.highlighter else {
            return Vec::new();
        };
        let mut in_block = false;
        let mut out = Vec::new();
        let start = rows.start.saturating_sub(200);
        for row in start..rows.end.min(self.line_count()) {
            let line = self.line(row);
            let spans = h.highlight_line(&line, &mut in_block);
            if row >= rows.start {
                out.extend(spans.into_iter().map(|(range, capture)| HighlightSpan {
                    row,
                    range,
                    style: SpanStyle::Capture(capture),
                }));
            }
        }
        out
    }

    fn language_name(&self) -> Option<SharedString> {
        self.language.clone()
    }

    fn text_snapshot(&self) -> Option<lector_text::Text> {
        Some(lector_text::Text::from(self.text.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_round_trip() {
        let text = Rope::from_str("a😀b\nçd");
        assert_eq!(offset_to_utf16(&text, 0), 0);
        assert_eq!(offset_to_utf16(&text, 1), 1);
        assert_eq!(offset_to_utf16(&text, 5), 3);
        assert_eq!(offset_to_utf16(&text, text.len()), 7);
        for (b, u) in [(0, 0), (1, 1), (5, 3), (6, 4), (7, 5), (9, 6), (10, 7)] {
            assert_eq!(utf16_to_offset(&text, u), b, "utf16 {u}");
        }
        // Inside a surrogate pair clips to the char start.
        assert_eq!(utf16_to_offset(&text, 2), 1);
        let long: String = "é".repeat(500);
        let text = Rope::from_str(&long);
        assert_eq!(offset_to_utf16(&text, 600), 300);
        assert_eq!(utf16_to_offset(&text, 300), 600);
    }

    #[test]
    fn maps_positions_through_edits() {
        // "abc\ndef": replace "c\nd" (0,2)-(1,1) with "XY".
        let delta = EditDelta {
            start_byte: 2,
            old_end_byte: 5,
            new_end_byte: 4,
            start: Point::new(0, 2),
            old_end: Point::new(1, 1),
            new_end: Point::new(0, 4),
        };
        assert_eq!(map_pos(Pos::new(0, 1), &delta), Pos::new(0, 1));
        assert_eq!(map_pos(Pos::new(1, 2), &delta), Pos::new(0, 5));
        assert_eq!(map_pos(Pos::new(0, 3), &delta), Pos::new(0, 4));
        assert_eq!(map_pos(Pos::new(3, 7), &delta), Pos::new(2, 7));
    }
}
