//! Floating popups that belong to an editable [`CodeView`] without taking
//! focus: the completion list and the signature help (`editor::completion`,
//! `editor::signature_help`).
//!
//! The view only hosts them. An owner registers a popup under a key with
//! [`CodeView::set_popup`] (a view, the buffer position it hangs from, its
//! exact size) and the view places it every frame next to that position with
//! [`place_popup`] (below the line, or above when there is no room, clamped
//! to the window), so it follows scrolling in both directions. It is dropped
//! — and the owner told through `on_dismiss` — when the view loses focus or
//! the anchor row scrolls out of view.
//!
//! The owner also tells the view which extra key context identifiers apply
//! ([`CodeView::set_context_flag`], e.g. `completion`), so bindings such as
//! `CodeView && completion` only exist while the popup is open, and it can
//! observe plain typing: [`CodeView::take_typed`] gives the text of the last
//! keystroke, and [`CodeView::set_input_interceptor`] runs before typed text
//! is inserted (commit characters).

use std::rc::Rc;

use gpui::{
    anchored, deferred, div, prelude::*, AnyElement, AnyView, App, Bounds, Corner, KeyContext,
    Pixels, Point, Size, Subscription, Window,
};

use super::selection::Pos;
use super::CodeView;
use crate::keymap::contexts;
use crate::scale::px;

/// Preferred side of the anchor line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vertical {
    /// Under the line, above when there is no room.
    Below,
    /// Over the line, below when there is no room.
    Above,
}

/// Called when the view drops a popup on its own (focus lost, anchor scrolled away).
pub type DismissHook = Rc<dyn Fn(&mut Window, &mut App)>;

/// Decides whether typed text is handled by the owner instead of being inserted:
/// `true` swallows it (the owner inserts it later itself).
pub type InputInterceptor = Rc<dyn Fn(&str, &mut Window, &mut App) -> bool>;

/// A popup hosted by a [`CodeView`].
#[derive(Clone)]
pub struct EditorPopup {
    /// The content. It must fill exactly `size` (the placement relies on it).
    pub view: AnyView,
    /// Buffer position the popup hangs from (top left of that character cell).
    pub anchor: Pos,
    /// Exact size in pixels.
    pub size: Size<Pixels>,
    /// Preferred side of the anchor line.
    pub prefer: Vertical,
    /// Runs when the view drops the popup itself.
    pub on_dismiss: Option<DismissHook>,
}

/// Where a popup of some size goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    /// Top left corner in window coordinates.
    pub origin: Point<Pixels>,
    /// The popup sits over the anchor line.
    pub above: bool,
}

/// Places a `size` popup that hangs from the line whose top left is `anchor`
/// (window coordinates, `line_height` tall) inside a window of `window` size
/// with `margin` kept free at every edge.
///
/// The preferred side wins when the popup fits there; else the other side when
/// it fits; else the side with more room (the popup is then pushed inside the
/// window as far as it goes). Horizontally it starts at the anchor and moves
/// left as far as needed to stay inside.
pub fn place_popup(
    anchor: Point<Pixels>,
    line_height: Pixels,
    size: Size<Pixels>,
    window: Size<Pixels>,
    prefer: Vertical,
    margin: Pixels,
) -> Placement {
    let below_y = anchor.y + line_height;
    let above_y = anchor.y - size.height;
    let room_below = window.height - margin - below_y;
    let room_above = anchor.y - margin;
    let fits_below = room_below >= size.height;
    let fits_above = room_above >= size.height;
    let above = match (prefer, fits_below, fits_above) {
        (Vertical::Below, true, _) => false,
        (Vertical::Below, false, true) => true,
        (Vertical::Above, _, true) => true,
        (Vertical::Above, true, false) => false,
        _ => room_above > room_below,
    };
    let y = if above { above_y } else { below_y };
    let max_y = (window.height - margin - size.height).max(margin);
    let max_x = (window.width - margin - size.width).max(margin);
    Placement {
        origin: gpui::point(anchor.x.min(max_x).max(margin), y.min(max_y).max(margin)),
        above,
    }
}

/// How the last layout relates to an anchor.
enum AnchorState {
    Visible(Point<Pixels>),
    /// The layout does not know the row (yet): draw nothing, keep the popup.
    Unknown,
    /// The row is scrolled out of view.
    Hidden,
}

/// Popup, key context and typing hooks of a [`CodeView`] (see the module docs).
#[derive(Default)]
pub(crate) struct PopupState {
    popups: Vec<(&'static str, EditorPopup)>,
    flags: Vec<&'static str>,
    typed: Option<String>,
    interceptor: Option<InputInterceptor>,
    blur: Option<Subscription>,
}

impl PopupState {
    /// The typing interceptor, if any (cloned so it can run while the view is borrowed).
    pub(crate) fn interceptor(&self) -> Option<InputInterceptor> {
        self.interceptor.clone()
    }

