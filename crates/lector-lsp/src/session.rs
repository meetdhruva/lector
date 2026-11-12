//! The UI-free completion session: what the popup shows and the edit that
//! accepting an item makes. No requests are made here; the UI sends
//! [`CompletionSession::request`] through `LspManager::completion`, feeds the
//! answer to [`CompletionSession::set_response`], calls
//! [`CompletionSession::update`] after every edit or caret move and applies
//! the [`CompletionCommit`] of [`CompletionSession::commit`] to the buffer.
//!
//! Coordinates: the text is a `lector_text::Rope`, offsets are byte offsets,
//! the server speaks UTF-16 and is converted with the rope's converters.

use std::cmp::Reverse;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;

use lector_text::{Bias, PointUtf16, Rope};

use crate::completion::{
    CompletionItem, CompletionRequest, CompletionResponse, CompletionTrigger, InsertFormat,
    InsertTextMode, ResolvedCompletion,
};
use crate::fuzzy::{self, FuzzyIndex, Query, Scratch};
use crate::snippet::{Snippet, TabStop};
use crate::types::{Command, TextEdit};

/// Score subtracted from deprecated items.
const DEPRECATED_PENALTY: i32 = 3000;
/// How many (tag, caret) pairs are remembered to place late edits.
const HISTORY: usize = 64;

/// Characters that belong to a word for "the word before the cursor".
/// Letters, digits and `_` always do; languages add a few (`$`, `-`, ...).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WordChars {
    extra: Vec<char>,
}

impl WordChars {
    /// Word chars with `extra` added to alphanumerics and `_`.
    pub fn new(extra: &str) -> WordChars {
        WordChars {
            extra: extra.chars().collect(),
        }
    }

    /// Word characters of a language (names as in `Language::name`).
    pub fn for_language(language: &str) -> WordChars {
        let extra = match language {
            "JavaScript" | "TypeScript" | "TSX" | "JSX" | "PHP" | "Perl" | "Bash" | "Shell" => "$",
            "CSS" | "SCSS" | "LESS" | "HTML" | "Vue" | "Svelte" => "-",
            "Lisp" | "Clojure" | "Scheme" | "Racket" | "Elisp" => "-?!*<>=/+",
            "Ruby" => "?!",
            _ => "",
        };
        WordChars::new(extra)
    }

    /// Whether `c` is part of a word.
    pub fn is_word(&self, c: char) -> bool {
        c.is_alphanumeric() || c == '_' || self.extra.contains(&c)
    }
}

/// Whether accepting replaces only the text before the cursor or the whole word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InsertMode {
    /// Insert at the cursor; the text after it stays (the default).
    #[default]
    Insert,
    /// Replace the word under the cursor, including the part after it.
    Replace,
}

/// Settings of a session (`editor.completion.*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOptions {
    /// `editor.completion.insert_mode`.
    pub insert_mode: InsertMode,
    /// `editor.completion.snippets`: keep snippet tab stops. When off, snippet
    /// text (should a server send some anyway) is flattened to plain text.
    pub snippets: bool,
    /// Word characters of the document's language.
    pub word_chars: WordChars,
    /// Typing an item's commit character accepts the selected item.
    pub commit_characters: bool,
}

impl Default for SessionOptions {
    fn default() -> Self {
        SessionOptions {
            insert_mode: InsertMode::Insert,
            snippets: false,
            word_chars: WordChars::default(),
            commit_characters: true,
        }
    }
}

/// What opened the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenReason {
    /// An identifier character was typed.
    Typing,
    /// A server trigger character was typed (the caret is after it).
    TriggerCharacter(char),
    /// Ctrl+Space.
    Manual,
}

/// Why a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The caret left the word (other row, before its start).
    CaretMoved,
    /// A non-word character was typed.
    LeftWord,
    /// Nothing matches the typed text any more.
    NoMatches,
    /// The text before the word changed.
    TextChanged,
    /// An item was accepted.
    Committed,
    /// Escape (or the UI closed it).
    Dismissed,
}

/// What [`CompletionSession::update`] asks of the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    /// Keep showing the (re-filtered) list.
    Continue,
    /// The list was `isIncomplete`: send this request (the manager cancels
    /// the one in flight) and feed the answer to `set_response`.
    Requery(CompletionRequest),
    /// Hide the popup.
    Close(CloseReason),
}

