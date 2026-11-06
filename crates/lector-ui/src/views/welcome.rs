//! The welcome view (`workspace::ShowWelcome`): shown in the center of a
//! window without open tabs and as a tab on request. Logo and version, the
//! main entry points as buttons, recent projects, and a few tips with the live
//! key bindings.

use std::path::{Path, PathBuf};

use gpui::{
    div, prelude::*, px, Action, AnyView, App, EventEmitter, FocusHandle, Focusable, FontWeight,
    SharedString, Window,
};

use super::app_menu::tildify;
use super::pane::{PaneItem, TabIcon};
use crate::actions::workspace::*;
use crate::components::{binding_text_for_action, h_flex, metrics, v_flex, IconButton, Tooltip};
use crate::icons::{Icon, IconName, IconSize};
use crate::theme::{ActiveTheme, ColorExt};

/// Pane item key of the welcome tab.
pub const WELCOME_ITEM_KEY: &str = "lector:welcome";

/// Events emitted by the [`WelcomeView`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WelcomeEvent {
    /// Open this recent project in the window.
    OpenRecent(PathBuf),
}

/// One recent project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentEntry {
    /// Project root.
    pub path: PathBuf,
    /// Folder name.
    pub name: SharedString,
    /// `~/…` display path.
    pub display: SharedString,
    /// Whether the folder still exists.
    pub exists: bool,
}

impl RecentEntry {
    /// Entry for `path` (checks that it exists).
    pub fn new(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        RecentEntry {
            display: tildify(&path).into(),
            name: name.into(),
            exists: path.is_dir(),
            path,
        }
    }
}

/// Recent projects, newest first.
pub fn recent_entries() -> Vec<RecentEntry> {
    crate::commands_bridge::recent_projects()
        .list()
        .into_iter()
        .map(RecentEntry::new)
        .collect()
}

/// The welcome view.
pub struct WelcomeView {
    recent: Vec<RecentEntry>,
    current_root: Option<PathBuf>,
    focus_handle: FocusHandle,
}

impl EventEmitter<WelcomeEvent> for WelcomeView {}

impl Focusable for WelcomeView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl WelcomeView {
    /// Welcome view; `current_root` (the open folder, if any) is left out of
    /// the recent list.
    pub fn new(current_root: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        WelcomeView {
            recent: recent_entries(),
            current_root,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Re-reads the recent projects list.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.recent = recent_entries();
        cx.notify();
    }

    /// Recent projects shown (tests).
    pub fn recent(&self) -> impl Iterator<Item = &RecentEntry> {
        let current = self.current_root.clone();
        self.recent
            .iter()
            .filter(move |r| Some(&r.path) != current.as_ref())
    }

    fn remove_recent(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Err(err) = crate::commands_bridge::recent_projects().remove(path) {
            tracing::warn!("cannot update recent projects: {err}");
        }
        self.refresh(cx);
    }

    fn render_action_row(
        &self,
        id: &'static str,
        icon: IconName,
        label: &'static str,
        action: Box<dyn Action>,
        cx: &App,
    ) -> impl IntoElement {
        let ui = &cx.theme().ui;
        let binding = binding_text_for_action(action.as_ref(), cx);
        let hover = ui.hover.hsla();
        let active = ui.active.hsla();
        h_flex()
            .id(id)
            .h(px(32.))
            .px(px(10.))
            .gap(px(10.))
            .rounded(metrics::RADIUS)
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .active(move |s| s.bg(active))
            .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
            .child(
                Icon::new(icon)
                    .size(IconSize::Medium)
                    .color(ui.text_accent.hsla()),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(metrics::TEXT)
                    .text_color(ui.text.hsla())
                    .child(label),
            )
            .when_some(binding, |el, kb| el.child(keycap(kb, cx)))
    }

    fn render_recent(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let entries: Vec<RecentEntry> = self.recent().take(8).cloned().collect();
        let mut list = v_flex().gap(px(1.));
        if entries.is_empty() {
            list = list.child(
                v_flex()
                    .px(px(10.))
                    .py(px(12.))
                    .gap(px(4.))
                    .text_size(metrics::TEXT_SM)
                    .text_color(ui.text_faint.hsla())
                    .child("No recent projects yet.")
                    .child("Folders you open appear here."),
            );
        }
        for (ix, entry) in entries.into_iter().enumerate() {
            let path = entry.path.clone();
            let remove_path = entry.path.clone();
            let exists = entry.exists;
            let hover = ui.hover.hsla();
            let row = h_flex()
                .id(("welcome-recent", ix))
                .group("welcome-recent-row")
                .h(px(40.))
                .pl(px(10.))
                .pr(px(4.))
                .gap(px(10.))
                .rounded(metrics::RADIUS)
                .hover(move |s| s.bg(hover))
                .when(exists, |el| {
                    el.cursor_pointer()
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(WelcomeEvent::OpenRecent(path.clone()))
                        }))
                })
                .when(!exists, |el| {
                    el.tooltip(Tooltip::text("This folder no longer exists"))
                })
                .child(
                    Icon::new(if exists {
                        IconName::Folder
                    } else {
                        IconName::FolderX
                    })
                    .size(IconSize::Medium)
                    .color(if exists {
                        ui.text_muted.hsla()
                    } else {
                        ui.text_faint.hsla()
                    }),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_size(metrics::TEXT)
                                .text_color(if exists {
                                    ui.text.hsla()
                                } else {
                                    ui.text_faint.hsla()
                                })
                                .when(!exists, |el| el.line_through())
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(entry.name.clone()),
                        )
                        .child(
                            div()
                                .text_size(metrics::TEXT_XS)
                                .text_color(ui.text_faint.hsla())
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(entry.display.clone()),
                        ),
                )
                .child(
                    IconButton::new(("welcome-recent-remove", ix), IconName::Close)
                        .icon_size(IconSize::XSmall)
                        .color(ui.text_faint.hsla())
                        .tooltip(Tooltip::text("Remove from Recent Projects"))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.remove_recent(&remove_path, cx)),
                        ),
                );
            list = list.child(row);
        }
        list
    }
}

