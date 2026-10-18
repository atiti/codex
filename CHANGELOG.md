The changelog can be found on the [releases page](https://github.com/openai/codex/releases).

## Unreleased

- Spill large terminal tool results to shared temporary files while preserving expanded output, bound replay payload bytes, and cap exact large-file diff tracking to prevent memory growth. Log diff sizes instead of full bodies.

- Fix missing command lifecycle events when unified exec cannot create a process, preserving approval rejection and sandbox retry behavior. Thanks @Marvinthebored ([#44557](https://github.com/openai/codex/issues/44557)).