/// One row of the filtered list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    /// Index into [`CompletionSession::items`].
    pub item: usize,
    /// Rank score (higher is better; deprecated items are reduced).
    pub score: i32,
}

/// A replacement in byte offsets of the text it was computed for. Same shape
/// as `lector_buffer::TextEdit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteEdit {
    /// Range to replace.
    pub range: Range<usize>,
    /// Replacement.
    pub new_text: String,
}

/// The result of accepting an item.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionCommit {
    /// Edits to apply as one undo step (`Buffer::apply_edits`): sorted,
    /// non-overlapping, offsets in the text passed to `commit`.
    pub edits: Vec<ByteEdit>,
    /// New caret offsets (after the edits), one per caret passed in, same
    /// order. With snippet tab stops the primary caret is at the start of the
    /// first stop.
    pub carets: Vec<usize>,
    /// Snippet tab stops of the primary caret, in visiting order, with ranges
    /// in the text after the edits. Empty unless snippets are on.
    pub tab_stops: Vec<TabStop>,
    /// A command the item asks to run afterwards (`LspManager::execute_command`),
    /// see [`CompletionCommit::triggers_signature_help`].
    pub command: Option<Command>,
    /// Additional edits dropped because they overlapped the main edit or could
    /// not be placed in the current text.
    pub skipped_additional_edits: usize,
    /// Other carets that were left alone (different prefix, or overlapping).
    pub skipped_carets: usize,
}

impl CompletionCommit {
    /// The item asked to open signature help (`editor.action.triggerParameterHints`).
    pub fn triggers_signature_help(&self) -> bool {
        self.command
            .as_ref()
            .is_some_and(|c| c.command == "editor.action.triggerParameterHints")
    }
}

/// Why [`CompletionSession::commit`] failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CommitError {
    /// No such item.
    #[error("no such completion item")]
    NoSuchItem,
    /// No carets given, or the primary index is out of range.
    #[error("no primary caret")]
    NoCaret,
    /// The caret is no longer in the word the session was opened for.
    #[error("the text changed since completion was requested")]
    TextChanged,
}

struct Entry {
    group: u16,
    rank: u32,
    preselect: bool,
    deprecated: bool,
}

#[derive(Default)]
struct ExtraEdits {
    edits: Vec<TextEdit>,
    /// Caret position (UTF-16) in the text the edits were computed for.
    base: Option<PointUtf16>,
}

/// The completion popup model. See the module docs.
pub struct CompletionSession {
    options: SessionOptions,
    reason: OpenReason,
    anchor: usize,
    anchor_row: u32,
    line_head: u64,
    caret: usize,
    caret_pos: PointUtf16,
    typed: String,
    history: Vec<(u64, PointUtf16)>,
    closed: Option<CloseReason>,
    // List state.
    generation: u64,
    items: Vec<CompletionItem>,
    entries: Vec<Entry>,
    extras: Vec<Option<ExtraEdits>>,
    index: FuzzyIndex,
    group_starts: Vec<usize>,
    base_order: Vec<Match>,
    is_incomplete: bool,
    resolve_supported: bool,
    has_response: bool,
    response_base: PointUtf16,
    // Filter state.
    queries: Vec<Query>,
    matches: Vec<Match>,
    scratch: Scratch,
    requested_typed: Option<String>,
    // Selection.
    selected: usize,
    user_moved: bool,
}

impl std::fmt::Debug for CompletionSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompletionSession")
            .field("typed", &self.typed)
            .field("items", &self.items.len())
            .field("matches", &self.matches.len())
            .field("closed", &self.closed)
            .finish()
    }
}

fn hash_of(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn offset_of(text: &Rope, p: PointUtf16) -> usize {
    text.point_utf16_to_offset(p)
        .min(text.line_range(p.row).end)
}

/// Maps a position of the text the server saw (caret at `base`) to an offset
/// in the current text (caret at `now`), assuming the user only typed or
/// deleted around the caret: positions after the old caret on its row move
/// with it. Range starts at the old caret stay (they are before what was
/// typed since), range ends at it move.
fn map_position(
    text: &Rope,
    p: PointUtf16,
    is_end: bool,
    base: PointUtf16,
    now: PointUtf16,
) -> usize {
    let shifts = p.row == base.row
        && p.row == now.row
        && (p.column > base.column || (is_end && p.column == base.column));
    let column = if shifts {
        (i64::from(p.column) + i64::from(now.column) - i64::from(base.column)).max(0) as u32
    } else {
        p.column
    };
    offset_of(text, PointUtf16 { row: p.row, column })
}

fn line_start(text: &Rope, offset: usize) -> usize {
    text.line_range(text.offset_to_point(offset).row).start
}

fn eol_of(text: &Rope) -> &'static str {
    let first = text.line_range(0);
    if text.byte_at(first.end) == Some(b'\r') {
        "\r\n"
    } else {
        "\n"
    }
}

