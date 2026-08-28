# RescueLoop

RescueLoop is a local-first observability and recovery agent that detects failures, explains them,
and applies verified, reversible fixes.

It watches the machine where a problem happened, collects the useful evidence, and can ask the AI
agent you already use to explain what went wrong. A suggested fix is never executed blindly:
RescueLoop checks its scope, asks for approval, verifies the result, and rolls back when verification
fails.

Today, RescueLoop supports macOS and Windows. Linux is next.

> RescueLoop is early software. It handles real incidents, but it does not claim to understand or
> repair every failure yet.

## The idea

Most observability tools begin after logs have been shipped somewhere. RescueLoop begins on the
machine where the problem happened.

The long-term goal is a complete local observability layer for desktop and development environments:
one place that can notice a crash, failed command, broken service, unhealthy container, resource
problem, or recurring regression; connect the relevant evidence; explain the likely cause; and help
return the system to a working state.

Detection is automatic. Repair is not.

AI output is treated as untrusted input. RescueLoop only accepts known repair types, binds them to
collected evidence, shows the plan before making changes, and keeps enough state to verify or roll
back the result.

## What works today

- Detects native crashes on macOS and Windows.
- Watches failed commands, system events, services, Docker, and Podman.
- Groups repeated failures without losing the original occurrences.
- Keeps evidence and operational history on the local machine.
- Sends only bounded, redacted evidence for AI analysis.
- Works with different AI providers and local agent CLIs.
- Supports typed, reversible repairs with explicit approval.
- Replays the original action to verify a fix.
- Rolls back file and configuration changes when verification fails.
- Exposes a small read-only MCP interface for local incident inspection.

Native crash artifacts, container events, macOS Unified Log, and Windows Event Log are connected.
Deeper OS tracing and application-specific health probes remain future work. See the
[roadmap](ROADMAP.md) for where the project is going.

## Install

Release installers download over HTTPS and verify the selected archive against `SHA256SUMS` before
changing the user PATH.

On macOS:

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/ostapondo/rescueloop/main/scripts/install.sh | sh
rescueloop setup
```

On Windows, download and review `scripts/install.ps1`, then run it in PowerShell. Releases also
include a macOS `.pkg`, Windows `.msi`, a Homebrew formula template, and WinGet manifests.

To build from source:

```sh
cargo build --release -p rescueloop
target/release/rescueloop setup
```

Install the background watcher for the current user:

```sh
rescueloop service install
rescueloop service status
```

Use `rescueloop service uninstall` to remove it. On macOS, Unified Log streaming needs an explicitly
privileged system daemon; the normal user watcher does not request that access.

## Use

Run `rescueloop` to open the terminal UI. On first launch, setup detects supported local AI CLIs and
asks which one should handle diagnosis. Codex CLI and Claude Code are supported today. No API keys
are copied into RescueLoop's config.

The background collector can also run in the foreground:

```sh
rescueloop watch
```

Sources are enabled by default and can be managed independently:

```sh
rescueloop sources list
rescueloop sources disable containers
rescueloop sources enable containers
```

In the TUI, use `↑`/`↓` or `j`/`k` to select an incident, `Enter` to open it, `a` to request analysis,
`y` to grant consent, `r` to review a repair, and `q` to quit. Analysis runs asynchronously and new
incidents appear without restarting the console.

For a plain terminal interface:

```sh
rescueloop console --plain
```

## Observe and recover

Observe a command and optionally retain its arguments for exact replay:

```sh
rescueloop run --record-args /path/to/program --flag
rescueloop replay .rescueloop/incidents/<id>.json
```

Arguments may contain secrets, so they are not retained unless `--record-args` is explicitly set.
Recorded arguments and local working paths are stripped from AI requests either way.

Request analysis through a provider adapter:

```sh
rescueloop analyze .rescueloop/incidents/<id>.json \
  --endpoint http://localhost:8080/v1/rescueloop/analyze \
  --output analysis.json
```

Review a repair without changing anything:

```sh
rescueloop repair .rescueloop/incidents/<id>.json analysis.json \
  --allow-root /exact/application/data/root
```

Apply it only after reviewing the printed transaction:

```sh
rescueloop repair .rescueloop/incidents/<id>.json analysis.json \
  --allow-root /exact/application/data/root \
  --approve
```

Repair targets must match collected evidence, stay inside the explicitly allowed root, and avoid
symbolic links and filesystem roots. RescueLoop backs up before mutation, replays the original
action, and rolls back supported changes when verification fails.

## Local data

Incidents are versioned JSON documents. Every detected event is stored as an immutable occurrence;
the UI groups equivalent active failures without erasing their history. A disposable SQLite index
keeps queries fast and can be rebuilt without deleting incidents, analyses, evidence, or repair
history.

```sh
rescueloop index status
rescueloop index rebuild
rescueloop logs --lines 250
```

Operational events are written as bounded, rotated JSONL logs under `.rescueloop/logs`. See the
source and validation scripts for the exact storage, logging, and performance behavior. The
background collector is event-driven rather than polling and is checked by platform-specific idle
performance smoke tests. Configure verbosity with `RUST_LOG` and retention with
`RESCUELOOP_LOG_RETENTION_DAYS`.

## Local MCP integration

RescueLoop can expose its local incident store to a user-selected MCP client:

```sh
rescueloop --incident-dir /absolute/path/to/.rescueloop/incidents mcp
```

The server uses local `stdio` and opens no network port. It exposes only `list_incidents` and
`get_incident`. Both are read-only and use the same bounded redaction as AI analysis.

MCP requires an absolute incident directory. On Unix, RescueLoop enforces owner-only (`0700`) access
on its state and incident directories and refuses symlinked or foreign-owned state roots. On Windows,
it replaces inherited access with an ACL for the current user, Local System, and Administrators.
Inbound protocol messages are capped at 1 MiB and tool arguments are validated against generated
schemas with unknown fields rejected.

Configure the MCP client with the absolute path to the `rescueloop` binary and the arguments above.
Only configure it in clients and workspaces you trust: a local MCP process shares the launching
client’s operating-system security boundary.
The MCP boundary is covered by protocol initialization, discovery, invalid-input, redaction, and
absence-of-mutation-tool tests.

MCP does not expose raw artifacts, arbitrary paths, launch arguments, working directories, analysis,
replay, repair, rollback, or shell execution. Incident IDs and tool arguments are treated as
untrusted input.

## Security boundary

- Detection never sends data over the network.
- Analysis is an explicit user action.
- Tokens are never persisted by RescueLoop.
- AI cannot request arbitrary shell execution.
- A proposal is not a repair: scope checks, backup, and approval happen first.
- A repair succeeds only after verification; supported mutations are rolled back on failure.

Please report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## Build and test

```sh
cargo build --workspace
cargo fmt --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The `scripts/` directory contains the platform, logging, recovery, MCP, and soak checks used by CI.

The release workflow signs macOS and Windows binaries when publisher certificates are configured.
Public signing and Apple notarization require external credentials and cannot be completed from
source code alone.

## Contributing

Bug reports, documentation fixes, redacted failure samples, platform research, and code are welcome.
Linux support and new evidence sources are especially useful areas to help with.

Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request. Security problems belong in a
private report, not a public issue. Everyone taking part follows the
[Code of Conduct](CODE_OF_CONDUCT.md).

## License

RescueLoop is available under the [MIT License](LICENSE).

© ostapondo
