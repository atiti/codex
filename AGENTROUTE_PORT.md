# AgentRoute 0.157 port candidate

Base: OpenAI `rust-v0.157.0`, commit
`00c972ed5d6ff6499317fd41b7f23605b8e6850d`.

This is the stable-release candidate branch, not the moving `main` mirror and not a
published AgentRoute binary release. The existing installed runtime is unchanged.

## Carried integrations

- Prompt/subagent routing, reasoning and profile selection, provider-state isolation,
  destination model metadata, compaction preservation, and route UI from runtime v40.
- Recovery of interrupted custom tool calls without a debug-build panic.
- Applied/rejected route receipts in Stop/SubagentStop hook payloads.

The upstream release manifest is versioned 0.157.0 but its checked-in Cargo lockfile
still uses 0.0.0 for workspace packages. This branch refreshes those package versions
without changing external dependency versions. Bazel lock regeneration produced no diff.
An obsolete downstream tool-metadata filter was removed: upstream already filters by
resolved request destination for HTTP and WebSocket requests.

## Validation on macOS arm64

- CLI compile check passed for the routing baseline.
- 206 focused core tests passed, covering providers, compaction, hooks, turn-setting
  changes, and interrupted custom-tool history recovery.
- All 180 hook crate tests passed, including generated schema fixtures.
- Two focused TUI tests passed: route notices remain transcript cells rather than
  warnings, and the status line uses the routed provider.
- `just fmt` and `just bazel-lock-update` completed successfully.
- Scoped `just fix -p codex-core -p codex-hooks -p codex-tui` and the final
  `cargo clippy -p codex-core --lib --locked` completed. Non-blocking style/argument-count
  warnings remain; unrelated upstream autofixes were deliberately excluded.

Reproduce the focused tests from `codex-rs`:

```sh
just test -p codex-core --lib -E 'test(provider) or test(compact) or test(hook) or test(step_activation) or test(normalize_adds_missing_output_for_custom_tool_call)'
just test -p codex-hooks --lib
just test -p codex-tui --lib -E 'test(agentroute_route_notices) or test(status_line_model_uses_the_routed_turn_provider)'
```

## Remaining release gates

The complete workspace test suite, live GPT/Azure/other-provider smoke tests, release
artifact builds, signing/notarization, installer pin/patch-export migration, and Desktop
compatibility have not been certified by these focused checks. Do not treat this branch
as an instruction to replace a working installation. Follow the AgentRoute repository's
release runbook before publishing binaries.
