#!/usr/bin/env bash
# Provision a disposable Gitea and this repository's server, run the agent
# evaluation harness against them, and tear everything down.
#
# Explicitly opt-in: nothing in CI or `cargo test` invokes this. It needs
# docker, a `claude` CLI able to run headless, and the network cost of one
# agent per task. Everything it touches is disposable: the Gitea container,
# the scratch directory, and the locally started server process.
#
# Usage:
#   scripts/eval/run_eval.sh [--only task,task] [--model MODEL]
#                            [--record BASELINE_PATH]
#
# The report always lands in the scratch output directory (printed at the
# end); --record additionally copies it to BASELINE_PATH, which is how a
# baseline is checked in.
set -euo pipefail

only=""
record=""
model=""
agent="claude"
binary=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --only) only="$2"; shift 2 ;;
    --record) record="$2"; shift 2 ;;
    --model) model="$2"; shift 2 ;;
    --agent) agent="$2"; shift 2 ;;
    --binary) binary="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
image="docker.io/gitea/gitea:1.26.4-rootless@sha256:cd1d2614b403fc9b085fa52ceb4424dde9c4dcf5da8e3263abb27955562070c4"
container_name="mcp-gitea-rs-eval-$(openssl rand -hex 4)"
password="$(openssl rand -hex 24)"
bearer="$(openssl rand -hex 32)"
scratch="$(mktemp -d)"
container_started=0
server_pid=""

cleanup() {
  status=$?
  trap - EXIT
  if [[ -n "$server_pid" ]] && ! kill "$server_pid" 2>/dev/null; then
    echo "server process was already gone" >&2
  fi
  if [[ "$container_started" -eq 1 ]] && ! docker rm -f "$container_name" >/dev/null; then
    echo "failed to remove disposable Gitea container" >&2
    status=1
  fi
  echo "evaluation artifacts retained at $scratch"
  exit "$status"
}
trap cleanup EXIT

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
  gitea_url="http://${address}:3000"
else
  port="$(docker port "$container_name" 3000/tcp | sed 's/.*://')"
  gitea_url="http://127.0.0.1:${port}"
fi
for _ in $(seq 1 180); do
  if curl --fail --silent "${gitea_url}/api/healthz" >/dev/null 2>&1; then
    break
  fi
  if [[ "$(docker inspect --format '{{.State.Running}}' "$container_name")" != "true" ]]; then
    docker logs "$container_name" >&2
    echo "disposable Gitea exited before becoming ready" >&2
    exit 1
  fi
  sleep 1
done
curl --fail --silent --show-error "${gitea_url}/api/healthz" >/dev/null

printf '%s\n' "$password" | docker exec --interactive "$container_name" sh -c \
  'IFS= read -r password; exec gitea admin user create --username scope-admin --password "$password" --email scope-admin@example.invalid --admin --must-change-password=false' \
  >/dev/null
printf '%s\n' "$password" | docker exec --interactive "$container_name" sh -c \
  'IFS= read -r password; exec gitea admin user create --username scope-collaborator --password "$password" --email scope-collaborator@example.invalid --must-change-password=false' \
  >/dev/null
admin_token="$(docker exec "$container_name" \
  gitea admin user generate-access-token --username scope-admin \
  --token-name eval-admin --scopes all --raw | tr -d '[:space:]')"

if [[ -z "$binary" ]]; then
  echo "building server" >&2
  cargo build --quiet --locked --package gitea-server --manifest-path "$repo_root/Cargo.toml"
  binary="$repo_root/target/debug/mcp-gitea-rs"
fi
if [[ ! -x "$binary" ]]; then
  echo "evaluation binary is not executable" >&2
  exit 2
fi

server_port="$(python3 - <<'EOF'
import socket
probe = socket.socket()
probe.bind(("127.0.0.1", 0))
print(probe.getsockname()[1])
probe.close()
EOF
)"

GITEA_MCP_UPSTREAM_URL="$gitea_url" \
GITEA_MCP_SERVICE_TOKEN="$admin_token" \
GITEA_MCP_TOKEN_USERNAME="scope-admin" \
GITEA_MCP_TOKEN_PASSWORD="$password" \
GITEA_MCP_GATEWAY_BEARER_CURRENT="$bearer" \
GITEA_MCP_HOST="127.0.0.1" \
GITEA_MCP_PORT="$server_port" \
  "$binary" >"$scratch/server.log" 2>&1 &
server_pid=$!

for _ in $(seq 1 60); do
  if curl --fail --silent "http://127.0.0.1:${server_port}/healthz" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$server_pid" 2>/dev/null; then
    cat "$scratch/server.log" >&2
    echo "server exited before becoming ready" >&2
    server_pid=""
    exit 1
  fi
  sleep 1
done
curl --fail --silent --show-error "http://127.0.0.1:${server_port}/healthz" >/dev/null

harness_args=(--output "$scratch/report.json" --transcripts "$scratch/transcripts" --agent "$agent")
if [[ -n "$only" ]]; then
  harness_args+=(--only "$only")
fi
if [[ -n "$model" ]]; then
  harness_args+=(--model "$model")
fi

status=0
binary_sha256="$(python3 - "$binary" <<'PYHASH'
import hashlib,sys
with open(sys.argv[1], "rb") as stream:
    print(hashlib.file_digest(stream, "sha256").hexdigest())
PYHASH
)"
GITEA_EVAL_BINARY_SHA256="$binary_sha256" \
GITEA_EVAL_URL="$gitea_url" \
GITEA_EVAL_ADMIN_TOKEN="$admin_token" \
GITEA_EVAL_ADMIN_BASIC="scope-admin:${password}" \
GITEA_EVAL_MCP_URL="http://127.0.0.1:${server_port}/mcp" \
GITEA_EVAL_MCP_BEARER="$bearer" \
  python3 "$repo_root/scripts/eval/harness.py" "${harness_args[@]}" || status=$?

if [[ -n "$record" ]]; then
  # A baseline is the comparison point for a surface decision: only a fully
  # passing run of the complete task set may become one.
  if [[ -n "$only" ]]; then
    echo "refusing to record a baseline from a partial task set" >&2
    status=1
  elif [[ "$status" -ne 0 ]]; then
    echo "refusing to record a baseline from a failing run" >&2
  else
    cp "$scratch/report.json" "$record"
    echo "baseline recorded at $record"
  fi
fi
exit "$status"
