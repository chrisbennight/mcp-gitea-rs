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
| `GITEA_MCP_HTTP_TIMEOUT_SECONDS` | Upstream/upload timeout; default 30, allowed 1–300 |
| `GITEA_MCP_MAX_REQUEST_BYTES` | MCP request body limit; default 8 MiB, allowed 1 KiB–64 MiB |
| `GITEA_MCP_MAX_CONCURRENT_REQUESTS` | Request concurrency; default 8, allowed 1–64 |
| `GITEA_MCP_FILE_PUBLIC_ORIGIN` | Optional bare HTTP(S) origin reachable by the uploading client; unset disables uploads |
| `GITEA_MCP_LOG_LEVEL` | Tracing filter, default `info` |

The ingress bearer variable retains its historical `GATEWAY` name for
compatibility. A local operator can configure it directly in the client.
Rotate by setting the old current value as previous and installing a new
current value; update clients, then remove the previous value. Never reuse a
Gitea credential as the ingress bearer.

Use HTTPS for remote credential transport. Plain HTTP is appropriate only on a
trusted local/private transport boundary. The HTTP listener itself does not
terminate TLS. Configure a reverse proxy when TLS termination is needed.

Gitea scopes and account permissions determine upstream authority. Start with
read scopes for the domains you need, and enable write or administrative access
only for intended tasks. Token lifecycle manages the configured account's PATs;
callers cannot select another account or supply upstream credentials. Revocation
accepts exactly one numeric ID or unambiguous name. Token names cannot be
numeric selectors, including signed forms.

Omit both token-administration variables for PAT-only use. Empty values also
count as omitted; configuring only one is a startup error. Token tools remain
visible, but calls fail with configuration guidance when credentials are absent.
A bootstrap request that includes `access_token` is refused before any upstream
request, so missing credentials cannot leave a partially created repository.
Configured credentials are validated at startup; malformed credentials are not
silently treated as an unavailable optional feature.

File upload requires `files/authorizeUpload`, the `x-mcp-file` argument extension,
and the returned upload URL/header contract. The value stays in bounded process
memory, is consumed once, and is never returned. Use a bare origin without a path
prefix. Uploads and retained results are not durable across process restarts;
multiple replicas require routing a session and its uploads to the same process.
Do not promise multi-user isolation from a shared bearer.
