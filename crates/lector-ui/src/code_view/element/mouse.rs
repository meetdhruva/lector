//! Mouse input of the [`CodeElement`](super::CodeElement): the window event handlers registered
//! during paint (selection, scrolling, scrollbar and minimap dragging,
//! gutter and annotation clicks, hover).

use std::rc::Rc;

use gpui::{
    point, px, Bounds, DispatchPhase, Entity, Hitbox, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, ScrollWheelEvent, Window,
};

use super::{LayoutSnapshot, GUTTER_LEFT_PAD, MINIMAP_ROW};
use crate::code_view::selection::Pos;
use crate::code_view::{CodeView, CodeViewEvent};
use crate::components::scrollbar::thumb_geometry;

pub(super) fn register_mouse_handlers(
    window: &mut Window,
    view: &Entity<CodeView>,
    layout: &Rc<LayoutSnapshot>,
    hitbox: &Hitbox,
) {
    // Scroll wheel.
    {
        let view = view.clone();
        let hitbox = hitbox.clone();
        let lh = layout.line_height;
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || !hitbox.should_handle_scroll(window) {
                return;
            }
            let mut delta = event.delta.pixel_delta(lh);
            if event.modifiers.shift && delta.x == px(0.) {
                delta = point(delta.y, px(0.));
            }
            view.update(cx, |v, cx| v.scroll_by(delta, cx));
            cx.stop_propagation();
        });
    }
    // Mouse down.
    {
        let view = view.clone();
        let hitbox = hitbox.clone();
        let layout = layout.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                return;
            }
            let p = event.position;
            let l = &layout;
            view.update(cx, |v, cx| {
                // Minimap.
                if let Some(mb) = l.minimap_bounds {
                    if mb.contains(&p) && event.button == MouseButton::Left {
                        v.minimap_drag = true;
                        minimap_scroll_to(v, l, mb, p.y, cx);
                        cx.stop_propagation();
                        return;
                    }
                }
                // Vertical scrollbar.
                let track = l.vscroll_track();
                if track.contains(&p)
                    && l.max_scroll.y > px(0.)
                    && event.button == MouseButton::Left
                {
                    if let Some((start, len)) = thumb_geometry(
                        f32::from(track.size.height),
                        f32::from(l.bounds.size.height),
                        f32::from(l.content_height()),
                        f32::from(l.scroll.y),
                    ) {
                        let rel = p.y - track.top();
                        let grab = if rel >= px(start) && rel <= px(start + len) {
                            rel - px(start)
                        } else {
                            px(len / 2.)
                        };
                        v.scrollbar_drag = Some(grab);
                        scrollbar_scroll_to(v, l, p.y, grab, len, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                // Sticky header click: jump to that row.
                if !l.sticky.is_empty()
                    && p.y < l.bounds.top() + l.sticky_height()
                    && p.x < l.text_bounds.right()
                {
                    let ix = ((p.y - l.bounds.top()) / l.line_height).floor() as usize;
                    if let Some(rl) = l.sticky.get(ix) {
                        let row = rl.row;
                        v.set_cursor(Pos::new(row, 0), cx);
                        v.scroll_to_row(row, false, cx);
                        window.focus(&v.focus_handle);
                    }
                    cx.stop_propagation();
                    return;
                }
                let Some(pos) = v.pos_for_point(p) else {
                    return;
                };
                match event.button {
                    MouseButton::Left => {
                        if l.gutter_bounds.contains(&p) {
                            let annotation_cb = (p.x < l.annotation_right)
                                .then(|| v.gutter_annotation_click_callback())
                                .flatten()
                                .and_then(|cb| {
                                    let ix = v.gutter_annotations()?.block_at(pos.row)?;
                                    Some((cb, ix))
                                });
                            let icon_cb = (p.x >= l.annotation_right
                                && p.x < l.annotation_right + px(GUTTER_LEFT_PAD + 2.))
                            .then(|| v.gutter_icon_click_callback())
                            .flatten()
                            .filter(|_| {
                                v.inline_marks()
                                    .gutter_icons
                                    .iter()
                                    .any(|i| i.row == pos.row && i.clickable)
                            });
                            if let Some((cb, ix)) = annotation_cb {
                                cx.defer_in(window, move |_, window, cx| cb(ix, window, cx));
                            } else if let Some(cb) = icon_cb {
                                let row = pos.row;
                                cx.defer_in(window, move |_, window, cx| cb(row, window, cx));
                            } else if p.x >= l.fold_x && l.fold_starts.contains(&pos.row) {
                                let folded = v.is_folded(pos.row);
                                v.set_folded(pos.row, !folded, cx);
                            } else {
                                v.begin_selection(
                                    Pos::new(pos.row, 0),
                                    3,
                                    event.modifiers.shift,
                                    false,
                                    window,
                                    cx,
                                );
                            }
                        } else if event.modifiers.secondary() {
                            if let Some(cb) = v.cmd_click_callback() {
                                window.focus(&v.focus_handle);
                                v.set_cursor(pos, cx);
                                cx.defer_in(window, move |_, window, cx| cb(pos, window, cx));
                            }
                        } else {
                            v.begin_selection(
                                pos,
                                event.click_count,
                                event.modifiers.shift,
                                event.modifiers.alt,
                                window,
                                cx,
                            );
                        }
                        cx.stop_propagation();
                    }
                    MouseButton::Right => {
                        window.focus(&v.focus_handle);
                        let inside = v.selections().iter().any(|sel| {
                            let (s, e) = sel.range();
                            pos >= s && pos <= e
                        });
                        if !inside {
                            v.set_cursor(pos, cx);
                        }
                        v.deploy_context_menu(p, window, cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            });
        });
    }
    // Mouse move: drag selection, scrollbar/minimap drags, hover.
    {
        let view = view.clone();
        let hitbox = hitbox.clone();
        let layout = layout.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let p = event.position;
            let l = &layout;
            view.update(cx, |v, cx| {
                let left_down = event.pressed_button == Some(MouseButton::Left);
                if let Some(grab) = v.scrollbar_drag {
                    if left_down {
                        let track = l.vscroll_track();
                        if let Some((_, len)) = thumb_geometry(
                            f32::from(track.size.height),
                            f32::from(l.bounds.size.height),
                            f32::from(l.content_height()),
                            f32::from(l.scroll.y),
                        ) {
                            scrollbar_scroll_to(v, l, p.y, grab, len, cx);
                        }
                    } else {
                        v.scrollbar_drag = None;
                    }
                    return;
                }
                if v.minimap_drag {
                    if left_down {
                        if let Some(mb) = l.minimap_bounds {
                            minimap_scroll_to(v, l, mb, p.y, cx);
                        }
                    } else {
                        v.minimap_drag = false;
                    }
                    return;
                }
                if v.is_selecting() {
                    if !left_down {
                        v.end_selection();
                        return;
                    }
                    // Autoscroll when dragging outside the text area.
                    let tb = l.text_bounds;
                    let mut delta = point(px(0.), px(0.));
                    if p.y < tb.top() {
                        delta.y = (tb.top() - p.y).min(l.line_height * 3.);
                    } else if p.y > tb.bottom() {
                        delta.y = -(p.y - tb.bottom()).min(l.line_height * 3.);
                    }
                    if l.wrap_cols.is_none() {
                        if p.x < tb.left() {
                            delta.x = (tb.left() - p.x).min(l.char_width * 4.);
                        } else if p.x > tb.right() {
                            delta.x = -(p.x - tb.right()).min(l.char_width * 4.);
                        }
                    }
                    if delta != point(px(0.), px(0.)) {
                        v.scroll_by(delta, cx);
                    }
                    let clamped = point(p.x, p.y.clamp(tb.top(), tb.bottom() - px(1.)));
                    if let Some(pos) = v.pos_for_point(clamped) {
                        v.extend_selection(pos, cx);
                    }
                    return;
                }
                let in_gutter = hitbox.is_hovered(window) && l.gutter_bounds.contains(&p);
                if in_gutter != v.gutter_hovered {
                    v.gutter_hovered = in_gutter;
                    cx.notify();
                }
                let over_text = hitbox.is_hovered(window)
                    && l.text_bounds.contains(&p)
                    && !l.vscroll_track().contains(&p);
                let pos = if over_text { v.pos_for_point(p) } else { None };
                v.hover_moved(pos, p, window, cx);
            });
        });
    }
    // Mouse up.
    {
        let view = view.clone();
        window.on_mouse_event(move |_: &MouseUpEvent, phase, _window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            view.update(cx, |v, cx| {
                if v.is_selecting() || v.scrollbar_drag.is_some() || v.minimap_drag {
                    v.end_selection();
                    v.scrollbar_drag = None;
                    v.minimap_drag = false;
                    cx.notify();
                }
            });
        });
    }
}

fn scrollbar_scroll_to(
    v: &mut CodeView,
    l: &LayoutSnapshot,
    y: Pixels,
    grab: Pixels,
    thumb_len: f32,
    cx: &mut gpui::Context<CodeView>,
) {
    let track = l.vscroll_track();
    let travel = track.size.height - px(thumb_len);
    if travel <= px(0.) {
        return;
    }
    let ratio = ((y - grab - track.top()) / travel).clamp(0., 1.);
    v.scroll.y = l.max_scroll.y * ratio;
    cx.emit(CodeViewEvent::Scrolled);
    cx.notify();
}

fn minimap_scroll_to(
    v: &mut CodeView,
    l: &LayoutSnapshot,
    mb: Bounds<Pixels>,
    y: Pixels,
    cx: &mut gpui::Context<CodeView>,
) {
    let d = l.minimap_top_row as f32 + ((y - mb.top()) / px(MINIMAP_ROW)).max(0.);
    let target = l.line_height * d - l.bounds.size.height / 2.;
    v.scroll.y = target.clamp(px(0.), l.max_scroll.y);
    cx.emit(CodeViewEvent::Scrolled);
    cx.notify();
}
