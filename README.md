# RescueLoop

RescueLoop is an early Windows/macOS failure detector and provider-neutral AI analysis pipeline.
Detection is local and automatic; AI analysis is a separate, explicit user action. AI output is data,
not executable code, and only allowlisted reversible repair action types are accepted.

## Current vertical slice

- macOS: watches new user/system DiagnosticReports (`.ips`, `.crash`, `.diag`, `.spin`, `.hang`).
- Windows: watches new Windows Error Reporting archives and queues (`.wer`).
- normalizes reports into a versioned incident JSON document;
- computes stable application, environment and incident fingerprints without UUIDs, timestamps,
  PIDs, raw addresses or private artifact paths;
- extracts at most 40 allowlisted diagnostic metadata lines; raw reports and paths stay local;
- stores incidents locally and prints a `DETECTED` notification;
- supervises explicitly launched commands and records non-success exit codes/signals;
- can replay an exact recorded action and reports `VERIFIED` or `NOT FIXED` from its exit status;
- sends an incident to any HTTP AI adapter implementing the documented JSON contract, only on `analyze`;
- rejects unknown, incomplete, evidence-invalid, or non-reversible repair action proposals;
- executes typed filesystem, configuration, permission, service and container repairs behind dry-run,
  exact evidence binding, explicit scope and approval;
- backs up before mutation, replays the original action, and rolls back when verification fails.
- appends outcomes to a tamper-evident local lineage ledger and distinguishes lifecycle updates,
  regressions, independent new failures and stale verification after an app/environment change.

This is foundational coverage, not a claim to detect every application-level error. Native crash
artifacts, container engines, macOS Unified Log and Windows Event Log are connected; deeper ETW,
Endpoint Security and application-specific health probes remain future integrations.

The background collector is event-driven rather than polling. See the explicit
[performance budget](docs/performance-budget.md).

## Operational logs

Structured JSONL operational events are written to `.rescueloop/logs` with
daily rotation and bounded retention. Inspect them with `rescueloop logs` or
`rescueloop logs --lines 250`. Configure verbosity with `RUST_LOG` and retention
with `RESCUELOOP_LOG_RETENTION_DAYS`. See the [logging contract](docs/logging.md).

## Run

```sh
cargo run -p rescueloop -- watch
```

`watch` runs a shared event-source runtime. Sources are enabled by default and can be managed with
`rescueloop sources list|enable|disable`. Docker/Podman streams activate when their CLIs are installed. Container
`die`, `oom`, and unhealthy events are normalized without polling; repeated failures are classified
as restart loops. When an engine is temporarily unavailable, its source reconnects with bounded
exponential backoff without stopping the other sources.

System service/resource failures use Windows Event Log subscriptions. macOS Unified Log support is
enabled only for an authorized root daemon because current macOS versions reject `log stream` for a
normal user LaunchAgent; the default user watcher never retries this unavailable source.

## Install and first-time setup

Release installers download over HTTPS and verify the selected archive against `SHA256SUMS` before
changing the user PATH:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/ostapondo/rescueloop/main/scripts/install.sh | sh
rescueloop setup
```

On Windows PowerShell, download and review `scripts/install.ps1`, then run it. Release artifacts also
include a macOS `.pkg`, Windows `.msi`, a Homebrew formula template, and WinGet manifests.

`rescueloop setup` performs explicit AI selection, Event Source selection, user PATH installation,
storage initialization and optional login-daemon installation. Afterwards, `rescueloop` with no
subcommand opens the TUI.

For a source build, install the watcher for the current user so it starts at login:

```sh
cargo build --release -p rescueloop
target/release/rescueloop service install
target/release/rescueloop service status
```

Use `rescueloop service uninstall` to remove the launch agent or scheduled task.

On macOS, Unified Log streaming requires an explicitly privileged system daemon. Install that mode
only when system service/resource diagnostics are required:

```sh
sudo target/release/rescueloop service install-system
sudo target/release/rescueloop service uninstall-system
```

In another terminal, connect an interactive console to the background watcher:

```sh
rescueloop
```

On first launch the console detects supported local AI CLIs and asks which one should handle
diagnosis. Setup can also be rerun explicitly:

```sh
rescueloop setup
```

The current adapters detect Codex CLI and Claude Code. The selected executable and agent kind are
stored locally in `.rescueloop/config.json`; no API keys are copied into this file. Event Source
preferences are stored in `.rescueloop/settings.json`.

```sh
rescueloop sources list
rescueloop sources disable containers
rescueloop sources enable containers
rescueloop service install
rescueloop service status
```

The default console is a full-screen terminal UI. Use `↑`/`↓` (or `j`/`k`) to select an incident,
`Enter` to open its evidence, `a` to request AI analysis, `y` to grant consent, `r` to review the
proposed repair, `Esc` to go back, and `q` to quit. AI analysis runs asynchronously: the UI remains
responsive and shows an animated status plus elapsed time while the selected agent is working.
New incidents appear automatically without restarting the console.

For terminals that do not support a full-screen interface, retain the command-based console with:

```sh
cargo run -p rescueloop -- console --plain
```

The plain console supports `incidents`, `details <n>`, `analyze <n>`, `replay <n>`, and `quit`.
Artifact-derived incident IDs are deterministic and written atomically, so concurrent watchers
cannot add the same crash report twice.

Equivalent active failures are grouped by their stable fingerprint. The console shows one row with
an occurrence count and first/last observation timestamps instead of duplicating every recurrence.

Versioned JSON incident documents remain the source of truth. `index-v1.db` is only a disposable
SQLite projection used for fast ordering and future correlation queries. RescueLoop verifies the
index, quarantines corruption, and rebuilds it from JSON; incompatible future schemas use a new
versioned filename instead of an in-place database migration.

```sh
rescueloop index status
rescueloop index rebuild
```

Rebuilding or deleting the index never deletes incidents, analyses, evidence, or repair history.
Every detected event is also written once to `occurrences/<event-id>.json`; grouping updates the
compact incident shown by the UI without erasing the immutable original occurrence.

AI receives a bounded evidence packet rather than the raw incident: local artifact paths and launch
arguments are removed, fields are allowlisted, diagnostic lines are capped, and completeness plus
missing-evidence metadata is included. Typed repairs currently cover quarantine, cache regeneration,
JSON config patching, POSIX permissions, exact service restart, and exact Docker/Podman container
restart. File/config/permission changes record rollback state; operational repairs require an exact
identity from evidence and write a verification receipt.

Observe a command and optionally retain its arguments for exact replay:

```sh
cargo run -p rescueloop -- run --record-args /path/to/program --flag
cargo run -p rescueloop -- replay .rescueloop/incidents/<id>.json
```

Arguments may contain secrets, so they are not retained unless `--record-args` is explicitly set.
Recorded arguments and local working paths are stripped from the AI request in either case.

After an incident is detected:

```sh
cargo run -p rescueloop -- analyze .rescueloop/incidents/<id>.json \
  --endpoint http://localhost:8080/v1/rescueloop/analyze \
  --output analysis.json
