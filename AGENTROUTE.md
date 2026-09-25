# AgentRoute downstream branch

This branch carries AgentRoute's Codex harness integrations. Routing policy and the
installer remain in https://github.com/atiti/agent-router.

Branch policy:

- `main` mirrors `openai/codex` and contains no downstream changes.
- `agentroute` is the downstream development line, periodically rebased onto a reviewed main.
- `agentroute-release-<version>` ports the integrations onto an exact stable release commit.
- `archive/main-before-agentroute-2026-09-25` preserves the old fork main and its
  remote-session work before the mirror was established.

Tracking main is not automatic merging or permission to publish a release. Preserve a
backup ref before each rebase, resolve conflicts deliberately, and validate the affected
hook, routing, authentication, provider-state, compaction, and UI behavior. Keep downstream
changes in focused commits; drop changes that upstream has adopted.

Stable AgentRoute builds must pin a reviewed commit. Do not substitute moving main for
the stable release base. The existing packaged patch stack remains the installer input
until its migration is complete; the fork and exported patches must agree before a release.

See https://github.com/atiti/agent-router/blob/main/docs/codex-fork.md for the maintenance
workflow and https://github.com/atiti/agent-router/blob/main/docs/releasing.md for release gates.
