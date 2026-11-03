//! lector-ui: the GPUI application shell, views and widget kit for Lector.
//!
//! * [`theme`]: the active theme as a GPUI global ([`ActiveTheme`]).
//! * [`components`]: reusable widgets (buttons, menus, picker, tree, tabs...).
//! * [`code_view`]: the virtualized read-only code view element.
//! * [`views`]: the workspace shell (title bar, sidebar, panes, status bar).
//!
//! Data comes in through traits ([`code_view::CodeSource`],
//! [`components::tree_view::TreeDelegate`], [`components::picker::PickerDelegate`])
//! so the core crates can be plugged in by the integration layer.
#![forbid(unsafe_code)]

pub use gpui;

pub mod actions;
pub mod assets;
pub mod code_view;
pub mod components;
pub mod editor;
pub mod fuzzy;
pub mod icons;
pub mod keymap;
pub mod markdown;
pub mod menus;
pub mod os;
pub mod settings;
pub mod startup;
pub mod theme;
pub mod views;

#[cfg(test)]
mod tests;

pub use theme::{ActiveTheme, ColorExt, ThemeMode, ThemeState};

use std::path::PathBuf;

use gpui::{
    point, px, size, App, AppContext, Bounds, Focusable, TitlebarOptions, WindowBounds,
    WindowDecorations, WindowOptions,
};

/// Initializes fonts, theme, settings (`lector-config`), app-level action
/// handlers, key bindings and the menu bar. Call once at startup, before
/// opening windows. `settings` are the effective settings for the first
/// window (defaults ← user file ← project file).
pub fn init(
    cx: &mut App,
    registry: lector_theme::ThemeRegistry,
    mode: ThemeMode,
    settings: (lector_config::Settings, Vec<String>),
) {
    assets::load_fonts(cx);
    if matches!(mode, ThemeMode::Named(_)) {
        cx.set_global(settings::CliTheme);
    }
    ThemeState::init(cx, registry, mode, lector_theme::Appearance::Dark);
    cx.set_global(code_view::EditorSettings::default());
    settings::apply_settings(cx, settings.0, settings.1);
    settings::bind_effective_keymap(cx);
    cx.on_action(|_: &actions::workspace::Quit, cx| cx.quit());
    cx.on_action(|_: &actions::workspace::About, _| {
        tracing::info!("Lector {}", env!("CARGO_PKG_VERSION"));
    });
    cx.set_menus(menus::app_menus());
}

/// What to open in a new window.
#[derive(Clone, Debug, Default)]
pub struct OpenRequest {
    /// Project root folder.
    pub root: PathBuf,
    /// Optional file to open, with 1-based line and column.
    pub file: Option<(PathBuf, Option<u32>, Option<u32>)>,
    /// Fixed window bounds at the origin (headless screenshots).
    pub fixed_size: Option<(f32, f32)>,
    /// The project, if already opened (e.g. on a background thread while
    /// the app starts); opened on demand otherwise.
    pub project: Option<lector_workspace::Project>,
}

/// Opens a workspace window for `request`.
pub fn open_workspace_window(cx: &mut App, request: OpenRequest) -> anyhow::Result<()> {
    let project = match request.project.clone() {
        Some(p) => p,
        None => lector_workspace::Project::open(
            &request.root,
            settings::AppSettings::get(cx).as_ref().clone(),
        )?,
    };
    let bounds = match request.fixed_size {
        Some((w, h)) => Bounds::new(point(px(0.), px(0.)), size(px(w), px(h))),
        None => Bounds::centered(None, size(px(1440.), px(900.)), cx),
    };
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some("Lector".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(12.), px(12.))),
        }),
        window_min_size: Some(size(px(640.), px(400.))),
        window_decorations: Some(WindowDecorations::Client),
        app_id: Some("io.github.meetdhruva.Lector".into()),
        ..Default::default()
    };
    cx.open_window(options, move |window, cx| {
        let workspace = cx.new(|cx| views::Workspace::new(project, window, cx));
        let restored = workspace.update(cx, |ws, cx| ws.restore_session(window, cx));
        if let Some((file, line, col)) = request.file.clone() {
            workspace.update(cx, |ws, cx| {
                ws.open_path_at(file, line, col, window, cx);
            });
        } else if !restored {
            let focus = workspace
                .read(cx)
                .sidebar()
                .read(cx)
                .file_tree()
                .focus_handle(cx);
            window.focus(&focus);
        }
        cx.new(|_| views::WorkspaceRoot { workspace })
    })?;
    cx.activate(true);
    Ok(())
}
