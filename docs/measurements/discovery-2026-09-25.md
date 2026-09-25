# Discovery efficiency evaluation

The release candidate reuses immutable catalog contracts, names the actual
callable execution tool, and recognizes CI and PR vocabulary. The complete
bootstrap contract stays published. An experimental schema-rich search option
was measured and rejected; it is not part of the release candidate.

## Instrument and baseline

The comparison uses Codex CLI with the same explicitly selected model,
medium reasoning, task definitions, and harness. Each run provisions its own
pinned disposable Gitea instance. Task success is verified against Gitea state;
missing prerequisites, client errors, and incomplete accounting are not passes.
The reports bind each executable, harness and wrapper through SHA-256 digests.

The [baseline](discovery-2026-09-25-baseline.json) passed all ten tasks with no
skips, 81 MCP calls, one tool error, and 387.1 seconds of summed task wall time.
The client reported 1,681,973 input tokens, including 1,407,616 cached input
tokens, and 4,368 output tokens. Cached input is a subset of input, not an
additional charge. The authenticated client does not report billed dollars,
so dollar cost is explicitly unavailable.

## Release decision and task comparison

The [final release run](discovery-2026-09-25-release.json) matches the baseline's
model, CLI, task set, harness and wrapper digests. Its executable digest matches
the smaller release binary used in the process measurements below.

| Client measurement | Baseline | Release candidate |
| --- | ---: | ---: |
| Successful tasks | 10/10 | 10/10 |
| Skipped tasks | 0 | 0 |
| MCP calls | 81 | 88 |
| Tool errors | 1 | 3 |
| Input tokens, including cached input | 1,681,973 | 1,777,076 |
| Cached input tokens | 1,407,616 | 1,538,048 |
| Output tokens | 4,368 | 4,547 |
| Summed task wall time | 387.1 s | 305.0 s |
| Billed dollar cost | unavailable | unavailable |

All tasks succeed, and summed wall time is lower, but total input tokens, calls
and errors are higher. This is not evidence of a model-token or error-rate win.
The decrease in uncached input does not establish billed savings because the
client supplies no dollar cost. A single pair is not a production latency study.

Ship the internal caching and directly tested discovery corrections: repeated
contracts are shared, actual callable routing is explicit, and CI/PR searches
find the intended operations. These address demonstrated defects without
replacing the tool surface. The local process measurements show the caching
benefit; the task report establishes preserved capability and its measured limits.
Do not promote the experimental search schema option or narrow bootstrap schemas.
The experiment's rejection is not a claim that it caused every observed regression:
the smaller release also reports more calls and errors than this baseline.

Per-task results are preserved in the linked reports, including recovery calls
and errors for tasks that ultimately succeeded. No failed or skipped task was
excluded from an aggregate.

## Schema-rich search experiment

The [experimental candidate](discovery-2026-09-25-rich-search.json) also passed
all ten tasks, with no skips. Its optional search schemas did not establish an
efficiency benefit against the fixed baseline:

| Client measurement | Baseline | Experimental candidate |
| --- | ---: | ---: |
| Successful tasks | 10/10 | 10/10 |
| Skipped tasks | 0 | 0 |
| MCP calls | 81 | 88 |
| Tool errors | 1 | 3 |
| Input tokens, including cached input | 1,681,973 | 1,715,839 |
| Cached input tokens | 1,407,616 | 1,418,880 |
| Output tokens | 4,368 | 4,760 |
| Summed task wall time | 387.1 s | 304.6 s |
| Billed dollar cost | unavailable | unavailable |

The client, model, reasoning setting, task set, harness digest and wrapper digest
match. The experiment completed faster, but required more calls, tokens and tool
errors. This single pair does not isolate the cause of each difference or prove a
causal latency effect. It does not support promoting the additional search option
under the repository's evaluation rule. The option was removed, while the cache
changes and directly tested discovery defects were retained for final validation.

The [experimental source patch](discovery-2026-09-25-rich-search.patch) recreates
the experiment from the release candidate's Rust source. It is evidence, not
runtime code. The reports identify the exact tested executables by digest.
The patch omits context lines; apply it with `git apply --unidiff-zero` in a
disposable checkout of this release to reproduce the experimental Rust source.

## Infrastructure failure record

The [first candidate run](discovery-2026-09-25-load-failed.json) passed four
of ten tasks. It then suffered timeouts in direct Gitea fixture setup and
verification, and incomplete client accounting. Direct fixture failures occur
outside the MCP service under evaluation. Host load was observed above 70
and later above 100 while multiple unrelated builds were active.

This run is retained as failure evidence. It is not a successful acceptance
run or a valid estimate of the candidate's speed. The completed experimental
comparison above used a fresh process after compilation and fixture recovery.

## Experimental process and RPC measurements

