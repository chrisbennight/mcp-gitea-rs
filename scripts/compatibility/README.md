# Docker client compatibility checks

These checks use the official Python and TypeScript MCP SDKs against a locally
built container and a disposable Gitea HTTP fake. They never use a live Gitea
account. The fixture requires Linux Docker host networking, binds both services
to loopback, generates an ingress bearer in memory, and removes its container
when finished. Run one fixture at a time; a free loopback port is selected before
Docker starts. A startup/cleanup failure fails the check.

From the repository root, prepare isolated clients:

```sh
./build-docker.sh
python3 -m venv /tmp/gitea-mcp-python-client
/tmp/gitea-mcp-python-client/bin/pip install 'mcp==2.2.0'
npm install --prefix /tmp/gitea-mcp-typescript-client --ignore-scripts \
  --no-audit --no-fund '@modelcontextprotocol/sdk@1.30.0'
cp scripts/compatibility/typescript.mjs /tmp/gitea-mcp-typescript-client/
/tmp/gitea-mcp-python-client/bin/python scripts/compatibility/run.py \
  mcp-gitea-rs:dev \
  --typescript-client /tmp/gitea-mcp-typescript-client/typescript.mjs
```

The CI image-validation job runs both clients before publication can proceed.
The TypeScript check is optional for local runs; omit its argument for Python alone. These
are development-only client dependencies; the container does not install them.
Their transitive dependencies are resolved at installation, so record the
installed package versions when comparing results across environments. Do not
point the client scripts at production systems: their expected responses belong
to this fixture.

The fixture checks PAT-only startup, rejection of unauthenticated MCP requests,
initialization, tool input schemas, catalog discovery, authenticated upstream
reads, large-result contents, and isolation between sessions sharing a bearer.
The Python check also verifies that a terminated session subsequently gets 404.
It does not claim exhaustive protocol conformance, GUI-client integration,
OAuth support, browser transport support, or generic file-upload support.

See [client compatibility](../../docs/clients.md) for observed behavior and
[configuration](../../docs/configuration.md) for the real deployment settings.
