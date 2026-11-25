# AgentRoute Codex release branch

This branch ports the AgentRoute runtime integrations onto upstream Codex
`rust-v0.171.0` (commit `ccf9be8cc458bd6b8336fe84d68d8893f2691a5e`).
The AgentRoute installer pins an immutable reviewed fork commit. Keep upstream
`main` as a mirror and deliver downstream changes through the versioned release
branch and its pull request.

Validate routing, provider switching, compaction, collaboration, and the CLI,
TUI, and app server before publishing a new runtime. The dependent AgentRoute
release must identify the exact fork commit and pass its own installer checks.

The port retains the final downstream feature delta from
`b960e203bd9bb0cbf1dba8f1ff42eafd41fd3a77`. That release branch preserves the
previous implementation history; this branch applies the resulting features to
the stable upstream tree. Upstream SQLx and opusic fixes replace the obsolete
vendored SQLx source and local opusic header patch.

## Provider switching and compaction

Mixed-provider histories use a plaintext summary for automatic and manual
compaction. Provider provenance is retained across restart, and foreign encrypted
reasoning and tool state are filtered before transport. Native harness message
identities survive this filtering. Encrypted native mail is checked against its
sender provider before its first delivery; foreign or unattributed ciphertext
remains stored but is omitted from transport. Same-provider compaction keeps
upstream's native checkpoint behavior.

An existing opaque checkpoint cannot be silently discarded when changing
providers. Such a request stops before sending the checkpoint and preserves the
history. To make a checkpoint portable, resume the thread with its original
provider and account, supplying:

```sh
codex --disable token_budget -c 'compact_prompt="Summarize the conversation in plaintext, preserving decisions and tool evidence."' resume THREAD_ID
```

Then run `/compact` once before switching providers. An explicit `compact_prompt`
selects ordinary Responses compaction so the owner can generate a plaintext
summary. The recovery command disables the optional token-budget context reset,
which deliberately starts a fresh context window without summarization. A
checkpoint with unavailable ownership requires a pre-compaction backup; its
ciphertext is never sent to a guessed provider.

## Existing state databases

Downstream migration versions 60 through 64 keep their original meaning and
checksums. The two new upstream 0.171 migrations use versions 65 and 66. The
compatibility shim also recognizes exact official upstream ledger checksums and
rekeys them atomically before continuing. Unknown or altered ledgers remain an
error rather than being rewritten speculatively.
