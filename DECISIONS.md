# Architectural decisions

## Published images target amd64 only

The supported container platform is amd64, so CI and the local image builder always request
`linux/amd64` and CI verifies the resulting image metadata before smoke testing
or publication. Runner architecture must not silently change the production
artifact. No ARM image or multi-architecture index is published because ARM runtime validation is not yet part of CI.

## The deployed specification is authoritative

Gitea 1.26.4's checked-in Swagger document defines the generated surface.
The official Go MCP server is used for behavioral comparison where surfaces
overlap, not as a completeness boundary.

## Generated operations are exhaustive, and reached through the lanes

Exhaustive generated operations are the coverage guarantee. None of them is
published as a tool of its own: a caller selects one through discovery and
executes it through the lane its risk class assigns, which is what keeps that
classification enforced rather than advisory.

The observation that once argued for the flat surface still holds on its own
terms: a model-visible surface is a prompt, its size is load-bearing, and
operations that differ only in wording compete for selection. What did not
hold, at the time, was the path from that observation to a replacement.
Swapping 467 working tools for a handful of dispatch tools is destructive and
hard to reverse, and the decision below requires such a change to be justified
by measurement — which, then, did not exist.

It exists now, recorded under that decision, and it is what authorised this
swap; judgement about surface size did not. The flat surface was removed
rather than kept behind a configuration mode, because a second surface nobody
serves is maintenance, and a second way to reach the same operations without
the lane classification.

Operations the specification itself marks deprecated stay executable through
their lane by exact name but appear in no listing or discovery view: a caller
holding the name keeps compatibility, while the near-duplicate no longer
competes with its canonical sibling for selection.

The load-bearing problems the flat surface created were fixed on the way here
and still hold: schemas carry no Go implementation detail, no two operations
share a summary, oversized payloads become handles rather than filling a
context window, and the `gitea-catalog:/index` resource plus `catalog.search`
and `catalog.describe` let a caller select an operation without loading every
schema.

This supersedes the schema-service decision and the original flat-surface
decision in turn. Each reversal is written down rather than quietly dropped,
and this one carries the measurement the earlier ones lacked.

## No raw-request tool

A generic HTTP, shell, or `tea` tool is rejected. It would accept caller-chosen
methods, paths, and bodies, hiding which operation is being performed and
requiring agents to rediscover Gitea's request syntax.

An earlier revision proposed a middle path: execution tools taking an
`operation_id` validated against the catalog, with arguments validated against
that operation's schema. That preserves the property that matters — the
operation is named explicitly, so it stays visible to gateway policy and audit
— without one tool per operation. It is now the entire execution surface: the
identifier is checked against the catalog, the
arguments against the operation's own generated schema, and the executed
operation's identity rides in the result metadata. The rejection of arbitrary
request construction is unaffected: a lane never accepts a caller-chosen
method, path, or unvalidated body.

## Risk classification is published on every call, enforced for lane calls

Every operation carries a generated risk classification — read, mutation,
destructive — plus administrative and credential-bearing markers, published on
two channels. Standard MCP annotations carry the hints a client already
understands: read-only, destructive, idempotent, open-world. The exact risk, the
administrative flag, the operation identifier, and the sensitive-result flag
have no standard annotation and ride in `org.cacahuate/`-prefixed `_meta`.
Every registered tool carries both channels, publishing quiet defaults where
it has nothing louder to say, so the exact-risk channel stays uniform for a
caller that classifies the surface through it — `server.version` includes the
`getVersion` operation id its ledger entry maps to it. For a catalog
operation the classification is also ENFORCED, because the per-risk lanes —
`api.read`, `api.mutate`, `api.destroy`, `api.admin` — are the only way to
execute one: an operation runs on the lane its risk class and administrative
flag assign it, and the wrong lane refuses it naming the right one. The
operation name is not accepted as a tool of its own, so nothing can route
around that classification.

An earlier decision here described lane enforcement as settled while nothing
was built, which made this document describe a system that did not exist. The
lanes exist now, and since the measurement below they are the only execution
path.

The consequence is worth stating plainly rather than leaving implicit: for
the hand-written tools, annotations remain advisory — a client that ignores
`destructive` will not be stopped by this server. Lane routing constrains
which lane may execute a catalog operation, not who may call a lane;
authorization
enforcement belongs at the gateway, which holds the policy and can now key it
on the lane name plus the operation identity a lane call names explicitly.

## Responses are bounded twice, for different reasons

