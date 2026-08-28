# Roadmap

RescueLoop is moving toward a complete local observability and recovery application. The goal is
one place that can notice a failure, collect the useful evidence, explain what probably happened,
and help return the system to a working state.

The order below is a direction, not a promise of release dates. Safety comes before coverage:
detection may be automatic, but repair must stay bounded, reviewable, verifiable, and reversible.

## Broader platform support

- Add Linux detection, packaging, and service integration.
- Deepen macOS and Windows system event coverage.
- Observe services, processes, filesystems, networks, and resource pressure.
- Improve Docker, Podman, and local development environment support.

## One local view of the system

- Build a desktop application for incidents, timelines, and system health alongside the existing
  CLI and TUI. The graphical app should not replace scriptable workflows.
- Add configurable native notifications for new incidents, completed analysis, repairs awaiting
  approval, verification failures, and rollbacks. Notifications must not expose private evidence.
- Correlate crashes, logs, resource pressure, and recent changes.
- Add searchable history across applications and services.
- Distinguish active failures, regressions, and resolved incidents.
- Make local retention and export configurable.

## Better diagnosis

- Add application-specific health checks and pluggable evidence collectors.
- Support local and hosted models without tying the core to one provider.
- Build a specialized diagnosis agent for crashes, logs, services, processes, and containers.
- Let the agent request additional bounded evidence, explain its conclusions, and propose typed
  repairs without giving it direct control of the machine.
- Improve diagnosis with tested failure scenarios, evaluation sets, and feedback from verified
  repair outcomes.
- Link conclusions to the evidence that supports them.
- Report confidence, ambiguity, and missing evidence plainly.

## Safe recovery

- Add more bounded and reversible repair actions.
- Improve dry runs and human-readable change previews.
- Build verification around each repair type.
- Roll back automatically when a recovery attempt makes things worse.
- Make recovery recipes reviewable, shareable, and testable.

## Open ecosystem

- Stabilize schemas for incidents and evidence.
- Extend read-only MCP access when new redacted observability data is safe to expose.
- Document adapters for agents and model providers.
- Add contributor-friendly fixtures for platforms and failure types.
- Keep builds and releases reproducible.

## MCP boundary

The MCP server will remain local and read-only by default. New observability features may add
backward-compatible fields or tools for bounded, redacted data. Repair application, replay,
rollback, shell execution, arbitrary file access, secrets, and raw diagnostic artifacts are outside
the MCP surface unless a separate capability and approval design is agreed first.
