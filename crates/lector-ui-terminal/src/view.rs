//! [`TerminalView`]: one terminal session drawn in a GPUI view.
//!
//! Frame scheduling: the core calls the wake callback on its parser thread; the callback only
//! sets a flag and (once) pokes a channel. A task on the GPUI thread drains the channel,
//! drains the session's events (titles, cwd, exit; bounded and cheap), marks the view dirty
//! and calls `cx.notify()`. The element takes **one** snapshot per frame, only when dirty,
//! and re-derives only rows whose `Arc` changed. The core sends no further `Wakeup` until the
//! next snapshot, so a hidden terminal (not painted, never snapshotting) costs nothing however
//! much output it produces; it only raises its "new output" flag for the tab strip.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    div, prelude::*, App, Bounds, ClipboardItem, Context, Entity, EntityInputHandler, EventEmitter,
    FocusHandle, Focusable, KeyDownEvent, Pixels, PromptLevel, SharedString, Task, UTF16Selection,
    WeakEntity, Window,
};
use lector_config::{TerminalCursorStyle, TerminalSettings};
use lector_terminal::{
    CursorShape, Direction, ExitInfo, GridPoint, LinkCandidate, LinkTarget, ScrollAmount,
    SearchMatch, SearchQuery, SelectionKind, Side, Snapshot, SpawnOptions, TermSize, Terminal,
    TerminalEvent, WakeFn,
};
use lector_ui::actions::terminal as actions;
use lector_ui::components::context_menu::{ContextMenu, ContextMenuState};
use lector_ui::components::text_input::{TextInput, TextInputEvent};
use lector_ui::settings::AppSettings;
use lector_ui::views::Workspace;

use crate::element::{ShapedRow, TerminalElement};
use crate::grid::RowCache;
use crate::keys::{route_keystroke, KeyOptions, KeyRoute};
use crate::routing::{paste_needs_confirmation, route_file, route_url, LinkAction};

/// Process-wide "don't ask again" for multi-line pastes (this run of Lector only).
static PASTE_CONFIRM_SUPPRESSED: AtomicBool = AtomicBool::new(false);

/// Events of a [`TerminalView`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalViewEvent {
    /// Title, cwd or running command changed (tab label).
    TitleChanged,
    /// New output arrived (for the tab's activity dot when hidden).
    Activity,
    /// The program rang the bell.
    Bell,
    /// The shell exited.
    Exited,
    /// The user asked to close this terminal (key press after exit).
    CloseRequested,
    /// The terminal got the keyboard focus.
    Focused,
}

/// Layout facts from the last prepaint, for mouse hit testing.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LayoutInfo {
    pub bounds: Bounds<Pixels>,
    pub cell_width: Pixels,
    pub line_height: Pixels,
    pub cols: u16,
    pub rows: u16,
}

/// Search state of the find bar.
pub(crate) struct FindState {
    pub input: Entity<TextInput>,
    pub regex: bool,
    pub case_sensitive: bool,
    pub error: Option<String>,
    pub active: Option<SearchMatch>,
    pub no_match: bool,
}

/// One terminal session as a view.
pub struct TerminalView {
    pub(crate) id: u64,
    pub(crate) session: Option<Arc<Terminal>>,
    pub(crate) error: Option<String>,
    pub(crate) focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    pub(crate) snapshot: Option<Snapshot>,
    pub(crate) dirty: bool,
    pub(crate) cache: RowCache<std::rc::Rc<ShapedRow>>,
    pub(crate) layout: Option<LayoutInfo>,
    pub(crate) size: TermSize,
    last_resize: Option<Instant>,
    resize_task: Option<Task<()>>,
    title: Option<String>,
    cwd: PathBuf,
    profile_name: String,
    pub(crate) exited: Option<ExitInfo>,
    pub(crate) has_new_output: bool,
    pub(crate) bell: bool,
    pub(crate) command_running: bool,
    pub(crate) last_command_failed: bool,
    current_command: Option<String>,
    pub(crate) marked_text: Option<String>,
    pub(crate) selecting: bool,
    pub(crate) mouse_forwarded: Option<lector_terminal::MouseButton>,
    pub(crate) scroll_remainder: f32,
    pub(crate) hovered_link: Option<LinkCandidate>,
    pub(crate) find: Option<FindState>,
    pub(crate) blink_on: bool,
    blink_task: Option<Task<()>>,
    pub(crate) focused: bool,
    pub(crate) palette_theme: Option<String>,
    context_menu: ContextMenuState,
    link_cancel: Option<Arc<AtomicBool>>,
    /// Painted at least once since it last became dirty (frame accounting for tests).
    pub(crate) frames: u64,
    _tasks: Vec<Task<()>>,
}

