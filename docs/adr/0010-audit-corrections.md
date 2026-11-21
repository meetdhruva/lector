# 0010 — Corrections after the independent audit

## Problem
An independent audit of steps 0–7 found places where the result is weaker than the records
claim. Measured: an agent can write into `.git` through a symlink inside the project
(`resolve_in_root` checks `.git` on the lexical path only); a remote on any host named
`github.*` / `gitlab.*` receives the user's github.com / gitlab.com token; a queued git write
is dropped when the panel that started it closes; path confinement exists three times with
different rules; one command of about a hundred is `UiOnly`; several guardrail rules are
defeated by an import alias. The first two are also in released 0.7.1.

## Decision
1. **One confinement function.** `lector_fs::confine` (new module) is the only code that
   decides whether a caller-supplied path is inside a root. It works on the *resolved* path:
   every existing ancestor is canonicalized; the result must be inside the canonical root; no
   component of the resolved path relative to the root may be a git directory name (`.git`,
   case-insensitive, and Windows short names); for a write, the final component may not be a
   symlink, and the write does not follow one. Agents, MCP and command path arguments all call
   it; their own implementations are deleted. `arch-check dupes` learns the names.
2. **Tokens go only to the host they belong to.** A host is a forge only if it is exactly
   `github.com` / `gitlab.com` (and `www.`) or listed by the user in `[forge]`. The
   `GITHUB_TOKEN`/`GH_TOKEN` and `GITLAB_TOKEN` environment tokens are sent only to
   `github.com` / `gitlab.com`; a user-listed host needs its own token (host-specific
   environment variable, `gh`/`glab` for that host, or the keychain entry for that host).
3. **Command sources are three-valued:** `All`, `Interactive` (UI and CLI: a person at a
   keyboard chose it), `UiOnly`. Commands that use the network or the user's credentials
   (`git.fetch`, `git.pull`, `git.push`, forge commands) are `Interactive`: never MCP or
   agents, with or without `--allow-writes`. Commands that answer for the user
   (`agent.answer_permission`) or start or steer an agent (`agent.start`, `agent.prompt`,
   `agent.cancel`) are `Interactive` too, except `agent.answer_permission`, which is `UiOnly`.
   A test enumerates the registry and fails for a command whose spec says network or
   credentials and whose sources are `All`.
4. **A write the user asked for runs.** A job whose kind is a write is not skipped because its
   token was cancelled before it started, and dropping a `BackgroundTask` of a write kind
   detaches it instead of cancelling. Cancellation of a write is only ever explicit
   (`cancel_key`), never a side effect of a view closing.
5. **Writes that run other people's programs do not sit on compute workers.** `GitWrite` (hooks,
   signing, pinentry) moves to its own serial-per-repository lane (one writer per repository
   at a time, which git requires anyway; not bounded by the compute workers). `DocumentSave`
   stays on compute (bounded by file size) unless a before-save hook calls a language server,
   in which case that wait is a blocking-lane job.
6. **Agent writes are gated by Lector, not by the agent's manners.** `fs/write_text_file` is
   refused when the backup into the shadow repository fails (in a folder without git: when
   the per-turn file backup fails; a turn must stay undoable as one step everywhere). The
   first write of a session to a path outside the files the user attached asks the user once
   per session ("allow edits in this project": once / for this session / reject) unless the
   user enabled `agent.auto_allow_writes` (user-level setting, default off).
7. **Lector's data is private.** The data directory and everything created under it (shadow
   repositories, logs, crash reports, session records) are owner-only: directory `0700`,
   files `0600` on Unix; an existing directory with wider permissions is narrowed once at
   start and the log says so.
8. **Guardrails resolve aliases.** `threads`, `writes`, `background` and `state` resolve
   `use … as …` and module aliases; `purity` allowances name a static *and* its type; `writes`
   covers every crate above `lector-git` except `lector-app` command modules, scans all of
   `lector-git/src`, and treats a write method added anywhere there as write API; `state`
   follows a path built with `push`/`format!`. Each bypass the audit found becomes a test in
   `lector-archcheck`.

## Not decided here (recorded as known gaps, each needs its own record)
- Saving a document and agent file writes are write paths beside the command registry, with
  their own backup and journal logic. Principle 4 is restated to say so.
- Forge and pull-request logic lives in a GPUI crate because `lector-app` may not depend on
  `lector-forge`; it should move behind an app service.
- Language servers found on `PATH` start when a file opens and read project configuration.
- Whether a project's settings may turn on auto-save and format on save.

## Rejected
- *Fix the agent path check in place*: three implementations already drifted once.
- *Mark network commands `UiOnly`*: breaks `lector run git.fetch` in scripts, which a person
  starts.

## Checked by
Tests named in each item; `arch-check dupes` for confinement; the registry source test;
`lector-archcheck` bypass tests.
