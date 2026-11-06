//! Pure editing policies shared by every front-end: buffer options from
//! settings, the auto-save decision, and the save-time cleanup edits.

use std::time::Duration;

use lector_buffer::{BufferOptions, CommentTokens, IndentStyle, TextEdit};
use lector_config::{AutoSave, Settings};
use lector_text::Rope;
use lector_workspace::{Document, Indentation, LineEnding};

/// Shortest auto-save delay honoured (typing faster than this never saves).
pub const MIN_AUTO_SAVE_DELAY: Duration = Duration::from_millis(100);

/// Buffer options for `doc` under `settings` (indentation detected from the
/// file unless `editor.insert_spaces` overrides it; comment tokens and
/// `:`-indentation from the language).
pub fn buffer_options(doc: &Document, settings: &Settings) -> BufferOptions {
    let tab_size = settings.tab_size.max(1);
    let detected = match doc.indentation() {
        Indentation::Tabs => IndentStyle::Tabs,
        Indentation::Spaces(n) => IndentStyle::Spaces(n.max(1)),
    };
    let indent = match settings.editor.insert_spaces {
        None => detected,
        Some(true) => match detected {
            IndentStyle::Spaces(n) => IndentStyle::Spaces(n),
            IndentStyle::Tabs => IndentStyle::Spaces(tab_size),
        },
        Some(false) => IndentStyle::Tabs,
    };
    let comment = doc
        .language()
        .map(|l| CommentTokens {
            line: l.line_comment.clone(),
            block: l.block_comment.clone(),
        })
        .unwrap_or_default();
    BufferOptions {
        tab_size,
        indent,
        auto_indent: settings.editor.auto_indent,
        auto_close_brackets: settings.editor.auto_close_brackets,
        // `:` opens a block only in indentation-structured languages.
        indent_after_colon: doc.language().is_some_and(|l| {
            matches!(
                l.lsp_id.as_str(),
                "python" | "yaml" | "nim" | "coffeescript" | "starlark"
            )
        }),
        comment,
    }
}

/// What prompted an auto-save check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoSaveTrigger {
    /// The text was just edited.
    Edited,
    /// A delay armed by an earlier [`AutoSaveDecision::SaveAfter`] expired.
    TimerFired,
    /// The tab or window lost focus.
    FocusLost,
}

/// The document facts the auto-save policy looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AutoSaveInputs {
    /// Unsaved changes exist.
    pub dirty: bool,
    /// The document can be saved at all.
    pub editable: bool,
    /// A "changed on disk" conflict is pending.
    pub conflict: bool,
    /// The file is gone from disk.
    pub deleted: bool,
    /// Edits are locked.
    pub read_only: bool,
}

/// What to do about auto-saving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoSaveDecision {
    /// Nothing (and cancel an armed timer).
    Never,
    /// Save now.
    SaveNow,
    /// (Re-)arm a timer; call again with [`AutoSaveTrigger::TimerFired`].
    SaveAfter(Duration),
}

/// The auto-save policy (pure; the caller owns the timer). Never saves over
/// a disk conflict or recreates a deleted file on its own.
pub fn auto_save_decision(
    mode: AutoSave,
    delay_ms: u64,
    trigger: AutoSaveTrigger,
    doc: AutoSaveInputs,
) -> AutoSaveDecision {
    let savable = doc.editable && doc.dirty && !doc.conflict && !doc.deleted;
    match (mode, trigger) {
        (AutoSave::AfterDelay, AutoSaveTrigger::Edited) if doc.editable => {
            AutoSaveDecision::SaveAfter(Duration::from_millis(delay_ms).max(MIN_AUTO_SAVE_DELAY))
        }
        (AutoSave::AfterDelay, AutoSaveTrigger::TimerFired) if savable => AutoSaveDecision::SaveNow,
        (AutoSave::OnFocusChange, AutoSaveTrigger::FocusLost) if savable => {
            AutoSaveDecision::SaveNow
        }
        _ => AutoSaveDecision::Never,
    }
}

