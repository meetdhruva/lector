//! Inline agent edit (v0.7): the UI-free model of "select code, press Ctrl+I, describe the
//! change, review the agent's edit in place hunk by hunk".
//!
//! The agent's write lands on disk exactly as every other agent write does (through the open
//! document, checkpointed, journaled, permission prompts apply). This module keeps the
//! bookkeeping around it:
//!
//! * the **state machine** [`InlinePhase`]: `Prompting -> Running -> Reviewing -> Done`, with
//!   `Cancelled` and `Failed` as the other ends;
//! * the **hunks** of the turn's change to the one file ([`compute_hunks`], line based, lossless:
//!   rejecting every hunk gives back the old text byte for byte, accepting every hunk leaves the
//!   new text), kept as byte ranges in the *current* buffer and mapped through every later edit
//!   ([`InlineEdit::user_edited`]);
//! * **accept / reject**: accepting a hunk keeps the text (nothing to write), rejecting it
//!   returns the [`TextEdit`] that restores the old lines in the buffer; the front-end applies
//!   it as one undo step;
//! * **conflicts**: the user editing inside the selected range while the agent works, or inside a
//!   pending hunk, ends the inline review ([`InlinePhase::Done`] with
//!   [`InlineOutcome::FellBack`]) so the front-end opens the turn's review tab instead of guessing;
//! * the **focused prompt** ([`InlineEdit::submit`]) and the notice about files the agent changed
//!   besides this one.
//!
//! No GPUI, no I/O except [`InlineEdit::start`] / [`InlineEdit::finish_from_session`], which talk to
//! an [`AgentSession`].

use std::ops::Range;
use std::sync::Arc;

use lector_buffer::TextEdit;
use lector_diff::{diff_lines, DiffOptions};

use crate::model::{PromptContext, Turn, TurnId, TurnState};
use crate::session::AgentSession;
use crate::AgentError;

/// Lines of surrounding code sent next to the selection.
pub const CONTEXT_LINES: u32 = 6;

/// A hunk this many lines (or more) away from the selected lines is flagged as "outside the
/// selection" so the user looks at it twice.
pub const OUTSIDE_MARGIN_LINES: u32 = 10;

/// What the user selected and the text of the file at that moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineRequest {
    /// Project-relative path with `/` separators.
    pub path: String,
    /// Selected lines `[start, end)`, 0-based, in `baseline`.
    pub lines: Range<u32>,
    /// The file's text (the open buffer, unsaved edits included) when the prompt box opened.
    pub baseline: Arc<str>,
    /// Whether the range comes from a selection (`false`: the caret's line).
    pub from_selection: bool,
}

impl InlineRequest {
    /// Builds a request from a byte `selection` of `text`: whole lines touched by the selection; an
    /// empty selection means the caret's line; a selection that ends at column 0 does not include
    /// that line.
    pub fn from_selection(path: &str, text: &str, selection: Range<usize>) -> InlineRequest {
        let starts = line_starts(text);
        let line_of = |off: usize| starts.partition_point(|s| *s <= off).saturating_sub(1) as u32;
        let (a, b) = (
            selection.start.min(text.len()),
            selection.end.min(text.len()),
        );
        let start = line_of(a);
        let from_selection = b > a;
        let end = if !from_selection {
            start + 1
        } else {
            let end_line = line_of(b);
            if starts.get(end_line as usize) == Some(&b) && end_line > start {
                end_line
            } else {
                end_line + 1
            }
        };
        InlineRequest {
            path: path.to_string(),
            lines: start..end,
            baseline: Arc::from(text),
            from_selection,
        }
    }

    /// Bytes of the selected lines in the baseline.
    pub fn byte_range(&self) -> Range<usize> {
        line_range_bytes(&self.baseline, &self.lines)
    }

    /// The selected text.
    pub fn selected_text(&self) -> &str {
        &self.baseline[self.byte_range()]
    }

    /// Human label: "lines 10-20" or "line 10".
    pub fn range_label(&self) -> String {
        range_label(&self.lines)
    }
}

fn range_label(lines: &Range<u32>) -> String {
    if lines.end <= lines.start + 1 {
        format!("line {}", lines.start + 1)
    } else {
        format!("lines {}-{}", lines.start + 1, lines.end)
    }
}

/// Where the inline edit is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlinePhase {
    /// The prompt box is open.
    Prompting,
    /// A turn works on it.
    Running {
        /// The agent turn.
        turn: TurnId,
    },
    /// The result is shown as hunks to keep or undo.
    Reviewing,
    /// Over: see the outcome.
    Done(InlineOutcome),
    /// Cancelled before anything changed.
    Cancelled,
    /// The agent failed and changed nothing.
    Failed(String),
}

/// How a finished inline edit ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineOutcome {
    /// Every hunk was decided.
    Resolved {
        /// Hunks kept.
        kept: usize,
        /// Hunks undone.
        undone: usize,
    },
    /// The agent left this file as it was.
    NoChanges,
    /// The inline review was abandoned: open the turn's review tab.
    FellBack(FallbackReason),
}