/// A small key-cap label for a binding.
pub fn keycap(text: SharedString, cx: &App) -> impl IntoElement {
    let ui = &cx.theme().ui;
    div()
        .flex_none()
        .px(px(6.))
        .py(px(1.))
        .rounded(px(4.))
        .border_1()
        .border_color(ui.border_variant.hsla())
        .bg(ui.surface.hsla())
        .text_size(metrics::TEXT_XS)
        .text_color(ui.text_muted.hsla())
        .child(text)
}

fn section_title(title: &'static str, cx: &App) -> impl IntoElement {
    let ui = &cx.theme().ui;
    div()
        .px(px(10.))
        .pb(px(6.))
        .text_size(metrics::TEXT_XS)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(ui.text_faint.hsla())
        .child(title.to_uppercase())
}

static LOGO_SMALL: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/app-icon/png/lector-48.png"
));
static LOGO_LARGE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/app-icon/png/lector-256.png"
));

/// The Lector app icon at `size` logical pixels (embedded PNGs; the icon has
/// its own transparent margin).
pub fn logo(size: f32, _cx: &App) -> impl IntoElement {
    use std::sync::{Arc, OnceLock};
    static SMALL: OnceLock<Arc<gpui::Image>> = OnceLock::new();
    static LARGE: OnceLock<Arc<gpui::Image>> = OnceLock::new();
    let (cell, bytes) = if size <= 24. {
        (&SMALL, LOGO_SMALL)
    } else {
        (&LARGE, LOGO_LARGE)
    };
    let image = cell
        .get_or_init(|| {
            Arc::new(gpui::Image::from_bytes(
                gpui::ImageFormat::Png,
                bytes.to_vec(),
            ))
        })
        .clone();
    gpui::img(gpui::ImageSource::Image(image))
        .size(px(size))
        .flex_none()
}

