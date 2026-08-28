# RescueLoop

Your computer breaks. RescueLoop figures out why and helps fix it.

It runs in the background and watches for crashes, failed commands, broken services, resource problems,
and unhealthy containers. When something goes wrong, RescueLoop keeps the useful evidence, finds the
likely cause, and prepares a repair.

Nothing is fixed behind your back. You see the plan, approve it, and RescueLoop checks whether the
problem is actually gone. If a repair fails, it rolls back the change when it can.

Everything stays on your machine unless you explicitly ask an AI agent for help.

**macOS and Windows today. Linux next.**

> RescueLoop is early software. It already handles real incidents, but coverage is still growing.

## How it works

1. **Detect** — notice a crash, failed process, broken service, container problem, or resource issue.
2. **Understand** — collect the relevant evidence and connect repeated failures.
3. **Repair** — prepare a small, reviewable change instead of running an arbitrary command.
4. **Verify** — repeat the failed action and roll back when the repair did not work.

## What works now

- Native crash detection on macOS and Windows
- Failed command and process supervision
- Windows Event Log and macOS Unified Log
- Docker and Podman failures, OOM events, and restart loops
- Local incident history with repeated failures grouped together
- Analysis through Codex CLI, Claude Code, or an HTTP adapter
- Approved repairs for files, JSON config, permissions, services, and containers
- Read-only local MCP access to redacted incidents

## Try it

Clone the repository and open the console:

```sh
git clone https://github.com/ostapondo/rescueloop.git
cd rescueloop
cargo run -p rescueloop -- console
```

Run the first-time setup when you want to connect an AI agent and choose event sources:

```sh
cargo run -p rescueloop -- setup
```

Install the background watcher when you want RescueLoop to start automatically:

```sh
cargo build --release -p rescueloop
target/release/rescueloop service install
target/release/rescueloop service status
```

Use `a` to analyze an incident, `r` to review a repair, and `y` to approve it.

To watch a command and keep enough context to verify a future repair:

```sh
rescueloop run --record-args /path/to/program --flag
```

Arguments may contain secrets, so RescueLoop records them only when `--record-args` is present. They
are never included in evidence sent to an AI agent.

## Safety

RescueLoop can change a machine, so the boundary is deliberately narrow:

- Detection is local and automatic.
- Analysis and repair are explicit actions.
- AI output is untrusted data, not executable code.
- Repairs use known action types and must match collected evidence.
- Every change is shown before approval and checked afterwards.
- Supported file and configuration changes are backed up for rollback.
- MCP cannot repair, replay, run a shell, or read arbitrary files.

The MCP server is local and read-only. It exposes only `list_incidents` and `get_incident`:

```sh
rescueloop --incident-dir /absolute/path/to/.rescueloop/incidents mcp
```

See [SECURITY.md](SECURITY.md) for the full security boundary and vulnerability reporting.

## Where it is going

The goal is one local view of what broke, why it broke, and whether it was actually fixed.

Next steps include Linux, a desktop app, broader system and network signals, better correlation,
application-specific health checks, and more safe repair types.

See [ROADMAP.md](ROADMAP.md) for the longer version.

## Build and test

```sh
cargo fmt --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Contributing

Bug reports, redacted failure samples, documentation, platform research, and code are welcome. Linux
support and new evidence sources are especially useful.

Start with [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE) © ostapondo
