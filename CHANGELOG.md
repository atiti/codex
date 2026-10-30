The changelog can be found on the [releases page](https://github.com/openai/codex/releases).

## Unreleased

- Identify progress reviews and ask the working agent to acknowledge adjustments and state its next action; later reviews assess follow-through against observed evidence.

- Accept progress-review verdicts enclosed in a single JSON code block, document the reason byte limit in the reviewer prompt, and report safe validation categories when a verdict is rejected.

- Preserve shipped fork migration records and data on 0.165.0 upgrades; recognize official upstream 60–62 by exact checksums and atomically map them to fork 62–64. Shipped fork rollback remains compatible; reopening with unpatched official upstream requires restoring its pre-upgrade backup.

- Preserve installed task-work migration IDs when upgrading to 0.163.0; new upstream feedback and read-state migrations use distinct IDs.

- Spill large terminal tool results to shared temporary files while preserving expanded output, bound replay payload bytes, and cap exact large-file diff tracking to prevent memory growth. Log diff sizes instead of full bodies.

- Fix missing command lifecycle events when unified exec cannot create a process, preserving approval rejection and sandbox retry behavior. Thanks @Marvinthebored ([#44557](https://github.com/openai/codex/issues/44557)).