/// Why the inline review gave way to the review tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    /// The file was edited inside the selected range while the agent worked.
    EditedSelection,
    /// The file was edited inside a pending hunk.
    EditedHunk,
    /// The buffer no longer matches what the agent wrote and the difference touches a hunk.
    BufferChanged,
}

impl FallbackReason {
    /// Sentence for a toast.
    pub fn message(self) -> &'static str {
        match self {
            FallbackReason::EditedSelection => {
                "You edited the selected code while the agent worked, so Lector opened the turn's review instead."
            }
            FallbackReason::EditedHunk => {
                "You edited inside the agent's change, so Lector opened the turn's review instead."
            }
            FallbackReason::BufferChanged => {
                "The file changed while the agent worked, so Lector opened the turn's review instead."
            }
        }
    }
}

/// Decision state of a hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkStatus {
    /// Waiting for the user.
    Pending,
    /// Kept (the new text stays).
    Kept,
    /// Undone (the old text was restored).
    Undone,
}

/// One changed block of the agent's edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineHunk {
    /// Id, stable while the inline edit lives.
    pub id: u32,
    /// The text the agent replaced (empty for an insertion).
    pub old_text: String,
    /// Where the new text is in the current buffer (empty for a pure deletion).
    pub new_range: Range<usize>,
    /// The replaced lines in the baseline.
    pub old_lines: Range<u32>,
    /// Decision.
    pub status: HunkStatus,
    /// Far from the selected lines (the agent went beyond what was asked).
    pub outside_range: bool,
}

/// One text change of the buffer in coordinates before the batch it belongs to
/// (see [`InlineEdit::user_edited`]); batches are ascending and disjoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditSpan {
    /// Replaced bytes.
    pub old: Range<usize>,
    /// Length of the replacement.
    pub new_len: usize,
}

/// What a user edit did to the inline edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditEffect {
    /// Positions shifted (or nothing to track).
    None,
    /// The selected range was edited while the agent works; the review tab will open at the end.
    SelectionEdited,
    /// The inline review is over: open the turn's review tab.
    Conflict(FallbackReason),
}

/// Something worth telling the user next to the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineNote {
    /// The turn was cancelled but the agent had already changed the file.
    CancelledWithChanges,
    /// The agent failed but had already changed the file (message).
    FailedWithChanges(String),
    /// The file was edited (outside the selection) while the agent worked: those edits may have
    /// been overwritten by the agent's write.
    EditedWhileWorking,
    /// Some hunks are far from the selection.
    OutsideSelection(usize),
}

impl InlineNote {
    /// Sentence for the status bar of the review.
    pub fn message(&self) -> String {
        match self {
            InlineNote::CancelledWithChanges => {
                "Cancelled, but the agent had already changed this file.".into()
            }
            InlineNote::FailedWithChanges(why) => {
                format!("The agent failed after changing this file: {why}")
            }
            InlineNote::EditedWhileWorking => {
                "You edited this file while the agent worked. Check that your edits are still there."
                    .into()
            }
            InlineNote::OutsideSelection(n) => format!(
                "{n} change{} outside the selected lines.",
                if *n == 1 { " is" } else { "s are" }
            ),
        }
    }
}

/// Errors of [`InlineEdit`] operations.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InlineError {
    /// The call does not fit the current phase.
    #[error("not possible while the inline edit is {0}")]
    WrongPhase(&'static str),
    /// Nothing typed.
    #[error("describe the change you want")]
    EmptyInstruction,
    /// No such pending hunk.
    #[error("that change was already decided")]
    NoSuchHunk,
    /// The agent session refused the prompt.
    #[error("{0}")]
    Agent(String),
}

/// The prompt of an inline edit, ready for [`AgentSession::prompt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlinePrompt {
    /// First text block: the task, the file, the lines and the instruction (on its own lines).
    pub text: String,
    /// The selection and its surroundings.
    pub context: Vec<PromptContext>,
}

/// What the front-end learned when the turn ended ([`InlineEdit::finish`]).
#[derive(Debug, Clone)]
pub struct FinishInput {
    /// How the turn ended.
    pub state: TurnState,
    /// The file on disk after the turn (what the agent wrote); `None`: unreadable or deleted.
    pub result_text: Option<String>,
    /// The open buffer now (the agent's write plus anything the user typed after it).
    pub buffer_text: String,
    /// Project-relative paths the turn changed (from its checkpoints).
    pub changed_files: Vec<String>,
}

/// The inline edit of one file.
#[derive(Debug, Clone)]
pub struct InlineEdit {
    request: InlineRequest,
    phase: InlinePhase,
    /// The text pending hunks are measured against (the file as it was before the agent edit, with
    /// the hunks the user already decided applied).
    baseline: Arc<str>,
    /// The buffer text as the model believes it is.
    current: String,
    /// While running: the selected lines in `current` (bytes).
    range: Range<usize>,
    hunks: Vec<InlineHunk>,
    next_hunk: u32,
    instructions: Vec<String>,
    turns: Vec<TurnId>,
    selection_edited: bool,
    edited_elsewhere: bool,
    other_files: Vec<String>,
    notes: Vec<InlineNote>,
    kept: usize,
    undone: usize,
}

