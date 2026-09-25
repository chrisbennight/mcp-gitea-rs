# Preparation plan

Prepare a single-operator Gitea MCP server distributed as a Linux amd64 Docker
container. GitHub is the source host; Gitea remains the upstream API. The source
was imported as a snapshot without prior Git history. The repository is now
public; container-package access is administered separately.

## Implemented during private preparation

- Import the source as a fresh GitHub root without Gitea history.
- Run GitHub Actions validation and Docker smoke without lab DNS, private
  actions, or a mandatory artifact proxy; publish privately to GHCR.
- Use Dependabot for dependency updates and AERB for current-head PR review.
- Allow PAT-only setup while retaining optional token lifecycle and repository
  bootstrap capabilities (issue #8, PR #11).
- Validate Docker with independent Python and TypeScript MCP clients and explain
  temporary-resource recovery and file-upload limits (issue #9, PR #13).
- Validate stable version tags, preserve immutable version aliases, gate
  publication on OSV checks, and include distribution metadata (issue #10,
  PR #14). See [container releases](docs/releases.md) for the procedure.

## Remaining administration and public-release review

The selected tea identity and [documentation writing guide](docs/writing.md)
now support the README's task descriptions and Docker setup. The service's
connection direction is explicit: MCP clients use this server to call Gitea.
The [visual guide](docs/branding/README.md) records the concept and maintained
assets.

- Configure main-branch protection and native Dependabot security alerts/updates
  in GitHub. The source workflows do not establish those owner settings.
- Complete third-party notices before public distribution (issue #12). The
  image inventory and project license do not replace upstream notices.
- Confirm source publication rights, a working private security-report route,
  and the exact source/image contents as part of release review.

Stdio, ARM images, additional Gitea/Forgejo versions, and multi-user remote
operation are separate future decisions. No work here requires migrating
private infrastructure, gateway code, historical issues, or Git history.

## Service audit implementation

[Epic #18](https://github.com/chrisbennight/mcp-gitea-rs/issues/18) tracks the
eight executive priorities from the service audit. The implementation sequence
is runtime security and execution bounds, complete pagination, selective retained
results, measured discovery improvements, and exact-artifact publication.

The runtime implementation addresses credential-safe diagnostics, process-wide MCP
execution capacity, explicit browser Origin validation, and bounded body reading
with immediate overload refusal (issues #19–#22). Every pinned operation,
bootstrap, and token lifecycle remains available within the configured limits.
Remaining epic outcomes stay open until their own reviewed changes are delivered.

Pagination normalization preserves bounded continuation and count metadata even
when the pinned specification omits those headers. Bootstrap token reconciliation
searches successive bounded pages and treats an incomplete inventory as a failed
lookup, never as permission to create a token (issue #23).

Container publication promotes the OCI artifact exercised by the image job's
health and independent MCP client checks. Its archive and manifest digests are
bound to the source revision and workflow run; publication rejects mismatched
evidence and verifies the registry digest without rebuilding (issue #26).

Retained-result access adds bounded text, literal search, and JSON selection,
plus session-authorized downloads for capable hosts (issue #24). Shared payload
storage keeps active readers within the memory reservation, and complete
`resources/read` compatibility remains available.

## Product invariants

- Every operation in the pinned specification remains reachable through a typed
  contract. Deprecated operations remain callable by exact name without listing.
- Generated operations execute through validated risk lanes; hand-written
  workflows preserve repository bootstrap and scoped token management.
- Credentials never appear in logs, errors, or ordinary metadata. Governed secret
  input and sensitivity propagation remain available.
- Inputs, outputs, stored resources, uploads, concurrency, and durations remain
  bounded. No arbitrary HTTP, shell, or embedded code execution tool is added.
- Mutations validate known preconditions and submit once. Ambiguous outcomes and
  partial workflow completion are reported rather than silently retried.
- Tests use loopback fakes or disposable Gitea, not production services.

See [CONTRIBUTING.md](CONTRIBUTING.md) for required verification and
[DECISIONS.md](DECISIONS.md) for the architecture.

## Discovery efficiency

Reuse immutable schemas, validators, index text and lookup data. Return the
actual callable tool and canonical operation identity in discovery, and cover
common CI and PR vocabulary. Fresh paired model tasks and cold/warm loopback
measurements evaluated optional bounded schemas in search. The experiment did
not improve token or tool-error outcomes and was not promoted. Keep complete
bootstrap contracts and every pinned operation reachable. Record the final
caching and discovery fixes in the [measurement report](docs/measurements/discovery-2026-09-25.md).
