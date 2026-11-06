//! Find References results: a pane item listing locations grouped by file
//! with highlighted preview lines; clicking opens the location.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use crate::scale::px;
use gpui::{
    div, prelude::*, AnyElement, AnyView, App, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, HighlightStyle, SharedString, StyledText, Task, Window,
};
use lector_workspace::{Document, Project, Target, TargetSource};

use crate::components::context_menu::{ContextMenu, ContextMenuState};
use crate::components::list::{virtual_list, VirtualListState};
use crate::components::{h_flex, metrics, v_flex};
use crate::icons::{FileIcon, Icon, IconName, IconSize};
use crate::theme::{ActiveTheme, ColorExt};
use crate::views::pane::{PaneItem, TabIcon};

/// Events emitted by [`ReferencesView`].
#[derive(Clone, Debug, PartialEq)]
pub enum ReferencesEvent {
    /// Open a location.
    Open {
        /// Absolute path.
        path: PathBuf,
        /// Row.
        row: u32,
        /// Byte columns within the row.
        cols: Range<usize>,
    },
}

struct Group {
    name: SharedString,
    dir: SharedString,
    targets: Vec<Target>,
    collapsed: bool,
}

#[derive(Clone, Copy)]
enum Row {
    File(usize),
    Ref(usize, usize),
}

/// The references results view.
pub struct ReferencesView {
    word: SharedString,
    project: Project,
    groups: Vec<Group>,
    rows: Arc<Vec<Row>>,
    loading: bool,
    source: Option<TargetSource>,
    selected: Option<usize>,
    list: VirtualListState,
    focus_handle: FocusHandle,
    task: Option<Task<()>>,
    context_menu: ContextMenuState,
}

impl EventEmitter<ReferencesEvent> for ReferencesView {}

impl Focusable for ReferencesView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ReferencesView {
    /// Empty view for references to `word`.
    pub fn new(word: String, project: Project, cx: &mut Context<Self>) -> Self {
        ReferencesView {
            word: word.into(),
            project,
            groups: Vec::new(),
            rows: Arc::new(Vec::new()),
            loading: true,
            source: None,
            selected: None,
            list: VirtualListState::new(),
            focus_handle: cx.focus_handle().tab_stop(true),
            task: None,
            context_menu: ContextMenuState::default(),
        }
    }

    /// Finds references to the symbol at `offset` of `doc` (background).
    pub fn load(&mut self, doc: Document, offset: usize, cx: &mut Context<Self>) {
        let project = self.project.clone();
        self.loading = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            let targets = cx
                .background_executor()
                .spawn(async move {
                    lector_workspace::symbols::references(&project, &doc, offset)
                        .unwrap_or_default()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.set_targets(targets);
                cx.notify();
            });
        }));
    }

    fn set_targets(&mut self, targets: Vec<Target>) {
        self.loading = false;
        self.source = targets.first().map(|t| t.source);
        let root = self.project.root().to_path_buf();
        let mut by_path: BTreeMap<PathBuf, Vec<Target>> = BTreeMap::new();
        for t in targets {
            by_path.entry(t.path.clone()).or_default().push(t);
        }
        self.groups = by_path
            .into_iter()
            .map(|(path, mut targets)| {
                targets.sort_by_key(|t| (t.range.start.row, t.range.start.column));
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let (dir, name) = match rel.rfind('/') {
                    Some(i) => (rel[..i].to_string(), rel[i + 1..].to_string()),
                    None => (String::new(), rel.clone()),
                };
                Group {
                    name: name.into(),
                    dir: dir.into(),
                    targets,
                    collapsed: false,
                }
            })
            .collect();
        self.rebuild();
    }

    fn rebuild(&mut self) {
        let mut rows = Vec::new();
        for (gi, g) in self.groups.iter().enumerate() {
            rows.push(Row::File(gi));
            if !g.collapsed {
                rows.extend((0..g.targets.len()).map(|ti| Row::Ref(gi, ti)));
            }
        }
        self.rows = Arc::new(rows);
    }

    fn total(&self) -> usize {
        self.groups.iter().map(|g| g.targets.len()).sum()
    }

    fn deploy_menu(
        &mut self,
        ix: usize,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (path, preview) = match self.rows.get(ix).copied() {
            Some(Row::File(gi)) => match self.groups[gi].targets.first() {
                Some(t) => (t.path.clone(), None),
                None => return,
            },
            Some(Row::Ref(gi, ti)) => {
                let t = &self.groups[gi].targets[ti];
                (t.path.clone(), Some(t.preview.trim().to_string()))
            }
            None => return,
        };
        let rel = self
            .project
            .relative_path(&path)
            .unwrap_or_else(|| path.display().to_string());
        let weak = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let open = weak.clone();
            let abs = path.display().to_string();
            let rel = rel.clone();
            let mut menu = menu.icon_entry("Open", IconName::File, move |_, cx| {
                let _ = open.update(cx, |v, cx| v.activate(ix, cx));
            });
            if let Some(line) = preview.clone() {
                menu = menu.icon_entry("Copy Line", IconName::Copy, move |_, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(line.clone()))
                });
            }
            menu.separator()
                .icon_entry("Copy Path", IconName::Link, move |_, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(abs.clone()))
                })
                .entry("Copy Relative Path", move |_, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(rel.clone()))
                })
        });
        self.context_menu
            .deploy(menu, position, window, cx, |this: &mut Self| {
                &mut this.context_menu
            });
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.rows.len();
        if count == 0 {
            return;
        }
        let next = match self.selected {
            Some(ix) => (ix as isize + delta).clamp(0, count as isize - 1) as usize,
            None if delta > 0 => 0,
            None => count - 1,
        };
        self.selected = Some(next);
        self.list.reveal(next);
        cx.notify();
    }

    fn activate(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected = Some(ix);
        match self.rows.get(ix).copied() {
            Some(Row::File(gi)) => {
                self.groups[gi].collapsed = !self.groups[gi].collapsed;
                self.rebuild();
            }
            Some(Row::Ref(gi, ti)) => {
                let t = &self.groups[gi].targets[ti];
                let end = if t.range.end.row == t.range.start.row {
                    t.range.end.column as usize
                } else {
                    t.range.start.column as usize
                };
                cx.emit(ReferencesEvent::Open {
                    path: t.path.clone(),
                    row: t.range.start.row,
                    cols: t.range.start.column as usize..end,
                });
            }
            None => {}
        }
        cx.notify();
    }
}

