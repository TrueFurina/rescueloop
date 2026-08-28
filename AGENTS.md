# RescueLoop agent instructions

This file is the repository-wide implementation contract for humans and coding agents. Source code
is authoritative when this document and the implementation disagree. Update this file when a stable
architectural rule changes.

## Product overview

RescueLoop is a local-first Rust recovery agent for Windows and macOS. It detects failures, builds
bounded evidence, requests analysis only through an explicit user action, validates proposed repairs
as untrusted data, requires approval, applies only typed reversible actions, verifies the result, and
rolls back when verification fails.

```text
detection -> bounded evidence -> explicit analysis -> validation -> dry-run plan
          -> human approval -> typed repair -> replay/verification -> rollback or commit
```

Do not collapse or bypass stages in this pipeline. AI output must never become arbitrary command execution.

## Sources of truth

- Versioned incident JSON is the durable source of truth for incidents and evidence.
- The SQLite index is a disposable projection. It must remain rebuildable and must not contain the
  only copy of user data.
- The append-only lineage ledger is the source of truth for repair outcomes and causal history.
- Raw artifacts, private paths, launch arguments, tokens, and secrets remain local and outside AI
  and MCP payloads.
- Existing source and tests outrank README files, issues, plans, and agent-generated notes.

## Workspace map

- `crates/rescueloop-core`: domain types, incidents, bounded evidence, analysis contracts, and traits.
- `crates/rescueloop-platform`: OS/container events, crash artifacts, diagnostics, supervised commands,
  replay context, and platform-specific behavior.
- `crates/rescueloop-agent`: provider adapters, local agent discovery, prompt construction, response
  extraction, and validation of model-generated analysis.
- `crates/rescueloop-repair`: typed repair plans, scope policy, transactions, receipts, verification,
  and rollback.
- `crates/rescueloop-ledger`: append-only hash-chained repair history and causal classification.
- `crates/rescueloop-index`: rebuildable SQLite projection over incident JSON.
- `apps/rescueloopd`: CLI/TUI composition root, event runtime, storage, logging, services, repair flow,
  and MCP server.

Keep dependency direction toward `rescueloop-core`. Cross-cutting orchestration belongs in the
application, not in the domain crate.

## Task workflow

Before a substantive change:

1. Identify affected crates, trust boundaries, persisted formats, and platforms.
2. Read the relevant implementation and tests; do not infer behavior from names alone.
3. Assess MCP, redaction, approval, verification, rollback, storage, and platform-parity impact.
4. Choose focused tests and negative cases before implementing.

While implementing:

- Make the smallest coherent change that preserves the safety pipeline.
- Prefer existing domain types and bounded serializers over parallel representations.
- Keep OS-specific code behind explicit platform boundaries.
- Preserve unrelated working-tree changes; never discard them to simplify the task.
- Write comments for durable constraints and non-obvious reasons, not the current task narrative.

Before completion:

1. Run focused tests and the repository quality gates below.
2. Re-check negative paths, redaction, cancellation, persistence, verification, and rollback.
3. Update user-facing and architectural documentation when behavior changed.
4. State MCP impact explicitly, including why no MCP change is needed when applicable.

## Core safety invariants

- Model-generated paths, IDs, actions, parameters, explanations, and confidence values are untrusted.
- Analysis may propose an action but may not execute or authorize it.
- Approval binds to the exact reviewed incident, evidence, plan, targets, and parameters.
- A dry run must not mutate user or system state.
- Back up recoverable state before mutation and record enough information to verify and roll back.
- Verification must test the original failure or an explicitly equivalent bounded check. A successful
  repair command is not proof that the incident is fixed.
- Verification failure must not be reported as success. Roll back when the action contract requires it.
- Never add generic shell actions, arbitrary command execution, script evaluation, or unrestricted
  filesystem primitives to the repair model.

## Filesystem, identity, and process safety

- Resolve targets from validated incident evidence or another explicitly bounded local source, never
  directly from caller- or model-provided paths.
- Authorize the resolved target, not a raw string prefix. Reject traversal, symlinks, reparse-point
  escapes, filesystem roots, and targets outside configured scope.
- Avoid check-then-use gaps. Keep identity and evidence binding valid through mutation and verification.
- Treat service names, container identities, executable paths, working directories, and launch
  arguments as privileged data.
- Replay only exact, locally recorded launch context. Never reconstruct a command from model text.
- Preserve platform security semantics. Unix ownership/mode checks and Windows ACL/reparse-point
  handling require platform-appropriate implementations and tests.

Reviewers must reject:

- model-provided paths passed directly to filesystem APIs;
- containment checks based on string prefixes;
- authorization performed before final identity resolution;
- repair targets absent from bound evidence;
- MCP or analysis consent treated as repair approval;
- logs containing raw arguments, home paths, tokens, artifacts, or unredacted model payloads.

## Evidence, privacy, and logging