impl EventEmitter<TerminalViewEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Settings for terminals (defaults when not loaded).
pub(crate) fn terminal_settings(cx: &App) -> TerminalSettings {
    AppSettings::get(cx).terminal.clone()
}

impl TerminalView {
    /// A view for a new session spawned with `options` (on a background thread).
    pub fn new(
        id: u64,
        options: SpawnOptions,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let cwd = options
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let mut this = Self::detached(
            id,
            cwd,
            options.profile.name.clone(),
            options.size,
            workspace,
            window,
            cx,
        );

        // Wake: parser thread → at most one pending message → GPUI task.
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
        let pending = Arc::new(AtomicBool::new(false));
        let wake_pending = pending.clone();
        let wake: WakeFn = Arc::new(move || {
            if !wake_pending.swap(true, Ordering::AcqRel) {
                let _ = tx.unbounded_send(());
            }
        });
        let wake_task = cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while rx.next().await.is_some() {
                pending.store(false, Ordering::Release);
                if this.update(cx, |view, cx| view.on_wake(cx)).is_err() {
                    break;
                }
            }
        });
        let spawn = cx.background_spawn(async move { Terminal::spawn(options, Some(wake)) });
        let spawn_task = cx.spawn(async move |this, cx| {
            let result = spawn.await;
            let _ = this.update(cx, |view, cx| view.on_spawned(result, cx));
        });
        this._tasks = vec![wake_task, spawn_task];
        this
    }

    /// A view without a process (tests; a session that failed to start shows its error).
    pub(crate) fn detached(
        id: u64,
        cwd: PathBuf,
        profile_name: String,
        size: TermSize,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle().tab_stop(true);
        cx.on_focus(&focus_handle, window, |view, _, cx| {
            view.set_focused(true, cx)
        })
        .detach();
        cx.on_blur(&focus_handle, window, |view, _, cx| {
            view.set_focused(false, cx)
        })
        .detach();

        TerminalView {
            id,
            session: None,
            error: None,
            focus_handle,
            workspace,
            snapshot: None,
            dirty: true,
            cache: RowCache::default(),
            layout: None,
            size,
            last_resize: None,
            resize_task: None,
            title: None,
            cwd,
            profile_name,
            exited: None,
            has_new_output: false,
            bell: false,
            command_running: false,
            last_command_failed: false,
            current_command: None,
            marked_text: None,
            selecting: false,
            mouse_forwarded: None,
            scroll_remainder: 0.,
            hovered_link: None,
            find: None,
            blink_on: true,
            blink_task: None,
            focused: false,
            palette_theme: None,
            context_menu: ContextMenuState::default(),
            link_cancel: None,
            frames: 0,
            _tasks: Vec::new(),
        }
    }

    fn on_spawned(
        &mut self,
        result: Result<Terminal, lector_terminal::TerminalError>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(term) => {
                let term = Arc::new(term);
                crate::registry_add(&term, cx);
                if self.size != term.lock().size() {
                    if let Err(err) = term.resize(self.size) {
                        tracing::warn!("terminal resize: {err}");
                    }
                }
                self.session = Some(term);
                self.palette_theme = None;
            }
            Err(err) => {
                tracing::warn!("terminal: {err}");
                self.error = Some(err.to_string());
            }
        }
        self.dirty = true;
        cx.emit(TerminalViewEvent::TitleChanged);
        cx.notify();
    }

    fn on_wake(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let mut title_changed = false;
        for event in session.drain_events() {
            match event {
                TerminalEvent::Wakeup => {}
                TerminalEvent::TitleChanged(title) => {
                    self.title = title;
                    title_changed = true;
                }
                TerminalEvent::Bell => {
                    self.bell = true;
                    cx.emit(TerminalViewEvent::Bell);
                }
                TerminalEvent::CwdChanged(cwd) => {
                    self.cwd = cwd;
                    title_changed = true;
                }
                TerminalEvent::PromptStarted => {
                    if self.command_running {
                        self.command_running = false;
                        self.current_command = None;
                        title_changed = true;
                    }
                }
                TerminalEvent::CommandStarted => {}
                TerminalEvent::CommandOutputStarted => {
                    self.command_running = true;
                    let em = session.lock();
                    self.current_command = em
                        .command_blocks()
                        .last()
                        .and_then(|b| em.command_text(b))
                        .filter(|t| !t.is_empty());
                    title_changed = true;
                }
                TerminalEvent::CommandFinished { exit_code } => {
                    self.command_running = false;
                    self.current_command = None;
                    self.last_command_failed = exit_code.is_some_and(|c| c != 0);
                    title_changed = true;
                }
                TerminalEvent::ClipboardCopy {
                    text,
                    primary_selection,
                } => {
                    // Programs may copy (OSC 52) only while the user works in this terminal.
                    if !primary_selection && self.focused {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                }
                TerminalEvent::ClipboardReadDenied => {}
                TerminalEvent::Exited(info) => {
                    self.exited = Some(info);
                    self.command_running = false;
                    title_changed = true;
                    cx.emit(TerminalViewEvent::Exited);
                }
            }
        }
        if title_changed {
            cx.emit(TerminalViewEvent::TitleChanged);
        }
        self.dirty = true;
        if !self.has_new_output {
            self.has_new_output = true;
            cx.emit(TerminalViewEvent::Activity);
        }
        cx.notify();
    }

    /// Label for the tab: running command, else the cwd's folder name, else the program's
    /// title, else the profile name.
    pub fn display_title(&self) -> String {
        if let Some(cmd) = self
            .current_command
            .as_deref()
            .filter(|_| self.command_running)
        {
            return truncate(cmd, 40);
        }
        if self.error.is_some() {
            return "Error".into();
        }
        if let Some(name) = self.cwd.file_name() {
            return name.to_string_lossy().into_owned();
        }
        if let Some(t) = self.title.as_deref().filter(|t| !t.is_empty()) {
            return truncate(t, 40);
        }
        if !self.cwd.as_os_str().is_empty() {
            return self.cwd.display().to_string();
        }
        self.profile_name.clone()
    }

    /// The working directory (last OSC 7 report, else where it started).
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Whether the shell has exited.
    pub fn has_exited(&self) -> bool {
        self.exited.is_some()
    }

    /// Whether a command is running (known only with shell integration).
    pub fn is_command_running(&self) -> bool {
        self.command_running && self.exited.is_none()
    }

    /// Whether the last command failed (shell integration).
    pub fn last_command_failed(&self) -> bool {
        self.last_command_failed
    }

    /// Clears the activity / bell flags (the tab became visible).
    pub(crate) fn mark_seen(&mut self) {
        self.has_new_output = false;
        self.bell = false;
    }

    /// The program's title, if it set one.
    pub fn program_title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Kills the session's processes (the view stays readable).
    pub fn kill(&mut self) {
        if let Some(s) = &self.session {
            s.kill();
        }
    }

    // ------------------------------------------------------------------ frame

    /// Applies a new grid size from layout, coalescing bursts (drag-resize) to one resize
    /// per 50 ms; the last size always lands.
    pub(crate) fn request_size(&mut self, size: TermSize, cx: &mut Context<Self>) {
        if size.cols == self.size.cols
            && size.rows == self.size.rows
            && size.cell_width == self.size.cell_width
            && size.cell_height == self.size.cell_height
        {
            self.resize_task = None;
            return;
        }
        let now = Instant::now();
        let recent = self
            .last_resize
            .is_some_and(|t| now.duration_since(t) < Duration::from_millis(50));
        if !recent {
            self.apply_size(size);
            return;
        }
        if self.resize_task.is_none() {
            self.resize_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(60))
                    .await;
                let _ = this.update(cx, |view, cx| {
                    view.resize_task = None;
                    view.last_resize = None;
                    cx.notify();
                });
            }));
        }
    }

    fn apply_size(&mut self, size: TermSize) {
        self.size = size;
        self.last_resize = Some(Instant::now());
        self.dirty = true;
        if let Some(s) = &self.session {
            if let Err(err) = s.resize(size) {
                tracing::warn!("terminal resize: {err}");
            }
        }
    }

    /// Takes the frame's snapshot when something changed; `true` when a new one was taken.
    pub(crate) fn refresh_snapshot(&mut self) -> bool {
        let Some(session) = &self.session else {
            return false;
        };
        if !self.dirty && self.snapshot.is_some() {
            return false;
        }
        self.dirty = false;
        self.snapshot = Some(session.snapshot());
        true
    }

    /// Focus changed: tell the program (focus reporting), restart or stop the blink, drop
    /// the link hover.
    pub(crate) fn set_focused(&mut self, focused: bool, cx: &mut Context<Self>) {
        if self.focused == focused {
            return;
        }
        self.focused = focused;
        if let Some(s) = &self.session {
            s.set_focused(focused);
        }
        if focused {
            self.restart_blink(cx);
            cx.emit(TerminalViewEvent::Focused);
        } else {
            self.blink_task = None;
            self.blink_on = true;
            self.hovered_link = None;
        }
        cx.notify();
    }

    /// Shows the cursor and restarts its blink cycle (input resets it, like editors do).
    fn restart_blink(&mut self, cx: &mut Context<Self>) {
        self.blink_on = true;
        let settings = terminal_settings(cx);
        if !self.focused || !settings.cursor_blink || lector_ui::a11y::reduce_motion(cx) {
            self.blink_task = None;
            return;
        }
        // Replacing the task cancels the previous cycle.
        self.blink_task = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(530))
                .await;
            let alive = this.update(cx, |view, cx| {
                view.blink_on = !view.blink_on;
                cx.notify();
            });
            if alive.is_err() {
                break;
            }
        }));
    }

    /// The cursor shape to draw: the program's choice, else the setting.
    pub(crate) fn cursor_shape(&self, program: CursorShape, cx: &App) -> CursorShape {
        if program != CursorShape::Block {
            return program;
        }
        match terminal_settings(cx).cursor_style {
            TerminalCursorStyle::Block => CursorShape::Block,
            TerminalCursorStyle::Beam => CursorShape::Beam,
            TerminalCursorStyle::Underline => CursorShape::Underline,
        }
    }

    // ------------------------------------------------------------------ input

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.marked_text.is_some() {
            return;
        }
        if self.exited.is_some() {
            cx.stop_propagation();
            cx.emit(TerminalViewEvent::CloseRequested);
            return;
        }
        let Some(session) = self.session.clone() else {
            return;
        };
        let settings = terminal_settings(cx);
        let alt_screen = self.snapshot.as_ref().is_some_and(|s| s.modes.alt_screen);
        // Ctrl+C copies a selection when the setting asks for it (Linux/Windows).
        if settings.ctrl_c_copies_selection
            && !cfg!(target_os = "macos")
            && event.keystroke.key == "c"
            && event.keystroke.modifiers.control
            && !event.keystroke.modifiers.shift
            && !event.keystroke.modifiers.alt
            && self.copy(cx)
        {
            self.clear_selection(cx);
            cx.stop_propagation();
            return;
        }
        let opts = KeyOptions::current(settings.option_as_meta, alt_screen);
        match route_keystroke(&event.keystroke, opts) {
            KeyRoute::Text => return,
            KeyRoute::Ignore => return,
            KeyRoute::Send(key, mods) => {
                session.send_key(key, mods);
            }
            KeyRoute::SendText(text) => session.send_text(&text),
            KeyRoute::Scroll(amount) => {
                session.scroll(amount);
                self.dirty = true;
            }
        }
        self.after_input(window, cx);
        cx.stop_propagation();
    }

    fn after_input(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.hovered_link = None;
        self.restart_blink(cx);
        cx.notify();
    }

    /// Copies the selection; `false` when nothing is selected.
    pub fn copy(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(session) = &self.session else {
            return false;
        };
        let text = session.lock().selection_text();
        match text.filter(|t| !t.is_empty()) {
            Some(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                true
            }
            None => false,
        }
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.lock().clear_selection();
            self.dirty = true;
            cx.notify();
        }
    }

    /// Pastes the clipboard (bracketed when the program enabled it). Multi-line text into a
    /// program without bracketed paste asks first (it would run every line).
    pub fn paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else {
            return;
        };
        self.paste_text(text, window, cx);
    }

    pub(crate) fn paste_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        if text.is_empty() || self.exited.is_some() {
            return;
        }
        let bracketed = session.lock().modes().bracketed_paste;
        let settings = terminal_settings(cx);
        if !paste_needs_confirmation(
            &text,
            bracketed,
            settings.confirm_multiline_paste,
            PASTE_CONFIRM_SUPPRESSED.load(Ordering::Relaxed),
        ) {
            session.paste(&text);
            self.after_input(window, cx);
            return;
        }
        let lines = text.lines().count().max(2);
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Paste {lines} lines into the terminal?"),
            Some(
                "The program did not enable bracketed paste, so the shell may run each line as \
                 a command. (Turn this question off with terminal.confirm_multiline_paste.)",
            ),
            &["Paste", "Paste, Don't Ask Again", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let choice = answer.await.ok();
            if !matches!(choice, Some(0) | Some(1)) {
                return;
            }
            if choice == Some(1) {
                PASTE_CONFIRM_SUPPRESSED.store(true, Ordering::Relaxed);
            }
            let _ = this.update_in(cx, |view, window, cx| {
                if let Some(s) = &view.session {
                    s.paste(&text);
                }
                view.after_input(window, cx);
            });
        })
        .detach();
    }

    /// Selects everything (screen and scrollback).
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.lock().select_all();
            self.dirty = true;
            cx.notify();
        }
    }

    /// Clears scrollback and screen; the shell redraws its prompt (Ctrl+L) when it is not a
    /// full-screen program.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let alt = {
            let mut em = session.lock();
            let alt = em.modes().alt_screen;
            if !alt {
                em.feed(b"\x1b[H\x1b[2J\x1b[3J");
            }
            alt
        };
        if !alt && self.exited.is_none() {
            session.write(vec![0x0c]);
        }
        self.dirty = true;
        cx.notify();
    }

    /// Scrolls to the previous / next command (shell integration).
    pub fn scroll_to_command(&mut self, direction: Direction, cx: &mut Context<Self>) -> bool {
        let Some(session) = &self.session else {
            return false;
        };
        let found = session.lock().scroll_to_command(direction).is_some();
        if !found && direction == Direction::Next {
            session.scroll(ScrollAmount::Bottom);
        }
        self.dirty = true;
        cx.notify();
        found
    }

    /// Scrolls the scrollback.
    pub fn scroll(&mut self, amount: ScrollAmount, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.scroll(amount);
            self.dirty = true;
            cx.notify();
        }
    }

    // ------------------------------------------------------------------ find

    /// Opens (or focuses) the find bar.
    pub fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(find) = &self.find {
            let handle = find.input.focus_handle(cx);
            window.focus(&handle);
            find.input.update(cx, |i, cx| i.select_all_text(cx));
            return;
        }
        let input = cx.new(|cx| TextInput::new("Find in terminal", cx));
        // Seed with the selection, like the editor's find.
        if let Some(text) = self
            .session
            .as_ref()
            .and_then(|s| s.lock().selection_text())
            .filter(|t| !t.contains('\n') && !t.is_empty())
        {
            input.update(cx, |i, cx| {
                i.set_text(text, cx);
                i.select_all_text(cx);
            });
        }
        cx.subscribe(&input, |view, _, event: &TextInputEvent, cx| {
            let TextInputEvent::Changed(_) = event;
            view.update_search(None, cx);
        })
        .detach();
        let handle = input.focus_handle(cx);
        self.find = Some(FindState {
            input,
            regex: false,
            case_sensitive: false,
            error: None,
            active: None,
            no_match: false,
        });
        window.focus(&handle);
        self.update_search(None, cx);
        cx.notify();
    }

    /// Closes the find bar and returns the focus to the terminal.
    pub fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.take().is_some() {
            if let Some(s) = &self.session {
                s.lock().clear_search();
            }
            window.focus(&self.focus_handle);
            self.dirty = true;
            cx.notify();
        }
    }

    pub(crate) fn toggle_find_option(&mut self, regex: bool, cx: &mut Context<Self>) {
        if let Some(find) = &mut self.find {
            if regex {
                find.regex = !find.regex;
            } else {
                find.case_sensitive = !find.case_sensitive;
            }
        }
        self.update_search(None, cx);
    }

    /// Re-applies the query; with `direction`, jumps to the next / previous match.
    pub(crate) fn update_search(&mut self, direction: Option<Direction>, cx: &mut Context<Self>) {
        let (Some(find), Some(session)) = (&mut self.find, &self.session) else {
            return;
        };
        let query = SearchQuery {
            pattern: find.input.read(cx).text().to_string(),
            regex: find.regex,
            case_sensitive: find.case_sensitive,
        };
        let mut em = session.lock();
        match em.set_search(&query) {
            Ok(()) => find.error = None,
            Err(err) => {
                find.error = Some(err.to_string());
                find.active = None;
                drop(em);
                cx.notify();
                return;
            }
        }
        if query.pattern.is_empty() {
            find.active = None;
            find.no_match = false;
        } else {
            find.active = em.search_next(direction.unwrap_or(Direction::Previous));
            find.no_match = find.active.is_none();
        }
        drop(em);
        self.dirty = true;
        cx.notify();
    }

    /// Next / previous match (opens the bar when closed).
    pub fn find_next(&mut self, direction: Direction, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.is_none() {
            self.open_find(window, cx);
            return;
        }
        self.update_search(Some(direction), cx);
    }

    // ------------------------------------------------------------------ links

    /// Opens a link candidate (Ctrl/Cmd+click, "Open Link").
    pub(crate) fn open_link(
        &mut self,
        link: LinkCandidate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &link.target {
            LinkTarget::Url(url) => match route_url(url) {
                LinkAction::OpenUrl(url) => cx.open_url(&url),
                LinkAction::Ignore(why) => self.toast(why, cx),
                LinkAction::OpenFile { .. } => {}
            },
            LinkTarget::File { .. } => {
                let Some(session) = self.session.clone() else {
                    return;
                };
                if let Some(old) = self.link_cancel.take() {
                    old.store(true, Ordering::Relaxed);
                }
                let cancel = Arc::new(AtomicBool::new(false));
                self.link_cancel = Some(cancel.clone());
                let target = link.target.clone();
                let root = self
                    .workspace
                    .upgrade()
                    .filter(|ws| ws.read(cx).has_folder())
                    .map(|ws| ws.read(cx).root().to_path_buf());
                // Existence checks touch the disk: off the UI thread, cancellable.
                let resolve = cx.background_spawn(async move {
                    let resolved = match session.resolve_link(&target, &cancel) {
                        Some(LinkTarget::File { path, line, col }) => Some((path, line, col)),
                        _ => None,
                    };
                    route_file(resolved, root.as_deref())
                });
                cx.spawn_in(window, async move |this, cx| {
                    let action = resolve.await;
                    let _ = this.update_in(cx, |view, window, cx| {
                        view.apply_file_link(action, window, cx)
                    });
                })
                .detach();
            }
        }
    }

    fn apply_file_link(&mut self, action: LinkAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            LinkAction::OpenFile {
                path,
                line,
                col,
                read_only,
            } => {
                let Some(ws) = self.workspace.upgrade() else {
                    return;
                };
                ws.update(cx, |ws, cx| {
                    let view =
                        ws.open_path_at(path, line.map(|l| l + 1), col.map(|c| c + 1), window, cx);
                    if read_only {
                        if let Some(view) = view {
                            let code = view.read(cx).code_view().clone();
                            code.update(cx, |c, cx| c.set_read_only(true, cx));
                        }
                    }
                });
            }
            LinkAction::Ignore(why) => self.toast(why, cx),
            LinkAction::OpenUrl(_) => {}
        }
    }

    fn toast(&self, message: &'static str, cx: &mut Context<Self>) {
        if let Some(ws) = self.workspace.upgrade() {
            ws.update(cx, |ws, cx| ws.show_toast(message, cx));
        }
    }

    /// The link at a viewport cell.
    pub(crate) fn link_at(&self, row: u32, col: u32) -> Option<LinkCandidate> {
        let session = self.session.as_ref()?;
        let em = session.lock();
        let point = em.viewport_point(row, col);
        em.link_at(point)
    }

    // ------------------------------------------------------------------ mouse

    pub(crate) fn start_selection(
        &mut self,
        kind: SelectionKind,
        point: GridPoint,
        side: Side,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = &self.session {
            session.lock().start_selection(kind, point, side);
            self.selecting = true;
            self.dirty = true;
            cx.notify();
        }
    }

    pub(crate) fn update_selection(
        &mut self,
        point: GridPoint,
        side: Side,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = &self.session {
            session.lock().update_selection(point, side);
            self.dirty = true;
            cx.notify();
        }
    }

    pub(crate) fn finish_selection(&mut self, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        self.selecting = false;
        if terminal_settings(cx).copy_on_select {
            self.copy(cx);
        }
    }

    pub(crate) fn deploy_context_menu(
        &mut self,
        position: gpui::Point<Pixels>,
        link: Option<LinkCandidate>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = self.focus_handle.clone();
        let has_selection = self
            .session
            .as_ref()
            .is_some_and(|s| s.lock().selection_span().is_some());
        let in_panel = crate::is_in_panel(self.id, cx);
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let mut menu = menu.context(focus);
            if let Some(link) = link {
                let label = match &link.target {
                    LinkTarget::Url(_) => "Open Link",
                    LinkTarget::File { .. } => "Open File",
                };
                menu = menu
                    .entry(label, move |window, cx| {
                        let link = link.clone();
                        let _ = this.update(cx, |v, cx| v.open_link(link, window, cx));
                    })
                    .separator();
            }
            menu = if has_selection {
                menu.action("Copy", Box::new(actions::Copy))
            } else {
                menu.disabled_action("Copy", Box::new(actions::Copy))
            };
            menu = menu
                .action("Paste", Box::new(actions::Paste))
                .action("Select All", Box::new(actions::SelectAll))
                .separator()
                .action("Find…", Box::new(actions::Find))
                .action("Clear", Box::new(actions::Clear))
                .separator()
                .action("Split Terminal", Box::new(actions::SplitTerminal))
                .action("Rename…", Box::new(actions::Rename));
            menu = if in_panel {
                menu.action("Move to Editor Area", Box::new(actions::MoveToEditor))
            } else {
                menu.action("Move to Panel", Box::new(actions::MoveToPanel))
            };
            menu.separator()
                .action("Kill Terminal", Box::new(actions::KillTerminal))
        });
        self.context_menu
            .deploy(menu, position, window, cx, |this: &mut Self| {
                &mut this.context_menu
            });
        cx.notify();
    }

    fn render_find_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        use lector_ui::components::{h_flex, IconButton, Tooltip};
        use lector_ui::icons::{IconName, IconSize};
        use lector_ui::scale::px;
        use lector_ui::theme::{ActiveTheme, ColorExt};
        let find = self.find.as_ref()?;
        let ui = cx.theme().ui.clone();
        let status: SharedString = if let Some(err) = &find.error {
            truncate(err, 40).into()
        } else if find.no_match {
            "No results".into()
        } else {
            "".into()
        };
        let regex = find.regex;
        let case = find.case_sensitive;
        Some(
            h_flex()
                .id("terminal-find")
                .key_context(lector_ui::keymap::contexts::TERMINAL_FIND)
                .absolute()
                .top(px(6.))
                .right(px(18.))
                .w(px(380.))
                .gap(px(4.))
                .px(px(6.))
                .py(px(4.))
                .rounded(px(6.))
                .bg(ui.elevated.hsla())
                .border_1()
                .border_color(ui.border.hsla())
                .shadow_md()
                .occlude()
                .on_action(cx.listener(|v, _: &actions::FindNext, w, cx| {
                    v.find_next(Direction::Next, w, cx)
                }))
                .on_action(cx.listener(|v, _: &actions::FindPrev, w, cx| {
                    v.find_next(Direction::Previous, w, cx)
                }))
                .on_action(cx.listener(|v, _: &actions::CloseFind, w, cx| v.close_find(w, cx)))
                .child(div().flex_1().min_w_0().child(find.input.clone()))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(if find.error.is_some() {
                            ui.error.hsla()
                        } else {
                            ui.text_muted.hsla()
                        })
                        .child(status),
                )
                .child(
                    IconButton::new("term-find-case", IconName::CaseSensitive)
                        .icon_size(IconSize::Small)
                        .selected(case)
                        .tooltip(Tooltip::text("Match Case"))
                        .on_click(cx.listener(|v, _, _, cx| v.toggle_find_option(false, cx))),
                )
                .child(
                    IconButton::new("term-find-regex", IconName::Regex)
                        .icon_size(IconSize::Small)
                        .selected(regex)
                        .tooltip(Tooltip::text("Use Regular Expression"))
                        .on_click(cx.listener(|v, _, _, cx| v.toggle_find_option(true, cx))),
                )
                .child(
                    IconButton::new("term-find-prev", IconName::ArrowUp)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::for_action("Previous Match", &actions::FindPrev))
                        .on_click(
                            cx.listener(|v, _, w, cx| v.find_next(Direction::Previous, w, cx)),
                        ),
                )
                .child(
                    IconButton::new("term-find-next", IconName::ArrowDown)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::for_action("Next Match", &actions::FindNext))
                        .on_click(cx.listener(|v, _, w, cx| v.find_next(Direction::Next, w, cx))),
                )
                .child(
                    IconButton::new("term-find-close", IconName::Close)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::for_action("Close", &actions::CloseFind))
                        .on_click(cx.listener(|v, _, w, cx| v.close_find(w, cx))),
                ),
        )
    }

    fn render_banner(&self, cx: &App) -> Option<impl IntoElement> {
        use lector_ui::scale::px;
        use lector_ui::theme::{ActiveTheme, ColorExt};
        let text: String = if let Some(err) = &self.error {
            format!("Could not start the terminal: {err}")
        } else {
            let info = self.exited.as_ref()?;
            match &info.signal {
                Some(sig) => format!("[Process exited: {sig}] Press any key to close."),
                None => format!(
                    "[Process exited with code {}] Press any key to close.",
                    info.code
                ),
            }
        };
        let ui = &cx.theme().ui;
        Some(
            div()
                .absolute()
                .bottom(px(4.))
                .left(px(8.))
                .px(px(8.))
                .py(px(3.))
                .rounded(px(4.))
                .bg(ui.elevated.hsla())
                .border_1()
                .border_color(ui.border.hsla())
                .text_size(px(12.))
                .text_color(if self.error.is_some() {
                    ui.error.hsla()
                } else {
                    ui.text_muted.hsla()
                })
                .child(text),
        )
    }
}

