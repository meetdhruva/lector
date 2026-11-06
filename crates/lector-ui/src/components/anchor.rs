//! Where did a view end up on screen? Keyboard-opened menus (shift-F10, the Menu key) and
//! popovers anchor to the focused control, which only the painted frame knows.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{canvas, point, prelude::*, Bounds, Pixels, Point};

/// The window-space bounds of a view from its last painted frame. Clone it into the
/// view's state and add [`TrackedBounds::element`] as a child of a `relative()` container.
#[derive(Clone, Default)]
pub struct TrackedBounds(Rc<Cell<Bounds<Pixels>>>);

impl TrackedBounds {
    /// No bounds yet (all zero).
    pub fn new() -> Self {
        Self::default()
    }

    /// The bounds of the last frame.
    pub fn get(&self) -> Bounds<Pixels> {
        self.0.get()
    }

    /// A point `dx`, `dy` inside the top-left corner (a good menu anchor).
    pub fn top_left_offset(&self, dx: Pixels, dy: Pixels) -> Point<Pixels> {
        let b = self.get();
        point(b.origin.x + dx, b.origin.y + dy)
    }

    /// Invisible element that records the bounds of its parent while painting. It has no
    /// hitbox: pointer events pass through.
    pub fn element(&self) -> impl IntoElement {
        let cell = self.0.clone();
        canvas(move |bounds, _, _| cell.set(bounds), |_, _, _, _| {})
            .absolute()
            .top_0()
            .left_0()
            .size_full()
    }
}