impl InlineEdit {
    /// A new inline edit in the [`InlinePhase::Prompting`] phase.
    pub fn new(request: InlineRequest) -> InlineEdit {
        let range = request.byte_range();
        InlineEdit {
            baseline: request.baseline.clone(),
            current: request.baseline.to_string(),
            range,
            request,
            phase: InlinePhase::Prompting,
            hunks: Vec::new(),
            next_hunk: 1,
            instructions: Vec::new(),
            turns: Vec::new(),
            selection_edited: false,
            edited_elsewhere: false,
            other_files: Vec::new(),
            notes: Vec::new(),
            kept: 0,
            undone: 0,
        }
    }

    /// What was asked for.
    pub fn request(&self) -> &InlineRequest {
        &self.request
    }

    /// Current phase.
    pub fn phase(&self) -> &InlinePhase {
        &self.phase
    }

    /// Whether the inline edit still owns the file's presentation (prompting, running or reviewing).
    pub fn is_active(&self) -> bool {
        matches!(
            self.phase,
            InlinePhase::Prompting | InlinePhase::Running { .. } | InlinePhase::Reviewing
        )
    }

    /// The turn currently running (or the last one).
    pub fn turn(&self) -> Option<TurnId> {
        match self.phase {
            InlinePhase::Running { turn } => Some(turn),
            _ => self.turns.last().copied(),
        }
    }

    /// Every turn this inline edit ran (the first one, then follow-ups).
    pub fn turns(&self) -> &[TurnId] {
        &self.turns
    }

    /// Instructions typed so far.
    pub fn instructions(&self) -> &[String] {
        &self.instructions
    }

    /// All hunks, in text order.
    pub fn hunks(&self) -> &[InlineHunk] {
        &self.hunks
    }

    /// Hunks still waiting for a decision.
    pub fn pending(&self) -> impl Iterator<Item = &InlineHunk> {
        self.hunks
            .iter()
            .filter(|h| h.status == HunkStatus::Pending)
    }

    /// Number of pending hunks.
    pub fn pending_count(&self) -> usize {
        self.pending().count()
    }

    /// Paths of other files the turn changed.
    pub fn other_files(&self) -> &[String] {
        &self.other_files
    }

    /// "The agent also changed N other files", when it did.
    pub fn other_files_notice(&self) -> Option<String> {
        match self.other_files.len() {
            0 => None,
            1 => Some("The agent also changed 1 other file".into()),
            n => Some(format!("The agent also changed {n} other files")),
        }
    }

    /// Other notices (cancelled with changes, edits while working, hunks outside the selection).
    pub fn notes(&self) -> &[InlineNote] {
        &self.notes
    }

    /// The buffer text the model tracks (valid from the first user edit / finish on).
    pub fn current_text(&self) -> &str {
        &self.current
    }

    /// The selected lines of the buffer while the agent works (bytes).
    pub fn working_range(&self) -> Range<usize> {
        self.range.clone()
    }

    /// Rows `[start, end)` of the selected lines in the current text.
    pub fn working_rows(&self) -> Range<u32> {
        bytes_to_rows(&self.current, &self.range)
    }

    /// Rows of a hunk's new text in the current buffer (empty at the insertion row for a
    /// deletion).
    pub fn hunk_rows(&self, hunk: &InlineHunk) -> Range<u32> {
        bytes_to_rows(&self.current, &hunk.new_range)
    }

    /// Hunks that would be undone by "Undo all" are the pending ones; this is the buffer text
    /// after applying that decision.
    pub fn text_if_all_undone(&self) -> String {
        let mut out = self.current.clone();
        for h in self
            .hunks
            .iter()
            .rev()
            .filter(|h| h.status == HunkStatus::Pending)
        {
            out.replace_range(h.new_range.clone(), &h.old_text);
        }
        out
    }

    // -- prompt -------------------------------------------------------------------------

    /// Builds the focused prompt for `instruction` (the prompt box was submitted). The phase stays
    /// `Prompting` until [`InlineEdit::turn_started`].
    pub fn submit(&mut self, instruction: &str) -> Result<InlinePrompt, InlineError> {
        if self.phase != InlinePhase::Prompting {
            return Err(InlineError::WrongPhase(self.phase_name()));
        }
        let instruction = instruction.trim();
        if instruction.is_empty() {
            return Err(InlineError::EmptyInstruction);
        }
        Ok(focused_prompt(&self.request, instruction))
    }

    /// Records that `turn` runs the prompt.
    pub fn turn_started(&mut self, instruction: &str, turn: TurnId) {
        self.instructions.push(instruction.trim().to_string());
        self.turns.push(turn);
        self.selection_edited = false;
        self.edited_elsewhere = false;
        self.phase = InlinePhase::Running { turn };
    }

