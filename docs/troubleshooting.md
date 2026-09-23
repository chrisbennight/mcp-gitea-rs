# Troubleshooting

- **Startup exits with a configuration error:** supply the named variable
  through the container environment. The binary does not load `.env` itself.
- **MCP returns 401:** check the ingress bearer and the `Bearer ` prefix.
  This credential is separate from the Gitea personal access token.
- **A host check fails:** match `GITEA_MCP_ALLOWED_HOSTS` to the client's Host
  header, including the port. Do not disable the check to bypass a proxy mismatch.
- **Gitea returns 401 or 403:** check the upstream account's permissions and PAT
  scopes. Token administration uses separate optional credentials.
- **The upstream cannot be reached:** use a Gitea address reachable from inside
  the container. `localhost` refers to the container itself.
- **TLS fails:** fix certificate trust or hostname configuration. Do not disable
  certificate verification.
- **Health passes but a tool fails:** `/healthz` checks the process, not Gitea.
  `server.version` checks connectivity but does not prove authenticated access.
  Read a private repository that the configured service account may access.
- **A large result disappears:** results are temporary and session-local. Read
  [temporary-result handling](clients.md#temporary-results) before recovery.
  Never repeat a mutation just to recover its response.
- **A token tool is unavailable:** configure both optional token-administration
  credentials, or omit token creation from the workflow. See
  [configuration](configuration.md).

For a bug report, include the revision or image digest, client and Gitea versions,
steps to reproduce, and a sanitized error. Include configuration names rather
than credential values. Use [the issue tracker](https://github.com/chrisbennight/mcp-gitea-rs/issues)
for ordinary bugs and [Security](../SECURITY.md) for private reports.
