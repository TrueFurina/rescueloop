# Storage contract

RescueLoop separates durable evidence from query acceleration.

## Source of truth

Versioned JSON documents under `incidents/`, `occurrences/`, `analyses/`, and `transactions/`, plus the append-only
repair ledger, are durable. A release must continue reading older supported document versions and
must not rewrite all documents during an ordinary update.

Every normalized event is first persisted as an immutable occurrence using `create_new`. The grouped
incident document is a compact UI projection that may advance its occurrence count and last-seen time;
the original evidence remains recoverable from occurrence documents.

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