    /// Submits `instruction` to `session` and moves to `Running`.
    pub fn start(
        &mut self,
        session: &AgentSession,
        instruction: &str,
    ) -> Result<TurnId, InlineError> {
        let prompt = self.submit(instruction)?;
        let turn = session
            .prompt(prompt.text, prompt.context)
            .map_err(|e: AgentError| InlineError::Agent(e.to_string()))?;
        self.turn_started(instruction, turn);
        Ok(turn)
    }

    /// A follow-up instruction that refines the pending hunks: a new turn in the same session. The
    /// pending hunks are measured against what the file would be with all of them undone, so the new
    /// result replaces them; hunks decided earlier stay decided.
    pub fn follow_up(
        &mut self,
        session: &AgentSession,
        instruction: &str,
    ) -> Result<TurnId, InlineError> {
        let prompt = self.prepare_follow_up(instruction)?;
        let turn = session
            .prompt(prompt.text, prompt.context)
            .map_err(|e: AgentError| InlineError::Agent(e.to_string()))?;
        self.follow_up_started(instruction, turn);
        Ok(turn)
    }

    /// The prompt of a follow-up (see [`InlineEdit::follow_up`]) without sending it.
    pub fn prepare_follow_up(&self, instruction: &str) -> Result<InlinePrompt, InlineError> {
        if self.phase != InlinePhase::Reviewing {
            return Err(InlineError::WrongPhase(self.phase_name()));
        }
        let instruction = instruction.trim();
        if instruction.is_empty() {
            return Err(InlineError::EmptyInstruction);
        }
        let hull = self.pending_hull_rows();
        Ok(follow_up_prompt(
            &self.request.path,
            &hull,
            instruction,
            &self.current,
        ))
    }

    /// Records that `turn` runs a follow-up built by [`InlineEdit::prepare_follow_up`].
    pub fn follow_up_started(&mut self, instruction: &str, turn: TurnId) {
        let hull = self.pending_hull_rows();
        let hull_bytes = line_range_bytes(&self.current, &hull);
        self.baseline = Arc::from(self.text_if_all_undone());
        // The pending hunks are replaced by the next result.
        self.hunks.retain(|h| h.status != HunkStatus::Pending);
        self.range = hull_bytes;
        self.request.lines = hull;
        self.instructions.push(instruction.trim().to_string());
        self.turns.push(turn);
        self.selection_edited = false;
        self.edited_elsewhere = false;
        self.notes.clear();
        self.other_files.clear();
        self.phase = InlinePhase::Running { turn };
    }

    fn pending_hull_rows(&self) -> Range<u32> {
        let mut start = u32::MAX;
        let mut end = 0;
        for h in self.pending() {
            let rows = self.hunk_rows(h);
            start = start.min(rows.start);
            end = end.max(rows.end.max(rows.start + 1));
        }
        if start == u32::MAX {
            self.request.lines.clone()
        } else {
            start..end
        }
    }

