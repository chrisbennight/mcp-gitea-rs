# Writing repository documentation

Write for someone who has a Gitea task and has not seen the code or the
maintainer's environment. State what they can do, show a first successful use,
and tell them where to go next. Use plain English and concrete verbs. Replace
claims such as "seamless," "powerful," or "production-ready" with a supported
task or a verifiable constraint.

## README structure

Keep the root README an entry point:

1. A compact identity, the direction of the connection, and a factual description.
2. Practical tasks, such as reading an issue or inspecting a pull request.
3. Prerequisites, Docker setup, MCP connection, and an expected first result.
4. Supported deployment and client limits where they affect adoption.
5. Links to configuration, troubleshooting, contributions, help, and licensing.

Keep detailed configuration tables and protocol explanations in linked guides.
Do not turn a comprehensive README into a copy of every reference document.
Use relative links within the repository and meaningful headings. Keep useful
information outside images, including setup instructions and limitations.
Explain MCP once for readers who do not know the term.

## Evidence and examples

Trace commands to checked-in scripts and configuration. Distinguish a process
health check, upstream connectivity, and authenticated access; none proves the
others. Use a read before a mutation, and a disposable repository before a real
write. A missing mutation response does not authorize retrying the operation.

Describe the service as an MCP server that calls Gitea's API. It provides the
connection from an AI client's tools to Gitea, not a new AI client. Preserve the
single-operator account boundary and explain optional capabilities where they
matter. Do not imply universal client support from SDK tests, or public image
availability while the package is private.

Never embed credentials in examples or ask readers to paste them into a model
prompt. Use secret configuration and placeholders. Screenshots must come from
an actual disposable example, not generated interface art. Keep hypothetical
task examples clearly separate from captured results.

Before landing a documentation change, run the repository checks, follow local
links, inspect rendered images in both themes, and compare the instructions with
the code. Run the branding export check when changing its source or assets.
Record skipped live checks honestly; do not use production infrastructure to
validate a tutorial.

## Sources and project examples

These sources informed the structure and presentation, reviewed on 2026-09-23:

- [GitHub: About READMEs](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/customizing-your-repository/about-readmes)
  identifies purpose, usefulness, getting started, help, and maintainers as
  reader needs; it recommends relative links and moving long reference material
  out of the README.
- [Open Source Guides: Starting a project](https://opensource.guide/starting-a-project/#writing-a-readme)
  connects a useful README with clear expectations, contribution guidance, and
  simple language. Its branding guidance treats writing as part of identity.
- [Waygate](https://github.com/chrisbennight/waygate) demonstrates a compact
  identity, task descriptions, an expected tutorial result, and deeper guides.
  Its [design language](https://github.com/chrisbennight/waygate/blob/main/docs/design.md)
  separates decorative connections from technical diagrams and status meanings.
- [mcp-ssh-rs](https://github.com/chrisbennight/mcp-ssh-rs) demonstrates a real
  disposable workflow and explicit prerequisites. Its
  [visual identity](https://github.com/chrisbennight/mcp-ssh-rs/blob/main/docs/branding/README.md)
  separates generated concept art from maintained production assets.

The tea motif and palette are the maintainer's selection, not requirements
imposed by these sources. Their useful common pattern is clear text supported
by restrained artwork and evidence of actual behavior.
