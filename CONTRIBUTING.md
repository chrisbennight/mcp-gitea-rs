# Contributing

Bug reports, documentation improvements, and focused changes are welcome.
Describe the task you need to accomplish before proposing a large redesign.
Keep reports and examples free of credentials and private repository content.
Use [SECURITY.md](SECURITY.md) for vulnerabilities.

For documentation and artwork changes, follow the [writing guide](docs/writing.md)
and [visual identity](docs/branding/README.md). Regenerate branding assets with
`python3 docs/branding/export.py` and verify them with
`python3 docs/branding/export.py --check` before submitting changes to that source.

Install the Rust toolchain in `rust-toolchain.toml`, Python 3, and Docker.
Native compilation also needs a C/C++ toolchain and CMake. Create an isolated
worktree under the ignored `.worktrees/` directory and make a task branch.

Before committing or pushing, run:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo doc --workspace --no-deps --locked
python3 scripts/check_docs.py
python3 scripts/generate_api.py --check
python3 -m unittest discover -s scripts/tests
```

For upstream behavior changes, also run the disposable integration suites:

```sh
bash scripts/test_token_scope.sh
bash scripts/test_bootstrap.sh
```

Tests must never target a production Gitea instance. Behavior changes need a
regression test. Run mutation testing for touched hand-written modules when
`cargo mutants` is available and investigate surviving mutations.

The pinned OpenAPI document is the coverage source. Change the generator rather
than editing its generated output, then run `python3 scripts/generate_api.py` and
review the result. Preserve operation coverage, sensitivity classification,
request/response bounds, and explicit outcomes after ambiguous network failures.

Open a PR with the design goal, observable acceptance criteria, and relevant
validation. Maintainers obtain AERB review and verify current-head CI. You do
not need access to the maintainer's gateway or paid agent evaluation to
contribute. The opt-in [evaluation harness](scripts/eval/README.md) spends model
tokens and is separate from ordinary tests.

Authors are responsible for understanding and testing submitted changes,
including AI-assisted changes. Be respectful, address technical disagreements
with evidence, and keep reviews focused on the proposed behavior. The project
has no guaranteed support response time or commercial support commitment.