    fn phase_name(&self) -> &'static str {
        match self.phase {
            InlinePhase::Prompting => "waiting for a prompt",
            InlinePhase::Running { .. } => "running",
            InlinePhase::Reviewing => "being reviewed",
            InlinePhase::Done(_) => "finished",
            InlinePhase::Cancelled => "cancelled",
            InlinePhase::Failed(_) => "failed",
        }
    }

    // -- cancel -------------------------------------------------------------------------

    /// Cancels from the prompt box: nothing ran. While running use the session's cancel and
    /// [`InlineEdit::finish`] (the turn ends as `Cancelled`).
    pub fn cancel_prompt(&mut self) -> bool {
        if self.phase == InlinePhase::Prompting {
            self.phase = InlinePhase::Cancelled;
            true
        } else {
            false
        }
    }

    /// Whether Cancel should go to the agent (a turn is running).
    pub fn needs_turn_cancel(&self) -> bool {
        matches!(self.phase, InlinePhase::Running { .. })
    }

    // -- buffer edits ---------------------------------------------------------------------

    /// The buffer changed by something other than this model: `edits` (ascending, coordinates
    /// before the batch) produced `new_text`. Pending hunks and the working range move with the
    /// text; an edit inside one ends the inline review.
    pub fn user_edited(&mut self, edits: &[EditSpan], new_text: &str) -> EditEffect {
        self.current = new_text.to_string();
        match self.phase {
            InlinePhase::Running { .. } => {
                let mut effect = EditEffect::None;
                if !edits.is_empty() {
                    self.edited_elsewhere = true;
                }
                match map_range(&self.range, edits) {
                    Some(r) => self.range = r,
                    None => {
                        self.selection_edited = true;
                        self.range = widen(&self.range, edits);
                        effect = EditEffect::SelectionEdited;
                    }
                }
                effect
            }
            InlinePhase::Reviewing => {
                if edits.is_empty() {
                    return EditEffect::None;
                }
                if map_hunks(&mut self.hunks, edits).is_err() {
                    self.phase =
                        InlinePhase::Done(InlineOutcome::FellBack(FallbackReason::EditedHunk));
                    return EditEffect::Conflict(FallbackReason::EditedHunk);
                }
                EditEffect::None
            }
            _ => EditEffect::None,
        }
    }

    // -- result -----------------------------------------------------------------------------

    /// The turn ended: computes the hunks and moves to `Reviewing`, `Done` (nothing changed, or the
    /// review tab must open instead), `Cancelled` or `Failed`.
    pub fn finish(&mut self, input: FinishInput) -> &InlinePhase {
        if !matches!(self.phase, InlinePhase::Running { .. }) {
            return &self.phase;
        }
        self.other_files = input
            .changed_files
            .iter()
            .filter(|p| p.as_str() != self.request.path)
            .cloned()
            .collect();
        let result = input
            .result_text
            .unwrap_or_else(|| input.buffer_text.clone());
        let changed = result != *self.baseline;
        let ended = match &input.state {
            TurnState::Cancelled => Some(None),
            TurnState::Failed(why) => Some(Some(why.clone())),
            _ => None,
        };
        if !changed {
            self.current = input.buffer_text;
            self.phase = match ended {
                Some(Some(why)) => InlinePhase::Failed(why),
                Some(None) => InlinePhase::Cancelled,
                None => InlinePhase::Done(InlineOutcome::NoChanges),
            };
            return &self.phase;
        }
        match &ended {
            Some(None) => self.notes.push(InlineNote::CancelledWithChanges),
            Some(Some(why)) => self.notes.push(InlineNote::FailedWithChanges(why.clone())),
            None => {}
        }
        if self.selection_edited {
            self.current = input.buffer_text;
            self.phase =
                InlinePhase::Done(InlineOutcome::FellBack(FallbackReason::EditedSelection));
            return &self.phase;
        }
        if self.edited_elsewhere {
            self.notes.push(InlineNote::EditedWhileWorking);
        }
        // Hunks of what the agent wrote, in the coordinates of the text it wrote.
        let outside_lines = self.request.lines.clone();
        let mut hunks = Vec::new();
        for raw in compute_hunks(&self.baseline, &result) {
            let id = self.next_hunk;
            self.next_hunk += 1;
            hunks.push(InlineHunk {
                id,
                old_text: self.baseline[raw.old.clone()].to_string(),
                new_range: raw.new.clone(),
                old_lines: raw.old_lines.clone(),
                status: HunkStatus::Pending,
                outside_range: is_outside(&raw.old_lines, &outside_lines),
            });
        }
        // The buffer may differ from what the agent wrote (the user typed after the write).
        if input.buffer_text != result {
            let edits: Vec<EditSpan> = compute_hunks(&result, &input.buffer_text)
                .into_iter()
                .map(|h| EditSpan {
                    new_len: h.new.len(),
                    old: h.old,
                })
                .collect();
            if map_hunks(&mut hunks, &edits).is_err() {
                self.current = input.buffer_text;
                self.hunks = Vec::new();
                self.phase =
                    InlinePhase::Done(InlineOutcome::FellBack(FallbackReason::BufferChanged));
                return &self.phase;
            }
        }
        self.current = input.buffer_text;
        let outside = hunks.iter().filter(|h| h.outside_range).count();
        if outside > 0 {
            self.notes.push(InlineNote::OutsideSelection(outside));
        }
        self.hunks.extend(hunks);
        self.hunks.sort_by_key(|h| h.new_range.start);
        self.phase = InlinePhase::Reviewing;
        &self.phase
    }

    /// [`InlineEdit::finish`] with the data read from `session`: the turn's state and changed files,
    /// and the file as it is on disk. `buffer_text` is the open buffer.
    pub fn finish_from_session(
        &mut self,
        session: &AgentSession,
        turn: TurnId,
        buffer_text: String,
    ) -> &InlinePhase {
        let (state, changed_files) = match session.turn(turn) {
            Some(t) => (
                t.state.clone(),
                t.changed_files.iter().map(|c| c.path.clone()).collect(),
            ),
            None => (TurnState::Failed("the turn is gone".into()), Vec::new()),
        };
        let abs = session.project().root().join(&self.request.path);
        let result_text = std::fs::read_to_string(abs).ok();
        self.finish(FinishInput {
            state,
            result_text,
            buffer_text,
            changed_files,
        })
    }

    // -- decisions ----------------------------------------------------------------------------

    /// Keeps a hunk. The text is already in the buffer: nothing to write.
    pub fn accept(&mut self, id: u32) -> Result<(), InlineError> {
        let ix = self.pending_index(id)?;
        self.hunks[ix].status = HunkStatus::Kept;
        self.kept += 1;
        self.after_decision();
        Ok(())
    }

    /// Undoes a hunk: returns the edit that restores the old lines in the buffer (apply it as one
    /// undo step). Positions of the other hunks are updated as if it had been applied.
    pub fn reject(&mut self, id: u32) -> Result<TextEdit, InlineError> {
        let ix = self.pending_index(id)?;
        let edit = self.undo_hunk(ix);
        self.undone += 1;
        self.after_decision();
        Ok(edit)
    }

    /// Keeps every pending hunk.
    pub fn accept_all(&mut self) -> usize {
        let mut n = 0;
        for h in &mut self.hunks {
            if h.status == HunkStatus::Pending {
                h.status = HunkStatus::Kept;
                n += 1;
            }
        }
        self.kept += n;
        self.after_decision();
        n
    }

    /// Undoes every pending hunk: the edits to apply together as one undo step (descending
    /// offsets, non-overlapping).
    pub fn reject_all(&mut self) -> Vec<TextEdit> {
        let ids: Vec<u32> = self
            .hunks
            .iter()
            .rev()
            .filter(|h| h.status == HunkStatus::Pending)
            .map(|h| h.id)
            .collect();
        let mut edits = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(ix) = self.hunks.iter().position(|h| h.id == id) {
                edits.push(self.undo_hunk(ix));
                self.undone += 1;
            }
        }
        self.after_decision();
        edits
    }

    fn pending_index(&self, id: u32) -> Result<usize, InlineError> {
        if self.phase != InlinePhase::Reviewing {
            return Err(InlineError::WrongPhase(self.phase_name()));
        }
        self.hunks
            .iter()
            .position(|h| h.id == id && h.status == HunkStatus::Pending)
            .ok_or(InlineError::NoSuchHunk)
    }

    fn undo_hunk(&mut self, ix: usize) -> TextEdit {
        let (range, old_text) = {
            let h = &self.hunks[ix];
            (h.new_range.clone(), h.old_text.clone())
        };
        let delta = old_text.len() as isize - range.len() as isize;
        self.current.replace_range(range.clone(), &old_text);
        for later in self.hunks.iter_mut().skip(ix + 1) {
            if later.status == HunkStatus::Pending {
                later.new_range = shift(&later.new_range, delta);
            }
        }
        let h = &mut self.hunks[ix];
        h.status = HunkStatus::Undone;
        h.new_range = range.start..range.start + old_text.len();
        TextEdit {
            range,
            new_text: old_text,
        }
    }

    fn after_decision(&mut self) {
        if self.phase == InlinePhase::Reviewing && self.pending_count() == 0 {
            self.phase = InlinePhase::Done(InlineOutcome::Resolved {
                kept: self.kept,
                undone: self.undone,
            });
        }
    }

    /// The pending hunk the caret at `row` belongs to: the one containing it, else the next one
    /// below, else the last.
    pub fn hunk_at_row(&self, row: u32) -> Option<u32> {
        let mut best: Option<(u32, u32)> = None;
        for h in self.pending() {
            let rows = self.hunk_rows(h);
            if rows.start <= row && row < rows.end.max(rows.start + 1) {
                return Some(h.id);
            }
            if rows.start > row && best.is_none_or(|(s, _)| rows.start < s) {
                best = Some((rows.start, h.id));
            }
        }
        best.map(|(_, id)| id)
            .or_else(|| self.pending().last().map(|h| h.id))
    }
}

