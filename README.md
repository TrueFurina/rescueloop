# RescueLoop

Your computer breaks. RescueLoop figures out why and helps fix it.

**Lightweight enough to keep running. Careful enough to trust with recovery.**

It runs in the background and watches for crashes, failed commands, broken services, resource problems,
and unhealthy containers. When something goes wrong, RescueLoop keeps the useful evidence, finds the
likely cause, and prepares a repair.

Nothing is fixed behind your back. You see the plan, approve it, and RescueLoop checks whether the
problem is actually gone. If a repair fails, it rolls back the change when it can.

Everything stays on your machine unless you explicitly ask an AI agent for help.

**macOS and Windows today. Linux next.**

> RescueLoop is early software. It already handles real incidents, but coverage is still growing.

![RescueLoop incident console](assets/rescueloop-console.png)

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

Clone the repository and start RescueLoop:

```sh
git clone https://github.com/ostapondo/rescueloop.git
cd rescueloop
cargo run -p rescueloop
```

The first run detects supported AI agents and walks you through setup automatically.

Install the background watcher when you want RescueLoop to start automatically:

```sh
cargo build --release -p rescueloop
target/release/rescueloop service install
target/release/rescueloop service status
```

## CLI reference

The main commands, close to the actual CLI output:

| Command | What it does |
| --- | --- |
| `rescueloop` | Start the interactive console; watch and manage incidents |
| `rescueloop run --record-args <cmd> [args...]` | Supervise a command and record its outcome for later verification |
| `rescueloop service install` | Install the background watcher service |
| `rescueloop service status` | Show the background service status |
| `rescueloop mcp` | Start the read-only local MCP server |
| `rescueloop --incident-dir <path> mcp` | Start MCP against a specific incident directory |

Examples:

```sh
# Watch for crashes and manage incidents (interactive)
rescueloop

# Supervise a command (args recorded only with --record-args)
rescueloop run --record-args ./deploy.sh --env prod

# Background service
rescueloop service install
rescueloop service status

# MCP server (read-only incident access)
rescueloop --incident-dir /absolute/path/to/.rescueloop/incidents mcp
```

On Windows, use the same commands from PowerShell with the compiled binary:

```powershell
target\release\rescueloop.exe service install
target\release\rescueloop.exe --incident-dir C:\absolute\path\.rescueloop\incidents mcp
```

Use `a` to analyze an incident, `r` to review a repair, and `y` to approve it.

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
