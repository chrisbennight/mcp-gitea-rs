#!/usr/bin/env bash
set -euo pipefail

image="docker.io/gitea/gitea:1.26.4-rootless@sha256:cd1d2614b403fc9b085fa52ceb4424dde9c4dcf5da8e3263abb27955562070c4"
container_name="mcp-gitea-rs-bootstrap-$(openssl rand -hex 4)"
password="$(openssl rand -hex 24)"
scratch="$(mktemp -d)"
container_started=0

cleanup() {
  status=$?
  trap - EXIT
  if [[ "$container_started" -eq 1 ]] && ! docker rm -f "$container_name" >/dev/null; then
    echo "failed to remove disposable Gitea container" >&2
    status=1
  fi
  if ! rm -rf -- "$scratch"; then
    echo "failed to remove bootstrap smoke scratch directory" >&2
    status=1
  fi
  exit "$status"
}
trap cleanup EXIT

ssh-keygen -q -t ed25519 -N "" -f "$scratch/primary"
ssh-keygen -q -t ed25519 -N "" -f "$scratch/failure"
primary_key="$(<"$scratch/primary.pub")"
failure_key="$(<"$scratch/failure.pub")"

docker run --detach --name "$container_name" \
  --publish 127.0.0.1::3000 \
  --env GITEA__database__DB_TYPE=sqlite3 \
  --env GITEA__security__INSTALL_LOCK=true \
  "$image" >/dev/null
container_started=1

if [[ -f /.dockerenv ]]; then
  address="$(docker inspect --format '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$container_name")"
  if [[ -z "$address" ]]; then
    echo "disposable Gitea has no reachable bridge address" >&2
    exit 1
  fi
  base_url="http://${address}:3000"
else
  port="$(docker port "$container_name" 3000/tcp | sed 's/.*://')"
  base_url="http://127.0.0.1:${port}"
fi
for _ in $(seq 1 180); do
  if curl --fail --silent "${base_url}/api/healthz" >/dev/null 2>&1; then
    break
  fi
  if [[ "$(docker inspect --format '{{.State.Running}}' "$container_name")" != "true" ]]; then
    docker logs "$container_name" >&2
    echo "disposable Gitea exited before becoming ready" >&2
    exit 1
  fi
  sleep 1
done
curl --fail --silent --show-error "${base_url}/api/healthz" >/dev/null

printf '%s\n' "$password" | docker exec --interactive "$container_name" sh -c \
  'IFS= read -r password; exec gitea admin user create --username scope-admin --password "$password" --email scope-admin@example.invalid --admin --must-change-password=false' \
  >/dev/null
printf '%s\n' "$password" | docker exec --interactive "$container_name" sh -c \
  'IFS= read -r password; exec gitea admin user create --username scope-collaborator --password "$password" --email scope-collaborator@example.invalid --must-change-password=false' \
  >/dev/null

GITEA_BOOTSTRAP_TEST_URL="$base_url" \
GITEA_BOOTSTRAP_TEST_PASSWORD="$password" \
GITEA_BOOTSTRAP_TEST_PRIMARY_KEY="$primary_key" \
GITEA_BOOTSTRAP_TEST_FAILURE_KEY="$failure_key" \
  cargo run --quiet --locked --package gitea-mcp --example bootstrap_smoke
