//! Inline merge-conflict actions in the normal editor: every open file whose
//! text contains conflict markers gets tinted Current / Incoming regions and a
//! small action bar on each `<<<<<<<` line ("Accept Current | Accept Incoming |
//! Accept Both | Compare | Open Merge Editor").
//!
//! Resolutions are buffer edits through [`DocumentModel::apply_edits`]: one
//! undo step each, shared by every tab of the file, nothing written to disk
//! until the user saves. Drawn through the code view's owner-keyed layers
//! (`CodeView::set_row_backgrounds` / `set_row_widgets`, owner [`LAYER`]).

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, prelude::*, AnyElement, App, AppContext, ClickEvent, Context, Entity, EntityId,
    EventEmitter, Hsla, SharedString, Subscription, Task, WeakEntity, Window,
};
use lector_ui::code_view::{CodeView, RowBackground, RowWidget, WidgetAnchor};
use lector_ui::components::{h_flex, metrics, Tooltip};
use lector_ui::editor::{DocumentEvent, DocumentModel, FileView};
use lector_ui::scale::px;
use lector_ui::views::Workspace;
use lector_ui::{ActiveTheme, ColorExt};
use lector_workspace::conflicts::{self, ConflictParse, Resolution, Side};

use crate::store::GitStore;

/// Layer owner name on the code view.
pub const LAYER: &str = "git-conflicts";

/// Debounce between an edit and re-parsing the markers.
const REPARSE_DELAY_MS: u64 = 80;

/// Colors of the conflict tints for the current theme.
#[derive(Clone, Copy)]
pub struct ConflictColors {
    /// Current (ours) accent.
    pub ours: Hsla,
    /// Incoming (theirs) accent.
    pub theirs: Hsla,
    /// Common ancestor accent.
    pub base: Hsla,
}

impl ConflictColors {
    /// From the active theme: green-ish current, blue-ish incoming, grey base.
    pub fn get(cx: &App) -> ConflictColors {
        let theme = cx.theme();
        ConflictColors {
            ours: theme.ui.success.hsla(),
            theirs: theme.ui.info.hsla(),
            base: theme.ui.text_faint.hsla(),
        }
    }
}

/// The parsed markers of a buffer snapshot.
#[derive(Clone)]
pub struct ParsedText {
    /// Rope version the parse belongs to.
    pub version: u64,
    /// The text that was parsed (offsets of `parse` refer to it).
    pub text: Arc<str>,
    /// Regions and problems.
    pub parse: Arc<ConflictParse>,
}

/// Emitted when the parse of the file changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileConflictsEvent {
    /// New parse.
    Changed,
}

/// Conflict tracking for one file view.
pub struct FileConflicts {
    store: Entity<GitStore>,
    model: Entity<DocumentModel>,
    code_view: WeakEntity<CodeView>,
    in_merge_editor: bool,
    parsed: Option<ParsedText>,
    task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<FileConflictsEvent> for FileConflicts {}

impl FileConflicts {
    /// Tracks `file_view`'s document; `in_merge_editor` hides "Open Merge Editor".
    pub fn new(
        store: Entity<GitStore>,
        file_view: &Entity<FileView>,
        in_merge_editor: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let fv = file_view.read(cx);
        let model = fv.model().clone();
        let code_view = fv.code_view().downgrade();
        let subs = vec![
            cx.subscribe(&model, |this, _, event: &DocumentEvent, cx| match event {
                DocumentEvent::Loaded
                | DocumentEvent::SourceChanged
                | DocumentEvent::PathChanged
                | DocumentEvent::ConflictChanged => this.schedule_parse(cx),
                _ => {}
            }),
            cx.observe_global::<lector_ui::ThemeState>(|this, cx| this.update_layers(cx)),
        ];
        let mut this = FileConflicts {
            store,
            model,
            code_view,
            in_merge_editor,
            parsed: None,
            task: None,
            _subscriptions: subs,
        };
        this.parse_now(cx);
        this
    }

    /// The document model.
    pub fn model(&self) -> &Entity<DocumentModel> {
        &self.model
    }

    /// The latest parse (may lag the buffer by [`REPARSE_DELAY_MS`]).
    pub fn parsed(&self) -> Option<&ParsedText> {
        self.parsed.as_ref()
    }

