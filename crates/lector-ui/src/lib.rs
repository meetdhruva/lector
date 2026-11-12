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

pub mod a11y;
pub mod actions;
pub mod assets;
pub mod code_view;
pub mod commands_bridge;
pub mod components;
pub mod editor;
pub mod fuzzy;
pub mod icons;
pub mod keymap;
pub mod lsp;
pub mod markdown;
pub mod menus;
pub mod os;
pub mod scale;
pub mod settings;
pub mod startup;
pub mod terminal_hook;
pub mod theme;
pub mod views;

#[cfg(test)]
mod a11y_tests;
#[cfg(test)]
mod editing_tests;
#[cfg(test)]
mod polish_tests;
#[cfg(test)]
mod review_ux_tests;
#[cfg(test)]
mod tests;

pub use theme::{ActiveTheme, ColorExt, ThemeMode, ThemeState};

use std::path::PathBuf;

use gpui::{
    point, px, size, App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowDecorations,
    WindowOptions,
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
    a11y::start_os_detection();
    if matches!(mode, ThemeMode::Named(_)) {
        cx.set_global(settings::CliTheme);
    }
    ThemeState::init(cx, registry, mode, lector_theme::Appearance::Dark);
    cx.set_global(code_view::EditorSettings::default());
    settings::apply_settings(cx, settings.0, settings.1);
    settings::bind_effective_keymap(cx);
    lsp::init(cx);
    commands_bridge::init(cx);
    cx.on_action(|_: &actions::workspace::Quit, cx| views::workspace::quit_with_prompts(cx));
    cx.on_action(|_: &actions::workspace::NewWindow, cx| {
        if let Err(err) = open_empty_window(cx, None) {
            tracing::error!("cannot open a window: {err:#}");
        }
    });
    // GPUI has no native prompts on Linux / FreeBSD; its fallback is unthemed,
    // truncates the detail text and ignores Enter / Escape.
    if cfg!(any(target_os = "linux", target_os = "freebsd")) {
        components::prompt::install(cx);
    }
    // macOS draws these natively; Linux and Windows draw the same model in
    // the title bar (`views::app_menu`).
    if cfg!(target_os = "macos") {
        let menus = menus::app_menus(cx);
        cx.set_menus(menus);
    }
}

/// The project root for a file: the nearest ancestor with `.git`, else its folder.
pub fn root_for_file(file: &std::path::Path) -> PathBuf {
    let parent = file
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut dir = Some(parent.as_path());
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return d.to_path_buf();
        }
        dir = d.parent();
    }
    parent
}

/// Opens a new window on `root` (recording it as a recent project),
/// optionally opening `file`.
pub fn open_folder_window(
    cx: &mut App,
    root: PathBuf,
    file: Option<PathBuf>,
) -> anyhow::Result<()> {
    if let Err(err) = commands_bridge::recent_projects().record(&root) {
        tracing::warn!("cannot record recent project: {err}");
    }
    open_workspace_window(
        cx,
        OpenRequest {
            root,
            file: file.map(|f| (f, None, None)),
            ..Default::default()
        },
    )
}

fn window_options(cx: &App, fixed_size: Option<(f32, f32)>) -> WindowOptions {
    let bounds = match fixed_size {
        Some((w, h)) => Bounds::new(point(px(0.), px(0.)), size(px(w), px(h))),
        None => Bounds::centered(None, size(px(1440.), px(900.)), cx),
    };
    WindowOptions {
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
    }
}

/// Opens a window with no folder: the welcome view, menus and palette.
/// `fixed_size` pins the bounds (headless screenshots).
pub fn open_empty_window(cx: &mut App, fixed_size: Option<(f32, f32)>) -> anyhow::Result<()> {
    let project = views::workspace::no_folder_project(cx)?;
    let options = window_options(cx, fixed_size);
    cx.open_window(options, move |window, cx| {
        let workspace = cx.new(|cx| views::Workspace::new_empty_with(project, window, cx));
        workspace.update(cx, |ws, cx| ws.focus_welcome_if_empty(window, cx));
        cx.new(|_| views::WorkspaceRoot { workspace })
    })?;
    cx.activate(true);
    Ok(())
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
    let options = window_options(cx, request.fixed_size);
    cx.open_window(options, move |window, cx| {
        let workspace = cx.new(|cx| views::Workspace::new(project, window, cx));
        let restored = workspace.update(cx, |ws, cx| ws.restore_session(window, cx));
        if let Some((file, line, col)) = request.file.clone() {
            workspace.update(cx, |ws, cx| {
                ws.open_path_at(file, line, col, window, cx);
            });
        } else if !restored {
            // Nothing restored: the welcome view fills the center; focus it so
            // key bindings work right away.
            workspace.update(cx, |ws, cx| ws.focus_welcome_if_empty(window, cx));
        }
        cx.new(|_| views::WorkspaceRoot { workspace })
    })?;
    cx.activate(true);
    Ok(())
}
