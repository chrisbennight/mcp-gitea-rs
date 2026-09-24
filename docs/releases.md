# Container releases

The distribution is a Linux amd64 container. The maintained source is `main`;
no stable version has been released. A release tag
is a tested snapshot, not a promise of backports to older versions. Support for
additional Gitea versions, Forgejo, ARM, and multi-user operation is not implied.

## Create a versioned snapshot

1. Update the workspace version and lockfile together in a reviewed PR. Describe
   operator-visible changes and any configuration migration in its release notes.
2. Merge only after the current PR head passes tests, image checks, and AERB.
3. Tag the reviewed main commit as `vMAJOR.MINOR.PATCH`, matching the workspace
   version exactly. Leading zeroes and prerelease suffixes are not supported by
   this workflow. Push the tag without moving an existing tag.
4. Monitor its CI run. A successful run publishes both `sha-<full commit>` and
   `vMAJOR.MINOR.PATCH` to GHCR. Tag builds never advance `latest`; that alias
   follows successful main builds.
5. Record the image digest and CI link alongside the version's change notes.
   Consumers should deploy the digest and retain the prior digest for rollback.

CI rejects a mismatched tag before building or contacting the registry. Existing
revision tags are reused, and an existing version alias must contain the same
manifest as its revision; a conflict fails publication. A transient error may be
recovered by rerunning the original workflow, after checking its observed
publication state. Never delete or move an existing version alias to make a run
pass. A wrong release requires a new version.

This procedure does not change repository or package visibility, create a
GitHub Release automatically, or claim image signing/attestation. The source
repository is public; package access is administered separately.

## Inspect the distribution

The final image includes the project license and a dependency inventory under
`/usr/share/mcp-gitea-rs/`. Extract them without starting the server:

```sh
docker create --name mcp-release-inspect IMAGE_REFERENCE
docker cp mcp-release-inspect:/usr/share/mcp-gitea-rs ./distribution-metadata
docker rm mcp-release-inspect
```

Replace `IMAGE_REFERENCE` with the exact tested digest. `dependencies.json`
records the lockfile hash, package versions, and declared license expressions.
It includes the complete Cargo resolution, including development and other
platform dependencies; it is an inventory, not a runtime-only SBOM or a complete
third-party notice bundle. It excludes local cache paths and registry URLs.

The build context excludes local credentials, worktrees, Cargo overrides,
agent configuration, and handoff notes. Runtime credentials must be injected
when the container starts; never pass them as build arguments. The optional
crate mirror argument is for a credential-free registry URL only.

## Preparation review

The migration review on 2026-09-11 found no OSV matches for the locked public
Cargo packages. A credential-pattern sweep of tracked source found no private
key, GitHub token, or AWS access-key matches. Neither result proves an absence
of vulnerabilities or secrets; repeat checks on the exact release candidate.
The [OSV batch API](https://google.github.io/osv.dev/post-v1-querybatch/) compares
package names and versions against published advisories.

All resolved Cargo packages declare license metadata. Some published archives
omit license/notice text, including `rmcp`, `jsonschema-regex`,
`jsonschema-value`, `uuid-simd`, and `vsimd`, with additional gaps among
platform-specific dependencies. Complete [the third-party notice review](https://github.com/chrisbennight/mcp-gitea-rs/issues/12) before
public distribution. Do not treat the project's MIT license label as replacing
upstream attribution requirements.

Specification provenance in `openapi/SOURCE.md` and the generated catalog
intentionally identifies the original source. The review-policy directory is
still `.gitea/pr-review/` because AERB reads that location on GitHub. The
`org.cacahuate` metadata namespace remains a client contract. These are deliberate
compatibility/provenance references, not runtime dependencies on the lab.

Release review includes source publication rights, third-party notices, a working
private security-report route, source/container contents, GitHub access rules,
and package visibility. Public source access does not establish that these
checks are complete.
