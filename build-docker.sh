#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"
image_tag="${TAG:-mcp-gitea-rs:dev}"
exec docker build --tag "$image_tag" --progress=plain "$@" --platform linux/amd64 .