/// Multi-line insert text re-indented and with the document's line endings.
struct Shaped {
    text: String,
    /// Per line: (offset in the source, offset in `text`, bytes of leading
    /// whitespace removed, bytes of indentation added).
    lines: Vec<(usize, usize, usize, usize)>,
}

impl Shaped {
    fn map(&self, offset: usize) -> usize {
        let i = self
            .lines
            .partition_point(|(orig, ..)| *orig <= offset)
            .saturating_sub(1);
        let (orig, new, stripped, added) = self.lines[i];
        let within = (offset - orig).saturating_sub(stripped);
        (new + added + within).min(self.text.len())
    }
}

fn shape(src: &str, adjust: bool, indent: &str, eol: &str) -> Shaped {
    if !src.contains('\n') {
        return Shaped {
            text: src.to_string(),
            lines: vec![(0, 0, 0, 0)],
        };
    }
    let lines: Vec<&str> = src.split('\n').collect();
    let leading = |l: &str| l.len() - l.trim_start_matches([' ', '\t']).len();
    let strip = if adjust {
        lines[1..]
            .iter()
            .filter(|l| !l.trim_end_matches('\r').is_empty())
            .map(|l| leading(l))
            .min()
            .unwrap_or(0)
    } else {
        0
    };
    let mut out = String::with_capacity(src.len() + indent.len() * lines.len());
    let mut table = Vec::with_capacity(lines.len());
    let mut orig = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let last = i + 1 == lines.len();
        if i == 0 {
            out.push_str(line.strip_suffix('\r').filter(|_| !last).unwrap_or(line));
            table.push((orig, 0, 0, 0));
        } else {
            out.push_str(eol);
            let blank = line.trim_end_matches('\r').is_empty();
            let stripped = if adjust { strip.min(leading(line)) } else { 0 };
            let added = if adjust && !blank { indent.len() } else { 0 };
            table.push((orig, out.len(), stripped, added));
            if adjust && !blank {
                out.push_str(indent);
            }
            let body = &line[stripped..];
            out.push_str(body.strip_suffix('\r').filter(|_| !last).unwrap_or(body));
        }
        orig += line.len() + 1;
    }
    Shaped {
        text: out,
        lines: table,
    }
}

impl CompletionSession {
    /// Opens a session with the caret at byte offset `caret` of `text`. The
    /// word before the caret (per [`WordChars`]) is the typed prefix.
    pub fn open(
        text: &Rope,
        caret: usize,
        reason: OpenReason,
        options: SessionOptions,
    ) -> CompletionSession {
        let caret = text.clip_offset(caret.min(text.len()), Bias::Left);
        let mut anchor = caret;
        for c in text.reversed_chars_at(caret) {
            if !options.word_chars.is_word(c) {
                break;
            }
            anchor -= c.len_utf8();
        }
        let caret_pos = text.offset_to_point_utf16(caret);
        let mut session = CompletionSession {
            options,
            reason,
            anchor,
            anchor_row: caret_pos.row,
            line_head: hash_of(&text.text_for_range(line_start(text, anchor)..anchor)),
            caret,
            caret_pos,
            typed: text.text_for_range(anchor..caret),
            history: vec![(text.version(), caret_pos)],
            closed: None,
            generation: 0,
            items: vec![],
            entries: vec![],
            extras: vec![],
            index: FuzzyIndex::default(),
            group_starts: vec![anchor],
            base_order: vec![],
            is_incomplete: false,
            resolve_supported: false,
            has_response: false,
            response_base: caret_pos,
            queries: vec![],
            matches: vec![],
            scratch: Scratch::default(),
            requested_typed: None,
            selected: 0,
            user_moved: false,
        };
        session.requested_typed = Some(session.typed.clone());
        session
    }