/// Edits applying `trim_trailing_whitespace` / `ensure_final_newline` to
/// `text` (offsets into `text`). Applied to the buffer before encoding, so
/// the buffer equals the file afterwards.
pub fn cleanup_edits(
    text: &Rope,
    line_ending: LineEnding,
    trim_trailing_whitespace: bool,
    ensure_final_newline: bool,
) -> Vec<TextEdit> {
    let mut edits = Vec::new();
    if trim_trailing_whitespace {
        for row in 0..text.line_count() {
            let range = text.line_range(row);
            let line = text.text_for_range(range.clone());
            let trimmed = line.trim_end_matches([' ', '\t']).len();
            if trimmed < line.len() {
                edits.push(TextEdit {
                    range: range.start + trimmed..range.end,
                    new_text: String::new(),
                });
            }
        }
    }
    if ensure_final_newline && !text.is_empty() {
        let len = text.len();
        let last_line = text.line_range(text.line_count() - 1);
        let ends_with_newline = last_line.is_empty() && text.line_count() > 1;
        if !ends_with_newline {
            let nl = match line_ending {
                LineEnding::CrLf => "\r\n",
                _ => "\n",
            };
            edits.push(TextEdit {
                range: len..len,
                new_text: nl.to_string(),
            });
        } else {
            // Collapse extra trailing blank lines into one newline.
            let mut row = text.line_count() - 1;
            while row > 1 && text.line_range(row - 1).is_empty() {
                row -= 1;
            }
            let keep_end = text.line_range(row.saturating_sub(1)).end;
            let nl_len = if text.byte_at(keep_end) == Some(b'\r') {
                2
            } else {
                1
            };
            let from = keep_end + nl_len;
            if from < len && row + 1 < text.line_count() {
                edits.push(TextEdit {
                    range: from..len,
                    new_text: String::new(),
                });
            }
        }
    }
    edits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(dirty: bool) -> AutoSaveInputs {
        AutoSaveInputs {
            dirty,
            editable: true,
            ..Default::default()
        }
    }

    #[test]
    fn auto_save_after_delay_arms_then_saves() {
        let d = auto_save_decision(
            AutoSave::AfterDelay,
            1000,
            AutoSaveTrigger::Edited,
            inputs(true),
        );
        assert_eq!(d, AutoSaveDecision::SaveAfter(Duration::from_millis(1000)));
        let d = auto_save_decision(
            AutoSave::AfterDelay,
            10,
            AutoSaveTrigger::Edited,
            inputs(true),
        );
        assert_eq!(d, AutoSaveDecision::SaveAfter(MIN_AUTO_SAVE_DELAY));
        let d = auto_save_decision(
            AutoSave::AfterDelay,
            1000,
            AutoSaveTrigger::TimerFired,
            inputs(true),
        );
        assert_eq!(d, AutoSaveDecision::SaveNow);
        // Undone back to clean meanwhile: nothing to save.
        let d = auto_save_decision(
            AutoSave::AfterDelay,
            1000,
            AutoSaveTrigger::TimerFired,
            inputs(false),
        );
        assert_eq!(d, AutoSaveDecision::Never);
    }

    #[test]
    fn auto_save_never_over_conflicts_or_deleted_files() {
        for (conflict, deleted) in [(true, false), (false, true)] {
            let doc = AutoSaveInputs {
                conflict,
                deleted,
                ..inputs(true)
            };
            for (mode, trigger) in [
                (AutoSave::AfterDelay, AutoSaveTrigger::TimerFired),
                (AutoSave::OnFocusChange, AutoSaveTrigger::FocusLost),
            ] {
                assert_eq!(
                    auto_save_decision(mode, 0, trigger, doc),
                    AutoSaveDecision::Never
                );
            }
        }
    }

    #[test]
    fn auto_save_modes_only_react_to_their_trigger() {
        let d = auto_save_decision(
            AutoSave::OnFocusChange,
            0,
            AutoSaveTrigger::Edited,
            inputs(true),
        );
        assert_eq!(d, AutoSaveDecision::Never);
        let d = auto_save_decision(
            AutoSave::OnFocusChange,
            0,
            AutoSaveTrigger::FocusLost,
            inputs(true),
        );
        assert_eq!(d, AutoSaveDecision::SaveNow);
        let d = auto_save_decision(AutoSave::Off, 0, AutoSaveTrigger::FocusLost, inputs(true));
        assert_eq!(d, AutoSaveDecision::Never);
    }

    #[test]
    fn cleanup_trims_and_normalizes_the_final_newline() {
        let apply = |text: &str, trim: bool, nl: bool| {
            let rope = Rope::from_str(text);
            let mut out = text.to_string();
            let mut edits = cleanup_edits(&rope, LineEnding::Lf, trim, nl);
            edits.sort_by_key(|e| std::cmp::Reverse(e.range.start));
            for e in edits {
                out.replace_range(e.range, &e.new_text);
            }
            out
        };
        assert_eq!(apply("a  \nb\t\n", true, false), "a\nb\n");
        assert_eq!(apply("a", false, true), "a\n");
        assert_eq!(apply("a\n\n\n", false, true), "a\n");
        assert_eq!(apply("", false, true), "");
    }
}
