//! The Problems tab (`workspace::ShowProblems`, also the status bar counts):
//! diagnostics of open files grouped by file; clicking one jumps there.

use std::path::PathBuf;

use crate::scale::px;
use gpui::{
    div, prelude::*, AnyView, App, Context, Entity, FocusHandle, Focusable, SharedString,
    WeakEntity, Window,
};

use super::diagnostics::{severity_icon, DiagnosticEntry, DiagnosticsStore};
use crate::actions::workspace::ShowProblems;
use crate::components::{h_flex, metrics, v_flex};
use crate::icons::{FileIcon, Icon, IconName, IconSize};
use crate::theme::{ActiveTheme, ColorExt};
use crate::views::pane::{PaneItem, TabIcon};
use crate::views::Workspace;

/// Item key of the Problems tab.
pub const PROBLEMS_KEY: &str = "lector:problems";

/// A problem with its 0-based position (for display and jumping).
#[derive(Clone, Debug, PartialEq)]
struct Row {
    entry: DiagnosticEntry,
    line: u32,
    col: u32,
}

/// The Problems list view.
pub struct ProblemsView {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    _subscription: gpui::Subscription,
}

impl ProblemsView {
    /// A view following the global [`DiagnosticsStore`].
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let store = DiagnosticsStore::global(cx);
        ProblemsView {
            workspace,
            focus_handle: cx.focus_handle(),
            _subscription: cx.observe(&store, |_, _, cx| cx.notify()),
        }
    }

    fn groups(&self, cx: &App) -> Vec<(PathBuf, Vec<Row>)> {
        let Some(store) = DiagnosticsStore::try_global(cx) else {
            return Vec::new();
        };
        store
            .read(cx)
            .all()
            .into_iter()
            .map(|(path, entries)| {
                let text = crate::editor::document_for_path(&path, cx)
                    .and_then(|m| m.read(cx).buffer().map(|b| b.read(cx).text().clone()));
                let rows = entries
                    .into_iter()
                    .map(|entry| {
                        let (line, col) = text
                            .as_ref()
                            .map(|t| {
                                let p = t.offset_to_point(entry.range.start.min(t.len()));
                                let col = t
                                    .text_for_range(t.line_range(p.row).start..t.point_to_offset(p))
                                    .chars()
                                    .count() as u32;
                                (p.row, col)
                            })
                            .unwrap_or((0, 0));
                        Row { entry, line, col }
                    })
                    .collect();
                (path, rows)
            })
            .collect()
    }
}

impl Focusable for ProblemsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ProblemsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let groups = self.groups(cx);
        let root = self
            .workspace
            .upgrade()
            .map(|w| w.read(cx).root().to_path_buf());
        let mut children: Vec<gpui::AnyElement> = Vec::new();
        if groups.is_empty() {
            children.push(
                div()
                    .p(px(16.))
                    .text_color(ui.text_muted.hsla())
                    .child("No problems in open files.")
                    .into_any_element(),
            );
        }
        for (gi, (path, rows)) in groups.into_iter().enumerate() {
            let name: SharedString = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
                .into();
            let dir: SharedString = path
                .parent()
                .map(|p| match &root {
                    Some(r) => p.strip_prefix(r).unwrap_or(p).display().to_string(),
                    None => p.display().to_string(),
                })
                .unwrap_or_default()
                .into();
            children.push(
                h_flex()
                    .id(("problems-file", gi))
                    .h(px(26.))
                    .px(px(10.))
                    .gap(px(6.))
                    .child(FileIcon::file(name.clone()))
                    .child(div().text_color(ui.text.hsla()).child(name))
                    .child(
                        div()
                            .text_size(metrics::text_sm())
                            .text_color(ui.text_faint.hsla())
                            .child(dir),
                    )
                    .child(
                        div()
                            .px(px(6.))
                            .rounded(px(8.))
                            .bg(ui.hover.hsla())
                            .text_size(metrics::text_xs())
                            .text_color(ui.text_muted.hsla())
                            .child(rows.len().to_string()),
                    )
                    .into_any_element(),
            );
            for (ri, row) in rows.into_iter().enumerate() {
                let color = match row.entry.severity {
                    lector_lsp::DiagnosticSeverity::Error => ui.error.hsla(),
                    lector_lsp::DiagnosticSeverity::Warning => ui.warning.hsla(),
                    lector_lsp::DiagnosticSeverity::Information => ui.info.hsla(),
                    lector_lsp::DiagnosticSeverity::Hint => ui.hint.hsla(),
                };
                let message: SharedString = row
                    .entry
                    .message
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string()
                    .into();
                let origin = row.entry.origin().unwrap_or_default();
                let weak = self.workspace.clone();
                let target = path.clone();
                let (line, col) = (row.line, row.col);
                children.push(
                    h_flex()
                        .id(("problem", gi * 100_000 + ri))
                        .h(px(24.))
                        .pl(px(30.))
                        .pr(px(10.))
                        .gap(px(6.))
                        .cursor_pointer()
                        .hover(|s| s.bg(ui.hover.hsla()))
                        .child(
                            Icon::new(severity_icon(row.entry.severity))
                                .size(IconSize::Small)
                                .color(color),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_color(ui.text.hsla())
                                .child(message),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(metrics::text_sm())
                                .text_color(ui.text_faint.hsla())
                                .child(format!("{origin}  [Ln {}, Col {}]", line + 1, col + 1)),
                        )
                        .tab_index(0)
                        .focus(crate::a11y::ring_focus(cx))
                        .on_click(move |_, window, cx| {
                            let target = target.clone();
                            let _ = weak.update(cx, |ws, cx| {
                                ws.open_path_at(target, Some(line + 1), Some(col + 1), window, cx);
                            });
                        })
                        .into_any_element(),
                );
            }
        }
        v_flex()
            .id("problems")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .bg(ui.background.hsla())
            .text_size(metrics::text())
            .py(px(4.))
            .children(children)
    }
}