    /// The request to send for this session's opening.
    pub fn request(&self) -> CompletionRequest {
        CompletionRequest {
            pos: self.caret_pos,
            trigger: match self.reason {
                OpenReason::TriggerCharacter(c) => CompletionTrigger::TriggerCharacter(c),
                OpenReason::Typing | OpenReason::Manual => CompletionTrigger::Invoked,
            },
        }
    }

    /// The session ended (the UI should hide the popup).
    pub fn is_closed(&self) -> bool {
        self.closed.is_some()
    }

    /// Why the session ended.
    pub fn close_reason(&self) -> Option<CloseReason> {
        self.closed
    }

    /// Ends the session (Escape).
    pub fn close(&mut self, reason: CloseReason) {
        self.closed.get_or_insert(reason);
    }

    /// Byte offset where the typed word starts (where to anchor the popup).
    pub fn anchor(&self) -> usize {
        self.anchor
    }

    /// The text typed since the word start (the filter query).
    pub fn typed(&self) -> &str {
        &self.typed
    }

    /// Whether the server's answer arrived.
    pub fn has_response(&self) -> bool {
        self.has_response
    }

    /// The list is incomplete: further typing asks the server again.
    pub fn is_incomplete(&self) -> bool {
        self.is_incomplete
    }

    /// Changes whenever [`CompletionSession::set_response`] replaces the
    /// items; guards late resolve answers (see `apply_resolved`).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// All items of the last answer (not filtered).
    pub fn items(&self) -> &[CompletionItem] {
        &self.items
    }

    /// One item.
    pub fn item(&self, index: usize) -> Option<&CompletionItem> {
        self.items.get(index)
    }

    /// The filtered, ranked rows (indices into [`CompletionSession::items`]).
    pub fn matches(&self) -> &[Match] {
        &self.matches
    }

    /// Takes the server's answer (replacing earlier items). Returns false and
    /// ignores it when the session is closed, the answer is for another row
    /// or the caret left the word.
    pub fn set_response(
        &mut self,
        text: &Rope,
        caret: usize,
        response: CompletionResponse,
    ) -> bool {
        if self.closed.is_some() || response.request_pos.row != self.anchor_row {
            return false;
        }
        if !self.track_caret(text, caret) {
            return false;
        }
        self.generation += 1;
        self.response_base = response.request_pos;
        self.resolve_supported = response.resolve_supported;
        self.is_incomplete = response.is_incomplete;
        self.has_response = true;
        let base = response.request_pos;
        let now = self.caret_pos;
        self.items = response.items;

        // Ranks by sortText (dense), groups by the start of each item's edit.
        let mut order: Vec<usize> = (0..self.items.len()).collect();
        let sort_key = |i: &CompletionItem| i.sort_text.clone().unwrap_or_else(|| i.label.clone());
        let keys: Vec<String> = self.items.iter().map(sort_key).collect();
        order.sort_by(|&a, &b| keys[a].cmp(&keys[b]));
        let mut ranks = vec![0u32; self.items.len()];
        let mut rank = 0u32;
        for (n, &i) in order.iter().enumerate() {
            if n > 0 && keys[i] != keys[order[n - 1]] {
                rank += 1;
            }
            ranks[i] = rank;
        }
        self.group_starts = vec![self.anchor];
        self.index = FuzzyIndex::with_capacity(self.items.len());
        self.entries = Vec::with_capacity(self.items.len());
        self.extras = Vec::with_capacity(self.items.len());
        for (n, item) in self.items.iter().enumerate() {
            let start = item
                .edit
                .as_ref()
                .map(|e| match self.options.insert_mode {
                    InsertMode::Insert => &e.insert,
                    InsertMode::Replace => &e.replace,
                })
                .filter(|r| r.start.row == self.anchor_row)
                .map(|r| map_position(text, r.start, false, base, now))
                .filter(|&s| s <= self.caret);
            let group = match start {
                Some(s) if s != self.anchor => match self.group_starts.iter().position(|&g| g == s)
                {
                    Some(g) => g,
                    None if self.group_starts.len() < 256 => {
                        self.group_starts.push(s);
                        self.group_starts.len() - 1
                    }
                    None => 0,
                },
                _ => 0,
            };
            self.index
                .push(item.filter_text.as_deref().unwrap_or(&item.label));
            self.entries.push(Entry {
                group: group as u16,
                rank: ranks[n],
                preselect: item.preselect,
                deprecated: item.deprecated,
            });
            self.extras
                .push((!item.additional_edits.is_empty()).then(|| ExtraEdits {
                    edits: item.additional_edits.clone(),
                    base: Some(base),
                }));
        }
        // Empty-query order, reused while nothing is typed.
        self.base_order = (0..self.items.len())
            .map(|i| Match {
                item: i,
                score: self.entry_score(i, 0),
            })
            .collect();
        let mut order = std::mem::take(&mut self.base_order);
        order.sort_unstable_by_key(|m| self.sort_key(m));
        self.base_order = order;
        self.queries.clear();
        self.matches.clear();
        self.refilter(text);
        self.user_moved = false;
        self.reset_selection();
        true
    }