    /// The buffer's rope, if the document is loaded and editable.
    fn rope(&self, cx: &App) -> Option<lector_text::Rope> {
        let model = self.model.read(cx);
        Some(model.buffer()?.read(cx).text().clone())
    }

    /// A parse of the current buffer text (re-parsed synchronously when stale).
    pub fn fresh(&mut self, cx: &mut Context<Self>) -> Option<ParsedText> {
        let rope = self.rope(cx)?;
        if self.parsed.as_ref().map(|p| p.version) != Some(rope.version()) {
            self.parse_now(cx);
        }
        self.parsed.clone()
    }

    fn schedule_parse(&mut self, cx: &mut Context<Self>) {
        let Some(rope) = self.rope(cx) else {
            return;
        };
        if self.parsed.as_ref().map(|p| p.version) == Some(rope.version()) {
            return;
        }
        self.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(REPARSE_DELAY_MS))
                .await;
            let work = cx.background_spawn(async move { parse_rope(&rope) });
            let parsed = work.await;
            let _ = this.update(cx, |this, cx| {
                this.task = None;
                this.set_parsed(parsed, cx);
            });
        }));
    }

    fn parse_now(&mut self, cx: &mut Context<Self>) {
        if let Some(rope) = self.rope(cx) {
            self.task = None;
            let parsed = parse_rope(&rope);
            self.set_parsed(parsed, cx);
        }
    }

    fn set_parsed(&mut self, parsed: ParsedText, cx: &mut Context<Self>) {
        let changed = self
            .parsed
            .as_ref()
            .is_none_or(|p| p.parse != parsed.parse || p.version != parsed.version);
        self.parsed = Some(parsed);
        self.update_layers(cx);
        if changed {
            cx.emit(FileConflictsEvent::Changed);
        }
    }

    /// Resolves conflict `index` of the current text. Returns whether the
    /// buffer changed (one undo step).
    pub fn resolve(
        &mut self,
        index: usize,
        resolution: Resolution,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(parsed) = self.fresh(cx) else {
            return false;
        };
        let Some(region) = parsed.parse.regions.get(index) else {
            return false;
        };
        let edit = conflicts::resolve(&parsed.text, region, resolution);
        self.apply(vec![edit], cx)
    }

    /// Resolves every conflict the same way (one undo step).
    pub fn resolve_all(&mut self, resolution: Resolution, cx: &mut Context<Self>) -> bool {
        let Some(parsed) = self.fresh(cx) else {
            return false;
        };
        let edits = conflicts::resolve_all(&parsed.text, &parsed.parse.regions, resolution);
        self.apply(edits, cx)
    }

    fn apply(&mut self, edits: Vec<lector_buffer::TextEdit>, cx: &mut Context<Self>) -> bool {
        if edits.is_empty() {
            return false;
        }
        let changed = self.model.update(cx, |m, cx| m.apply_edits(edits, cx));
        if changed {
            self.parse_now(cx);
        }
        changed
    }

    /// Index of the conflict at or after `row` (the one to act on from the cursor).
    pub fn conflict_for_row(&self, row: u32) -> Option<usize> {
        let parse = &self.parsed.as_ref()?.parse;
        parse
            .region_at_row(row)
            .or_else(|| parse.next_after(row, true))
    }

    /// Moves the caret of the code view to the next / previous conflict.
    /// Returns false when there is none.
    pub fn go_to_adjacent(&mut self, forward: bool, cx: &mut Context<Self>) -> bool {
        let Some(cv) = self.code_view.upgrade() else {
            return false;
        };
        let Some(parsed) = self.fresh(cx) else {
            return false;
        };
        let row = cv.read(cx).selection().head.row;
        let target = if forward {
            parsed.parse.next_after(row, true)
        } else {
            parsed.parse.prev_before(row, true)
        };
        let Some(ix) = target else {
            return false;
        };
        let start = parsed.parse.regions[ix].start_row;
        cv.update(cx, |v, cx| {
            v.set_cursor(lector_ui::code_view::Pos::new(start, 0), cx);
            v.scroll_to_row(start.saturating_sub(3), false, cx);
        });
        true
    }

    /// Opens a "Current ↔ Incoming" comparison tab of conflict `index`.
    pub fn compare(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(parsed) = self.fresh(cx) else {
            return;
        };
        let Some(region) = parsed.parse.regions.get(index) else {
            return;
        };
        let name = file_name(self.model.read(cx).path());
        let title = format!("{name}: Current ↔ Incoming ({})", index + 1);
        let ours = region.side(&parsed.text, Side::Ours).to_string();
        let theirs = region.side(&parsed.text, Side::Theirs).to_string();
        let item = lector_ui::editor::compare::CompareItem::new(title, None, &ours, &theirs, cx);
        lector_ui::editor::compare::open_compare_item(item, window, cx);
    }

    /// Pushes backgrounds and action bars to the code view.
    pub fn update_layers(&mut self, cx: &mut Context<Self>) {
        let Some(cv) = self.code_view.upgrade() else {
            return;
        };
        let parse = self.parsed.as_ref().map(|p| p.parse.clone());
        let Some(parse) = parse.filter(|p| !p.regions.is_empty()) else {
            cv.update(cx, |v, cx| {
                v.set_row_backgrounds(LAYER, Vec::new(), cx);
                v.set_row_widgets(LAYER, Vec::new(), cx);
            });
            return;
        };
        let colors = ConflictColors::get(cx);
        let backgrounds = region_backgrounds(&parse, colors);
        let editable = self.model.read(cx).is_editable();
        let widgets = if editable {
            let me = cx.entity().downgrade();
            let path = self.model.read(cx).path().to_path_buf();
            parse
                .regions
                .iter()
                .enumerate()
                .map(|(ix, r)| {
                    let me = me.clone();
                    let store = self.store.clone();
                    let path = path.clone();
                    let in_merge = self.in_merge_editor;
                    RowWidget {
                        row: r.start_row,
                        anchor: WidgetAnchor::AfterText,
                        render: Rc::new(move |_window, cx| {
                            action_bar(ix, &me, &store, &path, in_merge, cx)
                        }),
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        cv.update(cx, |v, cx| {
            v.set_row_backgrounds(LAYER, backgrounds, cx);
            v.set_row_widgets(LAYER, widgets, cx);
        });
    }
}

fn parse_rope(rope: &lector_text::Rope) -> ParsedText {
    let text: Arc<str> = rope.text_for_range(0..rope.len()).into();
    let parse = if conflicts::has_conflict_markers(&text) {
        conflicts::parse_conflicts(&text)
    } else {
        ConflictParse::default()
    };
    ParsedText {
        version: rope.version(),
        text,
        parse: Arc::new(parse),
    }
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Tints for every region: marker lines stronger, contents lighter.
pub fn region_backgrounds(parse: &ConflictParse, colors: ConflictColors) -> Vec<RowBackground> {
    let mut out = Vec::new();
    for r in &parse.regions {
        out.push(RowBackground {
            rows: r.start_row..r.start_row + 1,
            color: colors.ours.opacity(0.26),
            edge: Some(colors.ours),
        });
        out.push(RowBackground {
            rows: r.ours_rows(),
            color: colors.ours.opacity(0.11),
            edge: Some(colors.ours),
        });
        if let (Some(b), Some(rows)) = (r.base_row, r.base_rows()) {
            out.push(RowBackground {
                rows: b..b + 1,
                color: colors.base.opacity(0.22),
                edge: Some(colors.base),
            });
            out.push(RowBackground {
                rows,
                color: colors.base.opacity(0.09),
                edge: Some(colors.base),
            });
        }
        out.push(RowBackground {
            rows: r.separator_row..r.separator_row + 1,
            color: colors.base.opacity(0.14),
            edge: None,
        });
        out.push(RowBackground {
            rows: r.theirs_rows(),
            color: colors.theirs.opacity(0.11),
            edge: Some(colors.theirs),
        });
        out.push(RowBackground {
            rows: r.end_row..r.end_row + 1,
            color: colors.theirs.opacity(0.26),
            edge: Some(colors.theirs),
        });
    }
    out
}

/// "Accept Current | Accept Incoming | Accept Both | Compare | Open Merge Editor".
fn action_bar(
    ix: usize,
    me: &WeakEntity<FileConflicts>,
    store: &Entity<GitStore>,
    path: &std::path::Path,
    in_merge_editor: bool,
    cx: &mut App,
) -> AnyElement {
    let theme = cx.theme().clone();
    let ui = &theme.ui;
    let link = |id: &'static str, label: &'static str, tip: &'static str| {
        div()
            .id((id, ix))
            .px(px(3.))
            .rounded(px(3.))
            .text_color(ui.text_muted.hsla())
            .cursor_pointer()
            .hover(|s| s.text_color(ui.text.hsla()).bg(ui.hover.hsla()))
            .tab_index(0)
            .focus({
                let ring = ui.focus_ring.hsla();
                move |s| lector_ui::a11y::focus_ring_style(s, ring)
            })
            .tooltip(Tooltip::text(tip))
            .child(label)
    };
    let sep = || div().text_color(ui.text_faint.hsla()).child("|");
    let resolve = |res: Resolution| {
        let me = me.clone();
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            cx.stop_propagation();
            let _ = me.update(cx, |c, cx| c.resolve(ix, res, cx));
        }
    };
    let compare = {
        let me = me.clone();
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            cx.stop_propagation();
            let _ = me.update(cx, |c, cx| c.compare(ix, window, cx));
        }
    };
    let mut bar = h_flex()
        .id(("conflict-actions", ix))
        .occlude()
        .gap(px(2.))
        .text_size(metrics::text_xs())
        .child(
            link(
                "accept-current",
                "Accept Current",
                "Keep the current change (HEAD)",
            )
            .on_click(resolve(Resolution::Ours)),
        )
        .child(sep())
        .child(
            link(
                "accept-incoming",
                "Accept Incoming",
                "Take the incoming change",
            )
            .on_click(resolve(Resolution::Theirs)),
        )
        .child(sep())
        .child(
            link("accept-both", "Accept Both", "Current first, then incoming")
                .on_click(resolve(Resolution::OursThenTheirs)),
        )
        .child(sep())
        .child(link("compare", "Compare", "Compare current with incoming").on_click(compare));
    if !in_merge_editor {
        let store = store.clone();
        let path: PathBuf = path.to_path_buf();
        bar = bar.child(sep()).child(
            link(
                "open-merge-editor",
                "Open Merge Editor",
                "Resolve in the three-way merge editor",
            )
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                crate::merge_editor::open_merge_editor_for_path(&store, &path, window, cx);
            }),
        );
    }
    bar.into_any_element()
}

