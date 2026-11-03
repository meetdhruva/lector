//! Resizable split panes (horizontal or vertical) with draggable dividers,
//! and a single resize handle for fixed-size panels (sidebar).

use gpui::{
    div, prelude::*, px, relative, AnyElement, App, Axis, CursorStyle, DragMoveEvent, Entity,
    EntityId, Pixels, Window,
};

use crate::theme::{ActiveTheme, ColorExt};

/// Fractions of each child of a split; owned by the view that renders it.
pub struct SplitState {
    axis: Axis,
    fractions: Vec<f32>,
    min_fraction: f32,
}

impl SplitState {
    /// Equal split of `count` children along `axis`.
    pub fn new(axis: Axis, count: usize) -> Self {
        let count = count.max(1);
        SplitState {
            axis,
            fractions: vec![1.0 / count as f32; count],
            min_fraction: 0.1,
        }
    }

    /// Split axis (`Horizontal` = side by side).
    pub fn axis(&self) -> Axis {
        self.axis
    }

    /// Current fractions (sum to 1).
    pub fn fractions(&self) -> &[f32] {
        &self.fractions
    }

    /// Resets to `count` equal children.
    pub fn reset(&mut self, count: usize) {
        let count = count.max(1);
        self.fractions = vec![1.0 / count as f32; count];
    }

    /// Inserts a child after `ix`, splitting that child's space in half.
    pub fn insert_after(&mut self, ix: usize) {
        let ix = ix.min(self.fractions.len().saturating_sub(1));
        let half = self.fractions.get(ix).copied().unwrap_or(1.0) / 2.0;
        if let Some(f) = self.fractions.get_mut(ix) {
            *f = half;
        }
        self.fractions
            .insert((ix + 1).min(self.fractions.len()), half);
        self.normalize();
    }

    /// Removes child `ix`, giving its space to its neighbors.
    pub fn remove(&mut self, ix: usize) {
        if ix < self.fractions.len() && self.fractions.len() > 1 {
            self.fractions.remove(ix);
            self.normalize();
        }
    }

    fn normalize(&mut self) {
        let sum: f32 = self.fractions.iter().sum();
        if sum > 0.0 {
            for f in &mut self.fractions {
                *f /= sum;
            }
        }
    }

    /// Moves divider `ix` (between child `ix` and `ix + 1`) to `position`,
    /// a fraction of the container from its start.
    pub fn drag_divider(&mut self, ix: usize, position: f32) {
        if ix + 1 >= self.fractions.len() {
            return;
        }
        let before: f32 = self.fractions[..ix].iter().sum();
        let pair = self.fractions[ix] + self.fractions[ix + 1];
        let min = self.min_fraction.min(pair / 2.0);
        let first = (position - before).clamp(min, pair - min);
        self.fractions[ix] = first;
        self.fractions[ix + 1] = pair - first;
    }
}

#[derive(Clone)]
struct DraggedDivider {
    split: EntityId,
    ix: usize,
}

