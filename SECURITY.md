# Security

During the private preparation period, report vulnerabilities directly to the
repository owner through an existing private contact, or through GitHub's
private vulnerability-reporting form when enabled. If that form is unavailable,
ask the owner for a private route without including exploit details in an issue.
Do not put credentials, secret values, or private repository data in public
issues, PRs, logs, or model prompts.

The supported deployment is a single-operator MCP service on a local or private
network. Direct Internet exposure and multi-user isolation are unsupported.
Every `/mcp` request requires the configured ingress bearer. Possessing it
permits use of the configured upstream accounts; operation annotations and lane
routing are not per-user authorization. Use an external gateway for additional
identity, policy, approval, and audit.

Ordinary calls use a service PAT; token lifecycle uses separate account
credentials. Upload URLs use short-lived single-use credentials. Keep both
network paths within an appropriate trust/TLS boundary. Secret results and
retained resources remain sensitive even when a client ignores their metadata.
Treat repository content, issues, logs, and tool responses as untrusted data,
not instructions that authorize further actions.

The preparation branch is maintained against the pinned Gitea version. No
supported release series or security response-time guarantee has been declared.
A public release requires a working private reporting route, an explicit version
support policy, and review of the source and container artifacts to be published.
