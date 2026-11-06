//! Interface scale (`ui.scale`, View > Zoom UI In / Out / Reset).
//!
//! Lector's chrome is laid out in logical pixels (`px(..)`), not in rems, so GPUI's rem
//! size alone cannot scale it. Instead every view of this crate and of `lector-ui-git`
//! imports [`px`] from here, which multiplies by the current scale; the metrics tokens in
//! [`crate::components::metrics`] do the same. The window's rem size follows the scale as
//! well, so anything written in rems scales too.
//!
//! Not scaled, on purpose: window geometry (`lib.rs`), the code view's internals
//! (`code_view/`: its size comes from the code font zoom, `buffer_font_size` and
//! View > Zoom In / Out) and raster image pixels.
//!
//! The scale lives in a thread-local: GPUI builds and paints every view on its main
//! thread, and tests (one thread each) cannot disturb one another.

use std::cell::Cell;

use gpui::{App, Pixels, Window};

/// Smallest supported scale.
pub const MIN_SCALE: f32 = 0.75;
/// Largest supported scale.
pub const MAX_SCALE: f32 = 2.0;
/// The zoom steps View > Zoom UI In / Out walks through.
pub const STEPS: [f32; 8] = [0.75, 0.875, 1.0, 1.125, 1.25, 1.5, 1.75, 2.0];
/// GPUI's default rem size; scaled together with the interface.
pub const BASE_REM: f32 = 16.0;

thread_local! {
    static SCALE: Cell<f32> = const { Cell::new(1.0) };
}

/// The current interface scale (1.0 = 100 %).
pub fn scale() -> f32 {
    SCALE.with(Cell::get)
}

/// Sets the interface scale, clamped to [`MIN_SCALE`]..=[`MAX_SCALE`] (NaN becomes 1.0).
pub fn set_scale(scale: f32) {
    let scale = if scale.is_finite() {
        scale.clamp(MIN_SCALE, MAX_SCALE)
    } else {
        1.0
    };
    SCALE.with(|s| s.set(scale));
}

/// `value` logical pixels at the current interface scale. Drop-in replacement for
/// [`gpui::px`] in every view that is part of the scaled interface.
pub fn px(value: f32) -> Pixels {
    gpui::px(value * scale())
}

/// The next larger zoom step after `current` (stays at the maximum).
pub fn zoom_in(current: f32) -> f32 {
    STEPS
        .iter()
        .copied()
        .find(|s| *s > current + 0.001)
        .unwrap_or(MAX_SCALE)
}

/// The next smaller zoom step before `current` (stays at the minimum).
pub fn zoom_out(current: f32) -> f32 {
    STEPS
        .iter()
        .rev()
        .copied()
        .find(|s| *s < current - 0.001)
        .unwrap_or(MIN_SCALE)
}

/// Applies the scale of `settings` to the process (called whenever settings load).
pub fn apply_from_settings(settings: &lector_config::Settings) {
    set_scale(settings.ui.clamped_scale());
}

/// Keeps the window's rem size in step with the scale; call once per frame from the
/// workspace root.
pub fn sync_window(window: &mut Window) {
    let rem = gpui::px(BASE_REM * scale());
    if window.rem_size() != rem {
        window.set_rem_size(rem);
    }
}

/// Changes the scale at runtime (the zoom actions): updates the process scale and the
/// `ui.scale` of the settings global so a later settings reload does not undo it, then
/// redraws every window.
pub fn set_runtime_scale(new_scale: f32, cx: &mut App) {
    set_scale(new_scale);
    let applied = scale();
    if cx.has_global::<crate::settings::AppSettings>() {
        let current = crate::settings::AppSettings::get(cx);
        let mut settings = (*current).clone();
        settings.ui.scale = applied;
        let warnings = cx
            .global::<crate::settings::AppSettings>()
            .warnings
            .as_ref()
            .clone();
        cx.set_global(crate::settings::AppSettings {
            settings: std::sync::Arc::new(settings),
            warnings: std::sync::Arc::new(warnings),
        });
    }
    cx.refresh_windows();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn px_follows_the_scale() {
        set_scale(1.0);
        assert_eq!(f32::from(px(10.)), 10.0);
        set_scale(1.5);
        assert_eq!(f32::from(px(10.)), 15.0);
        set_scale(2.0);
        assert_eq!(f32::from(px(13.)), 26.0);
        set_scale(1.0);
    }

    #[test]
    fn scale_is_clamped() {
        set_scale(0.1);
        assert_eq!(scale(), MIN_SCALE);
        set_scale(9.0);
        assert_eq!(scale(), MAX_SCALE);
        set_scale(f32::NAN);
        assert_eq!(scale(), 1.0);
    }

    #[test]
    fn zoom_steps_walk_the_ladder_and_stop_at_the_ends() {
        assert_eq!(zoom_in(1.0), 1.125);
        assert_eq!(zoom_in(1.3), 1.5);
        assert_eq!(zoom_in(2.0), 2.0);
        assert_eq!(zoom_out(1.0), 0.875);
        assert_eq!(zoom_out(0.75), 0.75);
        // Every step is reachable from the default.
        let mut s = 1.0;
        for _ in 0..10 {
            s = zoom_in(s);
        }
        assert_eq!(s, MAX_SCALE);
    }
}
