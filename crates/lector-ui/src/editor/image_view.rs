//! Image preview: fit-to-window or 100%, dimensions and size, and a
//! checkerboard behind transparent pixels.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    canvas, div, fill, img, point, prelude::*, px, size, App, Bounds, EventEmitter, FocusHandle,
    Focusable, ImageSource, ObjectFit, Pixels, RenderImage, SharedString, Task, Window,
};
use lector_fs::ImageFormat;

use crate::components::{h_flex, metrics, v_flex, Button};
use crate::icons::IconName;
use crate::theme::{ActiveTheme, ColorExt};

/// Facts about an image file.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageInfo {
    /// Format.
    pub format: ImageFormat,
    /// Pixel dimensions, if they could be read.
    pub dimensions: Option<(u32, u32)>,
    /// File size in bytes.
    pub size: u64,
}

impl ImageInfo {
    /// Reads the header of `path` (blocking; call off the main thread).
    pub fn read(path: &Path, format: ImageFormat, size: u64) -> ImageInfo {
        let dimensions = match format {
            ImageFormat::Svg => std::fs::read_to_string(path)
                .ok()
                .and_then(|s| svg_dimensions(&s)),
            _ => image::image_dimensions(path).ok(),
        };
        ImageInfo {
            format,
            dimensions,
            size,
        }
    }
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut search = tag;
    loop {
        let ix = search.find(name)?;
        let before = search[..ix].chars().next_back();
        let rest = &search[ix + name.len()..];
        let rest_trim = rest.trim_start();
        if before.is_some_and(|c| c.is_whitespace()) && rest_trim.starts_with('=') {
            let value = rest_trim[1..].trim_start();
            let quote = value.chars().next()?;
            if quote == '"' || quote == '\'' {
                let value = &value[1..];
                return value.find(quote).map(|end| &value[..end]);
            }
            return None;
        }
        search = rest;
    }
}

fn parse_length(value: &str) -> Option<f32> {
    let v = value.trim().trim_end_matches("px");
    if v.ends_with('%') || v.chars().any(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    v.parse::<f32>().ok().filter(|v| *v > 0.)
}

/// Intrinsic size of an SVG from its root `width`/`height` or `viewBox`.
pub fn svg_dimensions(svg: &str) -> Option<(u32, u32)> {
    let start = svg.find("<svg")?;
    let end = svg[start..].find('>')? + start;
    let tag = &svg[start..end];
    let w = attr(tag, "width").and_then(parse_length);
    let h = attr(tag, "height").and_then(parse_length);
    if let (Some(w), Some(h)) = (w, h) {
        return Some((w.round() as u32, h.round() as u32));
    }
    let vb: Vec<f32> = attr(tag, "viewBox")?
        .split([' ', ','])
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    if vb.len() == 4 && vb[2] > 0. && vb[3] > 0. {
        return Some((vb[2].round() as u32, vb[3].round() as u32));
    }
    None
}

/// Zoom mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zoom {
    /// Scale down to fit the view.
    Fit,
    /// Actual pixels.
    Actual,
}

/// Events emitted by [`ImageView`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageViewEvent {
    /// The user asked to see the SVG source.
    ShowSource,
}

/// The image preview view.
pub struct ImageView {
    path: PathBuf,
    info: ImageInfo,
    svg: bool,
    zoom: Zoom,
    focus_handle: FocusHandle,
    stage: Rc<Cell<gpui::Size<Pixels>>>,
    /// SVG rasterized at the displayed size (crisp at any zoom).
    svg_raster: Option<((u32, u32), Arc<RenderImage>)>,
    raster_task: Option<((u32, u32), Task<()>)>,
}