    /// Remembers the text of a plain keystroke for [`CodeView::take_typed`].
    pub(crate) fn set_typed(&mut self, text: &str) {
        self.typed = Some(text.to_string());
    }
}

impl CodeView {
    /// Shows, replaces (same `key`) or removes (`None`) a popup.
    pub fn set_popup(
        &mut self,
        key: &'static str,
        popup: Option<EditorPopup>,
        cx: &mut gpui::Context<Self>,
    ) {
        let state = &mut self.popups;
        let existing = state.popups.iter().position(|(k, _)| *k == key);
        match (existing, popup) {
            (Some(ix), Some(p)) => state.popups[ix].1 = p,
            (Some(ix), None) => {
                state.popups.remove(ix);
            }
            (None, Some(p)) => state.popups.push((key, p)),
            (None, None) => return,
        }
        cx.notify();
    }

    /// Whether a popup is registered under `key`.
    pub fn has_popup(&self, key: &'static str) -> bool {
        self.popups.popups.iter().any(|(k, _)| *k == key)
    }

    /// Adds or removes an identifier of this view's key context (bindings like
    /// `CodeView && completion` apply while it is set).
    pub fn set_context_flag(&mut self, flag: &'static str, on: bool, cx: &mut gpui::Context<Self>) {
        let flags = &mut self.popups.flags;
        let present = flags.contains(&flag);
        if on && !present {
            flags.push(flag);
            cx.notify();
        } else if !on && present {
            flags.retain(|f| *f != flag);
            cx.notify();
        }
    }

    /// Whether `flag` is set in the key context.
    pub fn has_context_flag(&self, flag: &'static str) -> bool {
        self.popups.flags.contains(&flag)
    }

    /// The text of the last plain keystroke the view inserted (or swallowed
    /// type-over), once. Owners call it when the view reports an edit or a
    /// selection change.
    pub fn take_typed(&mut self) -> Option<String> {
        self.popups.typed.take()
    }

    /// Types `text` at every caret as a keystroke would (auto-close and
    /// type-over included), without asking the input interceptor. Used by the
    /// owner that took a keystroke over and now lets it through.
    pub fn insert_typed(&mut self, text: &str, cx: &mut gpui::Context<Self>) {
        if !self.is_editable() {
            return;
        }
        self.popups.set_typed(text);
        let text = text.to_string();
        self.edit_buffer(cx, move |b| b.insert(&text));
        cx.notify();
    }

    /// Installs (or removes) the hook that sees typed text before it is inserted.
    pub fn set_input_interceptor(&mut self, f: Option<InputInterceptor>) {
        self.popups.interceptor = f;
    }

    /// The key context of the view: `CodeView` plus the owners' flags.
    pub(crate) fn key_context(&self) -> KeyContext {
        let mut ctx = KeyContext::default();
        ctx.add(contexts::CODE_VIEW);
        for flag in &self.popups.flags {
            ctx.add(*flag);
        }
        ctx
    }

    /// Hooks the focus loss of the view (once, needs a window).
    pub(crate) fn ensure_popup_hooks(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.popups.blur.is_some() {
            return;
        }
        let focus = self.focus_handle.clone();
        self.popups.blur = Some(cx.on_blur(&focus, window, |this, window, cx| {
            this.dismiss_popups(window, cx)
        }));
    }

    /// Drops every popup and tells their owners.
    pub fn dismiss_popups(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let dropped: Vec<_> = self.popups.popups.drain(..).collect();
        if dropped.is_empty() {
            return;
        }
        cx.notify();
        for (_, popup) in dropped {
            if let Some(hook) = popup.on_dismiss {
                window.defer(cx, move |window, cx| hook(window, cx));
            }
        }
    }

    fn anchor_state(&self, anchor: Pos) -> AnchorState {
        let Some(layout) = self.last_layout.as_ref() else {
            return AnchorState::Unknown;
        };
        if let (Some(first), Some(last)) = (layout.rows.first(), layout.rows.last()) {
            if anchor.row < first.row || anchor.row > last.row {
                return AnchorState::Hidden;
            }
        }
        match self.point_for_pos(anchor) {
            Some(p)
                if p.y + layout.line_height > layout.text_bounds.top()
                    && p.y < layout.text_bounds.bottom() =>
            {
                AnchorState::Visible(p)
            }
            Some(_) => AnchorState::Hidden,
            None => AnchorState::Unknown,
        }
    }

