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

## Existing state databases

Downstream migration versions 60 through 64 keep their original meaning and
checksums. The two new upstream 0.171 migrations use versions 65 and 66. The
compatibility shim also recognizes exact official upstream ledger checksums and
rekeys them atomically before continuing. Unknown or altered ledgers remain an
error rather than being rewritten speculatively.