impl Drop for TerminalView {
    fn drop(&mut self) {
        if let Some(c) = &self.link_cancel {
            c.store(true, Ordering::Relaxed);
        }
        // Dropping the last `Arc<Terminal>` kills the process tree (never blocks).
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let entity = cx.entity();
        div()
            .id(("terminal-view", self.id))
            .relative()
            .size_full()
            .overflow_hidden()
            .on_action(cx.listener(|v, _: &actions::Copy, _, cx| {
                v.copy(cx);
            }))
            .on_action(cx.listener(|v, _: &actions::Paste, w, cx| v.paste(w, cx)))
            .on_action(cx.listener(|v, _: &actions::SelectAll, _, cx| v.select_all(cx)))
            .on_action(cx.listener(|v, _: &actions::Clear, _, cx| v.clear(cx)))
            .on_action(cx.listener(|v, _: &actions::Find, w, cx| v.open_find(w, cx)))
            .on_action(cx.listener(|v, _: &actions::ScrollToPrevCommand, _, cx| {
                v.scroll_to_command(Direction::Previous, cx);
            }))
            .on_action(cx.listener(|v, _: &actions::ScrollToNextCommand, _, cx| {
                v.scroll_to_command(Direction::Next, cx);
            }))
            .child(
                div()
                    .id(("terminal-surface", self.id))
                    .key_context(lector_ui::keymap::contexts::TERMINAL)
                    .track_focus(&self.focus_handle)
                    .size_full()
                    .on_key_down(cx.listener(Self::key_down))
                    .on_action(cx.listener(
                        |v, _: &lector_ui::actions::workspace::ShowContextMenu, w, cx| {
                            let pos = v
                                .layout
                                .map(|l| {
                                    l.bounds.origin + gpui::point(gpui::px(24.), gpui::px(24.))
                                })
                                .unwrap_or_default();
                            v.deploy_context_menu(pos, None, w, cx);
                        },
                    ))
                    .child(TerminalElement::new(entity)),
            )
            .children(self.render_banner(cx))
            .children(self.render_find_bar(cx))
            .children(self.context_menu.render())
            .children(lector_ui::a11y::region_ring(focused, cx, gpui::px(0.)))
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: std::ops::Range<usize>,
        adjusted: &mut Option<std::ops::Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let marked = self.marked_text.as_deref().unwrap_or("");
        let units: Vec<u16> = marked.encode_utf16().collect();
        let end = range.end.min(units.len());
        let start = range.start.min(end);
        adjusted.replace(start..end);
        String::from_utf16(&units[start..end]).ok()
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let len = self
            .marked_text
            .as_deref()
            .map(|m| m.encode_utf16().count())
            .unwrap_or(0);
        Some(UTF16Selection {
            range: len..len,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        self.marked_text
            .as_deref()
            .map(|m| 0..m.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked_text = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<std::ops::Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = None;
        if self.exited.is_some() {
            cx.emit(TerminalViewEvent::CloseRequested);
            return;
        }
        if let Some(session) = &self.session {
            if !text.is_empty() {
                session.send_text(text);
            }
        }
        self.after_input(window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<std::ops::Range<usize>>,
        new_text: &str,
        _: Option<std::ops::Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = (!new_text.is_empty()).then(|| new_text.to_string());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: std::ops::Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout?;
        let cursor = self.snapshot.as_ref()?.cursor;
        let origin = crate::element::cell_origin(&layout, cursor.row, cursor.col);
        Some(Bounds::new(
            origin,
            gpui::size(layout.cell_width, layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn titles_truncate() {
        assert_eq!(truncate("  cargo test  ", 40), "cargo test");
        assert_eq!(truncate("abcdef", 4), "abc…");
    }
}
