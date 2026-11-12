//! The headless terminal emulator: bytes in, snapshots and events out.
//!
//! [`Emulator`] wraps `alacritty_terminal`'s `Term` and VT parser, plus a
//! byte-stream [`Scanner`](crate::scan) for the sequences alacritty ignores
//! (OSC 7, OSC 133, modifyOtherKeys, X10 mouse). It has no PTY and no threads:
//! [`Terminal`](crate::Terminal) drives one from its reader thread, tests feed
//! it bytes directly.
//!
//! # Tracking lines through scrollback
//!
//! Command marks and the selection must follow their text as output scrolls
//! and as the scrollback is trimmed. alacritty does not expose a scroll
//! counter, but it rotates `Term::selection` with every scroll of the grid.
//! The emulator therefore keeps a *probe* selection parked far above the grid
//! (where nothing clears it) and, after each parsed segment, reads how far it
//! moved; the count is accumulated into a monotonic "lines scrolled" counter.
//! Marks are stored as absolute line numbers (`scrolled + grid line`) and are
//! dropped once they fall above the oldest scrollback line. The few operations
//! that drop a selection (erase display, erase scrollback, screen switches,
//! full reset) are split out by the scanner so the probe is read just before
//! them. The user's selection is kept outside the `Term` for the same reason
//! and installed only transiently.

use std::collections::VecDeque;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use alacritty_terminal::event::{Event as AlacEvent, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, GridCell, Scroll};
use alacritty_terminal::index::{Boundary, Column, Direction as AlacDirection, Line, Point};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::{RegexIter, RegexSearch};
use alacritty_terminal::term::{Config, Osc52, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{
    CursorShape as AlacCursorShape, Processor, Rgb, StdSyncHandler,
};
use parking_lot::Mutex;

use crate::error::TerminalError;
use crate::geometry::{grid_to_viewport, Direction, GridPoint, ScrollAmount, Side, TermSize};
use crate::input::{self, Key, Modifiers, MouseEvent};
use crate::links::{detect_links, LinkCandidate, LinkSource, LinkTarget};
use crate::osc::{local_hostname, parse_osc7, parse_shell_mark, sanitize_title, ShellMark};
use crate::scan::{ScanEvent, Scanner};
use crate::snapshot::{
    Cell, CursorShape, CursorState, Damage, MouseMode, SelectionSpan, Snapshot, SnapshotRow,
    TerminalModes,
};
use crate::TerminalEvent;

/// Line where the scroll probe is parked (far above any real scrollback).
const PROBE_LINE: i32 = -1_000_000_000;
/// Most command blocks kept.
const MAX_BLOCKS: usize = 4096;
/// Most pending emulator events kept between drains.
const MAX_PENDING_EVENTS: usize = 1024;
/// Most rows a logical (soft-wrapped) line is followed for links and searches.
const MAX_LOGICAL_ROWS: i32 = 64;
/// Largest clipboard payload accepted from OSC 52.
const MAX_CLIPBOARD_BYTES: usize = 8 * 1024 * 1024;

/// Configuration of an [`Emulator`].
#[derive(Debug, Clone)]
pub struct EmulatorConfig {
    /// Initial size.
    pub size: TermSize,
    /// Scrollback lines kept.
    pub scrollback: usize,
    /// Characters that end a word for double-click selection.
    pub word_separators: String,
    /// Working directory used to resolve links until the shell reports one (OSC 7).
    pub initial_cwd: Option<PathBuf>,
}

impl Default for EmulatorConfig {
    fn default() -> Self {
        Self {
            size: TermSize::default(),
            scrollback: crate::DEFAULT_SCROLLBACK,
            word_separators: DEFAULT_WORD_SEPARATORS.to_owned(),
            initial_cwd: None,
        }
    }
}

/// Default word separators for double-click selection: whitespace, quotes,
/// brackets and a few punctuation marks; `/`, `.`, `-` and `_` stay inside
/// words so paths and identifiers select whole.
pub const DEFAULT_WORD_SEPARATORS: &str = ",│`|:;\"' ()[]{}<>\t=";

#[derive(Clone)]
struct Listener {
    queue: Arc<Mutex<Vec<AlacEvent>>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: AlacEvent) {
        if matches!(
            event,
            AlacEvent::MouseCursorDirty | AlacEvent::Wakeup | AlacEvent::CursorBlinkingChange
        ) {
            return;
        }
        let mut q = self.queue.lock();
        if q.len() < MAX_PENDING_EVENTS {
            q.push(event);
        }
    }
}

/// Kind of selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelectionKind {
    /// Character-wise (click and drag).
    Simple,
    /// Word-wise (double click).
    Word,
    /// Line-wise (triple click).
    Line,
    /// Rectangular (Alt/Option + drag).
    Block,
}

/// A search request.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SearchQuery {
    /// Text or regular expression.
    pub pattern: String,
    /// Interpret `pattern` as a regular expression.
    pub regex: bool,
    /// Match case exactly.
    pub case_sensitive: bool,
}

/// A search match (inclusive cell range in grid coordinates).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SearchMatch {
    /// First cell.
    pub start: GridPoint,
    /// Last cell (inclusive).
    pub end: GridPoint,
}

/// A command block delimited by OSC 133 marks, in grid coordinates of the
/// primary screen (valid for the current scroll state; re-query after output).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommandBlock {
    /// Stable id.
    pub id: u64,
    /// Line where the prompt starts (`A`); clamped to the oldest line if trimmed.
    pub prompt_line: i32,
    /// Where the command text starts (`B`), if reported.
    pub command_start: Option<GridPoint>,
    /// Line where output starts (`C`), if reported.
    pub output_line: Option<i32>,
    /// Line where the command finished (`D`), if it has.
    pub end_line: Option<i32>,
    /// Exit code reported with `D`, if any.
    pub exit_code: Option<i32>,
    /// Part of the block was trimmed from scrollback.
    pub truncated: bool,
}

impl CommandBlock {
    /// Whether the command finished with a non-zero exit code.
    pub fn failed(&self) -> bool {
        self.exit_code.is_some_and(|c| c != 0)
    }

    /// Whether the command has finished.
    pub fn finished(&self) -> bool {
        self.end_line.is_some()
    }
}