/// Attaches [`FileConflicts`] to every file view of a workspace (scanned when
/// the active item changes); merge editors attach their own.
pub struct InlineConflicts {
    store: Entity<GitStore>,
    attached: HashMap<EntityId, (WeakEntity<FileView>, Entity<FileConflicts>)>,
}

impl InlineConflicts {
    /// The tracker for `store`'s workspace.
    pub fn new(store: Entity<GitStore>) -> Self {
        InlineConflicts {
            store,
            attached: HashMap::new(),
        }
    }

    /// Registers a tracker created elsewhere (the merge editor's result view).
    pub fn register(&mut self, file_view: &Entity<FileView>, tracker: Entity<FileConflicts>) {
        self.attached
            .insert(file_view.entity_id(), (file_view.downgrade(), tracker));
    }

    /// The tracker of `file_view`, if attached.
    pub fn tracker(&self, file_view: &Entity<FileView>) -> Option<Entity<FileConflicts>> {
        self.attached
            .get(&file_view.entity_id())
            .map(|(_, t)| t.clone())
    }

    /// Attaches trackers to new file views and drops those of closed ones.
    pub fn scan(&mut self, workspace: &Workspace, cx: &mut Context<Self>) {
        self.attached.retain(|_, (fv, _)| fv.upgrade().is_some());
        for fv in workspace.file_views(cx) {
            if self.attached.contains_key(&fv.entity_id()) {
                continue;
            }
            let store = self.store.clone();
            let tracker = cx.new(|cx| FileConflicts::new(store, &fv, false, cx));
            self.attached
                .insert(fv.entity_id(), (fv.downgrade(), tracker));
        }
    }
}

/// Tooltip-less label used in banners ("2 conflicts").
pub fn count_label(n: usize, what: &str) -> SharedString {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" }).into()
}
