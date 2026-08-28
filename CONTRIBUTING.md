# Contributing to RescueLoop

Thanks for taking a look. RescueLoop is early, and useful contributions do not have to be large.
Bug reports, documentation fixes, platform research, redacted failure samples, and code are all
welcome. Linux support and new evidence sources are especially useful areas to help with.

## Before you start

For a small fix, open a pull request when it is ready. For a new feature or a change to the safety
model, open an issue first so the design can be discussed before a lot of work is done.

Please do not post secrets, usernames, private paths, tokens, raw crash reports, or other personal
data. Reduce a failure sample to the smallest safe fixture you can share.

Security problems belong in a private report, not a public issue. See [SECURITY.md](SECURITY.md).

## Development

Install a current stable Rust toolchain, then run:

```sh
cargo build --workspace
cargo fmt --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Changes to MCP must also cover protocol initialization, tool discovery, invalid input, redaction,
and the absence of mutation tools:

```sh
./scripts/validate-mcp.sh
```

Platform-specific and end-to-end checks live in the `scripts/` directory and run in CI.

## Pull requests

Keep a pull request focused on one problem. Explain what changed, why it changed, and how you tested
it. Tests are expected for behavior changes. If a test is not practical, say why.

Every user-visible change to incidents, evidence, analysis, repair plans, lifecycle state, or history
must assess the MCP surface. Update its schemas, redaction, documentation, and tests when agents
should see the feature. Otherwise, state why no MCP change is needed.

By contributing, you agree that your contribution is licensed under the MIT License.