impl Render for WelcomeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let tips: [(&str, Box<dyn Action>); 4] = [
            ("Jump to any file", Box::new(ToggleFileFinder)),
            ("Run any command", Box::new(ToggleCommandPalette)),
            ("Search the whole project", Box::new(ToggleProjectSearch)),
            ("Review uncommitted changes", Box::new(ShowChanges)),
        ];
        let tip_rows: Vec<_> = tips
            .into_iter()
            .filter_map(|(label, action)| {
                let kb = binding_text_for_action(action.as_ref(), cx)?;
                Some(
                    h_flex()
                        .gap(px(8.))
                        .justify_between()
                        .text_size(metrics::TEXT_SM)
                        .child(div().text_color(ui.text_muted.hsla()).child(label))
                        .child(keycap(kb, cx)),
                )
            })
            .collect();
        let card = |cx: &App| {
            let ui = &cx.theme().ui;
            v_flex()
                .p(px(8.))
                .rounded(metrics::RADIUS_LG)
                .border_1()
                .border_color(ui.border_variant.hsla())
                .bg(ui.surface.hsla())
        };
        let start = v_flex()
            .child(section_title("Start", cx))
            .child(self.render_action_row(
                "welcome-open-folder",
                IconName::FolderOpen,
                "Open Folder…",
                Box::new(OpenFolder),
                cx,
            ))
            .child(self.render_action_row(
                "welcome-open-file",
                IconName::File,
                "Open File…",
                Box::new(OpenFile),
                cx,
            ))
            .child(self.render_action_row(
                "welcome-open-recent",
                IconName::History,
                "Open Recent…",
                Box::new(OpenRecent),
                cx,
            ))
            .child(self.render_action_row(
                "welcome-new-window",
                IconName::AppWindow,
                "New Window",
                Box::new(NewWindow),
                cx,
            ));
        let configure = v_flex()
            .child(section_title("Learn and configure", cx))
            .child(self.render_action_row(
                "welcome-palette",
                IconName::Command,
                "Command Palette",
                Box::new(ToggleCommandPalette),
                cx,
            ))
            .child(self.render_action_row(
                "welcome-settings",
                IconName::Settings,
                "Settings",
                Box::new(OpenSettings),
                cx,
            ))
            .child(self.render_action_row(
                "welcome-keybindings",
                IconName::Keyboard,
                "Keyboard Shortcuts",
                Box::new(ShowKeybindings),
                cx,
            ))
            .child(self.render_action_row(
                "welcome-theme",
                IconName::Palette,
                "Select Theme…",
                Box::new(SelectTheme),
                cx,
            ))
            .child(self.render_action_row(
                "welcome-docs",
                IconName::BookOpen,
                "Documentation",
                Box::new(OpenDocumentation),
                cx,
            ));
        div()
            .id("welcome")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .bg(ui.background.hsla())
            .child(
                v_flex()
                    .w_full()
                    .items_center()
                    .py(px(56.))
                    .px(px(32.))
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(px(760.))
                            .gap(px(28.))
                            .child(
                                h_flex().gap(px(16.)).child(logo(60., cx)).child(
                                    v_flex()
                                        .gap(px(2.))
                                        .child(
                                            div()
                                                .text_size(px(26.))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(ui.text.hsla())
                                                .child("Lector"),
                                        )
                                        .child(
                                            div()
                                                .text_size(metrics::TEXT)
                                                .text_color(ui.text_muted.hsla())
                                                .child(format!(
                                                    "Version {} · Read, review and edit code",
                                                    env!("CARGO_PKG_VERSION")
                                                )),
                                        ),
                                ),
                            )
                            .child(
                                h_flex()
                                    .items_start()
                                    .gap(px(20.))
                                    .child(
                                        v_flex()
                                            .w(px(300.))
                                            .flex_none()
                                            .gap(px(18.))
                                            .child(card(cx).child(start))
                                            .child(card(cx).child(configure)),
                                    )
                                    .child(
                                        v_flex()
                                            .flex_1()
                                            .min_w_0()
                                            .gap(px(18.))
                                            .child(
                                                card(cx)
                                                    .child(section_title("Recent projects", cx))
                                                    .child(self.render_recent(cx)),
                                            )
                                            .when(!tip_rows.is_empty(), |el| {
                                                el.child(
                                                    card(cx)
                                                        .child(section_title("Tips", cx))
                                                        .child(
                                                            v_flex()
                                                                .px(px(10.))
                                                                .pb(px(4.))
                                                                .gap(px(10.))
                                                                .children(tip_rows),
                                                        ),
                                                )
                                            }),
                                    ),
                            ),
                    ),
            )
    }
}

/// The welcome view as a tab.
pub struct WelcomeItem {
    view: gpui::Entity<WelcomeView>,
}

impl WelcomeItem {
    /// Wraps `view`.
    pub fn new(view: gpui::Entity<WelcomeView>) -> Self {
        WelcomeItem { view }
    }
}

impl PaneItem for WelcomeItem {
    fn title(&self, _: &App) -> SharedString {
        "Welcome".into()
    }

    fn icon(&self, _: &App) -> TabIcon {
        TabIcon::Ui(IconName::Sparkles)
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
        Some(Box::new(WelcomeItem {
            view: self.view.clone(),
        }))
    }

    fn item_key(&self, _: &App) -> Option<SharedString> {
        Some(WELCOME_ITEM_KEY.into())
    }
}