    /// Bounds of the text area in window coordinates (last frame).
    pub fn text_area_bounds(&self) -> Option<Bounds<Pixels>> {
        self.last_layout.as_ref().map(|l| l.text_bounds)
    }

    /// The popups as elements, placed against the last layout.
    pub(crate) fn render_popups(
        &mut self,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Vec<AnyElement> {
        if self.popups.popups.is_empty() {
            return Vec::new();
        }
        let Some(line_height) = self.line_height() else {
            return Vec::new();
        };
        let viewport = window.viewport_size();
        let mut out = Vec::new();
        let mut gone: Vec<&'static str> = Vec::new();
        for (key, popup) in &self.popups.popups {
            match self.anchor_state(popup.anchor) {
                AnchorState::Visible(point) => {
                    let placement = place_popup(
                        point,
                        line_height,
                        popup.size,
                        viewport,
                        popup.prefer,
                        px(8.),
                    );
                    out.push(
                        deferred(
                            anchored()
                                .position(placement.origin)
                                .anchor(Corner::TopLeft)
                                .snap_to_window()
                                .child(
                                    div()
                                        .occlude()
                                        .w(popup.size.width)
                                        .h(popup.size.height)
                                        .child(popup.view.clone()),
                                ),
                        )
                        .with_priority(3)
                        .into_any_element(),
                    );
                }
                AnchorState::Unknown => {}
                AnchorState::Hidden => gone.push(key),
            }
        }
        if !gone.is_empty() {
            let mut hooks = Vec::new();
            self.popups.popups.retain(|(key, popup)| {
                let drop = gone.contains(key);
                if drop {
                    hooks.extend(popup.on_dismiss.clone());
                }
                !drop
            });
            for hook in hooks {
                window.defer(cx, move |window, cx| hook(window, cx));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    const WINDOW: Size<Pixels> = Size {
        width: gpui::px(1000.),
        height: gpui::px(800.),
    };
    const LINE: Pixels = gpui::px(20.);
    const MARGIN: Pixels = gpui::px(8.);

    fn place(anchor: (f32, f32), popup: (f32, f32), prefer: Vertical) -> Placement {
        place_popup(
            point(gpui::px(anchor.0), gpui::px(anchor.1)),
            LINE,
            size(gpui::px(popup.0), gpui::px(popup.1)),
            WINDOW,
            prefer,
            MARGIN,
        )
    }

    #[test]
    fn goes_below_the_line_when_it_fits() {
        let p = place((100., 100.), (300., 240.), Vertical::Below);
        assert!(!p.above);
        assert_eq!(p.origin, point(gpui::px(100.), gpui::px(120.)));
    }

    #[test]
    fn flips_above_when_there_is_no_room_below() {
        // 700 + 20 + 240 > 800 - 8
        let p = place((100., 700.), (300., 240.), Vertical::Below);
        assert!(p.above);
        assert_eq!(p.origin.y, gpui::px(700. - 240.));
    }

    #[test]
    fn preferring_above_flips_below_near_the_top() {
        let p = place((100., 60.), (300., 100.), Vertical::Above);
        assert!(!p.above, "60 px above is not enough for 100");
        assert_eq!(p.origin.y, gpui::px(80.));
        let p = place((100., 300.), (300., 100.), Vertical::Above);
        assert!(p.above);
        assert_eq!(p.origin.y, gpui::px(200.));
    }

    #[test]
    fn neither_side_fits_takes_the_roomier_one_and_stays_inside() {
        let p = place((100., 500.), (300., 700.), Vertical::Below);
        assert!(p.above, "480 above beats 272 below");
        assert_eq!(p.origin.y, MARGIN, "pushed down to the margin");
        let p = place((100., 100.), (300., 700.), Vertical::Below);
        assert!(!p.above);
        assert_eq!(p.origin.y, gpui::px(800. - 8. - 700.));
    }

    #[test]
    fn clamps_to_the_window_horizontally() {
        let p = place((900., 100.), (300., 100.), Vertical::Below);
        assert_eq!(p.origin.x, gpui::px(1000. - 8. - 300.));
        let p = place((-50., 100.), (300., 100.), Vertical::Below);
        assert_eq!(p.origin.x, MARGIN);
        // Wider than the window: left margin wins, no panic.
        let p = place((10., 100.), (2000., 100.), Vertical::Below);
        assert_eq!(p.origin.x, MARGIN);
    }

    #[test]
    fn exact_fit_below_counts_as_fitting() {
        // room below = 800 - 8 - (500 + 20) = 272
        let p = place((100., 500.), (300., 272.), Vertical::Below);
        assert!(!p.above);
        let p = place((100., 500.), (300., 273.), Vertical::Below);
        assert!(p.above);
    }
}
