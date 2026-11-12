//! Immutable per-frame views of the terminal, independent of `alacritty_terminal`.
//!
//! A [`Snapshot`] holds the visible rows as `Arc<SnapshotRow>`; rows that did
//! not change since the previous snapshot are shared (pointer-equal), and
//! [`Snapshot::damage`] lists the rows that did change so a renderer re-shapes
//! only those. [`SnapshotRow::runs`] coalesces cells with equal style into
//! [`TextRun`]s.

use std::sync::Arc;

use alacritty_terminal::term::cell::{Cell as AlacCell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color as AlacColor, NamedColor};

use crate::geometry::{GridPoint, TermSize};

/// A color as the program asked for it; the UI maps it to its theme.
///
/// Palette entries a program redefined at run time (OSC 4/10/11) are already
/// resolved to [`TermColor::Rgb`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TermColor {
    /// Default foreground.
    Foreground,
    /// Default background.
    Background,
    /// Cursor color.
    Cursor,
    /// Bright variant of the default foreground.
    BrightForeground,
    /// Dim variant of the default foreground.
    DimForeground,
    /// 256-color palette index (0-7 normal ANSI, 8-15 bright, 16-255 cube/greys).
    Indexed(u8),
    /// Dim variant of ANSI color 0-7.
    DimIndexed(u8),
    /// True color.
    Rgb(u8, u8, u8),
}

impl TermColor {
    pub(crate) fn from_alac(color: AlacColor, colors: &Colors) -> Self {
        let index = match color {
            AlacColor::Spec(rgb) => return TermColor::Rgb(rgb.r, rgb.g, rgb.b),
            AlacColor::Indexed(i) => i as usize,
            AlacColor::Named(n) => n as usize,
        };
        if let Some(rgb) = colors[index] {
            return TermColor::Rgb(rgb.r, rgb.g, rgb.b);
        }
        match color {
            AlacColor::Indexed(i) => TermColor::Indexed(i),
            AlacColor::Named(n) => named(n),
            AlacColor::Spec(_) => TermColor::Foreground,
        }
    }
}

fn named(n: NamedColor) -> TermColor {
    use NamedColor as N;
    match n {
        N::Foreground => TermColor::Foreground,
        N::Background => TermColor::Background,
        N::Cursor => TermColor::Cursor,
        N::BrightForeground => TermColor::BrightForeground,
        N::DimForeground => TermColor::DimForeground,
        N::DimBlack => TermColor::DimIndexed(0),
        N::DimRed => TermColor::DimIndexed(1),
        N::DimGreen => TermColor::DimIndexed(2),
        N::DimYellow => TermColor::DimIndexed(3),
        N::DimBlue => TermColor::DimIndexed(4),
        N::DimMagenta => TermColor::DimIndexed(5),
        N::DimCyan => TermColor::DimIndexed(6),
        N::DimWhite => TermColor::DimIndexed(7),
        other => TermColor::Indexed((other as usize).min(255) as u8),
    }
}

/// Underline decoration of a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum UnderlineStyle {
    /// No underline.
    #[default]
    None,
    /// Single line.
    Single,
    /// Double line.
    Double,
    /// Curly line (undercurl).
    Curly,
    /// Dotted line.
    Dotted,
    /// Dashed line.
    Dashed,
}

/// Boolean text attributes of a cell (bit set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Attrs(u8);

impl Attrs {
    /// Bold (SGR 1).
    pub const BOLD: Attrs = Attrs(1);
    /// Italic (SGR 3).
    pub const ITALIC: Attrs = Attrs(1 << 1);
    /// Dim/faint (SGR 2).
    pub const DIM: Attrs = Attrs(1 << 2);
    /// Inverse video (SGR 7): the UI swaps fg and bg.
    pub const INVERSE: Attrs = Attrs(1 << 3);
    /// Hidden/concealed (SGR 8): the UI draws no glyph.
    pub const HIDDEN: Attrs = Attrs(1 << 4);
    /// Strikethrough (SGR 9).
    pub const STRIKETHROUGH: Attrs = Attrs(1 << 5);