    fn entry_score(&self, item: usize, fuzzy: i32) -> i32 {
        fuzzy
            - if self.entries[item].deprecated {
                DEPRECATED_PENALTY
            } else {
                0
            }
    }

    fn sort_key(&self, m: &Match) -> (Reverse<i32>, u32, bool, usize) {
        let e = &self.entries[m.item];
        (Reverse(m.score), e.rank, !e.preselect, m.item)
    }

    /// Records the caret; false when it left the word.
    fn track_caret(&mut self, text: &Rope, caret: usize) -> bool {
        let caret = text.clip_offset(caret.min(text.len()), Bias::Left);
        let pos = text.offset_to_point_utf16(caret);
        if pos.row != self.anchor_row || caret < self.anchor {
            return false;
        }
        self.caret = caret;
        self.caret_pos = pos;
        self.typed = text.text_for_range(self.anchor..caret);
        let version = text.version();
        if let Some(entry) = self.history.iter_mut().find(|(v, _)| *v == version) {
            entry.1 = pos;
        } else {
            if self.history.len() >= HISTORY {
                self.history.remove(0);
            }
            self.history.push((version, pos));
        }
        true
    }

    /// Call after every edit or caret move while the session is open.
    /// `caret` is the (primary) caret offset in `text`.
    pub fn update(&mut self, text: &Rope, caret: usize) -> Update {
        if let Some(reason) = self.closed {
            return Update::Close(reason);
        }
        if !self.track_caret(text, caret) {
            return self.finish(CloseReason::CaretMoved);
        }
        let head = hash_of(&text.text_for_range(line_start(text, self.anchor)..self.anchor));
        if head != self.line_head {
            return self.finish(CloseReason::TextChanged);
        }
        if self
            .typed
            .chars()
            .any(|c| !self.options.word_chars.is_word(c))
        {
            return self.finish(CloseReason::LeftWord);
        }
        if !self.has_response {
            return Update::Continue;
        }
        let changed = self.requested_typed.as_deref() != Some(self.typed.as_str());
        let previous = self.selected_item_index();
        self.refilter(text);
        if self.matches.is_empty() {
            if self.is_incomplete {
                self.requested_typed = Some(self.typed.clone());
                return Update::Requery(self.requery());
            }
            return self.finish(CloseReason::NoMatches);
        }
        match previous.filter(|_| self.user_moved) {
            Some(item) => {
                self.selected = self
                    .matches
                    .iter()
                    .position(|m| m.item == item)
                    .unwrap_or(0);
                if self.selected == 0 {
                    self.user_moved = false;
                }
            }
            None => self.reset_selection(),
        }
        if self.is_incomplete && changed {
            self.requested_typed = Some(self.typed.clone());
            return Update::Requery(self.requery());
        }
        Update::Continue
    }

    fn requery(&self) -> CompletionRequest {
        CompletionRequest {
            pos: self.caret_pos,
            trigger: CompletionTrigger::Incomplete,
        }
    }

    fn finish(&mut self, reason: CloseReason) -> Update {
        self.closed = Some(reason);
        Update::Close(reason)
    }

