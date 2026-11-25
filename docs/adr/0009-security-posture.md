# 0009 — Security posture carried over from 0.1–0.7

## Problem
Several rules that shape the product predate these records. Their reasons lived only in the old
architecture document, which step 7 replaced. Without a record they read as arbitrary limits
and are easy to relax by accident; 0.8 phase 2 will deliberately revisit one of them (agent
terminal access).

## Decision (as built; recorded, not changed)
1. **Writes follow an explicit user action, and are undone rather than confirmed.** A
   destructive operation backs up what it loses first and offers Undo; a confirmation remains
   only where a backup cannot be taken or the effect leaves the machine. Reason: confirmations
   are dismissed by habit and protect nothing; a backup protects even a mistaken "yes".
2. **Git writes go through the git CLI; reads through gix.** Reason: hooks, locks, filters,
   signing and credential helpers must behave exactly as in the user's terminal. Git write and
   shadow-repository commands use `std::process` directly rather than the supervisor so signing
   pinentries keep their terminal.
3. **Automation is opt-in; what runs or leaves the machine is user-level.** Auto-save, format
   on save, automatic fetch and update checks are off by default. A project's settings cannot
   turn on automatic fetch or update checks, nor choose a program to run or a host to talk to
   (language servers, agents, shell, forge). Reason: opening a repository must not run or send
   anything. As built, a project's settings *can* turn on auto-save and format on save; they
   act only on buffers the user edits, through the user's own formatter. Whether these two
   should become user-level as well was put to the owner on 2026-11-25, who decided to leave
   both project-settable; this is the boundary.
4. **Language servers may not write into the project.** They start with options that prevent
   it; their edits reach buffers, never files.
5. **Agents get file reads and writes inside the project and nothing else**: no terminal, no
   MCP servers, none of Lector's commands. Each write lands on disk at once, is journaled, and
   a turn is undone as one step. Reason for writing at once rather than holding edits in an
   overlay: the checkpoint, the journal and what the agent reads back must all describe the
   same files.
6. **Updates are notify-only.** Lector checks (when enabled), verifies the minisign signature
   of the release checksums against a compiled-in key, and tells the user; it never downloads
   or runs an update. Reason: a self-updater is a remote code execution path that a viewer does
   not need.
7. **Nothing is uploaded.** Crash reports and health data stay local.

## Rejected
- *Confirm every destructive action*: see 1.
- *An in-process git implementation for writes*: diverges from the CLI on hooks and signing.
- *Self-update*: see 6.

## Checked by
`tests/read_only.rs` and `tests/release_audit.rs` (reads never write), `tests/undo_roundtrip.rs`
(every write command), `lector_config` tests for user-only keys, `lector-agent` path
confinement tests, `lector_forge::releases` signature tests. Points 5 and 6 have no structural
guardrail: changing either needs a new record that supersedes this one.
