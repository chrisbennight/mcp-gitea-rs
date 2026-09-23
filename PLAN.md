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