fn is_outside(hunk_lines: &Range<u32>, selected: &Range<u32>) -> bool {
    let lo = selected.start.saturating_sub(OUTSIDE_MARGIN_LINES);
    let hi = selected.end.saturating_add(OUTSIDE_MARGIN_LINES);
    hunk_lines.end < lo || hunk_lines.start > hi
}

fn shift(r: &Range<usize>, delta: isize) -> Range<usize> {
    let f = |x: usize| (x as isize + delta).max(0) as usize;
    f(r.start)..f(r.end)
}

/// Maps `r` through `edits`; `None` when an edit overlaps it.
fn map_range(r: &Range<usize>, edits: &[EditSpan]) -> Option<Range<usize>> {
    let mut delta: isize = 0;
    for e in edits {
        if e.old.end <= r.start {
            delta += e.new_len as isize - e.old.len() as isize;
        } else if e.old.start >= r.end {
            break;
        } else {
            return None;
        }
    }
    Some(shift(r, delta))
}

/// `r` grown to cover the edits that overlap it, then moved through all of them (so it keeps
/// meaning "the selected code").
fn widen(r: &Range<usize>, edits: &[EditSpan]) -> Range<usize> {
    let (mut start, mut end) = (r.start, r.end);
    for e in edits {
        if e.old.end > r.start && e.old.start < r.end {
            start = start.min(e.old.start);
            end = end.max(e.old.end);
        }
    }
    let (mut before, mut inside): (isize, isize) = (0, 0);
    for e in edits {
        let delta = e.new_len as isize - e.old.len() as isize;
        if e.old.end <= start {
            before += delta;
        } else if e.old.start >= start && e.old.end <= end {
            inside += delta;
        }
    }
    shift(&(start..end), before).start..shift(&(start..end), before + inside).end
}