    /// No attributes.
    pub const fn empty() -> Self {
        Attrs(0)
    }

    /// Whether every attribute in `other` is set.
    pub const fn contains(self, other: Attrs) -> bool {
        self.0 & other.0 == other.0
    }

    /// Raw bits.
    pub const fn bits(self) -> u8 {
        self.0
    }

    fn set(&mut self, other: Attrs, on: bool) {
        if on {
            self.0 |= other.0;
        } else {
            self.0 &= !other.0;
        }
    }
}

impl std::ops::BitOr for Attrs {
    type Output = Attrs;
    fn bitor(self, rhs: Attrs) -> Attrs {
        Attrs(self.0 | rhs.0)
    }
}

/// An OSC 8 hyperlink attached to cells.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Hyperlink {
    /// Link id (explicit `id=` parameter, or generated); cells of one link share it.
    pub id: String,
    /// Target URI exactly as the program sent it (untrusted data).
    pub uri: String,
}

/// Visual style of a cell; equal styles coalesce into one [`TextRun`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CellStyle {
    /// Foreground color.
    pub fg: TermColor,
    /// Background color.
    pub bg: TermColor,
    /// Text attributes.
    pub attrs: Attrs,
    /// Underline decoration.
    pub underline: UnderlineStyle,
    /// Underline color (`None`: same as the foreground).
    pub underline_color: Option<TermColor>,
    /// OSC 8 hyperlink.
    pub hyperlink: Option<Arc<Hyperlink>>,
}

impl Default for CellStyle {
    fn default() -> Self {
        Self {
            fg: TermColor::Foreground,
            bg: TermColor::Background,
            attrs: Attrs::empty(),
            underline: UnderlineStyle::None,
            underline_color: None,
            hyperlink: None,
        }
    }
}

/// How a cell takes part in wide-character layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CellWidth {
    /// Ordinary one-column cell.
    #[default]
    Narrow,
    /// First column of a two-column character.
    Wide,
    /// Second column of a wide character: draw nothing (background only).
    WideSpacer,
    /// Placeholder at the end of a line whose next wide character wrapped: draw nothing.
    LeadingSpacer,
}

/// One cell of a snapshot row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Cell {
    /// Base character (`' '` for empty cells).
    pub ch: char,
    /// Zero-width / combining characters following `ch`.
    pub zerowidth: Option<Box<[char]>>,
    /// Layout role of the cell.
    pub width: CellWidth,
    /// Visual style.
    pub style: CellStyle,
}

impl Cell {
    pub(crate) fn from_alac(cell: &AlacCell, colors: &Colors) -> Self {
        let f = cell.flags;
        let mut attrs = Attrs::empty();
        attrs.set(Attrs::BOLD, f.contains(Flags::BOLD));
        attrs.set(Attrs::ITALIC, f.contains(Flags::ITALIC));
        attrs.set(Attrs::DIM, f.contains(Flags::DIM));
        attrs.set(Attrs::INVERSE, f.contains(Flags::INVERSE));
        attrs.set(Attrs::HIDDEN, f.contains(Flags::HIDDEN));
        attrs.set(Attrs::STRIKETHROUGH, f.contains(Flags::STRIKEOUT));
        let underline = if f.contains(Flags::UNDERCURL) {
            UnderlineStyle::Curly
        } else if f.contains(Flags::DOUBLE_UNDERLINE) {
            UnderlineStyle::Double
        } else if f.contains(Flags::DOTTED_UNDERLINE) {
            UnderlineStyle::Dotted
        } else if f.contains(Flags::DASHED_UNDERLINE) {
            UnderlineStyle::Dashed
        } else if f.contains(Flags::UNDERLINE) {
            UnderlineStyle::Single
        } else {
            UnderlineStyle::None
        };
        let width = if f.contains(Flags::WIDE_CHAR) {
            CellWidth::Wide
        } else if f.contains(Flags::WIDE_CHAR_SPACER) {
            CellWidth::WideSpacer
        } else if f.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
            CellWidth::LeadingSpacer
        } else {
            CellWidth::Narrow
        };
        let zerowidth = cell
            .zerowidth()
            .filter(|z| !z.is_empty())
            .map(|z| z.to_vec().into_boxed_slice());
        let hyperlink = cell.hyperlink().map(|h| {
            Arc::new(Hyperlink {
                id: h.id().to_owned(),
                uri: h.uri().to_owned(),
            })
        });
        Cell {
            ch: if cell.c == '\t' { ' ' } else { cell.c },
            zerowidth,
            width,
            style: CellStyle {
                fg: TermColor::from_alac(cell.fg, colors),
                bg: TermColor::from_alac(cell.bg, colors),
                attrs,
                underline,
                underline_color: cell
                    .underline_color()
                    .map(|c| TermColor::from_alac(c, colors)),
                hyperlink,
            },
        }
    }
}

