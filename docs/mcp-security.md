# MCP security and operations

RescueLoop exposes a local, read-only MCP server for incident discovery. This document is the
security contract for that interface.

## Trust boundary

- The only supported transport is `stdio`; RescueLoop opens no socket.
- The MCP process and its launching client have the same operating-system identity and privilege.
- Installing or configuring an MCP server means trusting the client that launches it. RescueLoop
  does not protect data from a malicious process already running as the same OS user.
- Remote MCP, proxying, bearer-token passthrough, and downstream credentials are not supported.

## Storage protection

- MCP requires an absolute incident directory and resolves it once at startup.
- The incident directory must be a real strict descendant of the state root; symlink roots and
  foreign-owned Unix roots are rejected.
- Standard `.rescueloop`/`RescueLoop` state roots are repaired to private permissions. Existing
  custom Unix roots must already be `0700`; RescueLoop refuses them rather than changing an
  arbitrary directory.
- Unix state and incident directories are owner-only (`0700`). Files inherit protection from the
  non-traversable state root.
- Windows removes inherited ACL entries and grants full control only to the current user, Local
  System, and the built-in Administrators group. New files inherit that ACL.

## Exposed data

`list_incidents` returns identifiers and bounded summary metadata. `get_incident` reuses
`AnalysisRequest::bounded` before mapping data into a dedicated MCP DTO. It never returns raw
artifact paths, working directories, launch arguments, unknown evidence fields, repair plans,
tokens, or arbitrary files. Dynamic allowlisted evidence values are encoded as compact JSON strings
inside a string-valued map so the public output schema stays portable.

## Tool policy

- All tools are read-only, closed-world, and non-destructive.
- Tool inputs use generated JSON Schema and deny unknown properties.
- Incidents are selected by UUID only; caller-provided paths are not accepted.
- Domain failures are returned as MCP tool errors without leaking internal filesystem paths.
- Inbound messages are limited to 1 MiB.
- A future mutation tool requires a separate threat model, explicit local human approval, evidence
  binding, least privilege, dry-run, audit logging, verification, and rollback. MCP consent is not
  repair approval.

## Release verification

MCP changes must pass workspace tests and Clippy on macOS and Windows. `scripts/validate-mcp.sh`
automates tool discovery and calls through the pinned reference MCP Inspector in strict mode. CI
runs it for every change. Security tests cover
read-only discovery, argument rejection, tool-error behavior, redaction, message limits, symlink
rejection, and private storage permissions/ACLs.
