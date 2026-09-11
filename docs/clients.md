# MCP client compatibility

Use a client that supports Streamable HTTP, a custom Authorization header, and
MCP session headers. Configure the `/mcp` endpoint and keep the ingress bearer in
the client's local secret configuration. Stdio and the older separate `/sse`
transport are not provided.

The migration checks exercised the Linux amd64 container using these official
SDKs on 2026-09-11:

| Client | Result |
| --- | --- |
| Python MCP SDK 2.2.0, explicit initialization handshake | Negotiated MCP 2025-11-25; tool schemas, catalog discovery, upstream reads, and retained-result retrieval passed |
| TypeScript MCP SDK 1.30.0 | Initialization, tool schemas, catalog reads, upstream reads, and retained-result retrieval passed |

Both clients confirmed that a second session cannot read a result retained by
the first session, even when both use the same ingress bearer. The tests use a
local Gitea fake for controlled responses; the separate token and bootstrap
integration tests exercise disposable Gitea. These results do not establish
compatibility with every application built on those SDKs or with the newer
2026-07-28 protocol.

The Python SDK logs `Session termination failed: 202` because this server's
pinned Rust transport returns HTTP 202 to DELETE. The compatibility check
separately verifies that the terminated session receives 404 on its next
request. The warning does not indicate a failed tool call. The
[2025-11-25 session-management specification](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports#session-management)
requires rejection of terminated sessions and does not prescribe a success
status for DELETE. Recheck this behavior when upgrading either SDK.

Run the [reproducible Docker checks](../scripts/compatibility/README.md) before
claiming support for a different client version. Official client sources are
[Python](https://github.com/modelcontextprotocol/python-sdk) and
[TypeScript](https://github.com/modelcontextprotocol/typescript-sdk).

## Temporary results

Large successful results may include a resource link and
`payload.resource_uri`. Keep the same MCP session open and read that URI through
`resources/read`. Preserve the returned data locally if it must survive the
conversation. `resources/list` is a discovery view, not durable storage.

The default server keeps a result for at most 15 minutes from insertion. Reading
it does not renew that lifetime. A result can disappear earlier when its session
ends or the process restarts. Each object is limited to 16 MiB, with a shared
64 MiB budget across sessions; a full store may refuse to retain a new payload.
Always inspect `payload.retained` before attempting retrieval. These are bounds,
not a promise that a URI remains available until a deadline.

An expired or missing resource does not mean its originating operation failed.
For a read, decide whether a fresh read is appropriate. For a mutation, inspect
the upstream object using a separate read and reconcile the outcome. Never
repeat a create, update, delete, token request, or bootstrap just to recover a
lost response. Token values are returned once; losing one requires deliberate
revocation/replacement rather than assuming creation never happened.

## File uploads

Ordinary tool-call and resource support do not implement the file-transfer
extension. `repository.secret.set_from_file` requires a client or gateway that
understands `files/authorizeUpload`, the `x-mcp-file` schema marker, the returned
upload URL and credential, and replacement with the staged file reference.
Neither SDK check above exercises that extension. Existing server tests cover
its authorization, expiry, one-time consumption, and secret handling.

Leave `GITEA_MCP_FILE_PUBLIC_ORIGIN` unset unless the client implements that
contract. Never paste a secret into a model prompt to compensate for a missing
upload integration. See [configuration](configuration.md) for the transfer
boundary and [operation responses](operations.md) for result metadata.