/// A run of adjacent cells with the same style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextRun {
    /// First column of the run.
    pub col: u32,
    /// Number of columns covered (wide characters count 2).
    pub cols: u32,
    /// Text of the run (spacer cells contribute nothing; combining characters follow their base).
    pub text: String,
    /// Shared style of every cell in the run.
    pub style: CellStyle,
    /// Whether the run contains wide characters (the UI must place glyphs per column).
    pub has_wide: bool,
}

/// One visible row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRow {
    /// Cells, exactly `cols` of them.
    pub cells: Vec<Cell>,
    /// The row continues on the next row (soft wrap).
    pub wrapped: bool,
}

impl SnapshotRow {
    /// Coalesces the row into styled runs, left to right, covering every column.
    pub fn runs(&self) -> Vec<TextRun> {
        let mut runs: Vec<TextRun> = Vec::new();
        for (col, cell) in self.cells.iter().enumerate() {
            let col = col as u32;
            match runs.last_mut() {
                Some(run) if run.style == cell.style => {
                    run.cols += 1;
                    push_cell_text(&mut run.text, cell);
                    run.has_wide |= cell.width == CellWidth::Wide;
                }
                _ => {
                    let mut text = String::new();
                    push_cell_text(&mut text, cell);
                    runs.push(TextRun {
                        col,
                        cols: 1,
                        text,
                        style: cell.style.clone(),
                        has_wide: cell.width == CellWidth::Wide,
                    });
                }
            }
        }
        runs
    }

    /// Plain text of the row (no trailing whitespace trimming).
    pub fn text(&self) -> String {
        let mut s = String::with_capacity(self.cells.len());
        for c in &self.cells {
            push_cell_text(&mut s, c);
        }
        s
    }
}

fn push_cell_text(out: &mut String, cell: &Cell) {
    match cell.width {
        CellWidth::WideSpacer | CellWidth::LeadingSpacer => {}
        CellWidth::Narrow | CellWidth::Wide => {
            out.push(cell.ch);
            if let Some(z) = &cell.zerowidth {
                out.extend(z.iter());
            }
        }
    }
}

/// Cursor shape requested by the program (DECSCUSR) or the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CursorShape {
    /// Filled block.
    #[default]
    Block,
    /// Underline.
    Underline,
    /// Vertical bar.
    Beam,
    /// Hollow block (also what to draw when the terminal is unfocused).
    HollowBlock,
}

/// Cursor state for one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CursorState {
    /// Viewport row (meaningful when `visible`).
    pub row: u32,
    /// Column.
    pub col: u32,
    /// Shape.
    pub shape: CursorShape,
    /// False when the program hid the cursor (DECTCEM) or it is scrolled out of view.
    pub visible: bool,
    /// Whether the program asked for a blinking cursor.
    pub blinking: bool,
    /// The cursor sits on a wide character (draw it two columns wide).
    pub wide: bool,
}

