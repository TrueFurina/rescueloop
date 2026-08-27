# Operational logging

RescueLoop writes structured JSON Lines logs independently from terminal output.

## Location and retention

Logs are stored next to the state directories:

```text
.rescueloop/logs/rescueloop.jsonl.YYYY-MM-DD
```

Files rotate daily. The newest 14 daily files are retained by default. Set
`RESCUELOOP_LOG_RETENTION_DAYS` to a positive integer to change retention.

Use the CLI to inspect the latest file:

```sh
rescueloop logs
rescueloop logs --lines 250
```

## Levels

The default level is `info` for RescueLoop crates. Override it with standard
`RUST_LOG` directives:

```sh
RUST_LOG=rescueloop=debug,rescueloop_platform=debug rescueloop watch
```

## Event contract

Every record contains a timestamp, level, target, message and stable `event`
name. Lifecycle records add identifiers and outcomes where applicable.

Important event families:

- `runtime.*`: startup, clean shutdown, failure and panic;
- `logging.*`: logger initialization;
- `watch.*` and `source.*`: heartbeat, source failure and recovery;
- `observation.*` and `incident.*`: detection, grouping and persistence;
- `analysis.*`: analysis lifecycle without prompts or tokens;
- `repair.*`: dry-run, apply, verification and rollback;
- `verification.*`: replay result;
- `lineage.*`: durable ledger append.

Logs never include AI bearer tokens, launch arguments, raw evidence bodies or
repair file contents. Local state paths may appear in logger initialization and
error messages.
