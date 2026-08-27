# Storage contract

RescueLoop separates durable evidence from query acceleration.

## Source of truth

Versioned JSON documents under `incidents/`, `occurrences/`, `analyses/`, and `transactions/`, plus the append-only
repair ledger, are durable. A release must continue reading older supported document versions and
must not rewrite all documents during an ordinary update.

Ledger readers take a shared file lock and appenders take an exclusive cross-process lock. Each
encoded entry, newline and hash-chain link is written as one locked append and synced before the
lock is released, preventing concurrent watcher/CLI processes from forking the chain.
Initial-lineage recovery uses one atomic `append_if_missing` operation: chain validation, duplicate
check and append happen under the same lock. This removes the previous second full-ledger read and
prevents concurrent recovery from creating duplicate initial entries.
If power loss leaves an unterminated final JSONL record, the next exclusive append preserves those
bytes in a `torn-*` quarantine file, truncates only to the last fully validated hash-chain record,
syncs the repair, and then appends. Invalid complete records still fail closed as tampering.

Every normalized event is first persisted as an immutable occurrence using `create_new`. The grouped
incident document is a compact UI projection that may advance its occurrence count and last-seen time;
the original evidence remains recoverable from occurrence documents.
Both paths use a same-directory synced temporary file. Immutable occurrences are published with an
atomic no-clobber link; grouped projections use an atomic replace and directory sync on Unix. Windows
replacement uses `MoveFileExW` with replace and write-through flags.
Incident projections and pending observation-journal records are capped at 4 MiB when read. Both
normal readers and disposable-index rebuilds stream only up to the limit plus one byte, preventing a
corrupt or replaced state document from causing an unbounded allocation.
Grouping, projection replacement, index update and initial lineage append are serialized by a
cross-process incident-store lock. Concurrent collectors therefore cannot lose occurrence-count or
evidence updates.
Occurrence creation is idempotent by UUID. Re-delivery of the same native event returns the existing
projection without incrementing its count or duplicating evidence.

Before an occurrence is published, RescueLoop durably writes an `observation-journal` transaction.
Grouped projections record the last applied occurrence UUID. On watcher startup—and before any new
ingestion—pending transactions are replayed under the store lock, missing lineage is restored, and
already-applied projections are recognized without double counting.

## Disposable index

`index-v1.db` contains only incident projections: identity, path, timestamps, grouping, application,
kind, status, and occurrence count. It never contains the only copy of evidence.

Rules:

- there are no destructive in-place index migrations;
- an incompatible schema gets a new filename such as `index-v2.db`;
- `PRAGMA quick_check` and `user_version` are verified before use;
- a broken index is renamed with a `corrupt-*` suffix and rebuilt;
- directory modification checkpoints detect JSON created while an index update was interrupted;
- direct JSON reading remains the fallback when indexing is unavailable;
- recurring observations use the indexed `group_key` path and validate only matching JSON
  projections; legacy documents without a stored key receive one compatibility scan;
- rebuild uses a temporary database and installs it only after a successful transaction.

This design gives SQLite query performance without making recovery or downgrade depend on SQLite.