/// Mouse reporting requested by the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MouseMode {
    /// No reporting: the UI handles the mouse (selection, scrolling).
    #[default]
    None,
    /// X10 (`?9`): button presses only.
    X10,
    /// Normal (`?1000`): presses and releases.
    Click,
    /// Button-event (`?1002`): also motion while a button is held.
    Drag,
    /// Any-event (`?1003`): all motion.
    Motion,
}

/// Terminal modes a renderer or input layer needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TerminalModes {
    /// Alternate screen is active (full-screen app; no scrollback).
    pub alt_screen: bool,
    /// Bracketed paste (`?2004`).
    pub bracketed_paste: bool,
    /// Application cursor keys (DECCKM, `?1`).
    pub app_cursor: bool,
    /// Application keypad (DECKPAM).
    pub app_keypad: bool,
    /// Mouse reporting mode.
    pub mouse: MouseMode,
    /// SGR mouse encoding (`?1006`).
    pub sgr_mouse: bool,
    /// UTF-8 mouse encoding (`?1005`).
    pub utf8_mouse: bool,
    /// Focus in/out reporting (`?1004`).
    pub focus_reporting: bool,
    /// Alternate scroll (`?1007`): wheel sends arrow keys on the alternate screen.
    pub alternate_scroll: bool,
    /// LNM: Enter sends CR LF.
    pub line_feed_new_line: bool,
    /// xterm modifyOtherKeys level (0, 1 or 2).
    pub modify_other_keys: u8,
}

impl TerminalModes {
    /// Whether mouse events should go to the program instead of the UI.
    ///
    /// Convention: when this is true the UI still selects text while Shift is held.
    pub fn wants_mouse(&self) -> bool {
        self.mouse != MouseMode::None
    }
}

/// Rows that changed since the previous snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Damage {
    /// Everything changed (first frame, resize, scroll, selection change, ...).
    Full,
    /// Only these viewport rows changed (sorted, unique; may be empty).
    Rows(Vec<u32>),
}

impl Damage {
    /// Whether viewport row `row` must be redrawn.
    pub fn contains(&self, row: u32) -> bool {
        match self {
            Damage::Full => true,
            Damage::Rows(rows) => rows.binary_search(&row).is_ok(),
        }
    }

    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        matches!(self, Damage::Rows(r) if r.is_empty())
    }
}

/// The current selection in grid coordinates (inclusive bounds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SelectionSpan {
    /// First selected cell.
    pub start: GridPoint,
    /// Last selected cell (inclusive).
    pub end: GridPoint,
    /// Block (rectangular) selection: columns `start.col..=end.col` on every line.
    pub block: bool,
}

impl SelectionSpan {
    /// Whether the cell at `point` is selected.
    pub fn contains(&self, point: GridPoint) -> bool {
        if point.line < self.start.line || point.line > self.end.line {
            return false;
        }
        if self.block {
            return point.col >= self.start.col && point.col <= self.end.col;
        }
        let after_start = point.line > self.start.line || point.col >= self.start.col;
        let before_end = point.line < self.end.line || point.col <= self.end.col;
        after_start && before_end
    }

    /// Selected columns on grid line `line` as a half-open range, if any.
    pub fn cols_on_line(&self, line: i32, cols: u16) -> Option<std::ops::Range<u32>> {
        if line < self.start.line || line > self.end.line {
            return None;
        }
        let cols = u32::from(cols);
        let (a, b) = if self.block {
            (self.start.col, self.end.col + 1)
        } else {
            let a = if line == self.start.line {
                self.start.col
            } else {
                0
            };
            let b = if line == self.end.line {
                self.end.col + 1
            } else {
                cols
            };
            (a, b)
        };
        let (a, b) = (a.min(cols), b.min(cols));
        (a < b).then_some(a..b)
    }
}

