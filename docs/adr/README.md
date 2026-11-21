# Architecture decision records

One short record per structural decision. A record is written **before** the code and is the
contract the code is reviewed against. Records are never rewritten after the fact: a changed
decision gets a new record that supersedes the old one.

Each record has: the problem (with measurements), the decision, what was rejected and why,
and how the result is checked automatically (`scripts/arch-check`, see `arch/`).

| # | Decision | Status |
|---|---|---|
| [0001](0001-one-app-layer.md) | One stateful app layer; review analysis is a stateless crate | Accepted |
| [0002](0002-one-write-path.md) | Every write is a registry command | Accepted |
| [0003](0003-one-scheduler.md) | One scheduler for background work | Accepted |
| [0004](0004-one-state-store.md) | One versioned state store | Accepted |
| [0005](0005-base-crate.md) | One cancel token and one fuzzy matcher, in `lector-base` | Accepted |
| [0006](0006-file-size.md) | Source files stay under 1,500 lines | Accepted |
| [0007](0007-architecture-doc.md) | The architecture document describes the present and is checked | Accepted |
| [0008](0008-guardrails.md) | Structural rules are enforced by a ratchet in CI | Accepted |
| [0009](0009-security-posture.md) | Security posture carried over from 0.1–0.7 (write policy, agents, updates) | Accepted |
| [0010](0010-audit-corrections.md) | Corrections after the independent audit (confinement, tokens, command sources, write jobs, guardrails) | Accepted |

## How work is done

- The architect writes the record and assigns file ownership; builders implement to it and may
  not change it. A builder who finds the record wrong stops and reports.
- One owner per file among agents working in parallel.
- A failing test comes before a fix.
- One step per pull request. Nothing merges with an open review finding or a red guardrail.
