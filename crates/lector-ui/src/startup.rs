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

/// Runs `work` as the very last step of quitting.
///
/// GPUI calls every quit hook in registration order and runs the synchronous part of each one
/// first; the futures they return are awaited afterwards (and after the windows are gone). Work
/// done inside the returned future therefore starts only once every hook has finished its own
/// synchronous work (sessions saved, terminals killed, the askpass server removed), however
/// late those hooks were registered. `lector` uses it for stopping child process trees and the
/// clean-exit marker.
pub fn on_quit_last(cx: &mut gpui::App, work: impl FnOnce() + 'static) {
    let work = std::cell::RefCell::new(Some(work));
    cx.on_app_quit(move |_| {
        let work = work.borrow_mut().take();
        async move {
            if let Some(work) = work {
                work();
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    #[gpui::test]
    fn quit_last_runs_after_the_other_hooks_whenever_registered(cx: &mut gpui::TestAppContext) {
        let log: Rc<RefCell<Vec<&'static str>>> = Rc::default();
        cx.update(|cx| {
            let l = log.clone();
            super::on_quit_last(cx, move || l.borrow_mut().push("shutdown + end_clean"));
            // Registered later, still runs first (like a window's session save).
            let l = log.clone();
            cx.on_app_quit(move |_| {
                l.borrow_mut().push("save sessions");
                async {}
            })
            .detach();
            let l = log.clone();
            cx.on_app_quit(move |_| {
                l.borrow_mut().push("kill terminals");
                async {}
            })
            .detach();
            cx.shutdown();
        });
        assert_eq!(
            *log.borrow(),
            vec!["save sessions", "kill terminals", "shutdown + end_clean"]
        );
    }
}
