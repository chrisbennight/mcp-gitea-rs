# Automation and releases

GitHub Actions is the CI entry point. `CI` calls the test and image workflows on
pull requests, main/tag pushes, and manual dispatch. Validation covers formatting,
Clippy, Rust tests/docs, catalog regeneration, Python checks, disposable Gitea
integration tests, and amd64 image health and independent MCP client checks. Jobs have bounded run times.

Only a push to main or a `v*` tag in `chrisbennight/mcp-gitea-rs` can publish. The publication
job has `packages: write`; tests and PR image jobs have only `contents: read`.
The workflow uses `GITHUB_TOKEN`, not a stored registry PAT or private action.
Images carry the repository source label so GHCR can associate their access.
The repository and new GHCR package are private during preparation. Do not
change visibility as part of a build or release.

A successful main build publishes `sha-<full commit>` and advances `latest`.
A stable `vMAJOR.MINOR.PATCH` tag matching the workspace version publishes its
revision and version alias without advancing `latest`. An
existing revision is not overwritten; when reused for latest it is pulled and
smoke-tested again. Registry errors stop publication. A manual workflow run
validates but does not publish; rerun the original successful-main push run to
recover a transient publication failure. See [container releases](releases.md) for version validation, immutable aliases,
distribution metadata, and publication review. Other architectures remain future work.

Dependabot checks Cargo, Docker, and GitHub Actions weekly. Dependency PRs run
the same checks as other changes and are never automatically merged. Rust
compiler pins in the toolchain, manifest, image, and test workflow must be
updated together. Review upstream specification changes separately from routine
dependency updates. Enable GitHub vulnerability alerts and security updates in
repository settings; a committed configuration does not establish those account
settings or guarantee private-repository security features are available.

The dependency-advisory workflow checks locked public Cargo packages against
OSV on each CI run and weekly. Publication waits for that check as well as the
test and image jobs. An unavailable service, incomplete response, or unsupported
private/git dependency fails the check rather than claiming a clean scan. It
sends only public package names and versions; GitHub vulnerability alerts remain
a separate owner setting. Run `python3 scripts/check_advisories.py` locally to
repeat it. This is not a base-image, JavaScript-client, or secret scan.

AERB supports GitHub through the review MCP. The current review service reads
`.gitea/pr-review/policy.yaml` even on GitHub, so the directory name is retained.
Request current-head review explicitly; the old Gitea webhook does not transfer.
AERB's GitHub App must have repository access and status/comment permissions to
post its verdict. Keep PR intent in the template's design goal and acceptance
criteria; disposition all findings before merging.

Configure repository rules to require the successful PR test/image checks and
`advisories / advisories`, and `pr-review/gate`, using the check names reported by GitHub. App installations,
rulesets, vulnerability-report settings, and package access are repository
administration, not effects of checking in these files. Keep the migration issue
open until actual CI and publication results are verified.