/// Rasterizes an SVG file to exactly `width` × `height` device pixels
/// (BGRA, straight alpha, as GPUI expects). Blocking.
pub fn rasterize_svg(path: &Path, width: u32, height: u32) -> Option<RenderImage> {
    let bytes = std::fs::read(path).ok()?;
    let tree = resvg::usvg::Tree::from_data(&bytes, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width.max(1), height.max(1))?;
    let transform = resvg::tiny_skia::Transform::from_scale(
        width as f32 / size.width(),
        height as f32 / size.height(),
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let mut data = pixmap.take();
    for px in data.chunks_exact_mut(4) {
        px.swap(0, 2);
        let a = px[3];
        if a > 0 && a < 255 {
            let f = 255. / a as f32;
            px[0] = (px[0] as f32 * f).min(255.) as u8;
            px[1] = (px[1] as f32 * f).min(255.) as u8;
            px[2] = (px[2] as f32 * f).min(255.) as u8;
        }
    }
    let buffer = image::RgbaImage::from_raw(width.max(1), height.max(1), data)?;
    Some(RenderImage::new(smallvec::smallvec![image::Frame::new(
        buffer
    )]))
}

impl EventEmitter<ImageViewEvent> for ImageView {}

impl Focusable for ImageView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ImageView {
    /// Preview of `path`.
    pub fn new(path: PathBuf, info: ImageInfo, svg: bool, cx: &mut Context<Self>) -> Self {
        ImageView {
            path,
            info,
            svg,
            zoom: Zoom::Fit,
            focus_handle: cx.focus_handle(),
            stage: Rc::new(Cell::new(gpui::Size::default())),
            svg_raster: None,
            raster_task: None,
        }
    }

    /// Whether this is an SVG (has a source view).
    pub fn is_svg(&self) -> bool {
        self.svg
    }

    /// Updates facts after a reload.
    pub fn set_info(&mut self, info: ImageInfo, cx: &mut Context<Self>) {
        self.info = info;
        self.svg_raster = None;
        self.raster_task = None;
        cx.notify();
    }

    /// The image source for a frame of `size` logical pixels: SVGs are
    /// rasterized in the background at the exact device size.
    fn source_for(
        &mut self,
        size: gpui::Size<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> ImageSource {
        if !self.svg {
            return ImageSource::from(self.path.clone());
        }
        let scale = window.scale_factor();
        let want = (
            (f32::from(size.width) * scale).round().clamp(1., 8192.) as u32,
            (f32::from(size.height) * scale).round().clamp(1., 8192.) as u32,
        );
        let pending = self.raster_task.as_ref().map(|(s, _)| *s);
        let have = self.svg_raster.as_ref().map(|(s, _)| *s);
        if have != Some(want) && pending != Some(want) {
            let path = self.path.clone();
            let task = cx.spawn(async move |this, cx| {
                let image = cx
                    .background_executor()
                    .spawn(async move { rasterize_svg(&path, want.0, want.1) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.raster_task = None;
                    if let Some(image) = image {
                        this.svg_raster = Some((want, Arc::new(image)));
                        cx.notify();
                    }
                });
            });
            self.raster_task = Some((want, task));
        }
        match &self.svg_raster {
            Some((_, image)) => ImageSource::Render(image.clone()),
            None => ImageSource::from(self.path.clone()),
        }
    }

    fn set_zoom(&mut self, zoom: Zoom, cx: &mut Context<Self>) {
        self.zoom = zoom;
        cx.notify();
    }
}

fn checkerboard(light: bool) -> impl IntoElement {
    let (a, b) = if light {
        (gpui::rgb(0xffffff), gpui::rgb(0xe8e8ec))
    } else {
        (gpui::rgb(0x2a2b30), gpui::rgb(0x34353b))
    };
    canvas(
        |_, _, _| {},
        move |bounds: Bounds<Pixels>, _, window, _| {
            window.paint_quad(fill(bounds, a));
            let tile = px(8.);
            let cols = (bounds.size.width / tile).ceil() as usize;
            let rows = (bounds.size.height / tile).ceil() as usize;
            if cols * rows > 40_000 {
                return;
            }
            for r in 0..rows {
                for c in (r % 2..cols).step_by(2) {
                    let origin = point(
                        bounds.left() + tile * c as f32,
                        bounds.top() + tile * r as f32,
                    );
                    let w = tile.min(bounds.right() - origin.x);
                    let h = tile.min(bounds.bottom() - origin.y);
                    window.paint_quad(fill(Bounds::new(origin, size(w, h)), b));
                }
            }
        },
    )
    .absolute()
    .size_full()
}

impl Render for ImageView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let light = !theme.appearance.is_dark();
        let dims: SharedString = match self.info.dimensions {
            Some((w, h)) => format!("{w} × {h}").into(),
            None => "Unknown size".into(),
        };
        let fmt = format!("{:?}", self.info.format).to_uppercase();
        let meta: SharedString =
            format!("{dims} · {fmt} · {}", super::format_size(self.info.size)).into();
        let weak = cx.entity().downgrade();
        let weak2 = weak.clone();
        let weak3 = weak.clone();
        let (w, h) = self
            .info
            .dimensions
            .map(|(w, h)| (px(w as f32), px(h as f32)))
            .unwrap_or((px(256.), px(256.)));
        let frame = div()
            .relative()
            .flex_none()
            .border_1()
            .border_color(ui.border.hsla())
            .shadow_md()
            .child(checkerboard(light));
        // Measure the stage so "Fit" keeps the aspect ratio exactly.
        let stage_cell = self.stage.clone();
        let entity = cx.entity().entity_id();
        let measure = canvas(
            move |bounds: Bounds<Pixels>, _, cx| {
                if stage_cell.get() != bounds.size {
                    stage_cell.set(bounds.size);
                    cx.notify(entity);
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let stage = match self.zoom {
            Zoom::Fit => {
                let avail = self.stage.get();
                let pad = px(48.);
                // Raster images only scale down; vectors fill the view.
                let max_scale = if self.svg { 16. } else { 1. };
                let scale = if avail.width > pad && avail.height > pad {
                    ((avail.width - pad) / w)
                        .min((avail.height - pad) / h)
                        .min(max_scale)
                } else {
                    0.
                };
                div()
                    .relative()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(measure)
                    .when(scale > 0., |el| {
                        let source = self.source_for(size(w * scale, h * scale), window, cx);
                        el.child(
                            frame
                                .w(w * scale)
                                .h(h * scale)
                                .child(img(source).object_fit(ObjectFit::Contain).size_full()),
                        )
                    })
                    .into_any_element()
            }
            Zoom::Actual => {
                let source = self.source_for(size(w, h), window, cx);
                div()
                    .id("image-scroll")
                    .size_full()
                    .overflow_scroll()
                    .p(px(24.))
                    .child(
                        frame
                            .w(w)
                            .h(h)
                            .child(img(source).object_fit(ObjectFit::Contain).w(w).h(h)),
                    )
                    .into_any_element()
            }
        };
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(ui.background.hsla())
            .child(
                h_flex()
                    .h(px(30.))
                    .flex_none()
                    .px(px(10.))
                    .gap(px(4.))
                    .border_b_1()
                    .border_color(ui.border_variant.hsla())
                    .child(
                        Button::new("zoom-fit", "Fit")
                            .compact()
                            .icon(IconName::Monitor)
                            .selected(self.zoom == Zoom::Fit)
                            .on_click(move |_, _, cx| {
                                let _ = weak.update(cx, |v, cx| v.set_zoom(Zoom::Fit, cx));
                            }),
                    )
                    .child(
                        Button::new("zoom-actual", "100%")
                            .compact()
                            .selected(self.zoom == Zoom::Actual)
                            .on_click(move |_, _, cx| {
                                let _ = weak2.update(cx, |v, cx| v.set_zoom(Zoom::Actual, cx));
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(metrics::TEXT_SM)
                            .text_color(ui.text_muted.hsla())
                            .child(meta),
                    )
                    .when(self.svg, |el| {
                        el.child(div().w(px(8.))).child(
                            Button::new("svg-source", "Source")
                                .compact()
                                .icon(IconName::Code)
                                .on_click(move |_, _, cx| {
                                    let _ = weak3
                                        .update(cx, |_, cx| cx.emit(ImageViewEvent::ShowSource));
                                }),
                        )
                    }),
            )
            .child(div().flex_1().min_h_0().child(stage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_sizes() {
        assert_eq!(
            svg_dimensions(r#"<svg xmlns="x" width="24" height="16px">"#),
            Some((24, 16))
        );
        assert_eq!(
            svg_dimensions(r#"<?xml?><svg viewBox="0 0 100 50" width="100%">"#),
            Some((100, 50))
        );
        assert_eq!(svg_dimensions("<svg>"), None);
        assert_eq!(svg_dimensions("not svg"), None);
    }
}