    fn refilter(&mut self, text: &Rope) {
        let queries: Vec<Query> = self
            .group_starts
            .iter()
            .map(|&s| {
                if s == self.anchor {
                    Query::new(&self.typed)
                } else if s <= self.caret {
                    Query::new(&text.text_for_range(s..self.caret))
                } else {
                    Query::default()
                }
            })
            .collect();
        if queries.iter().all(Query::is_empty) {
            self.matches = self.base_order.clone();
            self.queries = queries;
            return;
        }
        // Items matching a longer query also match its prefixes, so extending
        // the query only searches the previous survivors.
        let narrowing = !self.queries.is_empty()
            && self.queries.len() == queries.len()
            && self
                .queries
                .iter()
                .zip(&queries)
                .all(|(old, new)| old.is_prefix_of(new))
            && !self.queries.iter().all(Query::is_empty);
        let candidates: Vec<usize> = if narrowing {
            self.matches.iter().map(|m| m.item).collect()
        } else {
            (0..self.items.len()).collect()
        };
        let mut out = Vec::with_capacity(candidates.len().min(self.items.len()));
        for item in candidates {
            let q = &queries[self.entries[item].group as usize];
            if let Some(score) = fuzzy::score(&self.index, item, q, &mut self.scratch) {
                out.push(Match {
                    item,
                    score: self.entry_score(item, score),
                });
            }
        }
        out.sort_unstable_by_key(|m| self.sort_key(m));
        self.matches = out;
        self.queries = queries;
    }

    // Selection.

    fn reset_selection(&mut self) {
        self.selected = match self.matches.first() {
            Some(first) => self
                .matches
                .iter()
                .take_while(|m| m.score == first.score)
                .position(|m| self.entries[m.item].preselect)
                .unwrap_or(0),
            None => 0,
        };
    }

    fn selected_item_index(&self) -> Option<usize> {
        self.matches.get(self.selected).map(|m| m.item)
    }

    /// Position of the selected row in [`CompletionSession::matches`].
    pub fn selected(&self) -> Option<usize> {
        (!self.matches.is_empty()).then_some(self.selected)
    }

    /// The selected item: its index in [`CompletionSession::items`] and the item.
    pub fn selected_item(&self) -> Option<(usize, &CompletionItem)> {
        let i = self.selected_item_index()?;
        Some((i, &self.items[i]))
    }

    /// Selects row `row` (clamped).
    pub fn select(&mut self, row: usize) {
        if !self.matches.is_empty() {
            self.selected = row.min(self.matches.len() - 1);
            self.user_moved = true;
        }
    }

    /// Moves the selection by `delta` rows, wrapping around.
    pub fn select_relative(&mut self, delta: isize) {
        let n = self.matches.len() as isize;
        if n > 0 {
            self.selected = (self.selected as isize + delta).rem_euclid(n) as usize;
            self.user_moved = true;
        }
    }

    /// Next row (wraps).
    pub fn select_next(&mut self) {
        self.select_relative(1);
    }

    /// Previous row (wraps).
    pub fn select_previous(&mut self) {
        self.select_relative(-1);
    }

    /// Moves by a page without wrapping (PageUp/PageDown).
    pub fn select_page(&mut self, rows: isize) {
        if !self.matches.is_empty() {
            let target = (self.selected as isize + rows).clamp(0, self.matches.len() as isize - 1);
            self.select(target as usize);
        }
    }

    /// Byte ranges of the label to highlight as matching the typed text.
    pub fn label_highlights(&self, row: usize) -> Vec<Range<usize>> {
        let Some(m) = self.matches.get(row) else {
            return vec![];
        };
        let item = &self.items[m.item];
        let group = self.entries[m.item].group as usize;
        let Some(query) = self.queries.get(group).filter(|q| !q.is_empty()) else {
            return vec![];
        };
        let Some(positions) = fuzzy::positions(&item.label, query) else {
            return vec![];
        };
        let offsets: Vec<(usize, usize)> = item
            .label
            .char_indices()
            .map(|(i, c)| (i, i + c.len_utf8()))
            .collect();
        let mut out: Vec<Range<usize>> = Vec::new();
        for p in positions {
            let Some(&(s, e)) = offsets.get(p) else {
                continue;
            };
            match out.last_mut() {
                Some(last) if last.end == s => last.end = e,
                _ => out.push(s..e),
            }
        }
        out
    }

    // Commit characters.

    /// Whether typing `c` should accept the selected item first (then insert
    /// `c`). Call before inserting the character. Needs a visible match and
    /// either typed text or an explicit selection, so a freshly opened list
    /// does not swallow `(`.
    pub fn is_commit_character(&self, c: char) -> bool {
        if !self.options.commit_characters || self.closed.is_some() {
            return false;
        }
        if self.typed.is_empty() && !self.user_moved {
            return false;
        }
        self.selected_item()
            .is_some_and(|(_, item)| item.commit_characters.contains(&c))
    }

    // Resolve.

