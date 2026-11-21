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
6. **An agent write that cannot be undone is refused.** `fs/write_text_file` is refused when
   its backup fails (shadow repository, or in a folder without git the per-turn file backup):
   a turn stays undoable as one step everywhere. The document says "confined, journaled and
   undoable", not "permission-gated": asking before a write remains the agent's choice (see
   known gaps).
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
- Whether Lector itself asks the user before an agent's first write in a session.

## Rejected
- *Fix the agent path check in place*: three implementations already drifted once.
- *Mark network commands `UiOnly`*: breaks `lector run git.fetch` in scripts, which a person
  starts.

## Checked by
Tests named in each item; `arch-check dupes` for confinement; the registry source test;
`lector-archcheck` bypass tests.

## Clarifications at implementation
Recorded by the architect when reviewing the implementation, before merge.

- **Confinement** is `lector_fs::confine::Confinement` (`check`, `read`, `write`), with one
  configured instance for commands, agents and MCP (`lector_app::commands::confinement`) that
  also keeps callers out of Lector's own data and config directories. On Unix a confined write
  walks the path with `openat(O_NOFOLLOW)` and renames within the opened directory, so a link
  swapped in after the check fails instead of being followed. On Windows directories and the
  target are re-checked right before the rename; a short race window remains and is
  documented. The remaining `confine*` functions are thin delegates listed in
  `arch/one-impl.toml`.
- **Tokens for user-listed hosts** come from `LECTOR_FORGE_TOKEN_<HOST>`, from `gh` / `glab`
  asked for that host with the shared token variables removed from their environment, or from
  the keychain entry for the host. `GH_ENTERPRISE_TOKEN` is no longer read (it is shared
  across hosts). A token is never sent over plain HTTP except to loopback, redirects never
  carry it, and the authenticated fetch does not follow redirects.
- **Sources are derived.** `CommandSpec::touches` (network, credentials, steers an agent,
  answers for the user) gives a minimum, and `allowed_sources()` is the stricter of that and
  the declared sources. `agent.send_comments` is `Interactive` like `agent.prompt`. An
  exhaustive table test in `lector-cli` fails for an unclassified command.
- **Write jobs** carry a ticket (queued / started / revoked). `JobKind::is_write()` is the one
  predicate behind replacement, subject cancellation, drop behaviour, quit policy and
  run-once-queued. Git writes run on a serial lane keyed by repository, in submission order,
  on their own threads. `TerminalSessionsSave` uses the same lane per panel so an older save
  cannot land after a newer one. Undoing an agent turn is its own kind and drains fully at
  quit. Git writes started through `invoke_async` (palette, CLI, MCP) are not on the serial
  lane; git's own index lock orders them, as before.
- **Agent writes in a folder without git are refused** (there is no shadow repository to back
  them up into). This is narrower than 0.7.1, which wrote there without undo. A per-turn file
  backup for such folders is not built; it needs its own record.
- **Owner-only data** is created through `lector_base::private` (directories `0700`, files
  `0600`); shadow repositories use `core.sharedRepository=0600`. Start narrows an existing
  wider data directory and logs it.
- **Guardrails.** The write API of `lector-git` is "every public function not listed as a
  read" (`arch/git-read.allow`), so a new function is a write until classified. The stronger
  rules found 20 existing violations the old rules could not see; they are in the baseline
  under "rule changed, not code" and are the next debt to pay: undo performed and called
  outside commands, write-capable repository handles handed to callers, two file writes in
  UI code, one patch apply outside a command, and a second debounce in the file watcher.
  `--allow-increase` remains for this one case (a rule gets stronger) and needs a reason.
