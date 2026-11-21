//! Caret blinking and mouse input of the [`CodeView`]: selection by
//! dragging, hover tracking and the context menu (called from the element).

use std::time::Duration;

use gpui::{prelude::*, App, Pixels, Point, Window};

use super::selection::{Pos, Selection};
use super::{
    selection, CodeView, CodeViewEvent, DragMode, DragState, HoverTarget, LanguageFeatureProvider,
    BLINK_INTERVAL_MS, HOVER_DELAY_MS,
};
use crate::actions::editor::*;
use crate::components::context_menu::ContextMenu;
use crate::icons::IconName;

impl CodeView {
    // Caret blink.

    pub(super) fn reset_blink(&mut self) {
        self.cursor_visible = true;
        self.blink_epoch = self.blink_epoch.wrapping_add(1);
        self.blink_task = None;
    }

    /// Whether the caret blinks: `cursor_blink` is on and motion is not reduced
    /// (`ui.reduce_motion`, or the OS preference).
    fn blink_enabled(cx: &App) -> bool {
        crate::settings::AppSettings::get(cx).cursor_blink && !crate::a11y::reduce_motion(cx)
    }

    /// Whether a blink timer is running (tests).
    pub fn is_blinking(&self) -> bool {
        self.blink_task.is_some()
    }