struct DragPreview;
impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// Renders `children` split according to `state`, with draggable dividers.
pub fn resizable_split(
    id: impl Into<gpui::ElementId>,
    state: &Entity<SplitState>,
    children: Vec<AnyElement>,
    cx: &App,
) -> impl IntoElement {
    let split = state.read(cx);
    let axis = split.axis;
    let fractions = split.fractions.clone();
    let border = cx.theme().ui.border.hsla();
    let accent = cx.theme().ui.accent.hsla();
    let entity_id = state.entity_id();
    let count = children.len();
    let state_for_drag = state.clone();

    let mut container = div()
        .id(id.into())
        .flex()
        .size_full()
        .overflow_hidden()
        .on_drag_move(move |event: &DragMoveEvent<DraggedDivider>, window, cx| {
            let dragged = event.drag(cx).clone();
            if dragged.split != entity_id {
                return;
            }
            let b = event.bounds;
            let position = match axis {
                Axis::Horizontal => (event.event.position.x - b.left()) / b.size.width,
                Axis::Vertical => (event.event.position.y - b.top()) / b.size.height,
            };
            state_for_drag.update(cx, |s, _| s.drag_divider(dragged.ix, position));
            window.refresh();
        });
    container = match axis {
        Axis::Horizontal => container.flex_row(),
        Axis::Vertical => container.flex_col(),
    };

    for (ix, child) in children.into_iter().enumerate() {
        let fraction = fractions
            .get(ix)
            .copied()
            .unwrap_or(1.0 / count.max(1) as f32);
        let pane = div().relative().overflow_hidden().flex_none().child(child);
        let pane = match axis {
            Axis::Horizontal => pane.h_full().w(relative(fraction)),
            Axis::Vertical => pane.w_full().h(relative(fraction)),
        };
        container = container.child(pane);
        if ix + 1 < count {
            container = container.child(divider(axis, ix, entity_id, border, accent));
        }
    }
    container
}

fn divider(
    axis: Axis,
    ix: usize,
    split: EntityId,
    border: gpui::Hsla,
    accent: gpui::Hsla,
) -> impl IntoElement {
    let hit = px(6.);
    let line = div().absolute().bg(border);
    let (line, handle) = match axis {
        Axis::Horizontal => (
            line.top_0().bottom_0().left(px(-0.5)).w(px(1.)),
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(-hit / 2.)
                .w(hit)
                .cursor(CursorStyle::ResizeLeftRight),
        ),
        Axis::Vertical => (
            line.left_0().right_0().top(px(-0.5)).h(px(1.)),
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(-hit / 2.)
                .h(hit)
                .cursor(CursorStyle::ResizeUpDown),
        ),
    };
    let handle = handle
        .id(("split-divider", ix))
        .hover(move |s| s.bg(accent.opacity(0.4)))
        .on_drag(DraggedDivider { split, ix }, |_, _, _, cx| {
            cx.new(|_| DragPreview)
        });
    let base = div().relative().flex_none();
    let base = match axis {
        Axis::Horizontal => base.w(px(0.)).h_full(),
        Axis::Vertical => base.h(px(0.)).w_full(),
    };
    base.child(line).child(handle)
}

/// Marker for dragging a single panel edge (e.g. the sidebar).
#[derive(Clone)]
pub struct DraggedPanelEdge {
    /// Identifies which edge is dragged.
    pub id: &'static str,
}

/// A vertical drag handle on the right edge of a panel; the owner handles
/// `on_drag_move::<DraggedPanelEdge>` to update its width.
pub fn panel_resize_handle(id: &'static str, cx: &App) -> impl IntoElement {
    let accent = cx.theme().ui.accent.hsla();
    div()
        .id(id)
        .absolute()
        .top_0()
        .bottom_0()
        .right(px(-3.))
        .w(px(6.))
        .cursor(CursorStyle::ResizeLeftRight)
        .hover(move |s| s.bg(accent.opacity(0.4)))
        .on_drag(DraggedPanelEdge { id }, |_, _, _, cx| {
            cx.new(|_| DragPreview)
        })
}

/// Clamps a panel width between `min` and `max`.
pub fn clamp_width(width: Pixels, min: Pixels, max: Pixels) -> Pixels {
    width.max(min).min(max.max(min))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_divider_respects_minimum() {
        let mut s = SplitState::new(Axis::Horizontal, 2);
        s.drag_divider(0, 0.7);
        assert!((s.fractions()[0] - 0.7).abs() < 1e-6);
        s.drag_divider(0, 0.99);
        assert!((s.fractions()[1] - 0.1).abs() < 1e-6);
        s.insert_after(0);
        assert_eq!(s.fractions().len(), 3);
        assert!((s.fractions().iter().sum::<f32>() - 1.0).abs() < 1e-5);
        s.remove(1);
        assert_eq!(s.fractions().len(), 2);
    }
}
