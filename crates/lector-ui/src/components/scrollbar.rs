//! Overlay scrollbars for any GPUI `ScrollHandle` (divs, uniform lists).

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    point, px, size, App, Axis, Bounds, ContentMask, Corners, DispatchPhase, Element, ElementId,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, IntoElement, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Position, ScrollHandle, Style,
    Window,
};

use crate::theme::{ActiveTheme, ColorExt};

/// Thickness of the scrollbar hit area.
pub const SCROLLBAR_WIDTH: Pixels = px(10.);
const MIN_THUMB: Pixels = px(24.);

/// Per-scrollbar interaction state; keep one per scrollbar in the owning view.
#[derive(Clone, Default)]
pub struct ScrollbarState {
    drag_offset: Rc<Cell<Option<Pixels>>>,
}

impl ScrollbarState {
    /// Whether the thumb is being dragged.
    pub fn is_dragging(&self) -> bool {
        self.drag_offset.get().is_some()
    }
}

/// Thumb geometry along one axis: `(start, length)` within a track of
/// `track` length, for a viewport of `viewport` over `content`, scrolled by
/// `scrolled` (positive).
pub fn thumb_geometry(
    track: f32,
    viewport: f32,
    content: f32,
    scrolled: f32,
) -> Option<(f32, f32)> {
    if content <= viewport + 0.5 || track <= 0.0 {
        return None;
    }
    let len = (track * viewport / content)
        .max(f32::from(MIN_THUMB))
        .min(track);
    let max_scroll = content - viewport;
    let start = (scrolled / max_scroll).clamp(0.0, 1.0) * (track - len);
    Some((start, len))
}

/// A vertical overlay scrollbar for `handle`. Place it as the last child of
/// a `relative()` container that also contains the scrolled element.
pub fn vertical_scrollbar(
    id: impl Into<ElementId>,
    handle: &ScrollHandle,
    state: &ScrollbarState,
    cx: &App,
) -> Scrollbar {
    Scrollbar::new(id.into(), handle.clone(), state.clone(), Axis::Vertical, cx)
}

/// A horizontal overlay scrollbar for `handle`.
pub fn horizontal_scrollbar(
    id: impl Into<ElementId>,
    handle: &ScrollHandle,
    state: &ScrollbarState,
    cx: &App,
) -> Scrollbar {
    Scrollbar::new(
        id.into(),
        handle.clone(),
        state.clone(),
        Axis::Horizontal,
        cx,
    )
}

/// The scrollbar element.
pub struct Scrollbar {
    id: ElementId,
    handle: ScrollHandle,
    state: ScrollbarState,
    axis: Axis,
    thumb: Hsla,
    thumb_hover: Hsla,
}

impl Scrollbar {
    fn new(
        id: ElementId,
        handle: ScrollHandle,
        state: ScrollbarState,
        axis: Axis,
        cx: &App,
    ) -> Self {
        let ui = &cx.theme().ui;
        Scrollbar {
            id,
            handle,
            state,
            axis,
            thumb: ui.scrollbar_thumb.hsla(),
            thumb_hover: ui.scrollbar_thumb_hover.hsla(),
        }
    }
}