/// An immutable view of the terminal for one frame.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Increments with every snapshot of this terminal.
    pub seq: u64,
    /// Grid size.
    pub size: TermSize,
    /// Visible rows, `size.rows` of them; unchanged rows are shared with the previous snapshot.
    pub rows: Vec<Arc<SnapshotRow>>,
    /// Rows changed since the previous snapshot.
    pub damage: Damage,
    /// Cursor.
    pub cursor: CursorState,
    /// How many lines the view is scrolled up into scrollback (0 = live).
    pub display_offset: u32,
    /// Lines currently held in scrollback.
    pub history_size: u32,
    /// Selection, if any.
    pub selection: Option<SelectionSpan>,
    /// Modes relevant to rendering and input.
    pub modes: TerminalModes,
}

impl Snapshot {
    /// Grid line of viewport row `row`.
    pub fn grid_line(&self, row: u32) -> i32 {
        crate::geometry::viewport_to_grid(row, 0, self.display_offset).line
    }

    /// Selected columns of viewport row `row` (half-open), if any.
    pub fn selected_cols(&self, row: u32) -> Option<std::ops::Range<u32>> {
        self.selection?
            .cols_on_line(self.grid_line(row), self.size.cols)
    }

    /// Text of the visible screen, rows joined by `\n`, trailing spaces trimmed.
    pub fn text(&self) -> String {
        let mut lines: Vec<String> = self
            .rows
            .iter()
            .map(|r| r.text().trim_end().to_owned())
            .collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(ch: char, style: CellStyle, width: CellWidth) -> Cell {
        Cell {
            ch,
            zerowidth: None,
            width,
            style,
        }
    }

    #[test]
    fn runs_coalesce_and_cover_all_columns() {
        let red = CellStyle {
            fg: TermColor::Indexed(1),
            ..CellStyle::default()
        };
        let row = SnapshotRow {
            cells: vec![
                cell('a', CellStyle::default(), CellWidth::Narrow),
                cell('b', CellStyle::default(), CellWidth::Narrow),
                cell('\u{4e2d}', red.clone(), CellWidth::Wide),
                cell(' ', red.clone(), CellWidth::WideSpacer),
                cell('c', CellStyle::default(), CellWidth::Narrow),
            ],
            wrapped: false,
        };
        let runs = row.runs();
        assert_eq!(runs.len(), 3);
        assert_eq!(
            (runs[0].col, runs[0].cols, runs[0].text.as_str()),
            (0, 2, "ab")
        );
        assert_eq!(
            (runs[1].col, runs[1].cols, runs[1].text.as_str()),
            (2, 2, "\u{4e2d}")
        );
        assert!(runs[1].has_wide);
        assert_eq!((runs[2].col, runs[2].cols), (4, 1));
        assert_eq!(runs.iter().map(|r| r.cols).sum::<u32>(), 5);
    }

    #[test]
    fn selection_span_contains() {
        let s = SelectionSpan {
            start: GridPoint::new(0, 3),
            end: GridPoint::new(2, 1),
            block: false,
        };
        assert!(!s.contains(GridPoint::new(0, 2)));
        assert!(s.contains(GridPoint::new(1, 70)));
        assert!(s.contains(GridPoint::new(2, 1)));
        assert!(!s.contains(GridPoint::new(2, 2)));
        assert_eq!(s.cols_on_line(0, 10), Some(3..10));
        assert_eq!(s.cols_on_line(2, 10), Some(0..2));
        let b = SelectionSpan {
            block: true,
            start: GridPoint::new(0, 1),
            end: GridPoint::new(2, 3),
        };
        assert!(!b.contains(GridPoint::new(1, 4)));
        assert_eq!(b.cols_on_line(1, 10), Some(1..4));
    }

    #[test]
    fn damage_contains() {
        assert!(Damage::Full.contains(5));
        assert!(Damage::Rows(vec![1, 3]).contains(3));
        assert!(!Damage::Rows(vec![1, 3]).contains(2));
        assert!(Damage::Rows(vec![]).is_empty());
    }
}