/// Moves every pending hunk through `edits`; `Err` when an edit overlaps one (hunks are then
/// left unspecified: the caller abandons the inline review).
fn map_hunks(hunks: &mut [InlineHunk], edits: &[EditSpan]) -> Result<(), ()> {
    let mut mapped = Vec::with_capacity(hunks.len());
    for h in hunks.iter() {
        if h.status == HunkStatus::Pending {
            mapped.push(map_range(&h.new_range, edits).ok_or(())?);
        } else {
            // Decided hunks are plain text now; their range is not tracked.
            mapped.push(h.new_range.clone());
        }
    }
    for (h, r) in hunks.iter_mut().zip(mapped) {
        h.new_range = r;
    }
    Ok(())
}

// -- diff -------------------------------------------------------------------------------------

/// A changed block between two texts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawHunk {
    /// Bytes in the old text.
    pub old: Range<usize>,
    /// Bytes in the new text.
    pub new: Range<usize>,
    /// Lines in the old text.
    pub old_lines: Range<u32>,
    /// Lines in the new text.
    pub new_lines: Range<u32>,
}

/// Line-based hunks between `old` and `new`, as byte ranges. Lossless: replacing every `new` range
/// of `new` with the matching `old` text gives `old` back, and the gaps between hunks are identical
/// in both texts (a missing final newline counts as a difference of the last line).
pub fn compute_hunks(old: &str, new: &str) -> Vec<RawHunk> {
    if old == new {
        return Vec::new();
    }
    // The last line (even the empty one after a final newline) must differ from any line that has a
    // terminator: the diff sees a sentinel after the last byte.
    let (o, n) = (format!("{old}\u{1}"), format!("{new}\u{1}"));
    let diff = diff_lines(&o, &n, &DiffOptions::default());
    let (old_starts, new_starts) = (line_starts(old), line_starts(new));
    let off = |starts: &[usize], text: &str, line: u32| {
        starts.get(line as usize).copied().unwrap_or(text.len())
    };
    let hunks: Vec<RawHunk> = diff
        .hunks
        .iter()
        .map(|h| RawHunk {
            old: off(&old_starts, old, h.old.start)..off(&old_starts, old, h.old.end),
            new: off(&new_starts, new, h.new.start)..off(&new_starts, new, h.new.end),
            old_lines: h.old.clone(),
            new_lines: h.new.clone(),
        })
        .collect();
    if gaps_match(old, new, &hunks) {
        return hunks;
    }
    // Only reachable when the text itself contains the sentinel: one hunk for everything.
    vec![RawHunk {
        old: 0..old.len(),
        new: 0..new.len(),
        old_lines: 0..old_starts.len() as u32,
        new_lines: 0..new_starts.len() as u32,
    }]
}

fn gaps_match(old: &str, new: &str, hunks: &[RawHunk]) -> bool {
    let (mut o, mut n) = (0, 0);
    for h in hunks {
        if old.get(o..h.old.start) != new.get(n..h.new.start) {
            return false;
        }
        o = h.old.end;
        n = h.new.end;
    }
    old.get(o..) == new.get(n..)
}

/// Splices `new` and `old` along `hunks`: hunk `i` contributes its new text when `keep[i]`, else its
/// old text (`old`'s bytes). What the buffer holds after deciding every hunk.
pub fn splice(old: &str, new: &str, hunks: &[RawHunk], keep: &[bool]) -> String {
    let mut out = String::with_capacity(new.len());
    let mut at = 0;
    for (h, keep) in hunks.iter().zip(keep) {
        out.push_str(&new[at..h.new.start]);
        if *keep {
            out.push_str(&new[h.new.clone()]);
        } else {
            out.push_str(&old[h.old.clone()]);
        }
        at = h.new.end;
    }
    out.push_str(&new[at..]);
    out
}

/// Start offset of every line (split on `'\n'` only; a trailing newline starts an empty last line).
pub fn line_starts(text: &str) -> Vec<usize> {
    let mut v = vec![0];
    v.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    v
}

/// Number of lines a reader sees (no empty line after the final newline).
pub fn real_line_count(text: &str) -> u32 {
    let n = line_starts(text).len() as u32;
    if text.ends_with('\n') {
        n - 1
    } else {
        n
    }
}

/// Bytes of lines `[start, end)` (clamped to the text).
pub fn line_range_bytes(text: &str, lines: &Range<u32>) -> Range<usize> {
    let starts = line_starts(text);
    let off = |l: u32| starts.get(l as usize).copied().unwrap_or(text.len());
    off(lines.start)..off(lines.end)
}

/// Rows covered by `bytes` of `text` (a partial last line counts; empty range = an empty row
/// range at its row).
pub fn bytes_to_rows(text: &str, bytes: &Range<usize>) -> Range<u32> {
    let start = bytes.start.min(text.len());
    let end = bytes.end.min(text.len());
    let row_of = |off: usize| {
        text.as_bytes()[..off]
            .iter()
            .filter(|b| **b == b'\n')
            .count() as u32
    };
    let first = row_of(start);
    if end <= start {
        return first..first;
    }
    let mut last = row_of(end);
    if text.as_bytes()[end - 1] != b'\n' {
        last += 1;
    }
    first..last
}

// -- prompts ----------------------------------------------------------------------------------

