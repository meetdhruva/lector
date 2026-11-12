# Risk-ordering evaluation

How well does `lector_workspace::risk` put the file a reviewer must see first at the top of a changeset?
The review queue (`lector_app::review_queue::analyze`) is scored with top-1, top-3 and MRR (mean
reciprocal rank of the first risky file).

```sh
cargo run -p lector-app --example risk_eval                 # overall, tune/holdout, per category
cargo run -p lector-app --example risk_eval -- --misses     # cases whose risky file is not first
cargo run -p lector-app --example risk_eval -- --explain py-tls   # score and reasons of every file of a case
cargo run -p lector-app --example risk_eval -- --local      # also score imported pull requests
cargo run --release -p lector-app --example risk_fit        # refit the weights on the tune split
cargo test -p lector-app --test risk_eval                   # the floors (CI)
```

## Cases (`cases/*.json`)

One file per changeset:

```json
{ "id": "py-tls-verify-disabled", "split": "tune", "category": "security", "language": "python",
  "description": "...", "risky": ["app/http_client.py"],
  "files": [ { "path": "app/http_client.py", "base": "<text or null>", "head": "<text or null>" }, ... ] }
```

`base`/`head` are the whole file before and after (`null` = the file does not exist on that side: added or
deleted). `risky` lists the paths a reviewer must see first (any of them counts). All 60 cases are original,
synthetic snippets written for this set (no third-party code, nothing to license), 3-5 files each: one small
risky change in a mixed changeset next to decoys (big mechanical refactors, added public API, new tests,
docs, lockfiles, generated files). Ten categories: api-break, error-handling, security, unsafe-exec,
concurrency, migration, deps-ci, tests-removed, config-flip, hard-negative (decoys with scary paths or keywords
that are harmless).

The 70/30 split is fixed: the cases are numbered `01-` to `60-` in the file names and the cases at positions 2, 5
and 8 of every ten are `holdout` (18), the rest `tune` (42). The weights are fitted on `tune` only.

## Weights and fitting

`RiskWeights` (in `lector-workspace/src/risk/weights.rs`) has one base weight per signal. `baseline()` is the hand-set
start, `tuned()` (the `Default`) the result of `examples/risk_fit.rs`: deterministic coordinate search in
declaration order over factors 0.6x-1.5x of the start weight, accepting a move that improves tune top-1, then MRR, then
a hinge loss on the margin between the best risky file and the best other file (plus a small pull towards the start
weights), with a few ordering constraints (removing an export is at least as risky as changing it...). It moved two
weights. `-- --flat` starts from a neutral set with unbounded factors to show what the signals carry on their own.

## Importing real pull requests (`scripts/risk-eval-import.sh`)

```sh
scripts/risk-eval-import.sh OWNER/REPO 1234 --risky src/db/query.rs
cargo run -p lector-app --example risk_eval -- --local
```

Needs `gh` (REST calls only) and `jq`. Writes `local/OWNER__REPO-1234.json` (git-ignored: imported code carries its
project's license and is never committed) with the base and head content of up to 80 text files (< 200 kB each).
Cases without `--risky` are only listed, not scored; add the label by editing the JSON. Local cases never count
towards the committed numbers or the floor test.

## Limits (read before trusting the numbers)

* The cases were written by the same author as the signals, and the holdout cases were visible while the signals were
  designed: the holdout split guards against regressions, it is not an unbiased estimate of accuracy.
* The decoys are synthetic; real changesets have more, subtler and correlated changes. 60 cases are far from the
  200 real pull requests the plan asks for: none of the importer's output has been scored yet (the importer itself was
  only exercised against a mocked `gh`).
* Some risks have no syntactic signal (a limit raised from 100 to 1000, logic inverted); they are expected misses.
