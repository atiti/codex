# AgentRoute integration on Codex 0.161.0

Upstream base: OpenAI `rust-v0.161.0`, commit
`979011409de0a60b52f179721948e65531d26144`.

AgentRoute's runtime pin uses Codex commit
`e7c3db0015098c1aaca6c76d1e7b2b02b59501cd`, which contains the provider-history
resume fix on top of the 0.161.0 port. The surrounding fork branch also carries
CI and maintenance commits; they do not change the pinned runtime source.

## Resume compatibility fix

A resumed thread can contain encrypted reasoning or compaction state created by a
different model provider. Replaying that provider-owned state during Codex's resume
warmup or standalone remote compaction can make the destination provider fail to
decrypt or parse the request. The new provider-history helper identifies foreign
encrypted state using the rollout's provider metadata and filters those item IDs
through the existing mixed-provider session path. Ordinary conversation history
and state owned by the active provider remain available to the request.

The fix covers resume-history WebSocket warmup and standalone automatic or manual
remote compaction. Regression tests switch from an AgentRoute Azure provider to the
OpenAI provider and verify that foreign ciphertext is absent while user messages and
the new provider's compaction checkpoint remain present.

Provider-owned function-call arguments are cleared even when the rollout item has no
response-item ID. Resume prewarm retains only whole recent turns within an 8,000-byte
serialized history budget. Guardian's optional history instruction override is rejected
above a 900-token estimated limit.

## Upstream compatibility notes

Codex 0.161.0 removes the stable app-server v2 `PluginSummary.extensions` field and
its generated extension types. Existing app-server clients must stop relying on that
metadata. It also removes `tui.prompt_suggestions`; existing config files continue to
load, but that setting no longer has an effect.

## Validation recorded for this candidate

- Resume warmup, automatic and manual provider-switch compaction, and WebSocket
  resume tests: 4 passed.
- Ordinary resumed sampling after a provider switch, same-provider resume, and legacy rollout
  without provider metadata: 3 passed.
- UserPromptSubmit provider routing to the selected endpoint and applied/rejected Stop receipts:
  2 passed.
- Id-less encrypted function-call normalization and the prewarm/Guardian context
  limits: 5 passed.
- Provider ownership, same-provider reasoning continuation, compaction, and routed
  guardian client tests: 6 passed.
- `codex-app-server-protocol`: 319 passed, 1 skipped.
- `codex-hooks`: 185 passed.
- CLI, app-server, and TUI development build completed.
- The ChatGPT-Routed 0.161.0 app bundle was built, signed, and validated; the
  official Codex app was not modified.
- AgentRoute Python suite: 446 passed, 1 skipped.
- `just fmt` completed successfully with a temporary uv cache.

The complete Codex workspace test suite and the user's Desktop resume acceptance
remain separate checks. AgentRoute PR #65 is already merged; the 0.5.66 release and
local installation still depend on merging the Codex 0.161.0 port. Before local
installation, finish shared-server sessions and stop it gracefully so the new runtime
can become the owner.