/// The focused prompt of the first instruction.
pub fn focused_prompt(req: &InlineRequest, instruction: &str) -> InlinePrompt {
    let total = real_line_count(&req.baseline);
    let sel = req.lines.clone();
    let around = sel.start.saturating_sub(CONTEXT_LINES)..(sel.end + CONTEXT_LINES).min(total);
    let mut text = format!(
        "Inline edit of `{}`, {}. Change only this file, and only within or very near those lines; leave everything else exactly as it is.\n",
        req.path,
        req.range_label()
    );
    if !req.from_selection {
        text.push_str("(No text was selected: the line under the caret is the target.)\n");
    }
    text.push_str("Instruction:\n");
    text.push_str(instruction.trim());
    let mut context = vec![PromptContext::Range {
        path: req.path.clone(),
        start: sel.start,
        end: sel.end,
    }];
    if around != sel {
        context.push(PromptContext::Range {
            path: req.path.clone(),
            start: around.start,
            end: around.end,
        });
    }
    InlinePrompt { text, context }
}

fn follow_up_prompt(path: &str, hull: &Range<u32>, instruction: &str, text: &str) -> InlinePrompt {
    let total = real_line_count(text);
    let around = hull.start.saturating_sub(CONTEXT_LINES)..(hull.end + CONTEXT_LINES).min(total);
    InlinePrompt {
        text: format!(
            "Refine your previous inline edit of `{path}`, {}. The file now contains your proposed change; change it as the instruction says, only in this file and near those lines.\nInstruction:\n{}",
            range_label(hull),
            instruction.trim()
        ),
        context: vec![PromptContext::Range {
            path: path.to_string(),
            start: around.start,
            end: around.end,
        }],
    }
}

// -- availability -------------------------------------------------------------------------------

/// Facts the front-end gathers to decide whether "Edit with Agent" can run.
#[derive(Debug, Clone, Copy, Default)]
pub struct AvailabilityInput {
    /// The tab is a text file that can be edited (not binary, image, or a view without a buffer).
    pub is_text_file: bool,
    /// The tab is read-only (locked or `editor.read_only`).
    pub read_only: bool,
    /// At least one agent is configured or found on `PATH`.
    pub agent_configured: bool,
    /// This file has an unresolved inline edit.
    pub has_unresolved_edit: bool,
    /// The project's agent session is working on another turn.
    pub session_busy: bool,
}

/// Why "Edit with Agent" is not available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// Not an editable text file.
    NotEditable,
    /// The tab is read-only.
    ReadOnly,
    /// No agent to talk to: offer the setup.
    NoAgent,
    /// This file already has an inline edit to resolve.
    Unresolved,
    /// The agent is busy with another turn.
    SessionBusy,
}

impl Unavailable {
    /// The reason, as shown in a tooltip or toast.
    pub fn reason(self) -> &'static str {
        match self {
            Unavailable::NotEditable => "Edit with Agent works in text files you can edit.",
            Unavailable::ReadOnly => "This tab is read-only. Unlock it to edit with the agent.",
            Unavailable::NoAgent => "No agent is set up yet.",
            Unavailable::Unresolved => {
                "Keep or undo the agent's pending changes in this file first."
            }
            Unavailable::SessionBusy => {
                "The agent is busy with another request. Wait for it or cancel it."
            }
        }
    }
}

/// Whether an inline edit can start. The order of the checks is the order users should fix them in.
pub fn availability(input: &AvailabilityInput) -> Result<(), Unavailable> {
    if !input.is_text_file {
        Err(Unavailable::NotEditable)
    } else if input.read_only {
        Err(Unavailable::ReadOnly)
    } else if input.has_unresolved_edit {
        Err(Unavailable::Unresolved)
    } else if !input.agent_configured {
        Err(Unavailable::NoAgent)
    } else if input.session_busy {
        Err(Unavailable::SessionBusy)
    } else {
        Ok(())
    }
}

// -- status text ----------------------------------------------------------------------------------

/// One line about what the agent is doing, for the "Working…" marker: the newest tool call, else the
/// start of the newest answer.
pub fn status_line(turn: &Turn) -> String {
    if let Some(p) = turn.permissions.iter().rev().find(|p| p.pending) {
        return format!("Waiting for your permission: {}", p.title);
    }
    if let Some(call) = turn.tool_calls.last() {
        if !call.title.is_empty() {
            return call.title.clone();
        }
    }
    if let Some(msg) = turn
        .messages
        .iter()
        .rev()
        .find(|m| m.kind == crate::model::MessageKind::Agent)
    {
        let line = msg.text.lines().rev().find(|l| !l.trim().is_empty());
        if let Some(line) = line {
            let mut s: String = line.trim().chars().take(80).collect();
            if line.trim().chars().count() > 80 {
                s.push('…');
            }
            return s;
        }
    }
    if matches!(turn.state, TurnState::Starting) {
        return "Taking a checkpoint…".into();
    }
    "Working…".into()
}

#[cfg(test)]
mod tests;
