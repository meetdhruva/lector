//! Rendering of the [`Workspace`]: the status bar, the bottom panel, the
//! window resize edges and the root layout (`Render`).

use gpui::{
    div, prelude::*, AnyElement, App, Axis, CursorStyle, Decorations, DragMoveEvent, MouseButton,
    ResizeEdge, SharedString, Window,
};

use super::Workspace;
use crate::actions::{editor, workspace::*};
use crate::code_view::EditorSettings;
use crate::components::split::{clamp_width, panel_resize_handle, DraggedPanelEdge};
use crate::components::{h_flex, metrics, v_flex, Button, Tooltip};
use crate::icons::IconName;
use crate::keymap::contexts;
use crate::scale::px;
use crate::theme::{ActiveTheme, ColorExt, ThemeState};
use crate::views::sidebar::SidebarPanel;

impl Workspace {
    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.entity().downgrade();
        let ui = cx.theme().ui.clone();
        let settings = EditorSettings::get(cx);
        let view = self.active_code_view(cx);
        let meta = self.active_file_view(cx).and_then(|v| v.read(cx).meta(cx));
        let (cursor, language, selection) = match &view {
            Some(view) => {
                let v = view.read(cx);
                let (line, col) = v.cursor_line_col(settings.tab_size);
                let sel = v.selection();
                let selection = if sel.is_empty() {
                    None
                } else {
                    let (start, end) = sel.range();
                    let lines =
                        end.row - start.row + u32::from(end.col > 0 || end.row == start.row);
                    Some(if end.row - start.row < 2000 {
                        let chars = v.selected_text().chars().count();
                        if lines > 1 {
                            format!("{chars} selected ({lines} lines)")
                        } else {
                            format!("{chars} selected")
                        }
                    } else {
                        format!("{lines} lines selected")
                    })
                };
                let carets = self.selection_count_suffix(cx).unwrap_or_default();
                (
                    Some(format!("Ln {line}, Col {col}{carets}")),
                    v.source().language_name(),
                    selection,
                )
            }
            None => (None, None, None),
        };
        let fg = ui.status_bar_fg.hsla();
        let warn = cx.theme().git.modified.hsla();
        let item =
            |id: &'static str, label: SharedString| Button::new(id, label).compact().color(fg);
        let indexing = self.integ.index_progress.map(|(done, total)| {
            if total == 0 {
                "Indexing symbols…".to_string()
            } else {
                format!("Indexing symbols {}%", done * 100 / total.max(1))
            }
        });
        h_flex()
            .id("status-bar")
            .track_focus(&self.status_focus)
            .relative()
            .h(metrics::status_bar_height())
            .flex_none()
            .w_full()
            .px(px(6.))
            .bg(ui.status_bar_bg.hsla())
            .border_t_1()
            .border_color(ui.border.hsla())
            .text_size(metrics::text_sm())
            .text_color(fg)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .gap(px(2.))
                    .when_some(self.branch_status_view.clone(), |el, view| el.child(view))
                    .when_some(
                        self.branch_status
                            .clone()
                            .or(self.branch.clone())
                            .filter(|_| self.branch_status_view.is_none()),
                        |el, branch| {
                        el.child(
                            item("status-branch", branch)
                                .icon(IconName::GitBranch)
                                .tooltip(Tooltip::for_action("Branches", &ShowBranches))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(ShowBranches), cx)
                                }),
                        )
                    })
                    .when(self.project.repo().is_some(), |el| el.child({
                        let changed = self.integ.git_status.len();
                        let label: SharedString = if changed == 0 {
                            "Changes".into()
                        } else {
                            format!("{changed} changed").into()
                        };
                        item("status-changes", label)
                            .icon(IconName::GitCompare)
                            .tooltip(Tooltip::for_action("Show Changes", &ShowChanges))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(ShowChanges), cx)
                            })
                    }))
                    .when_some(
                        self.review_hint.clone().filter(|h| h.new_since_last && h.uncommitted > 0),
                        |el, hint| {
                            let open = hint.open_review;
                            el.child(
                                item(
                                    "status-new-changes",
                                    // Large UI scales have no room for the long form.
                                    if crate::scale::scale() >= 1.4 {
                                        "New changes: Review".into()
                                    } else {
                                        "New changes since you last looked: Review".into()
                                    },
                                )
                                .icon(IconName::GitCompare)
                                .color(cx.theme().git.modified.hsla())
                                .tooltip(Tooltip::text(
                                    "Open the changes in review order, riskiest files first",
                                ))
                                .on_click(move |_, window, cx| {
                                    window.dispatch_action(open(), cx)
                                }),
                            )
                        },
                    )
                    .when_some(indexing, |el, text| {
                        el.child(
                            item("status-indexing", text.into())
                                .icon(IconName::Loader)
                                .tooltip(Tooltip::text(
                                    "Building the project symbol index (Go to Symbol, navigation without LSP)",
                                )),
                        )
                    }),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(2.))
                    .when(self.bottom_panel.is_some(), |el| {
                        el.child(
                            item("status-terminal", "Terminal".into())
                                .icon(IconName::Terminal)
                                .selected(self.is_bottom_panel_visible())
                                .tooltip(Tooltip::for_action(
                                    "Toggle Terminal",
                                    &crate::actions::terminal::TogglePanel,
                                ))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(
                                        Box::new(crate::actions::terminal::TogglePanel),
                                        cx,
                                    )
                                }),
                        )
                    })
                    .children(self.extra_status_views.iter().map(|(_, v)| v.clone()))
                    .children(self.render_edit_status(cx))
                    .children(crate::lsp::problems::status_item(self, cx))
                    .when_some(selection, |el, text| {
                        el.child(item("status-sel", text.into()))
                    })
                    .when_some(cursor, |el, c| {
                        el.child(
                            item("status-cursor", c.into())
                                .tooltip(Tooltip::for_action("Go to Line", &editor::GoToLine))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(editor::GoToLine), cx)
                                }),
                        )
                    })
                    .when_some(meta.clone(), |el, m| {
                        el.child(item("status-indent", m.indentation).tooltip(
                            Tooltip::with_meta(
                                "Indentation",
                                "Detected from the file (tab size from settings)",
                            ),
                        ))
                    })
                    .when_some(meta.clone(), |el, m| {
                        if m.lossy {
                            el.child(
                                item("status-encoding", format!("{} (lossy)", m.encoding).into())
                                    .color(warn)
                                    .icon(IconName::AlertTriangle)
                                    .tooltip(Tooltip::text(
                                        "Some bytes are invalid in this encoding and are shown as \u{FFFD}",
                                    )),
                            )
                        } else {
                            el.child(item("status-encoding", m.encoding).tooltip(
                                Tooltip::with_meta("Encoding", "Kept as is when the file is saved"),
                            ))
                        }
                    })
                    .when_some(meta, |el, m| {
                        let tip = if m.line_ending.as_ref() == "CRLF" {
                            "Windows line endings (\\r\\n), kept when saving"
                        } else {
                            "Unix line endings (\\n), kept when saving"
                        };
                        el.child(
                            item("status-eol", m.line_ending)
                                .tooltip(Tooltip::with_meta("Line Endings", tip)),
                        )
                    })
                    .when_some(language, |el, l| {
                        el.child(item("status-language", l).tooltip(Tooltip::with_meta(
                            "Language",
                            "Detected from the file name and contents",
                        )))
                    })
                    .when(self.has_folder, |el| {
                        let (label, tip) = self.integ.lsp_status.clone().unwrap_or_else(|| {
                            ("LSP".into(), "No language server for this file".into())
                        });
                        let popover_open = self.lsp_popover.is_open();
                        el.child(
                            item("status-lsp", label)
                                .icon(IconName::CircleDot)
                                .selected(popover_open)
                                .when(!popover_open, |b| {
                                    b.tooltip(Tooltip::with_meta(
                                        tip,
                                        "Click for all language servers",
                                    ))
                                })
                                .on_click(move |_, window, cx| {
                                    // Above the status bar, right-aligned.
                                    let size = window.viewport_size();
                                    let position = gpui::point(
                                        size.width - px(8.),
                                        size.height - metrics::status_bar_height() - px(4.),
                                    );
                                    let _ = weak.update(cx, |ws, cx| {
                                        ws.toggle_lsp_popover(position, window, cx)
                                    });
                                }),
                        )
                    }),
            )
    }

    fn render_bottom_panel(&self, cx: &App) -> Option<impl IntoElement> {
        let panel = self.bottom_panel.as_ref().filter(|p| p.visible)?;
        Some(
            div()
                .id("bottom-panel")
                .relative()
                .flex_none()
                .w_full()
                .h(panel.height * crate::scale::scale())
                .border_t_1()
                .border_color(cx.theme().ui.border.hsla())
                .child(
                    panel
                        .view
                        .clone()
                        .cached(gpui::StyleRefinement::default().size_full()),
                )
                .child(crate::components::split::panel_resize_handle_top(
                    "bottom-panel-edge",
                    cx,
                )),
        )
    }

    fn render_resize_edges(&self, window: &mut Window) -> Vec<AnyElement> {
        let Decorations::Client { .. } = window.window_decorations() else {
            return Vec::new();
        };
        if window.is_maximized() {
            return Vec::new();
        }
        let edge = |id: &'static str, e: ResizeEdge, cursor: CursorStyle| {
            div()
                .id(id)
                .absolute()
                .occlude()
                .cursor(cursor)
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    cx.stop_propagation();
                    window.start_window_resize(e);
                })
        };
        let t = px(4.);
        vec![
            edge("edge-top", ResizeEdge::Top, CursorStyle::ResizeUpDown)
                .top_0()
                .left(t)
                .right(t)
                .h(t)
                .into_any_element(),
            edge("edge-bottom", ResizeEdge::Bottom, CursorStyle::ResizeUpDown)
                .bottom_0()
                .left(t)
                .right(t)
                .h(t)
                .into_any_element(),
            edge("edge-left", ResizeEdge::Left, CursorStyle::ResizeLeftRight)
                .left_0()
                .top(t)
                .bottom(t)
                .w(t)
                .into_any_element(),
            edge(
                "edge-right",
                ResizeEdge::Right,
                CursorStyle::ResizeLeftRight,
            )
            .right_0()
            .top(t)
            .bottom(t)
            .w(t)
            .into_any_element(),
            edge(
                "edge-tl",
                ResizeEdge::TopLeft,
                CursorStyle::ResizeUpLeftDownRight,
            )
            .top_0()
            .left_0()
            .size(t)
            .into_any_element(),
            edge(
                "edge-tr",
                ResizeEdge::TopRight,
                CursorStyle::ResizeUpRightDownLeft,
            )
            .top_0()
            .right_0()
            .size(t)
            .into_any_element(),
            edge(
                "edge-bl",
                ResizeEdge::BottomLeft,
                CursorStyle::ResizeUpRightDownLeft,
            )
            .bottom_0()
            .left_0()
            .size(t)
            .into_any_element(),
            edge(
                "edge-br",
                ResizeEdge::BottomRight,
                CursorStyle::ResizeUpLeftDownRight,
            )
            .bottom_0()
            .right_0()
            .size(t)
            .into_any_element(),
        ]
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;

        let title = match self.active_path(cx) {
            Some(p) => format!(
                "{} — {}",
                p.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                self.name
            ),
            None if !self.has_folder => "Lector".to_string(),
            None => format!("{} — Lector", self.name),
        };
        let title = format!("{}{title}", self.title_dirty_marker(cx));
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }

        let sidebar_width = self.sidebar_width * crate::scale::scale();
        let center = if self.center_is_empty(cx) {
            let welcome = self.welcome_view(window, cx);
            gpui::AnyView::from(welcome)
                .cached(gpui::StyleRefinement::default().size_full())
                .into_any_element()
        } else {
            self.center.render(0, cx)
        };
        crate::scale::sync_window(window);
        let mut root = v_flex();
        let registrations = std::mem::take(&mut self.action_registrations);
        for register in &registrations {
            root = register(root, window, cx);
        }
        self.action_registrations = registrations;
        let root = self.register_chrome_actions(root, cx);
        let root = self.register_editing_actions(root, cx);
        let root = self.register_region_actions(root, cx);
        root.id("workspace")
            .key_context(contexts::WORKSPACE)
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .font_family(crate::settings::UiFont::get(cx).family)
            .text_size(px(crate::settings::UiFont::get(cx).size))
            .text_color(ui.text.hsla())
            .bg(ui.background.hsla())
            .on_action(cx.listener(Self::toggle_command_palette))
            .on_action(cx.listener(Self::toggle_file_finder))
            .on_action(cx.listener(Self::toggle_outline_picker))
            .on_action(cx.listener(Self::toggle_symbol_finder))
            .on_action(cx.listener(Self::go_to_definition))
            .on_action(cx.listener(Self::find_references))
            .on_action(cx.listener(Self::toggle_markdown_preview))
            .on_action(cx.listener(Self::reload_settings_action))
            .on_action(cx.listener(Self::copy_permalink_action))
            .on_action(cx.listener(Self::select_theme))
            .on_action(cx.listener(Self::go_to_line))
            .on_action(cx.listener(Self::find))
            .on_action(cx.listener(Self::find_replace))
            .on_action(cx.listener(Self::replace_next))
            .on_action(cx.listener(Self::replace_all))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(
                cx.listener(|this, _: &ShowFiles, w, cx| {
                    this.show_panel(SidebarPanel::Files, w, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ShowChanges, w, cx| {
                this.show_panel(SidebarPanel::Changes, w, cx)
            }))
            .on_action(cx.listener(|this, _: &ShowHistory, w, cx| {
                this.show_panel(SidebarPanel::History, w, cx)
            }))
            .on_action(cx.listener(|this, _: &ShowBranches, w, cx| {
                this.show_panel(SidebarPanel::Branches, w, cx)
            }))
            .on_action(cx.listener(Self::toggle_project_search))
            .on_action(cx.listener(|this, _: &ShowOutline, w, cx| {
                this.show_panel(SidebarPanel::Outline, w, cx)
            }))
            .on_action(
                cx.listener(|this, _: &SplitRight, w, cx| this.split(Axis::Horizontal, w, cx)),
            )
            .on_action(cx.listener(|this, _: &SplitDown, w, cx| this.split(Axis::Vertical, w, cx)))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::close_other_tabs))
            .on_action(cx.listener(Self::close_tabs_to_right))
            .on_action(cx.listener(Self::reopen_closed_tab))
            .on_action(cx.listener(|this, _: &NextTab, w, cx| {
                this.active_pane
                    .update(cx, |p, cx| p.activate_relative(1, w, cx))
            }))
            .on_action(cx.listener(|this, _: &PrevTab, w, cx| {
                this.active_pane
                    .update(cx, |p, cx| p.activate_relative(-1, w, cx))
            }))
            .on_action(cx.listener(Self::nav_back))
            .on_action(cx.listener(Self::nav_forward))
            .on_action(cx.listener(Self::reveal_active_file))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom(Some(1.), cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom(Some(-1.), cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _, cx| this.zoom(None, cx)))
            .on_action(
                cx.listener(|_, _: &ToggleAppearance, _, cx| ThemeState::toggle_appearance(cx)),
            )
            .on_action(cx.listener(|_, _: &ToggleColorblindDiff, _, cx| {
                let on = cx.global::<ThemeState>().colorblind_diff();
                ThemeState::set_colorblind_diff(cx, !on);
            }))
            .on_action(cx.listener(|this, _: &editor::CopyPath, _, cx| this.copy_path(false, cx)))
            .on_action(
                cx.listener(|this, _: &editor::CopyRelativePath, _, cx| this.copy_path(true, cx)),
            )
            .on_action(cx.listener(Self::open_keymap_file))
            .on_action(cx.listener(Self::open_settings_file))
            .on_drag_move(
                cx.listener(|this, e: &DragMoveEvent<DraggedPanelEdge>, window, cx| {
                    if e.drag(cx).id == "bottom-panel-edge" {
                        if let Some(panel) = this.bottom_panel.as_mut() {
                            let h = e.bounds.bottom()
                                - metrics::status_bar_height()
                                - e.event.position.y;
                            panel.height =
                                clamp_width(h, px(80.), window.viewport_size().height * 0.8)
                                    / crate::scale::scale();
                            cx.notify();
                        }
                    }
                    if e.drag(cx).id == "sidebar-edge" {
                        let w = e.event.position.x - e.bounds.left();
                        this.sidebar_width =
                            clamp_width(w - px(46.), px(180.), window.viewport_size().width * 0.6)
                                / crate::scale::scale();
                        cx.notify();
                    }
                }),
            )
            .child(self.render_title_bar(window, cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .when(self.sidebar_visible, |el| {
                        el.child(
                            div()
                                .relative()
                                .h_full()
                                .flex_none()
                                .w(sidebar_width + px(46.))
                                .border_r_1()
                                .border_color(ui.border.hsla())
                                .child(
                                    gpui::AnyView::from(self.sidebar.clone())
                                        .cached(gpui::StyleRefinement::default().size_full()),
                                )
                                .child(panel_resize_handle("sidebar-edge", cx)),
                        )
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(div().flex_1().min_h_0().w_full().child(center))
                            .children(self.render_bottom_panel(cx)),
                    ),
            )
            .child(self.render_status_bar(cx))
            .children(self.render_toast(cx))
            .children(self.render_action_toast(cx))
            .children(self.render_lsp_banner(cx))
            .children(self.chrome_menu.render())
            .children(self.lsp_popover.render())
            .child(self.modal_layer.clone())
            .children(self.render_resize_edges(window))
    }
}
