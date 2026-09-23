# mcp-gitea-rs

<picture>
  <source media="(max-width: 600px) and (prefers-color-scheme: dark)" srcset="docs/branding/assets/wordmark-dark.svg">
  <source media="(max-width: 600px)" srcset="docs/branding/assets/wordmark-light.svg">
  <source media="(prefers-color-scheme: dark)" srcset="docs/branding/assets/header-dark.svg">
  <img src="docs/branding/assets/header-light.svg" width="960" alt="mcp-gitea-rs — Connect your AI tools to Gitea">
</picture>

**Connect your AI tools to Gitea.** Read issues, inspect pull requests, check
Actions, and manage repositories through your MCP client.

mcp-gitea-rs is a Rust service that connects an AI client to Gitea's API. It
exposes an MCP server and acts as a Gitea API client. MCP (Model Context Protocol)
lets an AI application discover and call these operations as tools. The service
provides typed operations from the pinned Gitea specification, plus repository
setup and access-token workflows.

**[Run with Docker](#run-with-docker)** ·
**[Explore the documentation](docs/README.md)** ·
**[Contribute](CONTRIBUTING.md)** ·
**[Get help](#help-and-contributions)**

## Things to try

**Inspect work in a repository.** Ask your client to find open issues, read a
pull request, or inspect a branch. It can discover the operation and its typed
inputs without constructing REST paths or shell commands.

**Check an Actions run.** Inspect workflow runs, jobs, and logs through the
catalog. Large results can be returned as temporary resource links; the client
must retrieve them in the same session. See [result handling](docs/clients.md#temporary-results).

**Set up a repository.** Use `repository.bootstrap` to create or adopt a
repository and apply its settings. Optional access-token creation needs
separate credentials. Partial failures report completed steps and recovery
options; bootstrap is not a transaction.

**Manage the configured account's access tokens.** Create, list, or revoke
personal access tokens (PATs) with the optional token-administration account.
Token values are sensitive and
returned once. See [configuration](docs/configuration.md) before enabling this.

The [operation reference](docs/operations.md) explains discovery and execution.
Every operation in the pinned Gitea specification remains reachable through
the typed interface, subject to the configured account's permissions.

## Run with Docker

This repository and its container package are private during release
preparation; you need repository access to clone it. The validated container
platform is Linux amd64 and the pinned upstream is Gitea 1.26.4.

You need Docker, Python 3 for the bearer-generation step below, a reachable
Gitea instance, a service PAT, and an MCP client supporting Streamable HTTP,
custom Authorization headers, and MCP sessions. See [tested clients](docs/clients.md).
The first build downloads public dependencies and can take several minutes.

### Build and configure

Clone using your authenticated Git setup:

```sh
git clone https://github.com/chrisbennight/mcp-gitea-rs.git
cd mcp-gitea-rs
./build-docker.sh
cp .env.example .env
chmod 600 .env
```

Edit `.env` locally. Set `GITEA_MCP_UPSTREAM_URL` to your Gitea installation URL
without `/api/v1`, and `GITEA_MCP_SERVICE_TOKEN` to a PAT with the permissions
you intend to use. Start with read access. The Gitea URL must be reachable from
inside the container; `localhost` there means the container itself.

Generate a separate, random ingress bearer without displaying it:

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

Keep `.env` out of commits and conversations. The ingress bearer protects the
MCP connection; it is not the Gitea PAT. Leave token-administration credentials
unset for ordinary operations and bootstrap without token creation. The
[configuration guide](docs/configuration.md) explains the optional account.

### Start the service

```sh
docker run --detach --name mcp-gitea-rs \
  --env-file .env \
  --publish 127.0.0.1:8000:8000 \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  mcp-gitea-rs:dev
curl --fail http://127.0.0.1:8000/healthz
```

Expect HTTP 200 and a JSON object whose `status` is `ok`, with a `tool_count`
field. This confirms the process is running; it does not validate Gitea access.
The published port is available only on the Docker host's loopback interface.

### Connect your client and make a first read

| Client setting | Value |
| --- | --- |
| Endpoint | `http://127.0.0.1:8000/mcp` |
| Transport | Streamable HTTP |
| Authorization header | `Bearer ` followed by your locally configured ingress bearer |

Store that header in your client's secret configuration, not in a model prompt.
The loopback URL assumes the client runs on the Docker host. A client in another
container or on another machine needs an explicitly configured private route;
read [configuration](docs/configuration.md) before exposing the listener.

In your client's tool interface:

1. Call `server.version` with `{}`. Expect a successful response containing
   the upstream Gitea version. This checks connectivity, not authenticated access.
2. Call `catalog.search` with `{"query":"repository.get","limit":5}`.
3. Call `catalog.describe` with `{"name":"repository.get","detail":"full"}`.
   Inspect the returned schema, then call `api.read` with the arguments below,
   replacing the owner and repository with a private repository accessible to
   the service account:

```json
{
  "operation_id": "repository.get",
  "arguments": {"owner": "your-owner", "repo": "your-private-repository"}
}
```

A successful private-repository read confirms authenticated access for that
operation. Use a disposable repository for your first mutation. Generated
operation names are invoked through their `api.*` execution lane, not directly
as MCP tools. See [the operation reference](docs/operations.md).

When finished, stop and remove the local container:

```sh
docker rm --force mcp-gitea-rs
```

Your local `.env` and built image remain. Remove the credential file locally
when no longer needed. For deployments, an authenticated Docker client can
instead pull `ghcr.io/chrisbennight/mcp-gitea-rs:sha-<commit>` after CI publishes
it. Prefer a SHA tag or digest. See [container releases](docs/releases.md).

## Deployment and compatibility

This is a **single-operator service**. Everyone holding the ingress bearer can
use the configured Gitea accounts' capabilities. Run it locally or on a private
network. An external gateway can add user identity, policy, approval, and audit;
the service itself does not provide separate user identities. A gateway is not
required for the local setup.

Streamable HTTP is supported; stdio and the older separate SSE transport are
not. Other container platforms and Gitea or Forgejo versions are not yet
validated. [Client compatibility](docs/clients.md) records the tested SDKs and
their limits. [Security](SECURITY.md) explains the account and network boundary.

`repository.secret.set_from_file` provides a bounded secret-upload workflow,
but requires a client or gateway implementing the file-transfer extension.
Ordinary MCP tool calls alone are insufficient. See [file uploads](docs/clients.md#file-uploads).

## Help and contributions

Start with [troubleshooting](docs/troubleshooting.md) for startup, authentication,
network, and missing-result problems. Use the
[issue tracker](https://github.com/chrisbennight/mcp-gitea-rs/issues) for bugs or
feature requests. Include the revision or image digest, client and Gitea
versions, reproduction steps, and sanitized errors. Never include credentials
or private repository data. Report vulnerabilities through [Security](SECURITY.md).

Maintained by [Chris Bennight](https://github.com/chrisbennight).
Documentation fixes, bug reports, and focused contributions are welcome.
[Contributing](CONTRIBUTING.md) covers development, tests, and review; no access
to the maintainer's lab or paid evaluation service is needed. The
[preparation plan](PLAN.md) records remaining public-release work, and
[architectural decisions](DECISIONS.md) explain the implementation constraints.

## License

[MIT](LICENSE).
