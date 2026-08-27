# Operational logging

RescueLoop writes structured JSON Lines logs independently from terminal output.

## Location and retention

Logs are stored next to the state directories:

```text
.rescueloop/logs/rescueloop-YYYY-MM-DD-RUN_ID-NNNN.jsonl
```

Files rotate daily or after 10 MiB, whichever comes first. Rotated files are
compressed with gzip. Files older than 14 days are removed. Set
`RESCUELOOP_LOG_RETENTION_DAYS` to change retention and
`RESCUELOOP_LOG_MAX_BYTES` to change the size threshold.
Per-run file locks prevent CLI, TUI and watcher processes from rotating,
compressing or deleting each other's active segments.

Use the CLI to inspect the latest file:

```sh
rescueloop logs
rescueloop logs --lines 250
rescueloop logs --follow --level warn
rescueloop logs --event repair.rolled_back --output json
rescueloop logs --correlation-id <incident-id>
rescueloop logs --since 2026-08-27T10:00:00Z --until 2026-08-27T11:00:00Z
rescueloop logs --verify --lines 0
```

## Levels

The default level is `info` for RescueLoop crates. Override it with standard
`RUST_LOG` directives:

```sh
RUST_LOG=rescueloop=debug,rescueloop_platform=debug rescueloop watch
```

## Optional OTLP export

Local JSONL remains authoritative. To additionally export records using
OTLP/HTTP JSON, set an exact logs endpoint:

```sh
RESCUELOOP_OTLP_ENDPOINT=https://collector.example/v1/logs rescueloop watch
```

Optional headers use comma-separated `name=value` pairs in
`RESCUELOOP_OTLP_HEADERS`. Header values are never logged. Records are committed
to a bounded disk spool before export. Failed batches retry with exponential
backoff; network failures do not block or remove local logs.

Native CI validates structured startup, restart, failure and panic records plus
querying on macOS and Windows. Run `scripts/validate-logging.sh` locally on
Unix-like systems.

## Event contract

Every record contains `schema_version`, `run_id`, `correlation_id`, timestamp,
level, target, message, stable `event`, sequence and SHA-256 chain fields.
`logs --verify` detects modified, deleted or reordered retained records.
Per-run monotonic nanoseconds preserve ordering when the wall clock moves.

Important event families:

- `runtime.*`: startup, clean shutdown, failure and panic;
- `logging.*`: logger initialization;
- `watch.*` and `source.*`: heartbeat, queue depth, counters, source failure and recovery;
- `observation.*` and `incident.*`: detection, grouping and persistence;
- `analysis.*`: analysis lifecycle without prompts or tokens;
- `repair.*`: dry-run, apply, verification and rollback;
- `verification.*`: replay result;
- `lineage.*`: durable ledger append.

The writer centrally redacts token, password, authorization, launch argument,
raw evidence, file-content and path fields. Home-directory fragments embedded
in other strings are replaced with `<HOME>`.
