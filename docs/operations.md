# Operation and response reference

## Generated API catalog

The byte-for-byte source and its provenance are in
[`openapi/`](../openapi/SOURCE.md). Regenerate and verify the checked-in catalog
with:

```sh
python3 scripts/generate_api.py
python3 scripts/generate_api.py --check
```

`catalog.search` filters the operation catalog — the generated operations
and the hand-written tools — by keyword, domain, risk, and administrative
flag, and `catalog.describe` returns one operation's contract — at `full`
detail, its exact input schema — by tool name or upstream operation id. Both
answer from the process-local registry without an upstream call, and both
describe operations rather than the tool listing: the generated operations
they describe are executed through the lanes, while the hand-written tools
they describe — `server.version`, the access-token trio, and
`repository.bootstrap` and `repository.secret.set_from_file` — are called
directly.

A session lists thirteen tools: the four `api.read`, `api.mutate`,
`api.destroy`, and `api.admin` execution lanes, the discovery pair, and the
seven hand-written tools, including `result.select`. A lane runs any catalog operation named by tool name
or upstream operation id, validated against that operation's generated schema
before anything is sent, with lane routing enforced and the executed
operation's identity in the result metadata. Catalog operations are not
published individually — that surface was retired on the replicated
measurement recorded in `scripts/eval/baselines/` and DECISIONS.md — and an
operation name handed to `tools/call` is refused with the lane that runs it.

Generated tool names use a searchable `<domain>.<operation>` shape, such as
`repository.get`, `repository.create_branch`, and
`organization.create_team`. Tool arguments are closed JSON Schema objects.
High-use tools use domain-specific names where Gitea's operation IDs are
ambiguous, such as `repository.create_deploy_key`,
`repository.update_pull_request_branch`, and
`notification.mark_threads`. Their descriptions include compact use guidance,
search vocabulary, and example inputs for progressive tool discovery.
Nested objects reject unknown fields unless the upstream schema explicitly
declares a map. Successful results contain `status`, `content_type`, declared
response `headers`, and normalized `data`; declared binary responses are
Base64-encoded explicitly. HTTP failures remain structured tool results, while
request-construction, transport, decoding, and hard-bound failures become
normalized MCP errors.

Successful paginated operations and `access_token.list` also return a
`pagination` object with `next_page`, `total_count`, and `complete`. A null
value means that the upstream response did not establish that fact. A short
page alone does not prove completeness. Recognized continuation links are
reduced to page numbers only after checking the configured origin and endpoint;
the server never follows a returned URL. Pass `next_page` to the same typed
operation, retaining its other arguments. This metadata remains inline when
the payload is retained. Concurrent upstream changes can still alter pages;
pagination does not provide a snapshot of the repository.

Bootstrap looks for an existing named access token through successive pages,
including when Gitea caps pages below the requested size. It creates a token
only after an empty page establishes absence. The search stops after 20 pages,
with at most 100 entries per page and the configured upstream timeout per
request. A bound or page failure reports an incomplete workflow and never
authorizes token creation. Previously completed workflow steps remain in the
report.

A successful payload above the context-scale ceiling is not inlined. Job logs,
artifacts, and repository archives routinely run to megabytes, and placing one
in a reply costs the caller its working context. Above the ceiling the result
carries the same envelope plus a `payload` object naming the byte count and the
ceiling that was exceeded.

When the payload is retained, that object also carries a `gitea-response:` URI
and its media type, the result carries an MCP resource link, and a text payload
carries a leading excerpt; the payload is read back through `resources/read`.
When the store refuses it — over the per-object cap, or the shared budget is
full — the result stays a success with `retained: false` and a detail saying
why, and carries no URI, link, or excerpt, because there is nothing to follow.
The call itself completed either way.

Use `result.select` with a retained URI to retrieve evidence without loading the
whole result. Text mode accepts a UTF-8 byte offset and a byte limit. Search mode
finds the next literal match with bounded surrounding lines; `next_offset`
continues after that match, so repeated calls make progress even when context
overlaps. JSON mode accepts an RFC 6901 pointer, an array row offset, and exact
object field names. It supports JSON payloads up to 2 MiB and at most 100 rows
per call. Text and search inspect at most the retained object limit. No query
language or regular expression interpreter is available.

Selections include source size, selected range and units, continuation,
completeness, and truncation. The serialized MCP reply stays within 8 KiB and the
configured context ceiling, including compatibility text and sensitivity
metadata. A row too large to fit requires a narrower pointer or field selection.
The original stays available through `resources/read` or the protected download
extension described in [client compatibility](clients.md#file-transfers).

For example, after retaining a job log, call `result.select` with
`{"uri":"gitea-response:/example/0","mode":"search","text":"FAILED"}`,
replacing the example URI with the returned handle. Continue with the returned
`next_offset` to inspect later matches.

Stored payloads are bounded per object, in aggregate, and by age, so the store
stays a landing area for one conversation's oversized reads rather than a cache.
A payload that expires or is evicted is simply absent. The call that produced it
already completed, so repeating that call would repeat any side effect it had —
which for a mutation or a deletion is rarely what a caller wants merely to
recover a copy of the result.

Concurrent selections and downloads share immutable payload bytes. Their memory
reservation lasts until the final reader releases the payload, including when
its store entry expires or its session closes. A download in progress therefore
cannot make its memory appear available for another retained object.

The reply that replaces a payload is itself held to the ceiling, measured as the
transport serializes it. The excerpt is fitted to whatever room remains and
dropped if none does; only then are declared headers shed, with the `payload`
object recording that they were. The handle is never shed. Error responses are never displaced this way —
they are already held to a much tighter bound and carry the detail needed to
recover.

## Discovery

The server sends a static orientation at initialisation covering the tool-name
shape, the risk annotations, the sensitivity marker, and the large-content
convention. It deliberately carries no counts that move with the pinned
specification, because it forms part of every prompt prefix and a figure that
changes on regeneration would invalidate that cache without telling a reader
anything.

`resources/list` carries `gitea-catalog:/index`, one tab-separated line per
operation giving its name, risk, domain, required arguments, and summary —
roughly 42KB for the whole catalog, read on demand. It exists so an agent can
find the right operation by scanning summaries rather than by loading every
schema. The `catalog.search` and `catalog.describe` tools filter and expand
the same rows for a caller that wants an answer rather than the whole index;
the three discovery views agree with each other because they derive from one
catalog, and they describe that catalog rather than the session's tool
listing — a layered session executes the described operations through the
lanes instead of listing them. The one deliberate asymmetry within discovery
is deprecation: operations the specification marks deprecated appear in none
of the three views, yet stay invocable — and describable — by exact name,
because a caller already holding the name keeps compatibility while nothing
advertises it.