    pub(super) fn ensure_blink(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !Self::blink_enabled(cx) {
            // Reduced motion (or blink off): a steady caret, and stop a timer that is running.
            self.blink_task = None;
            self.cursor_visible = true;
            return;
        }
        if self.blink_task.is_some() || !self.focus_handle.is_focused(window) {
            return;
        }
        let epoch = self.blink_epoch;
        self.blink_task = Some(cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(BLINK_INTERVAL_MS))
                .await;
            let go_on = this
                .update_in(cx, |v, window, cx| {
                    if v.blink_epoch != epoch
                        || !v.focus_handle.is_focused(window)
                        || !Self::blink_enabled(cx)
                    {
                        v.cursor_visible = true;
                        if v.blink_epoch == epoch {
                            v.blink_task = None;
                        }
                        cx.notify();
                        return false;
                    }
                    v.cursor_visible = !v.cursor_visible;
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !go_on {
                break;
            }
        }));
    }

    // Mouse-driven selection (called from the element).

    pub(crate) fn begin_selection(
        &mut self,
        pos: Pos,
        click_count: usize,
        extend: bool,
        add: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_command_hook(cx);
        window.focus(&self.focus_handle);
        let (mode, range) = match click_count {
            2 => (
                DragMode::Word,
                selection::word_range(self.source.as_ref(), pos),
            ),
            n if n >= 3 => (DragMode::Line, self.line_range(pos.row)),
            _ => (DragMode::Char, (pos, pos)),
        };
        if extend && mode == DragMode::Char {
            let primary = self.selection();
            self.selections = vec![Selection {
                anchor: primary.anchor,
                head: pos,
                goal_column: None,
            }];
            self.primary = 0;
            self.drag = Some(DragState {
                mode,
                origin: (primary.anchor, primary.anchor),
            });
        } else {
            let new = Selection {
                anchor: range.0,
                head: range.1,
                goal_column: None,
            };
            if add {
                if click_count == 1 && self.selections.len() > 1 {
                    // Alt-click on an existing caret removes it.
                    if let Some(ix) = self
                        .selections
                        .iter()
                        .position(|s| s.is_empty() && s.head == pos)
                    {
                        self.selections.remove(ix);
                        self.primary = self.selections.len() - 1;
                        self.drag = None;
                        self.selections_changed(false, cx);
                        return;
                    }
                }
                self.selections.push(new);
                let primary = self.selections.len() - 1;
                let (sels, primary) =
                    selection::normalize(std::mem::take(&mut self.selections), primary);
                self.selections = sels;
                self.primary = primary;
            } else {
                self.selections = vec![new];
                self.primary = 0;
            }
            self.drag = Some(DragState {
                mode,
                origin: range,
            });
        }
        self.selections_changed(false, cx);
    }

    pub(super) fn line_range(&self, row: u32) -> (Pos, Pos) {
        let next = row + 1;
        if next < self.source.line_count() {
            (Pos::new(row, 0), Pos::new(next, 0))
        } else {
            (Pos::new(row, 0), Pos::new(row, self.source.line(row).len()))
        }
    }

    pub(crate) fn extend_selection(&mut self, pos: Pos, cx: &mut Context<Self>) {
        let Some(drag) = self.drag else { return };
        let range = match drag.mode {
            DragMode::Char => (pos, pos),
            DragMode::Word => selection::word_range(self.source.as_ref(), pos),
            DragMode::Line => self.line_range(pos.row),
        };
        let (o_start, o_end) = drag.origin;
        let (anchor, head) = if range.0 < o_start {
            (o_end, range.0)
        } else {
            (o_start, range.1.max(o_end))
        };
        let new = Selection {
            anchor,
            head,
            goal_column: None,
        };
        if self.selections.get(self.primary) != Some(&new) {
            if let Some(slot) = self.selections.get_mut(self.primary) {
                *slot = new;
            }
            // Merge with carets the drag ran over (keeps the list valid).
            let (sels, primary) =
                selection::normalize(std::mem::take(&mut self.selections), self.primary);
            self.selections = sels;
            self.primary = primary;
            self.reset_blink();
            cx.emit(CodeViewEvent::SelectionChanged);
            cx.notify();
        }
    }

    pub(crate) fn end_selection(&mut self) {
        self.drag = None;
    }

    pub(crate) fn is_selecting(&self) -> bool {
        self.drag.is_some()
    }

    pub(crate) fn hover_moved(
        &mut self,
        pos: Option<Pos>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.hover_pos == pos {
            return;
        }
        self.hover_pos = pos;
        self.hover_task = None;
        cx.emit(CodeViewEvent::HoverMoved(pos));
        let (Some(pos), Some(callback)) = (pos, self.on_hover.clone()) else {
            return;
        };
        self.hover_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(HOVER_DELAY_MS))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.hover_pos == Some(pos) {
                    callback(&HoverTarget { pos, position }, window, cx);
                }
            });
        }));
    }

    pub(crate) fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = self.focus_handle.clone();
        let has_selection = self.selections.iter().any(|s| !s.is_empty());
        let path = self.path.clone();
        let has_buffer = self.buffer.is_some();
        let editable = self.is_editable();
        let read_only = self.is_read_only();
        let locked = self.read_only_locked;
        let (can_undo, can_redo) = self
            .buffer
            .as_ref()
            .map(|b| {
                let b = b.read(cx).buffer();
                (b.can_undo(), b.can_redo())
            })
            .unwrap_or((false, false));
        let features = path
            .as_deref()
            .map(|p| LanguageFeatureProvider::features(p, cx))
            .unwrap_or_default();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let item = |menu: ContextMenu,
                        enabled: bool,
                        label: &'static str,
                        icon: Option<IconName>,
                        action: Box<dyn gpui::Action>| {
                match (enabled, icon) {
                    (true, Some(icon)) => menu.icon_action(label, icon, action),
                    (true, None) => menu.action(label, action),
                    (false, _) => menu.disabled_action(label, action),
                }
            };
            let mut menu = menu.context(focus);
            if has_buffer {
                menu = item(menu, editable && can_undo, "Undo", None, Box::new(Undo));
                menu = item(menu, editable && can_redo, "Redo", None, Box::new(Redo));
                menu = menu.separator();
                menu = item(menu, editable, "Cut", None, Box::new(Cut));
            }
            menu = item(
                menu,
                true,
                if has_selection { "Copy" } else { "Copy Line" },
                Some(IconName::Copy),
                Box::new(Copy),
            );
            if has_buffer {
                menu = item(menu, editable, "Paste", None, Box::new(Paste));
            }
            menu = menu.action("Select All", Box::new(SelectAll));
            if has_buffer {
                menu = menu.separator();
                menu = item(
                    menu,
                    editable,
                    "Toggle Comment",
                    None,
                    Box::new(ToggleComment),
                );
                menu = item(
                    menu,
                    editable && features.format,
                    "Format Document",
                    None,
                    Box::new(Format),
                );
                menu = item(
                    menu,
                    editable && features.rename,
                    "Rename Symbol…",
                    None,
                    Box::new(Rename),
                );
                menu = item(
                    menu,
                    editable && features.code_actions,
                    "Code Actions…",
                    Some(IconName::Lightbulb),
                    Box::new(CodeActions),
                );
                menu = item(
                    menu,
                    editable,
                    "Edit with Agent…",
                    Some(IconName::Sparkles),
                    Box::new(crate::actions::agent::EditWithAgent),
                );
            }
            menu = menu
                .separator()
                .action("Go to Definition", Box::new(GoToDefinition))
                .action("Find References", Box::new(FindReferences));
            if path.is_some() {
                menu = menu
                    .separator()
                    .icon_action("Copy Path", IconName::Link, Box::new(CopyPath))
                    .action("Copy Relative Path", Box::new(CopyRelativePath))
                    .icon_action(
                        "Copy Permalink",
                        IconName::ExternalLink,
                        Box::new(CopyPermalink),
                    )
                    .separator()
                    .action("Toggle Blame", Box::new(ToggleBlame))
                    .icon_action("File History", IconName::History, Box::new(ShowFileHistory))
                    .action(
                        "Reveal in Files",
                        Box::new(crate::actions::workspace::RevealActiveFile),
                    );
            }
            if has_buffer {
                menu = menu.separator().icon_action(
                    if locked {
                        "Why Is This Read-Only?"
                    } else if read_only {
                        "Allow Editing"
                    } else {
                        "Make Read-Only"
                    },
                    if read_only {
                        IconName::LockOpen
                    } else {
                        IconName::Lock
                    },
                    Box::new(ToggleReadOnly),
                );
            }
            menu
        });
        self.context_menu
            .deploy(menu, position, window, cx, |this: &mut Self| {
                &mut this.context_menu
            });
        cx.notify();
    }
}
