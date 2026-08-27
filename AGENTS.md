# RescueLoop agent instructions

## Product boundary

RescueLoop is a local-first, safety-constrained recovery agent. Preserve the boundary between
detection, analysis, repair approval, verification, and rollback. AI output is untrusted data and
must never become arbitrary command execution.

## MCP compatibility rule

For every user-visible feature or change to incidents, evidence, analysis, repair plans, lifecycle
state, or history, explicitly assess whether the RescueLoop MCP surface needs a compatible change.

- Update MCP schemas, redaction, documentation, and tests when the feature should be available to
  agents.
- State in the implementation or review why no MCP change is needed when the feature is intentionally
  local-only, privileged, unsafe to expose, or outside the MCP product boundary.
- Keep MCP additive and backward-compatible where practical. Do not rename or remove a tool or output
  field without a versioning or migration plan.
- Treat all MCP clients, tool arguments, and model-generated content as untrusted input.

## MCP security boundary

- The default MCP transport is local `stdio`. Do not add a listening socket or remote transport
  without explicit user authorization, authentication, authorization scopes, TLS, and a threat-model
  update.
- MCP is read-only by default. Never expose repair application, replay, rollback, shell execution,
  arbitrary file reads, arbitrary paths, secrets, tokens, raw diagnostic artifacts, launch arguments,
  or working directories through MCP.
- Reuse the core bounded/redacted evidence representation. Do not create a second permissive
  serialization path.
- Resolve incidents only by validated incident ID inside the configured incident store. Reject path
  traversal and never accept a caller-provided filesystem path.
- If a mutation tool is ever proposed, require a separate capability design with least privilege,
  explicit local human approval, evidence binding, dry-run, audit logging, verification, and rollback.
  MCP consent alone is not repair approval.
- Keep protocol messages on stdout and diagnostics on stderr or in the existing local log sink.

## Required checks

After Rust changes, run `cargo fmt --check`, `cargo test --workspace`, and Clippy with warnings denied
when feasible. MCP changes must also test protocol initialization, tool discovery, invalid input,
redaction, and the absence of mutation tools.
