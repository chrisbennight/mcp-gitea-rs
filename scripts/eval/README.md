# Surface evaluation harness

The model-visible surface is a contract with a non-deterministic consumer, so
changes to it are justified by measured agent task outcomes, not inspection
(DECISIONS.md, "Tool-surface changes are evaluation-gated"). This directory is
the instrument that decision requires.

`run_eval.sh` provisions a disposable Gitea container (the same pinned image
and pattern as the bootstrap live suite), starts this repository's server
against it, and drives a headless `claude` agent through outcome-phrased tasks
drawn from PLAN.md's agent workloads — repository lifecycle, change review, CI
operation, publication, access administration, fleet queries, triage, and
content read. Each task seeds its own fixtures over the Gitea API and is
verified the same way, so success means the outcome exists in Gitea, not that
the transcript looked plausible. The report carries, per task and in
aggregate: success, tool-call count, tool-error count, token usage, and cost.

## Running

```sh
scripts/eval/run_eval.sh                       # full set, report in scratch
scripts/eval/run_eval.sh --only triage         # one task while iterating
scripts/eval/run_eval.sh --record scripts/eval/baselines/my-change.json
scripts/eval/run_eval.sh --model claude-opus-5 # pin the comparison's model
```

The server serves one surface, so the report records that label as a constant
rather than as a choice a run could get wrong.

`--record` overwrites its destination. Name a new file: the reports under
`baselines/` are the decision evidence described below, and a run made today
is not comparable with them.

Explicitly opt-in: CI and `cargo test` never invoke this. It needs docker, a
`claude` CLI able to run headless, and it spends real model tokens — bounded
by a per-task turn ceiling and wall-clock timeout. Everything it creates is
disposable and torn down on exit; transcripts and the report are retained in
the printed scratch directory. Transcripts are the telemetry input for any
future workflow-tool consolidation.

## Recorded baselines

`baselines/` holds checked-in reports: one per surface from the comparison
that decided the surface, plus a replicate of each. A surface change requires
a result at worst neutral on task success and better on token cost or
tool-error rate; the recorded comparison met that rule for the layered
surface — identical success, fewer tool errors, lower cost, replicated — and
DECISIONS.md records the outcome. The `flat.json` reports describe a surface
this server no longer serves; they are kept because they are the evidence the
decision rests on.

Retiring that surface changed the harness itself, so the four decision reports
carry the harness digest from before the retirement. A run made today records a
different digest and is not a fifth sample of that comparison: it measures the
one surface this server now serves. Compare against the decision reports only
for direction, and re-baseline both arms if a future change needs a like-for-like
comparison.

Comparisons are honest only between runs with the same task set and model;
the report records both. A task whose precondition is unmet is reported
skipped with its reason, distinct from passed and failed; an errored agent
run is a failed task with a visible reason, never silently dropped. The
evaluated agent runs with only the Gitea MCP tools allowed, with every
outcome-capable built-in explicitly denied, without the harness's own
credentials in its environment, and under an isolated per-run configuration
home seeded with the auth credential alone — so ambient settings, memory,
plugins, and hooks cannot steer the measurement, and the CLI's own session
persistence dies with the run instead of retaining raw tool results. The
retained transcripts are redacted of token-shaped values before they touch
disk, and the report records the driving CLI version.

## Credentials

The disposable instance's credentials are generated per run, live only in the
scratch directory and process environment, and are never printed. The agent
receives them through its MCP configuration file in the scratch directory.