    /// Whether `completionItem/resolve` may add documentation, detail and
    /// additional edits to `item` (the server supports it and it has not run).
    pub fn needs_resolve(&self, item: usize) -> bool {
        self.resolve_supported && self.items.get(item).is_some_and(|i| !i.resolved)
    }

    /// Merges a resolve answer into the item. `generation` is
    /// [`CompletionSession::generation`] at the time the request was made;
    /// answers for an older list are ignored (returns false).
    pub fn apply_resolved(
        &mut self,
        generation: u64,
        item: usize,
        resolved: ResolvedCompletion,
    ) -> bool {
        if generation != self.generation || item >= self.items.len() {
            return false;
        }
        let base = resolved.tag.and_then(|tag| {
            self.history
                .iter()
                .find(|(v, _)| *v == tag)
                .map(|(_, p)| *p)
        });
        let it = &mut self.items[item];
        it.resolved = true;
        if resolved.detail.is_some() {
            it.detail = resolved.detail;
        }
        if resolved.documentation.is_some() {
            it.documentation = resolved.documentation;
        }
        if resolved.label_details.is_some() {
            it.label_details = resolved.label_details;
        }
        if !resolved.additional_edits.is_empty() {
            it.additional_edits = resolved.additional_edits.clone();
            self.extras[item] = Some(ExtraEdits {
                edits: resolved.additional_edits,
                base,
            });
        }
        true
    }

    // Commit.

    fn word_end(&self, text: &Rope, from: usize) -> usize {
        let mut end = from;
        for c in text.chars_at(from) {
            if !self.options.word_chars.is_word(c) {
                break;
            }
            end += c.len_utf8();
        }
        end
    }

