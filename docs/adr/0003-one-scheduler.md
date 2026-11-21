# 0003 — One scheduler for background work

## Problem
`lector_app::scheduler` (priorities, keyed cancellation, debounce) is used only by the git
service. UI code calls GPUI's background executor about 100 times, and there are about 45
direct `thread::spawn` calls across crates. Nothing can prioritise a keystroke-driven job over
an indexing job, and cancelling "everything for this document/repo" is not possible.

## Decision
1. **Short background work runs on the `lector-app` scheduler** with a priority
   (`Interactive > Visible > Background > Idle`) and a `JobKey` (kind + subject) so newer work
   replaces older work with the same key and whole subjects can be cancelled.
2. **One GPUI bridge** in `lector-ui` (`background(cx, key, priority, f) -> Task<T>`) is the
   only way UI code starts background work; it runs on the scheduler and delivers the result
   on the main thread. Direct `background_spawn`/`background_executor` use is removed from
   feature code (timers and pure UI animation stay on GPUI).
3. **Owner threads are allowed but named**: a thread that *is* a long-lived resource (terminal
   PTY reader/writer, language-server reader, askpass listener, file watcher, the scheduler's
   own workers) is created through `lector_base::thread::spawn_named(kind, name, f)`, which
   registers it for diagnostics and shutdown. The list of kinds is closed and lives in
   `arch/threads.allow`.
4. Core crates below `lector-app` do not schedule: they expose blocking, cancellable
   functions (`rayon` inside one call is fine).

## Rejected
- *Use GPUI's executor everywhere*: core crates cannot depend on GPUI, and the CLI/MCP need the
  same scheduling.
- *tokio*: forbidden in core crates by the project rules; nothing here needs it.

## Checked by
`arch-check threads`: `thread::spawn`/`Builder::spawn` only in `lector-base` and the scheduler;
`spawn_named` kinds ⊆ allowlist. `arch-check background`: `background_spawn` /
`background_executor()` only inside the bridge module.
