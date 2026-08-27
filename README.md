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
- executes only `quarantine_path` and `regenerate_cache`, behind dry-run, explicit scope and approval;
- backs up before mutation, replays the original action, and rolls back when verification fails.
- appends outcomes to a tamper-evident local lineage ledger and distinguishes lifecycle updates,
  regressions, independent new failures and stale verification after an app/environment change.

This is foundational coverage, not a claim to detect every application-level error. ETW/Event Log,
Endpoint Security, hang probes, service/installer failures, desktop UI, more repair primitives and
richer ready-state verification are subsequent milestones.

The background collector is event-driven rather than polling. See the explicit
[performance budget](docs/performance-budget.md).

## Run

```sh
cargo run -p rescueloop -- watch
```

In another terminal, connect an interactive console to the background watcher:

```sh
cargo run -p rescueloop -- console
```

On first launch the console detects supported local AI CLIs and asks which one should handle
diagnosis. Setup can also be rerun explicitly:

```sh
cargo run -p rescueloop -- setup
```

The current adapters detect Codex CLI and Claude Code. The selected executable and agent kind are
stored locally in `.rescueloop/config.json`; no API keys are copied into this file.

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

The target must already exist, must be a strict descendant of `--allow-root`, and cannot be a
symbolic link. Filesystem roots are rejected. The repair is also rejected when the incident has no
exact replay context. Transaction records and backups are stored under `.rescueloop/transactions`.
Lineage is stored as append-only JSONL in `.rescueloop/repair-ledger.jsonl`; its hash chain is
verified whenever it is loaded.

The endpoint receives `AnalysisRequest` and returns `AnalysisResponse` as defined in
`crates/rescueloop-core/src/lib.rs`. This deliberately keeps RescueLoop independent of OpenAI,
Anthropic, Gemini, local models, or agent frameworks. A small adapter can translate this contract
to any provider.

## Security boundary

- Detection never sends data over the network.
- Analysis requires an explicit command.
- The bearer token is read from `RESCUELOOP_AI_TOKEN` or `--token` and is never persisted.
- AI cannot request arbitrary shell execution.
- A proposal is not a repair: deterministic compilation, scope checks, backup and approval happen first.
- A repair is accepted only when exact replay succeeds; otherwise RescueLoop restores the backup.
- If a regenerated cache becomes non-empty during a failed replay, rollback refuses to delete it and
  reports a critical condition instead of risking new user data.
