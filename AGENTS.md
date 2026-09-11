# Repository guidance

## Scope

This repository owns the Rust Gitea MCP server, its generated API catalog,
container image, and source tests. Operator deployment configuration, gateway policy, and runtime secrets belong
outside this source repository. Use an isolated worktree for every change.

## Product contract

- Provide a typed operational interface over Gitea; agents must not have to
  construct REST paths, JSON bodies, or `tea api` commands.
- The pinned Gitea OpenAPI document is the exhaustive API source. Every
  operation it defines stays reachable to an authorized caller, whatever shape
  the tool surface takes. Curated tools and workflows complement generated
  operations rather than narrowing what an agent can do.
- The official Go Gitea MCP server is a behavioral reference for overlapping
  operations, not a source-code port.
- Code Mode is a possible future gateway project and is not a dependency.
- Repository bootstrap and scoped personal access-token lifecycle support are
  required product capabilities.

## Security boundary

- Every `/mcp` request requires the rotating ingress bearer. This is a
  single-operator service; any multi-user authentication, policy, approval, and
  audit belong in an external gateway.
- Normal API calls use the service PAT. Token lifecycle uses separately
  configured Basic Auth credentials because that upstream boundary can differ.
- Credentials and newly created token values never appear in logs or errors.
- Mutations validate all known preconditions before the upstream call, submit
  once, and never retry after an ambiguous transport result.
- Generic arbitrary HTTP, shell, and `tea` execution are not production tools.
- Keep requests, responses, strings, pagination, concurrency, and durations
  bounded. Return normalized errors rather than raw upstream bodies.

## Crates

- `gitea-api`: bounded upstream transport, authentication lanes, wire models,
  OpenAPI operation metadata, and response normalization.
- `gitea-mcp`: MCP schemas, dispatch, annotations, workflows, and the unified
  tool registry.
- `gitea-server`: configuration, ingress bearer authentication, Streamable
  HTTP, health, and bounded file-upload ingress.

## Plan of record

`PLAN.md` is the implementation and PR plan. `DECISIONS.md` records durable
architectural decisions. Update them when implementation evidence changes the
plan.

## Required verification

Before every commit or push, run and read an explicit zero exit status for:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo doc --workspace --no-deps --locked
python3 scripts/check_docs.py
python3 -m unittest discover -s scripts/tests
```

Behavior changes require a test that fails when reverted. Tests use loopback
fakes or a disposable Gitea container and never contact live infrastructure.
Run `cargo mutants` for touched hand-written modules when available and
investigate survivors.

GitHub Actions runs the checks in `.github/workflows/ci.yml`; Dependabot manages
Cargo, Docker, and Actions updates. Image publication is limited to trusted
pushes to the canonical repository's main branch. Never grant package-write
permissions to pull-request jobs.

Use AERB for automated PR review. Its repository policy remains in
`.gitea/pr-review/` because that is the review service's policy lookup path,
even for GitHub. Request review through the AERB MCP after each PR head change;
GitHub does not inherit the former Gitea webhook. Merge requires successful
current-head CI and AERB review, with every finding dispositioned. Repository
rulesets and App installation are administered separately from these files.
