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

## Clarifications at implementation
Recorded by the architect when reviewing the implementation, before merge.

- **Job keys are closed.** `JobKey { kind: JobKind, subject: Subject }`; `JobKind` is one enum
  in `lector_app::scheduler::key` and declares, per kind: its lane, whether a newer job
  replaces an older one, whether closing its subject cancels it, whether it may run at
  Interactive, and what happens to it at quit.
- **Three lanes.** *Compute*: `max(cpus - 1, 2)` workers plus one worker reserved for
  Interactive kinds (DocumentParse, PickerRank, FindInFile only; any other kind asking for
  Interactive runs at Visible). *Blocking*: waits whose duration someone else controls
  (network, language servers, external programs), at most 16 threads, queued by priority.
  *Guaranteed*: kinds that other jobs or a person wait on (askpass prompts, agent requests and
  controls, terminal kill, agent turns) start at once and never queue behind a bound. A job on
  one lane never runs inline on another; a compute worker waiting on a compute job runs it
  inline.
- **The bridge returns `BackgroundTask<T>`**, a future of `Result<T, JobError>` (cancelled,
  panicked), not GPUI's `Task<T>`. Dropping it cancels the job synchronously; the token is
  checked again on the main thread before delivery, so a stale result is never delivered.
  `detach()` lets a job finish unobserved.
- **Subjects close with their owner.** Dropping a document, a repository state or a view
  cancels its subject. Kinds that write are never replaced and are not cancelled by closing a
  subject.
- **Quit drains writes.** Order: cancel every job whose kind is not a write and refuse new
  ones; let writes finish while child processes are alive (10 s; network writes 2 s); stop
  child process trees; stop the scheduler. Writes abandoned at the bound are named in the log
  and the crash log. Before 0.8, quit stopped child processes at once and waited for nothing.
- **Below `lector-app`**, an owner thread that has short work to hand off calls a
  caller-supplied `lector_base::thread::TaskRunner` (askpass connections, language-server
  launches, terminal kill escalation, agent requests); helpers that live inside one call use
  `std::thread::scope`.
- **Panics** in a job are caught, the waiter gets `JobError::Panicked`, the worker survives,
  and the process panic hook writes the crash report once.
- **Not UI-thread work:** a git or file-system call that starts a process or reads more than a
  few small files does not run in render, menu building or an action handler; it goes through
  the bridge and the view reads a cached value.