#[derive(Debug, Clone)]
struct Block {
    id: u64,
    prompt: i64,
    command: Option<(i64, u32)>,
    output: Option<i64>,
    end: Option<i64>,
    exit_code: Option<i32>,
}

struct UserSelection {
    selection: Selection,
    /// Value of the active screen's scroll counter when `selection` was positioned.
    anchor: i64,
}

struct SearchState {
    regex: RegexSearch,
    /// Last match, anchored like the selection.
    last: Option<(SearchMatch, i64)>,
}

/// Headless VT emulator. See the module docs.
pub struct Emulator {
    term: Term<Listener>,
    parser: Processor<StdSyncHandler>,
    scanner: Scanner,
    scan_buf: Vec<(usize, ScanEvent)>,
    queue: Arc<Mutex<Vec<AlacEvent>>>,
    events: Vec<TerminalEvent>,
    responses: Vec<u8>,
    size: TermSize,
    /// Lines scrolled into scrollback on the primary screen since creation.
    scrolled: i64,
    /// Same for the alternate screen (selection anchoring only).
    alt_scrolled: i64,
    /// History size when the probe was placed (fallback if the probe is lost).
    probe_history: usize,
    /// Whether the alternate screen was active when the probe was placed.
    probe_alt: bool,
    selection: Option<UserSelection>,
    search: Option<SearchState>,
    rows: Vec<Arc<SnapshotRow>>,
    seq: u64,
    force_full: bool,
    modify_other_keys: u8,
    x10_mouse: bool,
    title: Option<String>,
    cwd: Option<PathBuf>,
    initial_cwd: Option<PathBuf>,
    hostname: Option<String>,
    blocks: VecDeque<Block>,
    next_block_id: u64,
    query_colors: QueryColors,
}

/// Colors reported to programs that query them (OSC 10/11/12, OSC 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryColors {
    /// Default foreground (OSC 10).
    pub foreground: (u8, u8, u8),
    /// Default background (OSC 11).
    pub background: (u8, u8, u8),
    /// Cursor (OSC 12).
    pub cursor: (u8, u8, u8),
    /// ANSI colors 0-15 (OSC 4).
    pub ansi: [(u8, u8, u8); 16],
}

impl Default for QueryColors {
    fn default() -> Self {
        Self {
            foreground: (0xd8, 0xd8, 0xd8),
            background: (0x18, 0x18, 0x18),
            cursor: (0xd8, 0xd8, 0xd8),
            ansi: [
                (0x00, 0x00, 0x00),
                (0xcd, 0x00, 0x00),
                (0x00, 0xcd, 0x00),
                (0xcd, 0xcd, 0x00),
                (0x00, 0x00, 0xee),
                (0xcd, 0x00, 0xcd),
                (0x00, 0xcd, 0xcd),
                (0xe5, 0xe5, 0xe5),
                (0x7f, 0x7f, 0x7f),
                (0xff, 0x00, 0x00),
                (0x00, 0xff, 0x00),
                (0xff, 0xff, 0x00),
                (0x5c, 0x5c, 0xff),
                (0xff, 0x00, 0xff),
                (0x00, 0xff, 0xff),
                (0xff, 0xff, 0xff),
            ],
        }
    }
}

impl QueryColors {
    fn rgb(&self, index: usize) -> Rgb {
        let (r, g, b) = match index {
            0..=15 => self.ansi[index],
            16..=231 => {
                let i = index - 16;
                let level = |v: usize| if v == 0 { 0 } else { (v * 40 + 55) as u8 };
                (level(i / 36), level((i / 6) % 6), level(i % 6))
            }
            232..=255 => {
                let v = (8 + (index - 232) * 10) as u8;
                (v, v, v)
            }
            256 => self.foreground,
            257 => self.background,
            258 => self.cursor,
            _ => self.foreground,
        };
        Rgb { r, g, b }
    }
}

impl std::fmt::Debug for Emulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Emulator")
            .field("size", &self.size)
            .field("title", &self.title)
            .finish_non_exhaustive()
    }
}

fn probe_point() -> Point {
    Point::new(Line(PROBE_LINE), Column(0))
}

/// Line of a single-point selection, found through the public `intersects_range`.
fn selection_line(selection: &Selection) -> i32 {
    let (mut lo, mut hi) = (i64::from(i32::MIN), i64::from(i32::MAX));
    let line = |v: i64| Line(v as i32);
    if !selection.intersects_range(..=line(hi)) {
        return i32::MAX;
    }
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if selection.intersects_range(..=line(mid)) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo as i32
}

impl Emulator {
    /// Creates an emulator with an empty screen.
    pub fn new(config: EmulatorConfig) -> Self {
        let size = config.size.clamped();
        let queue = Arc::new(Mutex::new(Vec::new()));
        let term_config = Config {
            scrolling_history: config.scrollback,
            semantic_escape_chars: config.word_separators.clone(),
            osc52: Osc52::OnlyCopy,
            kitty_keyboard: false,
            ..Config::default()
        };
        let term = Term::new(
            term_config,
            &size,
            Listener {
                queue: queue.clone(),
            },
        );
        let mut emulator = Self {
            term,
            parser: Processor::new(),
            scanner: Scanner::new(),
            scan_buf: Vec::new(),
            queue,
            events: Vec::new(),
            responses: Vec::new(),
            size,
            scrolled: 0,
            alt_scrolled: 0,
            probe_history: 0,
            probe_alt: false,
            selection: None,
            search: None,
            rows: Vec::new(),
            seq: 0,
            force_full: true,
            modify_other_keys: 0,
            x10_mouse: false,
            title: None,
            cwd: None,
            initial_cwd: config.initial_cwd,
            hostname: local_hostname(),
            blocks: VecDeque::new(),
            next_block_id: 1,
            query_colors: QueryColors::default(),
        };
        emulator.place_probe();
        emulator
    }

    // ---------------------------------------------------------------- parsing