/// Byte range of the reference within the (possibly trimmed) preview line:
/// previews may have had leading whitespace removed, so locate `word` at the
/// occurrence closest to (and not after) the reference column.
fn preview_range(preview: &str, word: &str, col: usize, len: usize) -> Option<Range<usize>> {
    if !word.is_empty() {
        let best = preview
            .match_indices(word)
            .map(|(i, _)| i)
            .filter(|i| *i <= col)
            .max();
        if let Some(i) = best {
            return Some(i..i + word.len());
        }
    }
    (col + len <= preview.len()).then_some(col..col + len)
}

fn ref_row(t: &Target, word: &str, selected: bool, cx: &App) -> AnyElement {
    let ui = &cx.theme().ui;
    let preview = t.preview.trim_end();
    let trimmed = preview.trim_start();
    let lead = preview.len() - trimmed.len();
    let text: String = trimmed.chars().take(300).collect();
    let len = if t.range.end.row == t.range.start.row {
        (t.range.end.column - t.range.start.column) as usize
    } else {
        0
    };
    let col = (t.range.start.column as usize).saturating_sub(lead);
    let (start, end) = preview_range(&text, word, col, len)
        .map(|r| (r.start, r.end))
        .unwrap_or((0, 0));
    let highlights = if start < end
        && end <= text.len()
        && text.is_char_boundary(start)
        && text.is_char_boundary(end)
    {
        vec![(
            start..end,
            HighlightStyle {
                background_color: Some(ui.word_highlight.hsla()),
                font_weight: Some(FontWeight::SEMIBOLD),
                color: Some(ui.text.hsla()),
                ..Default::default()
            },
        )]
    } else {
        Vec::new()
    };
    h_flex()
        .h(px(24.))
        .pl(px(34.))
        .pr(px(12.))
        .gap(px(10.))
        .when(selected, |el| el.bg(ui.active.hsla()))
        .when(!selected, |el| el.hover(|s| s.bg(ui.hover.hsla())))
        .child(
            div()
                .flex_none()
                .min_w(px(40.))
                .text_size(metrics::text_xs())
                .text_color(ui.text_faint.hsla())
                .child(format!(
                    "{}:{}",
                    t.range.start.row + 1,
                    t.range.start.column + 1
                )),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .font_family(crate::code_view::EditorSettings::get(cx).font_family)
                .text_size(metrics::text_sm())
                .text_color(ui.text_muted.hsla())
                .child(StyledText::new(text).with_highlights(highlights)),
        )
        .into_any_element()
}

