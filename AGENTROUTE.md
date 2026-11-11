# AgentRoute Codex release branch

This branch ports the AgentRoute runtime integrations onto upstream Codex
`rust-v0.167.0` (commit `086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a`).
The AgentRoute installer pins an immutable reviewed fork commit. Keep upstream
`main` as a mirror and deliver downstream changes through the versioned release
branch and its pull request.

Validate routing, provider switching, compaction, collaboration, and the CLI,
TUI, and app server before publishing a new runtime. The dependent AgentRoute
release must identify the exact fork commit and pass its own installer checks.