impl IntoElement for Scrollbar {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for Scrollbar {
    type RequestLayoutState = ();
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style {
            position: Position::Absolute,
            ..Default::default()
        };
        match self.axis {
            Axis::Vertical => {
                style.inset.top = px(0.).into();
                style.inset.bottom = px(0.).into();
                style.inset.right = px(0.).into();
                style.size.width = SCROLLBAR_WIDTH.into();
            }
            Axis::Horizontal => {
                style.inset.left = px(0.).into();
                style.inset.right = px(0.).into();
                style.inset.bottom = px(0.).into();
                style.size.height = SCROLLBAR_WIDTH.into();
            }
        }
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) -> Option<Hitbox> {
        let viewport = self.handle.bounds().size;
        let max = self.handle.max_offset();
        let scrollable = match self.axis {
            Axis::Vertical => max.height > px(0.5) && viewport.height > px(0.),
            Axis::Horizontal => max.width > px(0.5) && viewport.width > px(0.),
        };
        scrollable.then(|| window.insert_hitbox(bounds, HitboxBehavior::BlockMouse))
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        hitbox: &mut Option<Hitbox>,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let Some(hitbox) = hitbox.take() else {
            return;
        };
        let viewport = self.handle.bounds().size;
        let max = self.handle.max_offset();
        let offset = self.handle.offset();
        let (track, view, content, scrolled) = match self.axis {
            Axis::Vertical => (
                bounds.size.height,
                viewport.height,
                viewport.height + max.height,
                -offset.y,
            ),
            Axis::Horizontal => (
                bounds.size.width,
                viewport.width,
                viewport.width + max.width,
                -offset.x,
            ),
        };
        let Some((start, len)) = thumb_geometry(
            f32::from(track),
            f32::from(view),
            f32::from(content),
            f32::from(scrolled),
        ) else {
            return;
        };
        let hovered = hitbox.is_hovered(window) || self.state.is_dragging();
        let thickness = if hovered { px(8.) } else { px(6.) };
        let pad = (SCROLLBAR_WIDTH - thickness) / 2.;
        let thumb_bounds = match self.axis {
            Axis::Vertical => Bounds::new(
                point(bounds.right() - thickness - pad, bounds.top() + px(start)),
                size(thickness, px(len)),
            ),
            Axis::Horizontal => Bounds::new(
                point(bounds.left() + px(start), bounds.bottom() - thickness - pad),
                size(px(len), thickness),
            ),
        };
        let color = if hovered {
            self.thumb_hover
        } else {
            self.thumb
        };
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.paint_quad(
                gpui::fill(thumb_bounds, color).corner_radii(Corners::all(thickness / 2.)),
            );
        });

        let axis = self.axis;
        let track_origin = match axis {
            Axis::Vertical => bounds.top(),
            Axis::Horizontal => bounds.left(),
        };
        let handle = self.handle.clone();
        let state = self.state.clone();
        let thumb_start = px(start);
        let thumb_len = px(len);
        let max_scroll = content - view;
        let set_scroll = move |handle: &ScrollHandle, along: Pixels| {
            let travel = track - thumb_len;
            if travel <= px(0.) {
                return;
            }
            let ratio = ((along - track_origin) / travel).clamp(0., 1.);
            let mut offset = handle.offset();
            match axis {
                Axis::Vertical => offset.y = -max_scroll * ratio,
                Axis::Horizontal => offset.x = -max_scroll * ratio,
            }
            handle.set_offset(offset);
        };
        let along = move |p: Point<Pixels>| match axis {
            Axis::Vertical => p.y,
            Axis::Horizontal => p.x,
        };

        window.on_mouse_event({
            let handle = handle.clone();
            let state = state.clone();
            let hitbox = hitbox.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || event.button != MouseButton::Left
                    || !hitbox.is_hovered(window)
                {
                    return;
                }
                let pos = along(event.position) - track_origin;
                if pos >= thumb_start && pos <= thumb_start + thumb_len {
                    state.drag_offset.set(Some(pos - thumb_start));
                } else {
                    // Jump so the thumb centers on the click, then drag.
                    let grab = thumb_len / 2.;
                    state.drag_offset.set(Some(grab));
                    set_scroll(&handle, along(event.position) - grab);
                }
                cx.stop_propagation();
                window.refresh();
            }
        });
        window.on_mouse_event({
            let handle = handle.clone();
            let state = state.clone();
            move |event: &MouseMoveEvent, _, window, _cx| {
                if let Some(grab) = state.drag_offset.get() {
                    if event.pressed_button == Some(MouseButton::Left) {
                        set_scroll(&handle, along(event.position) - grab);
                        window.refresh();
                    } else {
                        state.drag_offset.set(None);
                    }
                }
            }
        });
        window.on_mouse_event(move |_: &MouseUpEvent, _, window, _cx| {
            if state.drag_offset.take().is_some() {
                window.refresh();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumb_geometry_bounds() {
        assert!(thumb_geometry(100., 100., 100., 0.).is_none());
        let (start, len) = thumb_geometry(100., 50., 200., 0.).unwrap();
        assert_eq!(start, 0.);
        assert_eq!(len, 25.);
        let (start, len) = thumb_geometry(100., 50., 200., 150.).unwrap();
        assert_eq!(start + len, 100.);
        // Minimum thumb size.
        let (_, len) = thumb_geometry(100., 10., 100_000., 0.).unwrap();
        assert_eq!(len, 24.);
    }
}
