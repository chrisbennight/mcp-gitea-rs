# Configuration

Pass configuration through environment variables. Docker's `--env-file .env`
is one local option; production operators can inject it through their secret
provider. No particular secret manager or gateway is required.

| Variable | Requirement or default |
| --- | --- |
| `GITEA_MCP_UPSTREAM_URL` | Required Gitea installation URL, before `/api/v1`; no default |
| `GITEA_MCP_SERVICE_TOKEN` | Required Gitea PAT for ordinary operations |
| `GITEA_MCP_TOKEN_USERNAME` | Optional token-administration account; configure together with password |
| `GITEA_MCP_TOKEN_PASSWORD` | Optional password; configure together with username |
| `GITEA_MCP_GATEWAY_BEARER_CURRENT` | Required ingress bearer, at least 32 bytes |
| `GITEA_MCP_GATEWAY_BEARER_PREVIOUS` | Optional previous bearer during rotation |
| `GITEA_MCP_HOST` | Bind address, default `0.0.0.0` inside the container |
| `GITEA_MCP_PORT` | Listen port, default `8000` |
| `GITEA_MCP_ALLOWED_HOSTS` | Comma-separated Host allowlist; use the values in `.env.example` for the loopback Docker example |
| `GITEA_MCP_ALLOWED_ORIGINS` | Optional comma-separated HTTP(S) browser origins; unset rejects every present Origin header while allowing native clients without one |
| `GITEA_MCP_HTTP_TIMEOUT_SECONDS` | Upstream/upload timeout; default 30, allowed 1–300 |
| `GITEA_MCP_BODY_TIMEOUT_SECONDS` | MCP request-body deadline; default 30, allowed 1–300 |
| `GITEA_MCP_MAX_REQUEST_BYTES` | MCP request body limit; default 8 MiB, allowed 1 KiB–64 MiB |
| `GITEA_MCP_MAX_CONCURRENT_REQUESTS` | Shared MCP execution capacity and separate HTTP admission capacity; default 8, allowed 1–64 |
| `GITEA_MCP_FILE_PUBLIC_ORIGIN` | Optional bare HTTP(S) origin reachable by file-transfer clients; unset disables uploads and downloads |
| `GITEA_MCP_LOG_LEVEL` | Verbosity filter for reviewed service diagnostics, default `info`; dependency payload events remain disabled |

The ingress bearer variable retains its historical `GATEWAY` name for
compatibility. A local operator can configure it directly in the client.
Rotate by setting the old current value as previous and installing a new
current value; update clients, then remove the previous value. Never reuse a
Gitea credential as the ingress bearer.

Use HTTPS for remote credential transport. Plain HTTP is appropriate only on a
trusted local/private transport boundary. The HTTP listener itself does not
terminate TLS. Configure a reverse proxy when TLS termination is needed.

Browser clients must use an explicitly configured origin. Origins include the
scheme, host, and optional port; wildcard, opaque `null`, and path values are
not accepted. Preserve the client's Origin through a reverse proxy. This check
does not replace bearer authentication or the Host allowlist, and does not add
CORS support. Native clients can omit Origin.

The HTTP admission limit bounds body buffering through response construction;
an independent execution limit is shared across all MCP sessions and held while
tools and workflows run and construct their results. Resource reads use the same
execution capacity. Excess admission receives HTTP 503; excess MCP execution
receives `gitea_execution_busy` with `outcome: not_sent`. Admission and execution
never wait for a permit. A stalled MCP body receives HTTP 408 on its own
deadline. These bounds do not limit the lifetime of an idle SSE connection or
the client's download speed; keep connection limits at the reverse proxy.
Cancelling a submitted upstream call does not imply that its mutation was
reversed. Capacity remains held until that call returns or reaches its upstream
timeout, and ambiguous mutation outcomes must not be replayed automatically.

Only events on the reviewed `gitea_server::diagnostics` target are logged.
Debug and trace filters cannot enable dependency events or spans containing
protocol messages, headers, tool arguments, or retained payloads. Safe diagnostics
include the listening address, HTTP method/status, and normalized upload error
codes; dependency upgrades must pass the protocol logging regression tests.

Gitea scopes and account permissions determine upstream authority. Start with
read scopes for the domains you need, and enable write or administrative access
only for intended tasks. Token lifecycle manages the configured account's PATs;
callers cannot select another account or supply upstream credentials. Revocation
accepts exactly one numeric ID or unambiguous name. Token names cannot be
numeric selectors, including signed forms.

Omit both token-administration variables for PAT-only use. Empty values also
count as omitted. To enable token administration, use a dedicated account that
supports password-based API authentication; do not weaken a human account's
authentication for this optional capability. Configuring only one variable is
a startup error. Token tools remain visible, but calls fail with configuration
guidance when credentials are absent.
A bootstrap request that includes `access_token` is refused before any upstream
request, so missing credentials cannot leave a partially created repository.
Configured credentials are validated at startup; malformed credentials are not
silently treated as an unavailable optional feature.

File upload requires `files/authorizeUpload`, the `x-mcp-file` argument extension,
and the returned upload URL/header contract. The value stays in bounded process
memory, is consumed once, and is never returned. Use a bare origin without a path
prefix. Uploads and retained results are not durable across process restarts;
multiple replicas require routing a session and its file transfers to the same process.
Hosts can use `files/authorizeDownload` in the owning session to obtain a
short-lived grant for a retained result. Download authorization preserves size,
digest, and sensitivity metadata; transfer headers stay inside the host runtime.
Do not promise multi-user isolation from a shared bearer.
