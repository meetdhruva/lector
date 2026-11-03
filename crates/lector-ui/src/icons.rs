//! Icon elements: tinted UI icons (`svg()`) and colored file icons (`img()`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use gpui::{
    img, prelude::*, px, svg, App, Hsla, Image, ImageFormat, ImageSource, Pixels, SharedString,
    Window,
};
use lector_icons::{FileIcons, IconRef, UiIcon};

use crate::assets::UI_ICON_PREFIX;
use crate::theme::{ActiveTheme, ColorExt};

pub use lector_icons::UiIcon as IconName;

pub(crate) fn ui_icon_path(icon: UiIcon) -> SharedString {
    static PATHS: OnceLock<Vec<SharedString>> = OnceLock::new();
    let paths = PATHS.get_or_init(|| {
        UiIcon::ALL
            .iter()
            .map(|i| SharedString::from(format!("{UI_ICON_PREFIX}{}.svg", i.name())))
            .collect()
    });
    // `UiIcon::ALL` lists variants in declaration order.
    paths.get(icon as usize).cloned().unwrap_or_default()
}

/// Standard icon sizes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IconSize {
    /// 12px.
    XSmall,
    /// 14px.
    Small,
    /// 16px.
    Medium,
    /// 20px.
    Large,
    /// Explicit size.
    Custom(Pixels),
}

impl IconSize {
    /// Size in pixels.
    pub fn px(self) -> Pixels {
        match self {
            IconSize::XSmall => px(12.),
            IconSize::Small => px(14.),
            IconSize::Medium => px(16.),
            IconSize::Large => px(20.),
            IconSize::Custom(p) => p,
        }
    }
}

/// A monochrome UI icon tinted with a color (defaults to muted text).
#[derive(IntoElement)]
pub struct Icon {
    icon: UiIcon,
    size: IconSize,
    color: Option<Hsla>,
}

impl Icon {
    /// New icon at the medium size.
    pub fn new(icon: UiIcon) -> Self {
        Icon {
            icon,
            size: IconSize::Medium,
            color: None,
        }
    }

    /// Sets the size.
    pub fn size(mut self, size: IconSize) -> Self {
        self.size = size;
        self
    }

    /// Sets the tint color.
    pub fn color(mut self, color: impl Into<Hsla>) -> Self {
        self.color = Some(color.into());
        self
    }
}

impl RenderOnce for Icon {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let color = self
            .color
            .unwrap_or_else(|| cx.theme().ui.text_muted.hsla());
        let size = self.size.px();
        svg()
            .path(ui_icon_path(self.icon))
            .size(size)
            .flex_none()
            .text_color(color)
    }
}

thread_local! {
    static IMAGE_CACHE: RefCell<HashMap<&'static str, Arc<Image>>> = RefCell::new(HashMap::new());
}

/// Cached GPUI image for a file icon (decoded once per icon name).
pub fn file_icon_image(icon: IconRef) -> Arc<Image> {
    IMAGE_CACHE.with(|cache| {
        cache
            .borrow_mut()
            .entry(icon.name)
            .or_insert_with(|| {
                let svg = String::from_utf8_lossy(icon.svg);
                Arc::new(Image::from_bytes(
                    ImageFormat::Svg,
                    swap_red_blue(&svg).into_bytes(),
                ))
            })
            .clone()
    })
}

/// GPUI 0.2.2 rasterizes SVG images to RGBA but uploads them as BGRA (other
/// formats are converted), so colored SVGs render with red and blue
/// swapped. Pre-swapping the channels of every hex color compensates.
/// Remove once GPUI converts SVG pixmaps itself.
pub fn swap_red_blue(svg: &str) -> String {
    let bytes = svg.as_bytes();
    let mut out = String::with_capacity(svg.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' && (i == 0 || bytes[i - 1] != b'(') {
            let run = bytes[i + 1..]
                .iter()
                .take_while(|b| b.is_ascii_hexdigit())
                .count();
            let hex = &svg[i + 1..i + 1 + run];
            match run {
                6 => {
                    out.push('#');
                    out.push_str(&hex[4..6]);
                    out.push_str(&hex[2..4]);
                    out.push_str(&hex[0..2]);
                    i += 7;
                    continue;
                }
                3 => {
                    out.push('#');
                    out.push_str(&hex[2..3]);
                    out.push_str(&hex[1..2]);
                    out.push_str(&hex[0..1]);
                    i += 4;
                    continue;
                }
                _ => {}
            }
        }
        let ch = svg[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8().max(1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::swap_red_blue;

    #[test]
    fn swaps_hex_colors_only() {
        assert_eq!(
            swap_red_blue(
                r##"<path fill="#ff7043" stroke="#abc" d="M0"/><use href="#a"/><g fill="url(#abc)"/>"##
            ),
            r##"<path fill="#4370ff" stroke="#cba" d="M0"/><use href="#a"/><g fill="url(#abc)"/>"##
        );
    }
}

/// What a file icon represents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileIconKind {
    /// A file with this name.
    File(SharedString),
    /// A folder with this name.
    Folder {
        /// Folder name.
        name: SharedString,
        /// Expanded state.
        expanded: bool,
        /// Whether this is the project root.
        root: bool,
    },
}

/// A colored Material file/folder icon.
#[derive(IntoElement)]
pub struct FileIcon {
    kind: FileIconKind,
    size: Pixels,
}

impl FileIcon {
    /// Icon for a file name.
    pub fn file(name: impl Into<SharedString>) -> Self {
        FileIcon {
            kind: FileIconKind::File(name.into()),
            size: px(16.),
        }
    }

    /// Icon for a folder.
    pub fn folder(name: impl Into<SharedString>, expanded: bool, root: bool) -> Self {
        FileIcon {
            kind: FileIconKind::Folder {
                name: name.into(),
                expanded,
                root,
            },
            size: px(16.),
        }
    }

    /// From a kind.
    pub fn new(kind: FileIconKind) -> Self {
        FileIcon {
            kind,
            size: px(16.),
        }
    }

    /// Sets the size.
    pub fn size(mut self, size: Pixels) -> Self {
        self.size = size;
        self
    }
}

/// Resolves the Material icon for a kind.
pub fn resolve_file_icon(kind: &FileIconKind, light: bool) -> IconRef {
    let icons = FileIcons::get();
    match kind {
        FileIconKind::File(name) => icons.for_file(name, light),
        FileIconKind::Folder {
            name,
            expanded,
            root,
        } => icons.for_folder(name, *expanded, *root, light),
    }
}

impl RenderOnce for FileIcon {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let light = !cx.theme().appearance.is_dark();
        let icon = resolve_file_icon(&self.kind, light);
        img(ImageSource::Image(file_icon_image(icon)))
            .size(self.size)
            .flex_none()
    }
}
