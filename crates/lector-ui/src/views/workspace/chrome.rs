//! Window chrome and "ease of access" commands of the [`Workspace`]: the
//! title bar (menu bar, palette trigger, toolbar, caption buttons), the
//! welcome view, the no-folder state, window actions, and the file/folder/
//! recent/help commands. A child module of `workspace` so it can reach its
//! state.

use std::path::{Path, PathBuf};

use gpui::{
    div, prelude::*, px, AnyElement, App, Corner, Div, Entity, Focusable, MouseButton,
    MouseMoveEvent, Pixels, Point, Window, WindowControlArea,
};
use lector_workspace::Project;

use super::{Workspace, WorkspaceRoot};
use crate::actions::workspace::*;
use crate::components::picker::Picker;
use crate::components::{binding_text_for_action, h_flex, metrics, IconButton, Tooltip};
use crate::icons::{Icon, IconName, IconSize};
use crate::theme::{ActiveTheme, ColorExt};
use crate::views::about::{issue_url, AboutDialog, DOCUMENTATION_URL};
use crate::views::app_menu::{build_settings_menu, AppMenuBar, AppMenuEvent};
use crate::views::keybindings::{KeybindingsItem, KeybindingsView, KEYBINDINGS_ITEM_KEY};
use crate::views::lsp_status::LspStatusPopover;
use crate::views::recent::RecentProjectsDelegate;
use crate::views::welcome::{logo, WelcomeEvent, WelcomeItem, WelcomeView, WELCOME_ITEM_KEY};
use crate::views::window_controls::{self, caption_style, CaptionStyle};

/// Root folder of the placeholder project used by a window with no folder
/// open (Lector's own data dir; empty, never scanned or watched).
pub fn no_folder_root() -> PathBuf {
    let dir = lector_config::paths::data_dir().join("no-folder");
    match std::fs::create_dir_all(&dir) {
        Ok(()) => dir,
        Err(err) => {
            tracing::warn!("cannot create {}: {err}", dir.display());
            let tmp = std::env::temp_dir().join("lector-no-folder");
            let _ = std::fs::create_dir_all(&tmp);
            tmp
        }
    }
}

/// Opens the placeholder project for a no-folder window.
pub fn no_folder_project(cx: &App) -> anyhow::Result<Project> {
    let settings = crate::settings::AppSettings::get(cx).as_ref().clone();
    Ok(Project::open_detached(&no_folder_root(), settings)?)
}

impl Workspace {
    /// A workspace with no folder open: the welcome view, the menus and the
    /// command palette; Open Folder/File/Recent replace it in the window.
    pub fn new_empty(window: &mut Window, cx: &mut Context<Self>) -> anyhow::Result<Self> {
        let project = no_folder_project(cx)?;
        Ok(Self::new_with(project, false, window, cx))
    }

    /// [`Self::new_empty`] over an already opened placeholder project
    /// ([`no_folder_project`]).
    pub fn new_empty_with(project: Project, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_with(project, false, window, cx)
    }

    /// Whether a folder is open (false for a fresh window or after Close Folder).
    pub fn has_folder(&self) -> bool {
        self.has_folder
    }

    /// The in-window menu bar (Linux and Windows).
    pub fn app_menu(&self) -> &Entity<AppMenuBar> {
        &self.app_menu
    }

