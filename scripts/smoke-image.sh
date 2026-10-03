#!/bin/bash
set -euo pipefail

if [ "$#" -ne 2 ] || [ -z "$1" ] || [ -z "$2" ]; then
  echo "usage: smoke-image.sh IMAGE CONTAINER_NAME" >&2
  exit 2
fi

image=$1
name=$2
cid=''

cleanup() {
  result=$?
  trap - EXIT
  if [ -n "$cid" ] && ! docker rm -f -v "$cid" >/dev/null; then
    echo "failed to remove image smoke container" >&2
    result=1
  fi
  exit "$result"
}
trap cleanup EXIT

cid=$(docker create --name "$name" \
  -e GITEA_MCP_UPSTREAM_URL=http://127.0.0.1:9 \
  -e GITEA_MCP_SERVICE_TOKEN=smoke-token \
  -e GITEA_MCP_GATEWAY_BEARER_CURRENT=0123456789abcdef0123456789abcdef \
  "$image")
docker start "$cid" >/dev/null

ready=0
for _ in $(seq 1 30); do
  if docker exec "$cid" /mcp-gitea-rs --healthcheck >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 1
done
if [ "$ready" != 1 ]; then
  docker logs "$cid"
  exit 1
fi
