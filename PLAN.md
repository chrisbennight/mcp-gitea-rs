# Preparation plan

Prepare a single-operator Gitea MCP server distributed as a Linux amd64 Docker
container. GitHub is the source host; Gitea remains the upstream API. The private
GitHub repository starts from the current source snapshot without prior Git
history. Repository and package visibility remain private until a separate
publication decision.

## Migration

- Import the reviewed source tree as a fresh root commit.
- Run validation and container smoke tests in GitHub Actions without lab DNS,
  private actions, or an artifact-proxy requirement.
- Publish privately to GHCR from trusted main pushes using the workflow token.
- Use Dependabot for Cargo, Docker, and Actions updates.
- Verify real CI/publication results and arrange AERB access and required checks.

## Remaining usability work

- Deliver PAT-only setup and retain optional token lifecycle and repository
  bootstrap capabilities (tracked in GitHub issue #8).
- Exercise the Docker walkthrough with independent MCP clients; record protocol,
  schema, retained-resource, and file-upload compatibility.
- Improve resource-lifetime visibility without inviting unsafe mutation retries.
- Review the exact source/container contents before public visibility changes.
- Establish supported releases, security reporting, and versioned artifacts.

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