```

Review a repair without making changes:

```sh
cargo run -p rescueloop -- repair \
  .rescueloop/incidents/<id>.json analysis.json \
  --allow-root /exact/application/data/root
```

After reviewing the printed transaction, explicitly apply it:

```sh
cargo run -p rescueloop -- repair \
  .rescueloop/incidents/<id>.json analysis.json \
  --allow-root /exact/application/data/root \
  --approve
```

The target must already exist, must exactly match an artifact recorded in the incident evidence,
must be a strict descendant of `--allow-root`, and cannot be a symbolic link. Filesystem roots are
rejected. The repair is also rejected when the incident has no exact replay context. Transaction
records and backups are stored under `.rescueloop/transactions`.
Lineage is stored as append-only JSONL in `.rescueloop/repair-ledger.jsonl`; its hash chain is
verified whenever it is loaded.

The endpoint receives `AnalysisRequest` and returns `AnalysisResponse` as defined in
`crates/rescueloop-core/src/lib.rs`. This deliberately keeps RescueLoop independent of OpenAI,
Anthropic, Gemini, local models, or agent frameworks. A small adapter can translate this contract
to any provider.

## Local MCP integration

RescueLoop can expose its local incident store to a user-selected MCP client:

```sh
rescueloop --incident-dir /absolute/path/to/.rescueloop/incidents mcp
```

The server uses local `stdio` only: it opens no network port and runs with the same OS privileges as
the client that launches it. It exposes only `list_incidents` and `get_incident`. Both are read-only;
incident details pass through the same bounded redaction used for AI analysis. The MCP surface does
not expose raw artifacts, arbitrary paths, launch arguments, working directories, analysis,
replay, repair, rollback, or shell execution.

MCP requires an absolute incident directory. On Unix, RescueLoop enforces owner-only (`0700`) access
on its state and incident directories and refuses symlinked or foreign-owned state roots. On Windows,
it replaces inherited access with an ACL for the current user, Local System, and Administrators.
Inbound protocol messages are capped at 1 MiB and tool arguments are validated against generated
schemas with unknown fields rejected.

Configure the MCP client with the absolute path to the `rescueloop` binary and the arguments above.
Only configure it in clients and workspaces you trust: a local MCP process shares the launching
client's operating-system security boundary.
See the [MCP security and operations contract](docs/mcp-security.md) for the threat model and release
checks.

## Security boundary

- Detection never sends data over the network.
- Analysis requires an explicit command.
- The bearer token is read from `RESCUELOOP_AI_TOKEN` or `--token` and is never persisted.
- AI cannot request arbitrary shell execution.
- A proposal is not a repair: deterministic compilation, scope checks, backup and approval happen first.
- A repair is accepted only when exact replay succeeds; otherwise RescueLoop restores the backup.
- If a regenerated cache becomes non-empty during a failed replay, rollback refuses to delete it and
  reports a critical condition instead of risking new user data.

## Releases, updates and signing

Tagging `v*` runs the release workflow for macOS arm64/x86_64 and Windows x86_64, produces archives,
`.pkg`, `.msi`, and checksums, and publishes them to GitHub Releases. The installers use the verified
`latest` channel by default or a version selected with `RESCUELOOP_VERSION`.

The workflow signs macOS and Windows binaries when publisher certificates are configured. Public
signing and Apple notarization require external credentials and cannot be completed from source code
alone. See [release documentation](docs/releasing.md).
