//! Opt-in startup timing (`LECTOR_TRACE_STARTUP=1`): logs to stderr how long
//! after process start the first window frame was painted and presented.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use gpui::{canvas, AnyElement, IntoElement, Styled};

static PROCESS_START: OnceLock<Instant> = OnceLock::new();
static FIRST_PAINT_LOGGED: AtomicBool = AtomicBool::new(false);

/// Records the process start time. Call first thing in `main`.
pub fn mark_process_start() {
    PROCESS_START.get_or_init(Instant::now);
}

/// Whether `LECTOR_TRACE_STARTUP` is set (to anything but "0").
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("LECTOR_TRACE_STARTUP").is_some_and(|v| v != "0" && !v.is_empty())
    })
}

/// Milliseconds since [`mark_process_start`] (or since the first call).
pub fn elapsed_ms() -> f64 {
    PROCESS_START
        .get_or_init(Instant::now)
        .elapsed()
        .as_secs_f64()
        * 1000.0
}

/// Logs a startup milestone when tracing is enabled.
pub fn log(milestone: &str) {
    if enabled() {
        eprintln!("lector: startup: {milestone} at {:.1} ms", elapsed_ms());
    }
}

/// An invisible element that logs the first paint (and, one frame later, the
/// first presented frame). `None` once logged or when tracing is off.
pub(crate) fn first_paint_probe() -> Option<AnyElement> {
    if !enabled() || FIRST_PAINT_LOGGED.load(Ordering::Relaxed) {
        return None;
    }
    Some(
        canvas(
            |_, _, _| (),
            |_, _, window, _| {
                if FIRST_PAINT_LOGGED.swap(true, Ordering::Relaxed) {
                    return;
                }
                log("first paint");
                window.on_next_frame(|_, _| log("first frame presented (next frame start)"));
            },
        )
        .absolute()
        .size_0()
        .into_any_element(),
    )
}