    pub(super) fn subscribe_app_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let menu = self.app_menu.clone();
        let sub = cx.subscribe_in(
            &menu,
            window,
            |this, _, event: &AppMenuEvent, window, cx| match event {
                AppMenuEvent::OpenRecent(path) => {
                    this.open_folder_in_window(path.clone(), None, window, cx)
                }
            },
        );
        self._subscriptions.push(sub);
    }

    /// The welcome view of this window (created on first use).
    pub fn welcome_view(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<WelcomeView> {
        if let Some(view) = &self.welcome {
            return view.clone();
        }
        let root = self.has_folder.then(|| self.root.clone());
        let view = cx.new(|cx| WelcomeView::new(root, cx));
        let sub = cx.subscribe_in(
            &view,
            window,
            |this, _, event: &WelcomeEvent, window, cx| match event {
                WelcomeEvent::OpenRecent(path) => {
                    this.open_folder_in_window(path.clone(), None, window, cx)
                }
            },
        );
        self._subscriptions.push(sub);
        self.welcome = Some(view.clone());
        view
    }

    /// Whether the center shows nothing but one empty pane (the welcome view
    /// is drawn there instead).
    pub(super) fn center_is_empty(&self, cx: &App) -> bool {
        match &self.center {
            super::PaneNode::Pane(p) => p.read(cx).item_count() == 0,
            _ => false,
        }
    }

    /// Focuses the welcome view when the center is empty (keeps key bindings
    /// working after the last tab closes, and in a new window).
    pub fn focus_welcome_if_empty(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.center_is_empty(cx) {
            let view = self.welcome_view(window, cx);
            view.update(cx, |v, cx| v.refresh(cx));
            let focus = view.focus_handle(cx);
            window.focus(&focus);
        }
    }

    // Opening folders and files.

    /// Replaces this window's workspace with `root` (optionally opening
    /// `file`), saving the current session first. Asks Save All / Don't Save /
    /// Cancel first when tabs with unsaved changes would close.
    pub fn open_folder_in_window(
        &mut self,
        root: PathBuf,
        file: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_folder && file.is_none() && paths_equal(&root, self.project.root()) {
            return;
        }
        if self.dirty_file_views(cx).is_empty() {
            self.replace_with_folder(root, file, window, cx);
            return;
        }
        let confirm = self.confirm_close(window, cx);
        cx.spawn_in(window, async move |this, cx| {
            if confirm.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.replace_with_folder(root, file, window, cx)
                });
            }
        })
        .detach();
    }

    fn replace_with_folder(
        &mut self,
        root: PathBuf,
        file: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (settings, _) = lector_config::load_settings_for(Some(&root));
        let project = match Project::open(&root, settings) {
            Ok(p) => p,
            Err(err) => {
                self.show_toast(format!("Cannot open {}: {err}", root.display()), cx);
                return;
            }
        };
        if let Err(err) = lector_config::RecentProjects::new().record(project.root()) {
            tracing::warn!("cannot record recent project: {err}");
        }
        self.save_session_now(cx);
        // Spawned, not deferred: building a workspace registers window
        // callbacks, which the X11 backend forbids while it dispatches input.
        cx.spawn_in(window, async move |_, cx| {
            let _ = cx.update(move |window, cx| {
                let ws = cx.new(|cx| Workspace::new(project, window, cx));
                let restored = ws.update(cx, |ws, cx| ws.restore_session(window, cx));
                ws.update(cx, |ws, cx| {
                    if let Some(file) = file {
                        ws.open_path(file, false, window, cx);
                    } else if !restored {
                        ws.focus_welcome_if_empty(window, cx);
                    }
                });
                window.replace_root(cx, |_, _| WorkspaceRoot { workspace: ws });
            });
        })
        .detach();
    }

    /// Opens `path`: in a tab when it belongs to this project, else in a
    /// window rooted at its repository (or parent folder) — this one when no
    /// folder is open, a new one otherwise.
    pub fn open_file_anywhere(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_folder && self.project.relative_path(&path).is_some() {
            self.open_path(path, false, window, cx);
            return;
        }
        let root = crate::root_for_file(&path);
        if self.has_folder {
            if let Err(err) = crate::open_folder_window(cx, root, Some(path)) {
                self.show_toast(format!("Cannot open window: {err}"), cx);
            }
        } else {
            self.open_folder_in_window(root, Some(path), window, cx);
        }
    }

    fn open_file_dialog(&mut self, _: &OpenFile, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    for path in paths {
                        this.open_file_anywhere(path, window, cx);
                    }
                });
            }
        })
        .detach();
    }

    pub(super) fn open_folder(
        &mut self,
        _: &OpenFolder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open Folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                if let Some(root) = paths.into_iter().next() {
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.open_folder_in_window(root, None, window, cx)
                    });
                }
            }
        })
        .detach();
    }

    fn open_recent(&mut self, _: &OpenRecent, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.has_folder.then(|| self.root.clone());
        let weak = cx.entity().downgrade();
        let on_open: crate::views::recent::OpenRecentHandler =
            std::rc::Rc::new(move |path, new_window, window, cx| {
                if new_window {
                    if let Err(err) = crate::open_folder_window(cx, path, None) {
                        tracing::warn!("cannot open window: {err}");
                    }
                } else {
                    let _ = weak.update(cx, |ws, cx| {
                        ws.open_folder_in_window(path, None, window, cx)
                    });
                }
            });
        self.toggle_modal(window, cx, move |window, cx| {
            Picker::new(RecentProjectsDelegate::new(current, on_open), window, cx)
        });
    }

    fn close_folder(&mut self, _: &CloseFolder, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_folder {
            return;
        }
        let confirm = self.confirm_close(window, cx);
        cx.spawn_in(window, async move |this, cx| {
            if confirm.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.save_session_now(cx);
                    this.replace_with_empty(window, cx);
                });
            }
        })
        .detach();
    }

    fn replace_with_empty(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let project = match no_folder_project(cx) {
            Ok(p) => p,
            Err(err) => {
                self.show_toast(format!("Cannot close folder: {err}"), cx);
                return;
            }
        };
        cx.spawn_in(window, async move |_, cx| {
            let _ = cx.update(move |window, cx| {
                let ws = cx.new(|cx| Workspace::new_with(project, false, window, cx));
                ws.update(cx, |ws, cx| ws.focus_welcome_if_empty(window, cx));
                window.replace_root(cx, |_, _| WorkspaceRoot { workspace: ws });
            });
        })
        .detach();
    }

    fn close_all_tabs(&mut self, _: &CloseAllTabs, window: &mut Window, cx: &mut Context<Self>) {
        // One unsaved-changes prompt per pane (Save All / Don't Save / Cancel).
        for pane in self.center.panes() {
            pane.update(cx, |p, cx| p.close_all(window, cx));
        }
        cx.notify();
    }

    fn show_welcome(&mut self, _: &ShowWelcome, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.welcome_view(window, cx);
        view.update(cx, |v, cx| v.refresh(cx));
        if self.center_is_empty(cx) {
            let focus = view.focus_handle(cx);
            window.focus(&focus);
            return;
        }
        let pane = self.active_pane.clone();
        if let Some(ix) = pane.read(cx).index_for_key(WELCOME_ITEM_KEY, cx) {
            pane.update(cx, |p, cx| p.activate(ix, true, window, cx));
        } else {
            self.add_item(Box::new(WelcomeItem::new(view)), false, window, cx);
        }
    }

    fn show_keybindings(
        &mut self,
        _: &ShowKeybindings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.active_pane.clone();
        if let Some(ix) = pane.read(cx).index_for_key(KEYBINDINGS_ITEM_KEY, cx) {
            pane.update(cx, |p, cx| p.activate(ix, true, window, cx));
            return;
        }
        let view = cx.new(|cx| KeybindingsView::new(window, cx));
        self.add_item(Box::new(KeybindingsItem::new(view)), false, window, cx);
    }

    fn open_log_file(&mut self, _: &OpenLogFile, window: &mut Window, cx: &mut Context<Self>) {
        let path = lector_config::paths::log_file();
        if path.is_file() {
            self.open_path(path, false, window, cx);
        } else {
            self.show_toast(format!("No log file yet ({})", path.display()), cx);
        }
    }

    fn about(&mut self, _: &About, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_modal(window, cx, |_, cx| AboutDialog::new(cx));
    }

    fn close_window(&mut self, _: &CloseWindow, window: &mut Window, cx: &mut Context<Self>) {
        let confirm = self.confirm_close(window, cx);
        cx.spawn_in(window, async move |this, cx| {
            if confirm.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.save_session_now(cx);
                    window.remove_window();
                });
            }
        })
        .detach();
    }

    fn show_app_menu(&mut self, _: &ShowAppMenu, window: &mut Window, cx: &mut Context<Self>) {
        if cfg!(target_os = "macos") {
            return;
        }
        self.app_menu.update(cx, |menu, cx| {
            if menu.is_open() {
                menu.close(window, cx);
            } else {
                menu.open_menu(0, true, window, cx);
            }
        });
    }

    /// Registers the handlers of this module on the workspace element.
    pub(super) fn register_chrome_actions(&mut self, div: Div, cx: &mut Context<Self>) -> Div {
        div.on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(Self::open_file_dialog))
            .on_action(cx.listener(Self::open_recent))
            .on_action(cx.listener(Self::close_folder))
            .on_action(cx.listener(Self::close_all_tabs))
            .on_action(cx.listener(Self::show_welcome))
            .on_action(cx.listener(Self::show_keybindings))
            .on_action(cx.listener(Self::open_log_file))
            .on_action(cx.listener(Self::about))
            .on_action(cx.listener(Self::close_window))
            .on_action(cx.listener(Self::show_app_menu))
            .on_action(
                cx.listener(|_, _: &OpenDocumentation, _, cx| cx.open_url(DOCUMENTATION_URL)),
            )
            .on_action(cx.listener(|_, _: &ReportIssue, _, cx| cx.open_url(&issue_url())))
            .on_action(cx.listener(|_, _: &Minimize, window, _| window.minimize_window()))
            .on_action(cx.listener(|this, _: &ToggleMaximize, window, cx| {
                if !window_controls::toggle_maximize(window) {
                    this.show_toast("Use the restore button in the title bar to restore", cx);
                }
            }))
            .on_action(cx.listener(|_, _: &ToggleFullScreen, window, _| window.toggle_fullscreen()))
    }

    // Menus and popovers.

    fn deploy_settings_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let context = self.focus_handle.clone();
        let menu = build_settings_menu(context, window, cx);
        self.chrome_menu
            .deploy(menu, position, window, cx, |this: &mut Self| {
                &mut this.chrome_menu
            });
        cx.notify();
    }

    pub(super) fn toggle_lsp_popover(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.lsp_popover.is_open() {
            self.lsp_popover.close();
            cx.notify();
            return;
        }
        let enabled = crate::settings::AppSettings::get(cx).lsp.enabled;
        let servers = self.project.lsp().map(|l| l.status()).unwrap_or_default();
        let capabilities: Vec<_> = self
            .project
            .lsp()
            .map(|lsp| {
                servers
                    .iter()
                    .filter_map(|s| {
                        lsp.server_capabilities(&s.name)
                            .map(|c| (s.name.clone(), c))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let view = cx.new(|cx| {
            let mut popover = LspStatusPopover::new(servers, enabled, cx);
            popover.set_capabilities(capabilities);
            popover
        });
        self.lsp_popover.show(
            view,
            position,
            Corner::BottomRight,
            window,
            cx,
            |this: &mut Self, _, cx| {
                this.lsp_popover.close();
                cx.notify();
            },
        );
        cx.notify();
    }

    // Rendering.

    pub(super) fn render_title_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let caption = caption_style(window);
        let mac = cfg!(target_os = "macos");
        let windows = cfg!(target_os = "windows");
        let linux_like = !mac && !windows;
        let width = window.viewport_size().width;
        let compact_palette = width < px(980.);
        if !mac {
            let compact_menu = width < px(1060.);
            self.app_menu
                .update(cx, |m, cx| m.set_compact(compact_menu, cx));
        }
        let palette_key = binding_text_for_action(&ToggleCommandPalette, cx).unwrap_or_default();
        let weak = cx.entity().downgrade();
        let controls = window_controls::window_controls(window, cx);

        let left = h_flex()
            .flex_1()
            .min_w_0()
            .gap(px(6.))
            .when(!mac, |el| {
                el.child(div().pr(px(2.)).child(logo(22., cx)))
                    .child(self.app_menu.clone())
            })
            .when(self.has_folder, |el| {
                el.when(!mac, |el| {
                    el.child(div().w(px(1.)).h(px(16.)).mx(px(4.)).bg(ui.border.hsla()))
                })
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(ui.title_bar_fg.hsla())
                        .child(self.name.clone()),
                )
                .when_some(self.branch.clone(), |el, branch| {
                    el.child(
                        h_flex()
                            .id("branch-pill")
                            .occlude()
                            .flex_none()
                            .gap(px(4.))
                            .h(px(22.))
                            .px(px(8.))
                            .rounded(px(11.))
                            .bg(ui.hover.hsla())
                            .border_1()
                            .border_color(ui.border_variant.hsla())
                            .text_size(metrics::TEXT_SM)
                            .text_color(ui.text_muted.hsla())
                            .cursor_pointer()
                            .hover(|s| s.bg(ui.active.hsla()))
                            .tooltip(Tooltip::for_action("Branches", &ShowBranches))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(ShowBranches), cx)
                            })
                            .child(
                                Icon::new(IconName::GitBranch)
                                    .size(IconSize::XSmall)
                                    .color(ui.text_muted.hsla()),
                            )
                            .child(branch),
                    )
                })
            })
            .when(mac && !self.has_folder, |el| {
                el.child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(ui.title_bar_fg.hsla())
                        .child("Lector"),
                )
            });

        let palette: AnyElement = if compact_palette {
            IconButton::new("palette-trigger", IconName::Search)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::for_action(
                    "Command Palette",
                    &ToggleCommandPalette,
                ))
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(ToggleCommandPalette), cx)
                })
                .into_any_element()
        } else {
            h_flex()
                .id("palette-trigger")
                .occlude()
                .flex_shrink()
                .w(px(420.))
                .min_w(px(220.))
                .h(px(26.))
                .px(px(10.))
                .gap(px(8.))
                .rounded(px(6.))
                .bg(ui.input_bg.hsla())
                .border_1()
                .border_color(ui.border.hsla())
                .text_size(metrics::TEXT_SM)
                .text_color(ui.text_faint.hsla())
                .cursor_pointer()
                .hover(|s| s.border_color(ui.text_faint.hsla()))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(ToggleCommandPalette), cx)
                })
                .child(
                    Icon::new(IconName::Search)
                        .size(IconSize::XSmall)
                        .color(ui.text_faint.hsla()),
                )
                .child(div().flex_1().overflow_hidden().whitespace_nowrap().child(
                    if self.has_folder {
                        "Search files, symbols, commands…"
                    } else {
                        "Search commands…"
                    },
                ))
                .child(div().text_size(metrics::TEXT_XS).child(palette_key))
                .into_any_element()
        };

        let toolbar = h_flex()
            .id("title-toolbar")
            .occlude()
            .flex_none()
            .gap(px(2.))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .when(self.has_folder, |el| {
                el.child(
                    IconButton::new("toggle-sidebar", IconName::PanelLeft)
                        .icon_size(IconSize::Small)
                        .selected(self.sidebar_visible)
                        .tooltip(Tooltip::for_action("Toggle Sidebar", &ToggleSidebar))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(ToggleSidebar), cx)
                        }),
                )
                .child(
                    IconButton::new("split", IconName::Columns)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::for_action("Split Right", &SplitRight))
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(SplitRight), cx)),
                )
            })
            .child(
                IconButton::new(
                    "theme",
                    if cx.theme().appearance.is_dark() {
                        IconName::Sun
                    } else {
                        IconName::Moon
                    },
                )
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::for_action("Toggle Light/Dark", &ToggleAppearance))
                .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleAppearance), cx)),
            )
            .child(
                IconButton::new("title-settings", IconName::Settings)
                    .icon_size(IconSize::Small)
                    .selected(self.chrome_menu.is_open())
                    .tooltip(Tooltip::text("Settings and Help"))
                    .on_click(move |e, window, cx| {
                        let position = e.position();
                        let _ =
                            weak.update(cx, |ws, cx| ws.deploy_settings_menu(position, window, cx));
                    }),
            );

        h_flex()
            .id("title-bar")
            .h(metrics::TITLE_BAR_HEIGHT)
            .flex_none()
            .w_full()
            .gap(px(8.))
            .pl(px(if mac { 80. } else { 8. }))
            .pr(px(if caption == CaptionStyle::Windows {
                0.
            } else {
                8.
            }))
            .bg(ui.title_bar_bg.hsla())
            .border_b_1()
            .border_color(ui.border.hsla())
            .text_size(metrics::TEXT)
            .when(windows, |el| {
                el.window_control_area(WindowControlArea::Drag)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &gpui::MouseDownEvent, window, _| {
                    if e.click_count >= 2 {
                        this.title_drag_pending = false;
                        if mac {
                            window.titlebar_double_click();
                        } else if linux_like {
                            window_controls::toggle_maximize(window);
                        }
                    } else {
                        this.title_drag_pending = linux_like;
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, window, _| {
                if this.title_drag_pending && e.pressed_button == Some(MouseButton::Left) {
                    this.title_drag_pending = false;
                    window.start_window_move();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.title_drag_pending = false),
            )
            .when(linux_like, |el| {
                el.on_mouse_down(MouseButton::Right, |e, window, _| {
                    window.show_window_menu(e.position)
                })
            })
            .child(left)
            .child(palette)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .justify_end()
                    .child(toolbar)
                    .children(controls),
            )
    }
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}