impl Render for ReferencesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let total = self.total();
        let summary: SharedString = if self.loading {
            format!("Finding references to {}…", self.word).into()
        } else {
            let via = match self.source {
                Some(TargetSource::Lsp) => " · language server",
                Some(TargetSource::Tags) => " · symbol index",
                Some(TargetSource::Text) => " · text search",
                None => "",
            };
            format!(
                "{total} reference{} to {} in {} file{}{via}",
                if total == 1 { "" } else { "s" },
                self.word,
                self.groups.len(),
                if self.groups.len() == 1 { "" } else { "s" }
            )
            .into()
        };
        let weak = cx.entity().downgrade();
        let count = self.rows.len();
        let body = virtual_list("references", count, &self.list, cx, move |range, _, cx| {
            let Some(view) = weak.upgrade() else {
                return Vec::new();
            };
            let v = view.read(cx);
            let rows = v.rows.clone();
            range
                .map(|ix| {
                    let selected = v.selected == Some(ix);
                    let el = match rows[ix] {
                        Row::File(gi) => {
                            let g = &v.groups[gi];
                            let ui = &cx.theme().ui;
                            h_flex()
                                .h(px(24.))
                                .px(px(12.))
                                .gap(px(6.))
                                .when(selected, |el| el.bg(ui.active.hsla()))
                                .child(
                                    Icon::new(if g.collapsed {
                                        IconName::ChevronRight
                                    } else {
                                        IconName::ChevronDown
                                    })
                                    .size(IconSize::XSmall)
                                    .color(ui.text_faint.hsla()),
                                )
                                .child(FileIcon::file(g.name.clone()).size(px(15.)))
                                .child(div().text_color(ui.text.hsla()).child(g.name.clone()))
                                .child(
                                    div()
                                        .text_size(metrics::text_xs())
                                        .text_color(ui.text_faint.hsla())
                                        .child(g.dir.clone()),
                                )
                                .child(
                                    div()
                                        .px(px(6.))
                                        .rounded(px(8.))
                                        .bg(ui.hover.hsla())
                                        .text_size(metrics::text_xs())
                                        .text_color(ui.text_muted.hsla())
                                        .child(format!("{}", g.targets.len())),
                                )
                                .into_any_element()
                        }
                        Row::Ref(gi, ti) => {
                            ref_row(&v.groups[gi].targets[ti], &v.word, selected, cx)
                        }
                    };
                    let weak = view.downgrade();
                    let menu_weak = view.downgrade();
                    div()
                        .id(("ref-row", ix))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            let _ = weak.update(cx, |v, cx| v.activate(ix, cx));
                        })
                        .on_mouse_down(gpui::MouseButton::Right, move |event, window, cx| {
                            let position = event.position;
                            let _ = menu_weak
                                .update(cx, |v, cx| v.deploy_menu(ix, position, window, cx));
                        })
                        .child(el)
                        .into_any_element()
                })
                .collect()
        });
        v_flex()
            .size_full()
            .key_context(crate::keymap::contexts::LIST_PANEL)
            .track_focus(&self.focus_handle)
            .on_action(
                cx.listener(|this, _: &crate::actions::picker::SelectNext, _, cx| {
                    this.move_selection(1, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::picker::SelectPrev, _, cx| {
                    this.move_selection(-1, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::picker::Confirm, _, cx| {
                    if let Some(ix) = this.selected {
                        this.activate(ix, cx);
                    }
                }),
            )
            .on_action(cx.listener(
                |this, _: &crate::actions::workspace::ShowContextMenu, window, cx| {
                    match this.selected {
                        Some(ix) => {
                            let position = this.list.row_anchor(ix, px(24.));
                            this.deploy_menu(ix, position, window, cx)
                        }
                        None => cx.propagate(),
                    }
                },
            ))
            .bg(ui.background.hsla())
            .child(
                h_flex()
                    .h(px(34.))
                    .flex_none()
                    .px(px(14.))
                    .gap(px(8.))
                    .border_b_1()
                    .border_color(ui.border_variant.hsla())
                    .child(
                        Icon::new(IconName::Link)
                            .size(IconSize::Small)
                            .color(ui.text_muted.hsla()),
                    )
                    .child(
                        div()
                            .text_size(metrics::text())
                            .text_color(ui.text.hsla())
                            .child(summary),
                    ),
            )
            .child(div().flex_1().min_h_0().pt(px(4.)).child(body))
            .children(self.context_menu.render())
    }
}

/// Pane item wrapper for [`ReferencesView`].
pub struct ReferencesItem {
    view: Entity<ReferencesView>,
    title: SharedString,
}

impl ReferencesItem {
    /// Item for `view`.
    pub fn new(view: Entity<ReferencesView>, word: String) -> Self {
        ReferencesItem {
            view,
            title: format!("References: {word}").into(),
        }
    }
}

impl PaneItem for ReferencesItem {
    fn title(&self, _: &App) -> SharedString {
        self.title.clone()
    }

    fn icon(&self, _: &App) -> TabIcon {
        TabIcon::Ui(IconName::Link)
    }

    fn path(&self, _: &App) -> Option<PathBuf> {
        None
    }

    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }

    fn tab_kind(&self, _: &App) -> Option<lector_config::TabKind> {
        None
    }

    fn clone_item(&self, _window: &mut Window, _cx: &mut App) -> Option<Box<dyn PaneItem>> {
        Some(Box::new(ReferencesItem {
            view: self.view.clone(),
            title: self.title.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::preview_range;

    #[test]
    fn locates_word_in_trimmed_preview() {
        let p = "let s = Rc::new(DocumentSource::new(x));";
        assert_eq!(preview_range(p, "DocumentSource", 24, 14), Some(16..30));
        assert_eq!(preview_range(p, "nope", 3, 2), Some(3..5));
        assert_eq!(preview_range("ab", "", 1, 9), None);
    }
}
