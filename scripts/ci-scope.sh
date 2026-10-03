#!/usr/bin/env bash
set -euo pipefail

rust=false
python=false
docs=false
catalog=false
integration=false
deps=false
image=false
publish=false
case "${GITHUB_EVENT_NAME:?event is required}" in
  workflow_dispatch|schedule) full=true ;;
  push|pull_request)
    full=false
    if [[ "$GITHUB_EVENT_NAME" == push && "${GITHUB_REF:-}" == refs/tags/* ]]; then full=true; fi
    ;;
  *) echo 'Unsupported CI event' >&2; exit 1 ;;
esac
if [[ "$full" == true ]]; then
  publish=true
  rust=true; python=true; docs=true; catalog=true; integration=true; deps=true; image=true
else
  [[ "${BASE_SHA:-}" =~ ^[0-9a-f]{40}$ ]] || { echo 'A full base commit is required' >&2; exit 1; }
  changed_files="$(mktemp)"
  trap 'rm -f "$changed_files"' EXIT
  if [[ "$GITHUB_EVENT_NAME" == pull_request ]]; then
    git diff --name-only --no-renames -z "$BASE_SHA...HEAD" >"$changed_files"
  else
    git diff --name-only --no-renames -z "$BASE_SHA" HEAD >"$changed_files"
  fi
  while IFS= read -r -d '' path; do
    case "$path" in
      scripts/ci-scope.sh) rust=true; python=true; docs=true; catalog=true; integration=true; deps=true; image=true ;;

      .github/workflows/security.yml) deps=true ;;
      .github/workflows/test.yml) rust=true; python=true; docs=true; catalog=true; integration=true ;;
      .github/workflows/build.yml) image=true ;;
      .github/workflows/*) rust=true; python=true; docs=true; catalog=true; integration=true; deps=true; image=true ;;
      Cargo.toml|Cargo.lock|rust-toolchain.toml|rust-toolchain|crates/*/Cargo.toml) rust=true; deps=true; integration=true; image=true ;;
      .cargo/*) rust=true; integration=true ;;
      rustfmt.toml|.rustfmt.toml|clippy.toml|.clippy.toml) rust=true ;;
      crates/*/tests/*|crates/*/benches/*) rust=true ;;
      crates/*/*.md) docs=true ;;
      crates/*) rust=true; image=true ;;
      generated/*) rust=true; catalog=true; python=true; integration=true; image=true ;;
      openapi/*) catalog=true; python=true ;;
      scripts/compatibility/*.md) docs=true ;;
      Dockerfile|.dockerignore|scripts/image_artifact.py|scripts/dependency_inventory.py|scripts/smoke-image.sh|scripts/compatibility/*|scripts/release_version.py|LICENSE|THIRD_PARTY_NOTICES.md)
        image=true ;;
      scripts/test_token_scope.sh|scripts/test_bootstrap.sh) integration=true ;;
      scripts/check_advisories.py) deps=true ;;
    esac
    case "$path" in
      *.md) docs=true ;;
      scripts/check_docs.py) docs=true; python=true ;;
      scripts/*) python=true ;;
    esac

    case "$path" in
      crates/gitea-api/src/*|crates/gitea-api/examples/token_scope_smoke.rs|crates/gitea-mcp/src/lib.rs|crates/gitea-mcp/src/bootstrap*|crates/gitea-mcp/src/owners.rs|crates/gitea-mcp/src/lanes.rs|crates/gitea-mcp/src/discovery.rs|crates/gitea-mcp/src/resources.rs|crates/gitea-mcp/examples/bootstrap_smoke.rs)
        integration=true ;;
      scripts/generate_api.py) catalog=true ;;
    esac
    case "$path" in
      crates/*/tests/*|crates/*/benches/*|crates/*/examples/*|crates/*/*.md) ;;
      Cargo.toml|Cargo.lock|rust-toolchain.toml|rust-toolchain|crates/*|generated/*|Dockerfile|.dockerignore|scripts/release_version.py|scripts/dependency_inventory.py|LICENSE|THIRD_PARTY_NOTICES.md) publish=true ;;
    esac
    if [[ ! -e "$path" ]]; then docs=true; fi
  done <"$changed_files"
fi
if [[ "$publish" == true ]]; then deps=true; fi
printf 'rust=%s\npython=%s\ndocs=%s\ncatalog=%s\nintegration=%s\ndeps=%s\nimage=%s\npublish=%s\n' \
  "$rust" "$python" "$docs" "$catalog" "$integration" "$deps" "$image" "$publish" >>"${GITHUB_OUTPUT:?output file is required}"
