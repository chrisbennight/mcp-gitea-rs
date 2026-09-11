# mcp-gitea-rs

A Rust MCP server for managing Gitea repositories, issues, pull requests,
Actions, organizations, and access tokens. It provides typed operations from
the pinned Gitea API specification, plus repository bootstrap and governed
secret-upload workflows.

This is a single-operator service. Run it locally or on a private network, with
one Gitea service account. Every MCP client holding the ingress bearer can use
that account's configured capabilities. A gateway can add caller authorization,
approval, and audit; the server does not provide separate user identities.

The GitHub repository and its container package are private during migration.
The supported container platform is Linux amd64; the pinned upstream is Gitea
1.26.4. Other versions and platforms are not yet validated.

## Run with Docker

You need Docker, a reachable Gitea instance, a service personal access token
(PAT), and an ingress bearer. Token-administration credentials are optional;
ordinary operations and repository bootstrap without token creation need only
the service PAT. To manage access tokens, configure a dedicated account that
supports password-based API authentication. Do not weaken a human account's
authentication to enable this optional capability.

Build from your authenticated checkout:

```sh
./build-docker.sh
cp .env.example .env
chmod 600 .env
```

Edit `.env` locally. Set your Gitea URL, service token, and a random ingress
bearer of at least 32 bytes. Keep this
file out of commits and conversations. To replace the bearer placeholder with
an independently generated value without displaying it:

```sh
python3 - <<'PY'
from pathlib import Path
import secrets
path = Path('.env')
text = path.read_text()
placeholder = 'replace-with-at-least-32-byte-bearer'
if placeholder not in text:
    raise SystemExit('Bearer placeholder not found; no changes made')
path.write_text(text.replace(placeholder, secrets.token_hex(32)))
PY
```

Start the container with a host-loopback port:

```sh
docker run --detach --name mcp-gitea-rs \
  --env-file .env \
  --publish 127.0.0.1:8000:8000 \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  mcp-gitea-rs:dev
curl --fail http://127.0.0.1:8000/healthz
```

Health reports that the process is running; it does not validate the Gitea
credentials. The upstream URL is required and must be reachable from inside the
container. `localhost` inside it refers to the container itself.

Configure an MCP client that supports Streamable HTTP:

| Setting | Value |
| --- | --- |
| Endpoint | `http://127.0.0.1:8000/mcp` |
| Authorization header | `Bearer ` followed by the ingress bearer from your local configuration |
| Transport | Streamable HTTP |

Use your client's local secret configuration for that header. Do not enter the
bearer in a model prompt. Invoke `server.version` with `{}` to verify upstream
connectivity (the version endpoint alone does not prove authentication), then use `catalog.search` and `catalog.describe`
to select an operation and call its execution lane. Use a read of a private repository to verify the service account before making
changes; use a disposable repository for your first mutation.

The container image is also published privately at
`ghcr.io/chrisbennight/mcp-gitea-rs:sha-<commit>` after CI passes on main.
Authenticate your Docker client to GHCR before pulling it. Use an immutable
SHA tag or digest for deployment; `latest` follows successful main builds.
See [automation](docs/automation.md) and [versioned container releases](docs/releases.md).

Stop and remove your local container with `docker rm --force mcp-gitea-rs`.

## Configuration and troubleshooting

[Configuration](docs/configuration.md) describes required values, bearer
rotation, allowed hosts, limits, and optional file upload. [Security](SECURITY.md)
explains the trust boundary and private reporting.

- Startup exits with a configuration error: supply the named variable through
  the container environment. The binary does not load `.env` by itself.
- MCP returns 401: check the ingress bearer, including the `Bearer ` prefix.
- A host check fails: match `GITEA_MCP_ALLOWED_HOSTS` to the client's Host header,
  including the port. Do not disable the check to work around a proxy mismatch.
- Gitea returns 401/403: check the relevant upstream account and scopes. The
  ingress bearer is separate from Gitea credentials.
- TLS fails: fix certificate trust or hostname configuration. Do not disable
  certificate verification.
- A large result's resource disappears: resources are temporary and local to
  the session. Do not repeat a mutation just to recover its result.

## Capabilities and development

The [operation and response reference](docs/operations.md) covers discovery,
execution lanes, errors, and large-result retrieval. Every operation in the
pinned specification remains reachable through the typed interface to an
appropriately authorized caller. [Specification provenance](openapi/SOURCE.md)
records the input and regeneration procedure.

`repository.bootstrap` can create or adopt a repository and converge its
settings. Partial failures report completed steps and compensation options;
they are not transactions. Access-token creation returns sensitive data once.
`repository.secret.set_from_file` accepts secret bytes through a separate,
bounded upload path; it requires a client or gateway implementing that transfer
contract. Ordinary MCP tool-call support alone is insufficient.

See [CONTRIBUTING.md](CONTRIBUTING.md) for development and review, [PLAN.md](PLAN.md)
for remaining preparation, and [DECISIONS.md](DECISIONS.md) for architectural
constraints. Licensed under [MIT](LICENSE).