- Reuse the core bounded/redacted evidence representation. Do not create a permissive serialization
  path for AI, MCP, telemetry, logs, or exports.
- Prefer allowlists to blocklists. Keep size, count, and line-length bounds explicit and tested.
- Represent missing evidence honestly; do not manufacture completeness or confidence.
- Structured logs must pass through the existing enrichment and redaction path.
- Keep protocol messages on stdout and diagnostics on stderr or in the existing local log sink.
- Do not place secrets in command-line arguments used by tests or development scripts.

## Async runtime and event sources

- Event sources must be bounded, cancellable, and isolated so one stalled source does not block others.
- Prefer native event APIs to polling. Polling and retry loops require documented intervals/backoff,
  cancellation, and resource bounds.
- Reconnect with bounded backoff and without busy loops.
- Preserve graceful shutdown and durable handoff of already accepted observations.
- Move blocking filesystem, database, or process work behind an appropriate bounded blocking boundary.

## Storage and compatibility

- Persisted formats must be versioned or backward-compatible. Do not silently reinterpret old data.
- Write durable state atomically where interruption could corrupt it.
- Index corruption must be recoverable from incident JSON without losing evidence, analysis, repair
  history, or occurrence data.
- Validate all IDs before store lookup. Never accept a caller-provided incident filesystem path.
- New lifecycle states require transition rules, serialization tests, UI behavior, history implications,
  and MCP assessment.

## MCP compatibility rule

For every user-visible change to incidents, evidence, analysis, repair plans, lifecycle state, or
history, explicitly assess whether the RescueLoop MCP surface needs a compatible change.

- Update MCP schemas, redaction, documentation, and tests when the feature belongs on the agent surface.
- State why no MCP change is needed when the feature is local-only, privileged, unsafe to expose, or
  outside the MCP boundary.
- Keep MCP additive and backward-compatible. Do not rename or remove tools or output fields without
  a versioning or migration plan.
- Treat clients, arguments, protocol frames, and model-generated content as untrusted input.

## MCP security boundary

- The only current transport is local `stdio`. Do not add a listening socket or remote transport
  without explicit authorization, authentication, scopes, TLS, and a threat model.
- MCP is read-only by default. Never expose repair, replay, rollback, shell execution, arbitrary file
  reads or paths, secrets, tokens, raw artifacts, launch arguments, or working directories.
- Resolve incidents only by validated ID inside the configured incident store.
- MCP consent is not repair approval.
- A proposed mutation tool requires a separate least-privilege capability design, explicit local human
  approval, evidence binding, dry-run, audit logging, verification, and rollback.

MCP changes must test initialization, tool discovery, stable schemas, malformed input, unknown tools,
invalid arguments, traversal, out-of-store IDs, bounds, redaction, and the absence of mutation tools.

## Rust implementation guidelines

- Prefer explicit domain types and enums over strings or `serde_json::Value` beyond protocol boundaries.
- Make invalid repair and lifecycle states difficult to represent.
- Do not use `unwrap`, `expect`, or panic for untrusted runtime input. Tests may use them when they
  express the test invariant clearly.
- Attach context to operational errors without leaking sensitive values.
- Avoid `unsafe`. If an OS boundary requires it, keep it minimal, document invariants, and add focused tests.
- Keep public APIs narrow. Review new dependencies for need, maintenance, security, licensing, and
  platform impact.
- Reuse existing hashing, redaction, storage, and validation helpers instead of duplicating policy.

## Required checks

Run focused checks during iteration:

```text
cargo test -p <changed-crate>
cargo test -p rescueloop <module-or-test-filter>
```

Before completing Rust changes, run:

```text
cargo fmt --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

If a check cannot run, report the exact command and reason. Never describe an unrun check as passing.

Also run relevant validation for:

- MCP: protocol validation and MCP-specific tests;
- logging/redaction: logging validation and sensitive-field negative tests;
- Windows storage, services, events, ACLs, or processes: Windows-native E2E tests;
- installers/releases: relevant packaging and build checks;
- event durability, retries, or shutdown: focused failure tests and an appropriate bounded soak.

## Security-fix hygiene

This is a public repository. Do not reveal an exploitable recipe in branches, commits, PR titles,
test names, fixtures, or comments. Describe the enforced behavior neutrally:

```text
Good: validate incident identifiers before store lookup
Bad:  fix traversal exploit using crafted MCP incident IDs
```

Never use real usernames, hostnames, home paths, customers, incident contents, tokens, or private
artifacts in public material. Use synthetic fixtures. Confidential reports belong in the project's
private disclosure channel.

## Definition of done

A change is complete only when:

- requested behavior is implemented without weakening the safety pipeline;
- success, failure, and adversarial cases are covered;
- persisted data and platform compatibility are addressed;
- logs, AI payloads, and MCP responses remain bounded and redacted;
- verification and rollback semantics remain honest;
- documentation reflects user-visible behavior;
- MCP impact is handled explicitly;
- required checks pass, or unrun checks are disclosed precisely.
