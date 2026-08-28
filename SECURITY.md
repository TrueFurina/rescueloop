# Security policy

RescueLoop watches failures and can apply approved repairs, so security reports are taken seriously.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting for this repository. Do not open a public issue
for a vulnerability, and do not include secrets or unrelated personal data in a report.

Include the affected version or commit, the operating system, the impact, and the smallest safe way
to reproduce the problem. You can expect an acknowledgement within seven days. A fix or public
disclosure date depends on severity and complexity; the report will stay private while it is being
investigated.

## Supported versions

RescueLoop is pre-1.0 software. Security fixes are made on the latest release and the `main` branch.
Older releases may not receive patches.

## Security boundaries

- Detection is local and does not send evidence over the network.
- AI analysis is an explicit action and receives bounded, redacted evidence.
- Model output is untrusted data and never becomes arbitrary command execution.
- Repairs use allowlisted action types, exact evidence binding, dry-run, explicit approval,
  verification, and rollback where supported.
- The MCP server uses local `stdio`, is read-only, and exposes no repair, replay, rollback, shell,
  arbitrary file, secret, raw artifact, launch argument, or working-directory access.

The MCP boundary is covered by protocol initialization, discovery, invalid-input, redaction, and
absence-of-mutation-tool tests in `scripts/validate-mcp.sh`.