    /// The edit that accepts `item` (an index into [`CompletionSession::items`]).
    ///
    /// `carets` are the offsets of all (empty) carets in ascending order and
    /// `primary` indexes the one the session was opened at. The main edit
    /// replaces the item's `textEdit` range (insert or replace range per
    /// [`InsertMode`]), or else the typed word, with the item's text; the
    /// range is re-anchored to what was typed since the request. Other carets
    /// get the same text over their own text when the text before (and, for
    /// replace mode, after) them equals the primary's; otherwise they are left
    /// alone. Additional edits are for the primary only. Resolve first when
    /// [`CompletionSession::needs_resolve`] says it can add edits.
    pub fn commit(
        &self,
        text: &Rope,
        carets: &[usize],
        primary: usize,
        item: usize,
    ) -> Result<CompletionCommit, CommitError> {
        let it = self.items.get(item).ok_or(CommitError::NoSuchItem)?;
        let &p = carets.get(primary).ok_or(CommitError::NoCaret)?;
        let p = text.clip_offset(p.min(text.len()), Bias::Left);
        let now = text.offset_to_point_utf16(p);
        if now.row != self.anchor_row || p < self.anchor {
            return Err(CommitError::TextChanged);
        }
        let base = self.response_base;

        // Main edit.
        let (mut start, mut end, source) = match &it.edit {
            Some(e) => {
                let range = match self.options.insert_mode {
                    InsertMode::Insert => &e.insert,
                    InsertMode::Replace => &e.replace,
                };
                (
                    map_position(text, range.start, false, base, now),
                    map_position(text, range.end, true, base, now),
                    e.new_text.as_str(),
                )
            }
            None => (
                self.anchor,
                match self.options.insert_mode {
                    InsertMode::Insert => p,
                    InsertMode::Replace => self.word_end(text, p),
                },
                it.insert_text.as_deref().unwrap_or(&it.label),
            ),
        };
        start = start.min(p);
        end = end.max(start);
        // An item whose range ended before the caret still replaces what was typed.
        if matches!(self.options.insert_mode, InsertMode::Insert) {
            end = end.max(p);
        }

        let (flat, stops) = match it.format {
            InsertFormat::Snippet => {
                let s = Snippet::parse(source);
                (
                    s.text,
                    if self.options.snippets {
                        s.tab_stops
                    } else {
                        vec![]
                    },
                )
            }
            InsertFormat::PlainText => (source.to_string(), vec![]),
        };
        let eol = eol_of(text);
        let adjust = it.insert_mode == InsertTextMode::AdjustIndentation;
        let indent_of = |at: usize| -> String {
            let ls = line_start(text, at);
            let head = text.text_for_range(ls..at);
            head[..head.len() - head.trim_start_matches([' ', '\t']).len()].to_string()
        };
        let main_shaped = shape(&flat, adjust, &indent_of(start), eol);

        // Candidate edits in priority order: main, additional, other carets.
        let mut accepted: Vec<ByteEdit> = vec![ByteEdit {
            range: start..end,
            new_text: main_shaped.text.clone(),
        }];
        // Overlap: shared interior, the same range, or two insertions at one point
        // (their order would be ambiguous). An insertion right before or after
        // another edit is fine.
        let overlaps = |list: &[ByteEdit], r: &Range<usize>| {
            list.iter().any(|e| {
                (e.range.start < r.end && r.start < e.range.end)
                    || (e.range.start == r.start && e.range.is_empty() && r.is_empty())
                    || e.range == *r
            })
        };

        let mut skipped_additional = 0usize;
        let extra = self.extras[item].as_ref();
        if let Some(extra) = extra {
            match extra.base {
                Some(extra_base) => {
                    for e in &extra.edits {
                        // Unlike the main edit, these never cover what was typed since:
                        // everything from the old caret on moves with the caret.
                        let s = map_position(text, e.range.start, true, extra_base, now);
                        let en = map_position(text, e.range.end, true, extra_base, now).max(s);
                        let r = s..en;
                        if overlaps(&accepted, &r) {
                            skipped_additional += 1;
                        } else {
                            let shaped = shape(&e.new_text, false, "", eol);
                            accepted.push(ByteEdit {
                                range: r,
                                new_text: shaped.text,
                            });
                        }
                    }
                }
                None => skipped_additional = extra.edits.len(),
            }
        }

        let before = text.text_for_range(start..p);
        let after = text.text_for_range(p..end.max(p));
        let mut skipped_carets = 0usize;
        let mut other_edit_of: Vec<Option<(Range<usize>, usize)>> = vec![None; carets.len()];
        for (i, &c) in carets.iter().enumerate() {
            if i == primary {
                continue;
            }
            let c = text.clip_offset(c.min(text.len()), Bias::Left);
            let ok = c >= before.len()
                && text.text_for_range(c - before.len()..c) == before
                && text.text_for_range(c..c + after.len()) == after;
            if !ok {
                skipped_carets += 1;
                continue;
            }
            let r = c - before.len()..c + after.len();
            if overlaps(&accepted, &r) {
                skipped_carets += 1;
                continue;
            }
            let shaped = shape(&flat, adjust, &indent_of(r.start), eol);
            other_edit_of[i] = Some((r.clone(), accepted.len()));
            accepted.push(ByteEdit {
                range: r,
                new_text: shaped.text,
            });
        }

        // Positions after the edits: an offset moves by the growth of every edit
        // that ends at or before it (except its own edit).
        let post = |offset: usize, own: Option<usize>| -> usize {
            let mut delta = 0isize;
            for (i, e) in accepted.iter().enumerate() {
                if Some(i) != own && e.range.end <= offset {
                    delta += e.new_text.len() as isize - e.range.len() as isize;
                }
            }
            (offset as isize + delta).max(0) as usize
        };
        let main_start_post = post(start, Some(0));
        let mut new_carets = Vec::with_capacity(carets.len());
        for (i, &c) in carets.iter().enumerate() {
            if i == primary {
                new_carets.push(main_start_post + main_shaped.text.len());
            } else if let Some((r, idx)) = &other_edit_of[i] {
                new_carets.push(post(r.start, Some(*idx)) + accepted[*idx].new_text.len());
            } else {
                new_carets.push(post(c, None));
            }
        }
        let tab_stops: Vec<TabStop> = stops
            .into_iter()
            .map(|t| TabStop {
                index: t.index,
                ranges: t
                    .ranges
                    .iter()
                    .map(|r| {
                        main_start_post + main_shaped.map(r.start)
                            ..main_start_post + main_shaped.map(r.end)
                    })
                    .collect(),
                choices: t.choices,
            })
            .collect();
        if let Some(first) = tab_stops.first().and_then(|t| t.ranges.first()) {
            new_carets[primary] = first.start;
        }
        let mut edits = accepted;
        edits.sort_by_key(|e| (e.range.start, e.range.end));
        Ok(CompletionCommit {
            edits,
            carets: new_carets,
            tab_stops,
            command: it.command.clone(),
            skipped_additional_edits: skipped_additional,
            skipped_carets,
        })
    }
}

#[cfg(test)]
mod tests;