/// The Problems tab.
pub struct ProblemsItem {
    view: Entity<ProblemsView>,
}

impl PaneItem for ProblemsItem {
    fn title(&self, cx: &App) -> SharedString {
        let (errors, warnings) = DiagnosticsStore::try_global(cx)
            .map(|s| s.read(cx).counts())
            .unwrap_or_default();
        if errors + warnings == 0 {
            "Problems".into()
        } else {
            format!("Problems ({})", errors + warnings).into()
        }
    }

    fn icon(&self, _: &App) -> TabIcon {
        TabIcon::Ui(IconName::CircleX)
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

    fn clone_item(&self, _: &mut Window, _: &mut App) -> Option<Box<dyn PaneItem>> {
        Some(Box::new(ProblemsItem {
            view: self.view.clone(),
        }))
    }

    fn item_key(&self, _: &App) -> Option<SharedString> {
        Some(PROBLEMS_KEY.into())
    }
}

pub(crate) fn show_problems(
    ws: &mut Workspace,
    _: &ShowProblems,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let pane = ws.active_pane().clone();
    if let Some(ix) = pane.read(cx).index_for_key(PROBLEMS_KEY, cx) {
        pane.update(cx, |p, cx| p.activate(ix, true, window, cx));
        return;
    }
    let weak = cx.entity().downgrade();
    let view = cx.new(|cx| ProblemsView::new(weak, cx));
    ws.add_item(Box::new(ProblemsItem { view }), false, window, cx);
}

/// Status bar item "⊗ 2 ⚠ 5" (errors / warnings in open files); clicking it
/// opens the Problems tab. `None` without a folder or language servers.
pub fn status_item(ws: &Workspace, cx: &App) -> Option<gpui::AnyElement> {
    if !ws.has_folder() || ws.project().lsp().is_none() {
        return None;
    }
    let (errors, warnings) = DiagnosticsStore::try_global(cx)
        .map(|s| s.read(cx).counts())
        .unwrap_or_default();
    let ui = cx.theme().ui.clone();
    let fg = ui.status_bar_fg.hsla();
    let count = |icon: IconName, n: usize, color: gpui::Hsla| {
        h_flex()
            .gap(px(3.))
            .child(
                Icon::new(icon)
                    .size(IconSize::XSmall)
                    .color(if n > 0 { color } else { fg }),
            )
            .child(n.to_string())
    };
    Some(
        h_flex()
            .id("status-problems")
            .h(px(20.))
            .px(px(6.))
            .gap(px(8.))
            .rounded(metrics::radius())
            .cursor_pointer()
            .text_color(fg)
            .hover(|s| s.bg(ui.hover.hsla()))
            .child(count(IconName::CircleX, errors, ui.error.hsla()))
            .child(count(IconName::AlertTriangle, warnings, ui.warning.hsla()))
            .tooltip(crate::components::Tooltip::for_action(
                "Problems",
                &ShowProblems,
            ))
            .on_click(|_, window, cx| window.dispatch_action(Box::new(ShowProblems), cx))
            .into_any_element(),
    )
}