Every response passing the upstream boundary is subject to a transport ceiling
sized for process safety. Beneath it sits a context-scale ceiling sized for a
reply a caller can actually read: a successful payload above it is not inlined,
and the reply that replaces it is itself held to that ceiling as the transport
serializes it. That reply carries a handle when the store retains the payload
and an accounting of what was dropped when it cannot — the store has its own
per-object and aggregate caps, so a displaced payload is addressable in the
ordinary case rather than in every case.

The context ceiling applies to generated and hand-written tool results. All
paths use the same result-fitting step, and retained resources preserve the
sensitivity classification of their originating result.

One limit could not do both jobs. A bound large enough to keep the process alive
is far too large to keep a reply readable, and a bound small enough to keep a
reply readable would reject payloads a caller legitimately wants. Two ceilings
with an addressable band between them refuses nothing and floods nothing.

An unbounded response spends the caller's context on data it did not choose. A
response that becomes an error spends a round trip and, for a mutation, invites
repeating a call that already happened — so a payload that cannot be retained is
still reported as the success it was.

## Tool-surface changes are evaluation-gated

Changes to the model-visible surface are justified by measured agent task
outcomes on a held-out task set, not by inspection. Reported metrics include
task success, tool-call count, token cost, and tool-error rate.

The surface is a contract with a non-deterministic consumer. Reasoning about
whether an agent will select the right tool is unreliable in both directions:
plausible-looking consolidations regress, and changes that look risky measure
clean. Without a baseline, a redesign is an unfalsifiable claim.

This decision has teeth: it is the reason the layered surface was dropped rather
than shipped on judgement, and it is the rule the eventual comparison answered
to. The measured comparison ran the same task set, model, and instrument
digest against both surfaces, replicated: task success was identical, the
layered surface produced fewer tool errors in every run, and it cost less in
every run — the recorded rule's condition for a default change is met, with
the runs preserved in `scripts/eval/baselines/`. The operator took that
decision, and the flat surface was retired with it.
Fixing a demonstrable defect — a schema carrying Go struct names, two tools
sharing one summary, a payload nobody can read — is not a surface redesign
and is not gated on measurement. Replacing the surface is, and the
measurement that gates it now exists on the record.

## Published input schemas apply no combinator at their root

A published tool states its arguments with `type`, `properties`, and `required`,
and never applies `oneOf`, `anyOf`, `allOf`, or `not` to the schema root. A
client whose tool definitions must satisfy the Anthropic API rejects a
root-level combinator and drops that one tool while registering the rest of the
server. The capability then disappears with nothing failing anywhere this
server or the gateway can observe, which makes the construction far more
expensive than the validation it buys.

A required choice between arguments is enforced where those arguments are
parsed instead, refusing the same calls one layer later. Nesting a combinator
under a property is accepted and stays available; only the root is constrained.
Generated catalog operations are unaffected because the lanes validate against
them rather than publishing them as tool definitions.

## Separate upstream authentication lanes

Ordinary calls use a service PAT. Access-token lifecycle calls use separately
configured Basic Auth credentials. Caller input can never select or supply an
upstream credential. Both Basic Auth values may be omitted for PAT-only use;
a partial pair is a configuration error. Token tools stay discoverable and
report the missing configuration. Bootstrap checks whether token administration
is needed before sending any upstream request, so optional credentials cannot
cause a known failure after repository creation.

## Secret input uses the MCP file-transfer boundary

`repository.secret.set_from_file` is the dedicated file-based secret-input
workflow. Its value is an MCP file argument, not an inline string: the gateway obtains a
short-lived upload authorization, transfers bytes directly, and replaces its
own file URI with an opaque single-use staged reference before calling the
tool. The service verifies optional size and SHA-256 metadata, holds the value
only in bounded process memory, spends the reference once, and never returns
the value.

The upload endpoint is authorized by its own random single-use credential, not
the gateway bearer. That credential grants only one upload of at most 64 KiB,
expires after 60 seconds, is compared in constant time, and is never a tool
argument. The service caps outstanding tickets and staged values together.
Deterministic repository and secret-name validation happens before the staged
reference is consumed; the upstream mutation is submitted once and preserves
the generated operation's ambiguous-outcome reporting.

This is a narrow addition to the model-visible surface rather than a second API
execution path. It exists because the generated operation accepts an inline
secret value, which violates the model-boundary handling required for governed
credential provisioning. The ordinary generated operation remains available
through its risk lane for exhaustive API coverage and compatibility.

## Code Mode is external

If implemented, Code Mode belongs primarily in the gateway. This server
provides the typed operation catalog and structured outputs that a connector
generator consumes, but it does not embed an arbitrary-code runtime. A
generator reads the published tool list and its schemas; the generated catalog
under `generated/` carries the same operations with their risk classification
and is the richer input, but it is a build artifact rather than a served
surface.