    /// Feeds program output (any chunking; sequences may span calls).
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut found = std::mem::take(&mut self.scan_buf);
        found.clear();
        self.scanner.scan(bytes, &mut found);
        let mut pos = 0;
        for (off, event) in found.drain(..) {
            // Offsets are non-decreasing; one equal to an already fed byte is handled in place.
            let feed_final = off >= pos && off < bytes.len();
            if feed_final {
                self.advance(&bytes[pos..off]);
            }
            let sync_pending = self.parser.sync_bytes_count() > 0;
            if sync_pending && matches!(event, ScanEvent::Osc(_)) {
                // Marks need the real cursor position: end the synchronized update early.
                self.parser.stop_sync(&mut self.term);
            }
            self.read_probe();
            let erase_scroll = match event {
                ScanEvent::EraseDisplay if feed_final && !sync_pending && !self.is_alt() => {
                    Some(self.erase_display_scroll())
                }
                _ => None,
            };
            if feed_final {
                self.advance(&bytes[off..=off]);
                pos = off + 1;
            }
            if let Some(n) = erase_scroll {
                // The erase dropped the probe; account for the lines it pushed into scrollback.
                self.scrolled += n as i64;
                self.place_probe();
            }
            self.read_probe();
            self.handle_scan_event(event);
        }
        if pos < bytes.len() {
            self.advance(&bytes[pos..]);
        }
        self.read_probe();
        self.scan_buf = found;
        self.drain_term_events();
        self.trim_blocks();
    }

    fn advance(&mut self, bytes: &[u8]) {
        if !bytes.is_empty() {
            self.parser.advance(&mut self.term, bytes);
        }
    }

    /// When a synchronized update (`?2026`) is pending, the time it expires.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// Ends an expired synchronized update; call when [`sync_deadline`](Self::sync_deadline) passed.
    pub fn flush_expired_sync(&mut self) {
        if self.sync_deadline().is_some_and(|d| d <= Instant::now()) {
            self.parser.stop_sync(&mut self.term);
            self.read_probe();
            self.drain_term_events();
        }
    }

    fn is_alt(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    fn place_probe(&mut self) {
        self.term.selection = Some(Selection::new(
            SelectionType::Simple,
            probe_point(),
            Side::Left.to_alac(),
        ));
        self.probe_history = self.term.grid().history_size();
        self.probe_alt = self.is_alt();
    }

    /// Accumulates the scrolling observed since the probe was placed, then re-places it.
    fn read_probe(&mut self) {
        let history = self.term.grid().history_size();
        let delta = match &self.term.selection {
            Some(sel) => i64::from(PROBE_LINE) - i64::from(selection_line(sel)),
            // Lost (unexpected): fall back to history growth, exact until the history is full.
            None if self.probe_alt == self.is_alt() => {
                history.saturating_sub(self.probe_history) as i64
            }
            // Screen switched: the other grid did not scroll.
            None => 0,
        };
        if delta != 0 {
            if self.is_alt() {
                self.alt_scrolled += delta;
            } else {
                self.scrolled += delta;
            }
        }
        if delta != 0 || self.term.selection.is_none() || history != self.probe_history {
            self.place_probe();
        }
    }

    /// How many lines `CSI 2 J` will push into scrollback (mirrors alacritty's `clear_viewport`).
    fn erase_display_scroll(&self) -> usize {
        let grid = self.term.grid();
        for line in (0..grid.screen_lines() as i32).rev() {
            let row = &grid[Line(line)];
            if (0..grid.columns()).any(|c| !row[Column(c)].is_empty()) {
                return line as usize + 1;
            }
        }
        0
    }

    fn handle_scan_event(&mut self, event: ScanEvent) {
        match event {
            ScanEvent::Osc(payload) => {
                if let Some(mark) = parse_shell_mark(&payload) {
                    self.shell_mark(mark);
                } else if let Some(path) = parse_osc7(&payload, self.hostname.as_deref()) {
                    if self.cwd.as_ref() != Some(&path) {
                        self.cwd = Some(path.clone());
                        self.events.push(TerminalEvent::CwdChanged(path));
                    }
                }
            }
            ScanEvent::ModifyOtherKeys(level) => self.modify_other_keys = level,
            ScanEvent::X10Mouse(on) => self.x10_mouse = on,
            ScanEvent::ScreenSwitch => {
                self.selection = None;
                self.force_full = true;
                if !self.is_alt() {
                    // Back on the primary screen: drop marks the alternate screen made unreachable.
                    self.trim_blocks();
                }
            }
            ScanEvent::FullReset => {
                self.selection = None;
                self.blocks.clear();
                self.modify_other_keys = 0;
                self.x10_mouse = false;
                self.force_full = true;
            }
            ScanEvent::EraseDisplay | ScanEvent::EraseScrollback => {
                self.selection = None;
                self.force_full = true;
            }
        }
    }

    fn cursor_abs(&self) -> (i64, u32) {
        let p = self.term.grid().cursor.point;
        (self.scrolled + i64::from(p.line.0), p.column.0 as u32)
    }

    fn shell_mark(&mut self, mark: ShellMark) {
        if self.is_alt() {
            return;
        }
        let (line, col) = self.cursor_abs();
        match mark {
            ShellMark::PromptStart => {
                // A repeated `A` on the same line (prompt redraw) replaces the open block.
                if let Some(last) = self.blocks.back() {
                    if last.prompt == line && last.output.is_none() && last.end.is_none() {
                        self.blocks.pop_back();
                    }
                }
                let id = self.next_block_id;
                self.next_block_id += 1;
                self.blocks.push_back(Block {
                    id,
                    prompt: line,
                    command: None,
                    output: None,
                    end: None,
                    exit_code: None,
                });
                while self.blocks.len() > MAX_BLOCKS {
                    self.blocks.pop_front();
                }
                self.events.push(TerminalEvent::PromptStarted);
            }
            ShellMark::CommandStart => {
                if let Some(b) = self.blocks.back_mut().filter(|b| b.end.is_none()) {
                    b.command = Some((line, col));
                }
                self.events.push(TerminalEvent::CommandStarted);
            }
            ShellMark::OutputStart => {
                if let Some(b) = self.blocks.back_mut().filter(|b| b.end.is_none()) {
                    b.output = Some(line);
                }
                self.events.push(TerminalEvent::CommandOutputStarted);
            }
            ShellMark::CommandEnd(code) => {
                if let Some(b) = self.blocks.back_mut().filter(|b| b.end.is_none()) {
                    b.end = Some(line);
                    b.exit_code = code;
                }
                self.events
                    .push(TerminalEvent::CommandFinished { exit_code: code });
            }
        }
    }

    /// First absolute line still held (oldest scrollback line) on the primary screen.
    fn oldest_abs(&self) -> i64 {
        self.scrolled - self.term.grid().history_size() as i64
    }

    fn trim_blocks(&mut self) {
        if self.is_alt() {
            return;
        }
        let oldest = self.oldest_abs();
        while let Some(front) = self.blocks.front() {
            let last = [
                Some(front.prompt),
                front.command.map(|c| c.0),
                front.output,
                front.end,
            ]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or(front.prompt);
            if last < oldest {
                self.blocks.pop_front();
            } else {
                break;
            }
        }
    }

    fn drain_term_events(&mut self) {
        let pending = std::mem::take(&mut *self.queue.lock());
        for event in pending {
            match event {
                AlacEvent::Title(t) => {
                    let t = sanitize_title(&t);
                    self.title = Some(t.clone());
                    self.events.push(TerminalEvent::TitleChanged(Some(t)));
                }
                AlacEvent::ResetTitle => {
                    self.title = None;
                    self.events.push(TerminalEvent::TitleChanged(None));
                }
                AlacEvent::Bell => self.events.push(TerminalEvent::Bell),
                AlacEvent::ClipboardStore(ty, text) => {
                    if text.len() <= MAX_CLIPBOARD_BYTES {
                        let primary =
                            matches!(ty, alacritty_terminal::term::ClipboardType::Selection);
                        self.events.push(TerminalEvent::ClipboardCopy {
                            text,
                            primary_selection: primary,
                        });
                    }
                }
                AlacEvent::ClipboardLoad(..) => {
                    self.events.push(TerminalEvent::ClipboardReadDenied)
                }
                AlacEvent::ColorRequest(index, format) => {
                    let rgb =
                        self.term.colors()[index].unwrap_or_else(|| self.query_colors.rgb(index));
                    self.responses.extend_from_slice(format(rgb).as_bytes());
                }
                AlacEvent::PtyWrite(text) => self.responses.extend_from_slice(text.as_bytes()),
                AlacEvent::TextAreaSizeRequest(format) => {
                    let ws = WindowSize {
                        num_lines: self.size.rows,
                        num_cols: self.size.cols,
                        cell_width: self.size.cell_width,
                        cell_height: self.size.cell_height,
                    };
                    self.responses.extend_from_slice(format(ws).as_bytes());
                }
                _ => {}
            }
        }
        // Bound the queue if nobody drains it (headless use).
        if self.events.len() > MAX_PENDING_EVENTS {
            let excess = self.events.len() - MAX_PENDING_EVENTS;
            self.events.drain(..excess);
        }
    }

    /// Takes the events produced since the last call.
    pub fn take_events(&mut self) -> Vec<TerminalEvent> {
        std::mem::take(&mut self.events)
    }

    /// Takes the bytes the emulator wants written back to the program
    /// (answers to device attribute, cursor position, color and size queries).
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.responses)
    }

    /// Sets the colors reported to programs that query them (match the UI theme).
    pub fn set_query_colors(&mut self, colors: QueryColors) {
        self.query_colors = colors;
    }

    // ---------------------------------------------------------------- state

    /// Current size.
    pub fn size(&self) -> TermSize {
        self.size
    }

    /// Title set by the program (sanitised), if any.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Working directory: the last OSC 7 report, else the starting directory.
    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref().or(self.initial_cwd.as_deref())
    }

    /// Lines currently in scrollback.
    pub fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    /// How far the view is scrolled up (0 = live).
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Modes for rendering and input encoding.
    pub fn modes(&self) -> TerminalModes {
        let m = *self.term.mode();
        let mouse = if m.contains(TermMode::MOUSE_MOTION) {
            MouseMode::Motion
        } else if m.contains(TermMode::MOUSE_DRAG) {
            MouseMode::Drag
        } else if m.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseMode::Click
        } else if self.x10_mouse {
            MouseMode::X10
        } else {
            MouseMode::None
        };
        TerminalModes {
            alt_screen: m.contains(TermMode::ALT_SCREEN),
            bracketed_paste: m.contains(TermMode::BRACKETED_PASTE),
            app_cursor: m.contains(TermMode::APP_CURSOR),
            app_keypad: m.contains(TermMode::APP_KEYPAD),
            mouse,
            sgr_mouse: m.contains(TermMode::SGR_MOUSE),
            utf8_mouse: m.contains(TermMode::UTF8_MOUSE),
            focus_reporting: m.contains(TermMode::FOCUS_IN_OUT),
            alternate_scroll: m.contains(TermMode::ALTERNATE_SCROLL),
            line_feed_new_line: m.contains(TermMode::LINE_FEED_NEW_LINE),
            modify_other_keys: self.modify_other_keys,
        }
    }

    /// Text of grid line `line` (trailing blanks trimmed).
    pub fn line_text(&self, line: i32) -> String {
        let grid = self.term.grid();
        if line < grid.topmost_line().0 || line > grid.bottommost_line().0 {
            return String::new();
        }
        let start = Point::new(Line(line), Column(0));
        let end = Point::new(Line(line), grid.last_column());
        let text = self.term.bounds_to_string(start, end);
        text.trim_end_matches('\n').trim_end().to_owned()
    }

    /// Text of the live screen (rows joined by `\n`, trailing blanks and empty rows trimmed).
    pub fn screen_text(&self) -> String {
        let mut lines: Vec<String> = (0..self.size.rows as i32)
            .map(|l| self.line_text(l))
            .collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    // ---------------------------------------------------------------- resize

    /// Resizes the grid (reflowing soft-wrapped lines when the width changes).
    ///
    /// Command marks are re-anchored by counting logical lines from the cursor,
    /// which reflow keeps on its line; while the alternate screen is active they
    /// are left as they are. The selection is cleared.
    pub fn resize(&mut self, size: TermSize) {
        let size = size.clamped();
        if size.cols == self.size.cols && size.rows == self.size.rows {
            self.size = size;
            return;
        }
        self.read_probe();
        let plan = (!self.is_alt()).then(|| self.reflow_plan());
        let old_cols = self.size.cols;
        self.term.resize(size);
        self.size = size;
        if let Some(plan) = plan {
            self.apply_reflow_plan(plan, old_cols);
        }
        self.selection = None;
        if let Some(search) = &mut self.search {
            search.last = None;
        }
        self.force_full = true;
        self.place_probe();
        self.trim_blocks();
    }

    fn is_wrapped(&self, line: i32) -> bool {
        let grid = self.term.grid();
        grid[Line(line)][grid.last_column()]
            .flags
            .contains(Flags::WRAPLINE)
    }

    /// For each tracked absolute line: (logical lines above the cursor's, row offset within its logical line).
    fn reflow_plan(&self) -> Vec<(i64, Option<(usize, usize)>)> {
        let mut lines: Vec<i64> = Vec::new();
        for b in &self.blocks {
            lines.push(b.prompt);
            lines.extend(b.command.map(|c| c.0));
            lines.extend(b.output);
            lines.extend(b.end);
        }
        lines.sort_unstable();
        lines.dedup();
        let grid = self.term.grid();
        let top = grid.topmost_line().0;
        let cursor = grid.cursor.point.line.0;
        let lowest = lines.first().map_or(cursor, |&l| {
            (l - self.scrolled).clamp(i64::from(top), i64::from(cursor)) as i32
        });
        // Logical index (0 = cursor's logical line) of each row in lowest..=cursor, walking up.
        let mut index_of = vec![0usize; (cursor - lowest + 1).max(1) as usize];
        let mut k = 0usize;
        for r in (lowest..=cursor).rev() {
            index_of[(r - lowest) as usize] = k;
            if r > top && !self.is_wrapped(r - 1) {
                k += 1;
            }
        }
        lines
            .into_iter()
            .map(|abs| {
                let g = abs - self.scrolled;
                if g < i64::from(top) {
                    return (abs, None);
                }
                let g = g.min(i64::from(cursor)) as i32;
                let k = index_of.get((g - lowest) as usize).copied().unwrap_or(0);
                let mut start = g;
                while start > top && self.is_wrapped(start - 1) && g - start < MAX_LOGICAL_ROWS * 4
                {
                    start -= 1;
                }
                (abs, Some((k, (g - start) as usize)))
            })
            .collect()
    }

    fn apply_reflow_plan(&mut self, plan: Vec<(i64, Option<(usize, usize)>)>, old_cols: u16) {
        let max_k = plan
            .iter()
            .filter_map(|(_, p)| p.map(|p| p.0))
            .max()
            .unwrap_or(0);
        let grid = self.term.grid();
        let top = grid.topmost_line().0;
        let cursor = grid.cursor.point.line.0;
        // starts[k] = first row of the k-th logical line above (and including) the cursor's.
        let mut starts: Vec<i32> = Vec::new();
        let mut r = cursor;
        while r >= top && starts.len() <= max_k {
            if r == top || !self.is_wrapped(r - 1) {
                starts.push(r);
            }
            r -= 1;
        }
        let new_cols = usize::from(self.size.cols.max(1));
        let map: std::collections::HashMap<i64, i64> = plan
            .into_iter()
            .map(|(abs, p)| {
                let new_abs = match p.and_then(|(k, offset)| starts.get(k).map(|&s| (k, s, offset)))
                {
                    Some((k, start, offset)) => {
                        let end = if k == 0 { cursor } else { starts[k - 1] - 1 };
                        let row = (offset * usize::from(old_cols)) / new_cols;
                        let line = (start + row as i32).min(end.max(start));
                        self.scrolled + i64::from(line)
                    }
                    // Trimmed by the reflow.
                    None => i64::MIN / 2,
                };
                (abs, new_abs)
            })
            .collect();
        let remap = |l: i64| map.get(&l).copied().unwrap_or(l);
        for b in &mut self.blocks {
            b.prompt = remap(b.prompt);
            b.command = b.command.map(|(l, c)| (remap(l), c));
            b.output = b.output.map(remap);
            b.end = b.end.map(remap);
        }
    }

    // ---------------------------------------------------------------- snapshot

    /// Builds the frame snapshot and resets damage.
    ///
    /// Only rows damaged since the previous call are rebuilt; the others are
    /// shared with the previous snapshot.
    pub fn snapshot(&mut self) -> Snapshot {
        self.flush_expired_sync();
        let rows = usize::from(self.size.rows);
        let mut full = self.force_full || self.rows.len() != rows;
        let mut damaged: Vec<u32> = Vec::new();
        match self.term.damage() {
            TermDamage::Full => full = true,
            TermDamage::Partial(iter) => {
                if !full {
                    damaged.extend(iter.map(|d| d.line as u32).filter(|&l| (l as usize) < rows));
                }
            }
        }
        self.term.reset_damage();
        self.force_full = false;
        let display_offset = self.term.grid().display_offset();
        if full {
            self.rows = (0..rows)
                .map(|r| Arc::new(self.build_row(r, display_offset)))
                .collect();
        } else {
            damaged.sort_unstable();
            damaged.dedup();
            for &r in &damaged {
                let row = Arc::new(self.build_row(r as usize, display_offset));
                if let Some(slot) = self.rows.get_mut(r as usize) {
                    *slot = row;
                }
            }
        }
        self.seq += 1;
        let modes = self.modes();
        Snapshot {
            seq: self.seq,
            size: self.size,
            rows: self.rows.clone(),
            damage: if full {
                Damage::Full
            } else {
                Damage::Rows(damaged)
            },
            cursor: self.cursor_state(display_offset),
            display_offset: display_offset as u32,
            history_size: self.term.grid().history_size() as u32,
            selection: self.selection_span(),
            modes,
        }
    }

    fn build_row(&self, viewport_row: usize, display_offset: usize) -> SnapshotRow {
        let grid = self.term.grid();
        let line = Line(viewport_row as i32 - display_offset as i32);
        let row = &grid[line];
        let colors = self.term.colors();
        let cells: Vec<Cell> = (0..grid.columns())
            .map(|c| Cell::from_alac(&row[Column(c)], colors))
            .collect();
        SnapshotRow {
            cells,
            wrapped: row[grid.last_column()].flags.contains(Flags::WRAPLINE),
        }
    }

    fn cursor_state(&self, display_offset: usize) -> CursorState {
        let grid = self.term.grid();
        let mut point = grid.cursor.point;
        if grid[point].flags.contains(Flags::WIDE_CHAR_SPACER) && point.column.0 > 0 {
            point.column -= 1;
        }
        let style = self.term.cursor_style();
        let shown = self.term.mode().contains(TermMode::SHOW_CURSOR)
            && style.shape != AlacCursorShape::Hidden;
        let row = point.line.0 + display_offset as i32;
        let in_view = (0..i32::from(self.size.rows)).contains(&row);
        CursorState {
            row: row.max(0) as u32,
            col: point.column.0 as u32,
            shape: match style.shape {
                AlacCursorShape::Underline => CursorShape::Underline,
                AlacCursorShape::Beam => CursorShape::Beam,
                AlacCursorShape::HollowBlock => CursorShape::HollowBlock,
                AlacCursorShape::Block | AlacCursorShape::Hidden => CursorShape::Block,
            },
            visible: shown && in_view,
            blinking: style.blinking,
            wide: grid[point].flags.contains(Flags::WIDE_CHAR),
        }
    }

    // ---------------------------------------------------------------- scrolling

    /// Scrolls the view over scrollback.
    pub fn scroll(&mut self, amount: ScrollAmount) {
        let scroll = match amount {
            ScrollAmount::Lines(n) => Scroll::Delta(n),
            ScrollAmount::PageUp => Scroll::PageUp,
            ScrollAmount::PageDown => Scroll::PageDown,
            ScrollAmount::Top => Scroll::Top,
            ScrollAmount::Bottom => Scroll::Bottom,
        };
        self.term.scroll_display(scroll);
    }

    fn scroll_line_into_view_top(&mut self, line: i32) {
        let target = (-line).clamp(0, self.history_size() as i32);
        let delta = target - self.display_offset() as i32;
        if delta != 0 {
            self.term.scroll_display(Scroll::Delta(delta));
        }
    }

    // ---------------------------------------------------------------- selection

    fn active_scrolled(&self) -> i64 {
        if self.is_alt() {
            self.alt_scrolled
        } else {
            self.scrolled
        }
    }

    fn rebased(&self, selection: &Selection, anchor: i64) -> Option<Selection> {
        let delta = self.active_scrolled() - anchor;
        if delta == 0 {
            return Some(selection.clone());
        }
        let delta = i32::try_from(delta).ok()?;
        let region = Line(0)..Line(i32::from(self.size.rows));
        selection.clone().rotate(&self.term, &region, delta)
    }

    fn current_selection(&self) -> Option<Selection> {
        let s = self.selection.as_ref()?;
        self.rebased(&s.selection, s.anchor)
    }

    fn clamp_point(&self, point: GridPoint) -> Point {
        let grid = self.term.grid();
        let line = point
            .line
            .clamp(grid.topmost_line().0, grid.bottommost_line().0);
        let col = (point.col as usize).min(grid.columns() - 1);
        Point::new(Line(line), Column(col))
    }

    /// Starts a selection at `point` (e.g. on mouse down; `Word`/`Line` for double/triple click).
    pub fn start_selection(&mut self, kind: SelectionKind, point: GridPoint, side: Side) {
        let ty = match kind {
            SelectionKind::Simple => SelectionType::Simple,
            SelectionKind::Word => SelectionType::Semantic,
            SelectionKind::Line => SelectionType::Lines,
            SelectionKind::Block => SelectionType::Block,
        };
        let selection = Selection::new(ty, self.clamp_point(point), side.to_alac());
        self.selection = Some(UserSelection {
            selection,
            anchor: self.active_scrolled(),
        });
        self.force_full = true;
    }

    /// Extends the selection to `point` (mouse drag).
    pub fn update_selection(&mut self, point: GridPoint, side: Side) {
        let Some(mut selection) = self.current_selection() else {
            return;
        };
        selection.update(self.clamp_point(point), side.to_alac());
        self.selection = Some(UserSelection {
            selection,
            anchor: self.active_scrolled(),
        });
        self.force_full = true;
    }

    /// Selects everything (scrollback and screen).
    pub fn select_all(&mut self) {
        let grid = self.term.grid();
        let start = Point::new(grid.topmost_line(), Column(0));
        let end = Point::new(grid.bottommost_line(), grid.last_column());
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left.to_alac());
        selection.update(end, Side::Right.to_alac());
        self.selection = Some(UserSelection {
            selection,
            anchor: self.active_scrolled(),
        });
        self.force_full = true;
    }

    /// Clears the selection.
    pub fn clear_selection(&mut self) {
        if self.selection.take().is_some() {
            self.force_full = true;
        }
    }

    /// The selection as an inclusive span, if any (empty selections are `None`).
    pub fn selection_span(&self) -> Option<SelectionSpan> {
        let selection = self.current_selection()?;
        if selection.is_empty() {
            return None;
        }
        let range = selection.to_range(&self.term)?;
        Some(SelectionSpan {
            start: GridPoint::from_alac(range.start),
            end: GridPoint::from_alac(range.end),
            block: range.is_block,
        })
    }

    /// Selected text: soft-wrapped lines are joined, wide characters appear once,
    /// trailing blanks of hard lines are dropped.
    pub fn selection_text(&mut self) -> Option<String> {
        let selection = self.current_selection().filter(|s| !s.is_empty())?;
        self.read_probe();
        let probe = self.term.selection.replace(selection);
        let text = self.term.selection_to_string();
        self.term.selection = probe;
        self.read_probe();
        text
    }

    // ---------------------------------------------------------------- search

    /// Sets (or with an empty pattern clears) the active search.
    pub fn set_search(&mut self, query: &SearchQuery) -> Result<(), TerminalError> {
        if query.pattern.is_empty() {
            self.search = None;
            return Ok(());
        }
        let body = if query.regex {
            query.pattern.clone()
        } else {
            regex::escape(&query.pattern)
        };
        let flags = if query.case_sensitive {
            "(?-i)"
        } else {
            "(?i)"
        };
        let regex = RegexSearch::new(&format!("{flags}{body}"))
            .map_err(|e| TerminalError::InvalidPattern(e.to_string()))?;
        self.search = Some(SearchState { regex, last: None });
        Ok(())
    }

    /// Clears the active search.
    pub fn clear_search(&mut self) {
        self.search = None;
    }

    fn to_match(m: &RangeInclusive<Point>) -> SearchMatch {
        SearchMatch {
            start: GridPoint::from_alac(*m.start()),
            end: GridPoint::from_alac(*m.end()),
        }
    }

    /// Finds the next match in `direction` (wrapping around), scrolls it into
    /// view and returns it. The first search starts from the bottom of the view
    /// (`Previous`) or its top (`Next`).
    pub fn search_next(&mut self, direction: Direction) -> Option<SearchMatch> {
        let scrolled = self.active_scrolled();
        let last = self.search.as_ref()?.last;
        let last = last.and_then(|(m, anchor)| {
            let shift = i32::try_from(scrolled - anchor).ok()?;
            let start = GridPoint::new(m.start.line.checked_sub(shift)?, m.start.col);
            let end = GridPoint::new(m.end.line.checked_sub(shift)?, m.end.col);
            (start.line >= self.term.grid().topmost_line().0).then_some(SearchMatch { start, end })
        });
        let display_offset = self.display_offset() as i32;
        let origin = match (direction, last) {
            (Direction::Next, Some(m)) => m.end.to_alac().add(&self.term, Boundary::None, 1),
            (Direction::Previous, Some(m)) => m.start.to_alac().sub(&self.term, Boundary::None, 1),
            (Direction::Next, None) => Point::new(Line(-display_offset), Column(0)),
            (Direction::Previous, None) => Point::new(
                Line(i32::from(self.size.rows) - 1 - display_offset),
                self.term.grid().last_column(),
            ),
        };
        let (dir, side) = match direction {
            Direction::Next => (AlacDirection::Right, alacritty_terminal::index::Side::Left),
            Direction::Previous => (AlacDirection::Left, alacritty_terminal::index::Side::Left),
        };
        let search = self.search.as_mut()?;
        let found = self
            .term
            .search_next(&mut search.regex, origin, dir, side, None)?;
        let m = Self::to_match(&found);
        search.last = Some((m, scrolled));
        self.term.scroll_to_point(*found.start());
        Some(m)
    }

    /// All matches intersecting the visible rows (for highlighting).
    pub fn visible_matches(&mut self) -> Vec<SearchMatch> {
        let display_offset = self.display_offset() as i32;
        let top = -display_offset;
        let bottom = i32::from(self.size.rows) - 1 - display_offset;
        let start_line = self.logical_start(top);
        let end_line = self.logical_end(bottom);
        let start = Point::new(Line(start_line), Column(0));
        let end = Point::new(Line(end_line), self.term.grid().last_column());
        let Some(search) = self.search.as_mut() else {
            return Vec::new();
        };
        RegexIter::new(
            start,
            end,
            AlacDirection::Right,
            &self.term,
            &mut search.regex,
        )
        .filter(|m| m.end().line.0 >= top && m.start().line.0 <= bottom)
        .take(10_000)
        .map(|m| Self::to_match(&m))
        .collect()
    }

    // ---------------------------------------------------------------- links

    fn logical_start(&self, line: i32) -> i32 {
        let top = self.term.grid().topmost_line().0;
        let mut l = line.max(top);
        while l > top && line - l < MAX_LOGICAL_ROWS && self.is_wrapped(l - 1) {
            l -= 1;
        }
        l
    }

    fn logical_end(&self, line: i32) -> i32 {
        let bottom = self.term.grid().bottommost_line().0;
        let mut l = line.min(bottom);
        while l < bottom && l - line < MAX_LOGICAL_ROWS && self.is_wrapped(l) {
            l += 1;
        }
        l
    }

    /// Text of the logical line `first..=last` and, per character, its cell.
    fn logical_text(&self, first: i32, last: i32) -> (String, Vec<(usize, GridPoint, bool)>) {
        let grid = self.term.grid();
        let mut text = String::new();
        let mut map = Vec::new();
        for line in first..=last {
            let row = &grid[Line(line)];
            for c in 0..grid.columns() {
                let cell = &row[Column(c)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                map.push((
                    text.len(),
                    GridPoint::new(line, c as u32),
                    cell.flags.contains(Flags::WIDE_CHAR),
                ));
                text.push(if cell.c == '\t' { ' ' } else { cell.c });
                if let Some(z) = cell.zerowidth() {
                    text.extend(z.iter());
                }
            }
        }
        (text, map)
    }

    fn hyperlink_spans(&self, first: i32, last: i32) -> Vec<LinkCandidate> {
        let grid = self.term.grid();
        let mut out: Vec<LinkCandidate> = Vec::new();
        let mut current: Option<(
            alacritty_terminal::term::cell::Hyperlink,
            GridPoint,
            GridPoint,
            String,
        )> = None;
        for line in first..=last {
            let row = &grid[Line(line)];
            for c in 0..grid.columns() {
                let cell = &row[Column(c)];
                let link = cell.hyperlink();
                let p = GridPoint::new(line, c as u32);
                match (&mut current, link) {
                    (Some((cur, _, end, text)), Some(l)) if *cur == l => {
                        *end = p;
                        if !cell
                            .flags
                            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                        {
                            text.push(cell.c);
                        }
                    }
                    (_, link) => {
                        if let Some((cur, start, end, text)) = current.take() {
                            out.push(hyperlink_candidate(&cur, start, end, text));
                        }
                        current = link.map(|l| (l, p, p, cell.c.to_string()));
                    }
                }
            }
        }
        if let Some((cur, start, end, text)) = current {
            out.push(hyperlink_candidate(&cur, start, end, text));
        }
        out
    }

    fn links_in_logical(&self, first: i32, last: i32) -> Vec<LinkCandidate> {
        let mut out = self.hyperlink_spans(first, last);
        let (text, map) = self.logical_text(first, last);
        for link in detect_links(&text) {
            let start_idx = map.partition_point(|(off, _, _)| *off < link.range.start);
            let end_idx = map.partition_point(|(off, _, _)| *off < link.range.end);
            let (Some(start), Some(end)) = (
                map.get(start_idx),
                end_idx.checked_sub(1).and_then(|i| map.get(i)),
            ) else {
                continue;
            };
            let mut end_point = end.1;
            if end.2 {
                end_point.col += 1;
            }
            let candidate = LinkCandidate {
                start: start.1,
                end: end_point,
                text: text[link.range.clone()].to_owned(),
                target: link.target,
                source: LinkSource::Detected,
            };
            // OSC 8 links win over detected text.
            if !out
                .iter()
                .any(|o| o.contains(candidate.start) || candidate.contains(o.start))
            {
                out.push(candidate);
            }
        }
        out.sort_by_key(|c| (c.start.line, c.start.col));
        out
    }

    /// The link under `point`, if any (OSC 8 first, then detected text).
    /// File targets are unresolved; check them with [`resolve_link`](crate::resolve_link).
    pub fn link_at(&self, point: GridPoint) -> Option<LinkCandidate> {
        let grid = self.term.grid();
        if point.line < grid.topmost_line().0 || point.line > grid.bottommost_line().0 {
            return None;
        }
        let first = self.logical_start(point.line);
        let last = self.logical_end(point.line);
        self.links_in_logical(first, last)
            .into_iter()
            .find(|c| c.contains(point))
    }

    /// Every link candidate on the visible rows (unresolved; for hover hints).
    pub fn visible_links(&self) -> Vec<LinkCandidate> {
        let display_offset = self.display_offset() as i32;
        let top = -display_offset;
        let bottom = i32::from(self.size.rows) - 1 - display_offset;
        let mut out = Vec::new();
        let mut line = self.logical_start(top);
        while line <= bottom {
            let last = self.logical_end(line);
            out.extend(self.links_in_logical(line, last));
            line = last + 1;
        }
        out
    }

    // ---------------------------------------------------------------- commands

    /// Command blocks still (at least partly) in the buffer, oldest first.
    pub fn command_blocks(&self) -> Vec<CommandBlock> {
        if self.is_alt() {
            return Vec::new();
        }
        let oldest = self.oldest_abs();
        let rel = |abs: i64| -> Option<i32> {
            if abs < oldest {
                return None;
            }
            i32::try_from(abs - self.scrolled).ok()
        };
        self.blocks
            .iter()
            .filter_map(|b| {
                let prompt = rel(b.prompt);
                let block = CommandBlock {
                    id: b.id,
                    prompt_line: prompt.unwrap_or_else(|| (oldest - self.scrolled) as i32),
                    command_start: b
                        .command
                        .and_then(|(l, c)| rel(l).map(|l| GridPoint::new(l, c))),
                    output_line: b.output.and_then(rel),
                    end_line: b.end.and_then(rel),
                    exit_code: b.exit_code,
                    truncated: prompt.is_none(),
                };
                let any_left = !block.truncated
                    || block.output_line.is_some()
                    || block.end_line.is_some()
                    || block.command_start.is_some();
                any_left.then_some(block)
            })
            .collect()
    }

    /// The command line typed in `block` (from its `B` mark to the end of that logical line).
    pub fn command_text(&self, block: &CommandBlock) -> Option<String> {
        let start = block.command_start?;
        let last = self.logical_end(start.line);
        let end = Point::new(Line(last), self.term.grid().last_column());
        let text = self.term.bounds_to_string(start.to_alac(), end);
        Some(text.trim_end().to_owned())
    }

    /// Scrolls so the previous/next command's prompt is at the top of the view
    /// and returns that block.
    pub fn scroll_to_command(&mut self, direction: Direction) -> Option<CommandBlock> {
        let top = -(self.display_offset() as i32);
        let blocks = self.command_blocks();
        let target = match direction {
            Direction::Previous => blocks.iter().rev().find(|b| b.prompt_line < top).copied(),
            Direction::Next => blocks.iter().find(|b| b.prompt_line > top).copied(),
        }?;
        self.scroll_line_into_view_top(target.prompt_line);
        Some(target)
    }

    // ---------------------------------------------------------------- input

    /// Encodes a key for the current modes (see [`input::encode_key`]).
    pub fn encode_key(&self, key: Key, mods: Modifiers) -> Option<Vec<u8>> {
        input::encode_key(key, mods, &self.modes())
    }

    /// Encodes pasted text for the current modes (see [`input::encode_paste`]).
    pub fn encode_paste(&self, text: &str) -> Vec<u8> {
        input::encode_paste(text, self.modes().bracketed_paste)
    }

    /// Encodes a mouse event for the current modes (see [`input::encode_mouse`]).
    pub fn encode_mouse(&self, event: MouseEvent) -> Option<Vec<u8>> {
        input::encode_mouse(event, &self.modes())
    }

    /// Encodes a focus change for the current modes (see [`input::encode_focus`]).
    pub fn encode_focus(&mut self, focused: bool) -> Option<Vec<u8>> {
        self.term.is_focused = focused;
        input::encode_focus(focused, &self.modes())
    }

    /// Converts a viewport cell to grid coordinates for the current scroll position.
    pub fn viewport_point(&self, row: u32, col: u32) -> GridPoint {
        crate::geometry::viewport_to_grid(row, col, self.display_offset() as u32)
    }

    /// Viewport row of grid line `line`, if visible.
    pub fn viewport_row(&self, line: i32) -> Option<u32> {
        grid_to_viewport(
            GridPoint::new(line, 0),
            self.display_offset() as u32,
            self.size.rows,
        )
    }
}

fn hyperlink_candidate(
    link: &alacritty_terminal::term::cell::Hyperlink,
    start: GridPoint,
    end: GridPoint,
    text: String,
) -> LinkCandidate {
    let uri = link.uri().to_owned();
    let target = match uri.strip_prefix("file://") {
        Some(rest) => match parse_osc7(
            format!("7;file://{rest}").as_bytes(),
            local_hostname().as_deref(),
        ) {
            Some(path) => LinkTarget::File {
                path,
                line: None,
                col: None,
            },
            None => LinkTarget::Url(uri),
        },
        None => LinkTarget::Url(uri),
    };
    LinkCandidate {
        start,
        end,
        text,
        target,
        source: LinkSource::Hyperlink,
    }
}