After compilation finished, the same instrument ran fresh experimental server
processes in before/after/after/before order. Each process measured first use and thirty warm
calls per case. Both executables came from the workspace integration-test build
with all features enabled. The records bind executable, source and instrument
digests. The candidate executable is the one exercised by the passing server
configuration and production-logging integration tests.

| Measurement | Before | After |
| --- | ---: | ---: |
| Process start to health readiness | 509–521 ms | 334–471 ms |
| Warm search median | 17.27–33.99 ms | 8.25–9.17 ms |
| Warm describe median | 17.94–19.42 ms | 8.98–10.16 ms |
| Warm tools/list median | 25.56–25.97 ms | 23.73–27.87 ms |
| Warm validated API read median | 10.02–14.99 ms | 13.80–22.11 ms |
| Server peak resident memory | 32,800–33,520 KiB | 31,180–31,248 KiB |
| Search RPC JSON bytes | 613 | 673 |
| Describe RPC JSON bytes | 1,487 | 1,547 |
| Validated-read RPC JSON bytes | 578–579 | 578–579 |
| tools/list RPC JSON bytes | 39,800–39,801 | 40,411–40,412 |
| Summed name/description/inputSchema bytes | 21,191 | 21,629 |
| Bootstrap name/description/inputSchema bytes | 13,342 | 13,342 |

Ranges span the two runs, not confidence intervals. Warm latency includes local
HTTP, server work, serialization and Python decoding. Search and describe improved
in both runs; the instrument shows no consistent tools/list improvement and no
validated-read improvement. It does not isolate validator execution from network
and scheduling overhead. Additional discovery fields increase response bytes.
The complete bootstrap schema is unchanged. These measurements belong to the
experimental executable and must not be attributed to the smaller release binary.

Memory comes from Linux process RSS/high-water readings. It is not an allocation
count and excludes client memory. JSON bytes include the RPC envelope and exclude
SSE framing; they are not model tokens. These are debug-build, loopback observations,
not production capacity or tail-latency estimates. No task build ran concurrently,
but shared-host load remained present and is recorded.

Raw evidence: [before run 1](discovery-2026-09-25-before-1.json),
[before run 2](discovery-2026-09-25-before-2.json),
[after run 1](discovery-2026-09-25-after-1.json),
[after run 2](discovery-2026-09-25-after-2.json), and
[order/load metadata](discovery-2026-09-25-measurement-order.json).
The records also include initialization and first-use latency for each case.

Reproduce with the same script against each preserved binary and its source tree:

```sh
python3 scripts/measure_discovery.py /absolute/path/to/mcp-gitea-rs --source-root /absolute/path/to/source
```

## Release process and RPC measurements

The smaller release binary was rebuilt and passed the workspace tests before
the same instrument repeated before/after/after/before. No compilation ran
concurrently. Both after records match the release executable and source digests;
the baseline executable and instrument are unchanged.

| Measurement | Before | Release candidate |
| --- | ---: | ---: |
| Process start to health readiness | 221–271 ms | 145–169 ms |
| Warm search median | 15.39–16.21 ms | 3.78–7.36 ms |
| Warm describe median | 12.95–13.11 ms | 2.77–5.18 ms |
| Warm tools/list median | 15.40–19.24 ms | 8.23–16.21 ms |
| Warm validated API read median | 7.65–8.37 ms | 6.87–7.93 ms |
| Server peak resident memory | 33,220–33,528 KiB | 31,000–31,312 KiB |
| Search RPC JSON bytes | 613 | 673 |
| Describe RPC JSON bytes | 1,487 | 1,547 |
| Validated-read RPC JSON bytes | 578–579 | 578–579 |
| tools/list RPC JSON bytes | 39,800–39,801 | 40,003–40,004 |
| Summed name/description/inputSchema bytes | 21,191 | 21,290 |
| Bootstrap name/description/inputSchema bytes | 13,342 | 13,342 |

Search and describe are faster in both release runs and peak server memory is
lower. Tools/list and validated-read ranges overlap; these small samples do not
establish a uniform improvement. The callable-routing field and clearer guidance
slightly increase response bytes. The earlier limitations concerning shared-host
load, debug builds, loopback timing, RSS and bytes apply equally here.

Raw release evidence: [before run 1](discovery-2026-09-25-release-before-1.json),
[before run 2](discovery-2026-09-25-release-before-2.json),
[after run 1](discovery-2026-09-25-release-after-1.json),
[after run 2](discovery-2026-09-25-release-after-2.json), and
[order/load metadata](discovery-2026-09-25-release-order.json).

## Capability constraint

Advanced bootstrap settings remain typed and fully visible. Hiding them in a
shorter default schema could make valid existing calls unavailable to clients
that validate against the published contract. The experiment therefore adds an
optional bounded schema view to discovery, preserving full describe and complete
bootstrap access. The measured outcome did not justify shipping that option.
The released surface retains summary search and full describe, including all
advanced bootstrap settings.
