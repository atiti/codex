# Remote Sessions

Codex exposes a local remote session endpoint for interactive CLI sessions.

`codex exec` can also register itself as an attachable local session when you opt in with:

```bash
codex exec --register-remote-session "explain this repo"
```

Every running session creates metadata and a Unix domain socket under:

```text
~/.codex/remote/
```

Example:

```text
~/.codex/remote/7bfa.json
~/.codex/remote/7bfa.sock
```

## Architecture

The interactive session keeps its normal terminal UX.

A thin remote session controller mirrors runtime events onto a local socket:

```text
runtime -> app event bus -> session controller -> terminal UI
                                         \-> remote socket clients
```

The controller does not replace the agent loop. It only:

- publishes session metadata
- streams live events
- accepts remote commands
- queues remote commands so only one is active at a time

## Metadata Files

Each running session writes a JSON file:

```json
{
  "session_id": "7bfa",
  "cwd": "/Users/example/projects/calendar",
  "title": "calendar-cli",
  "status": "running",
  "pid": 12345,
  "created_at": "2026-03-07T12:34:56Z"
}
```

Active metadata files use `status: "running"`.

Cleanup behavior:

- clean shutdown removes both the `.json` and `.sock` files
- `codex sessions` prunes stale entries whose PID no longer exists
- orphaned sockets without matching metadata are also removed during cleanup

## CLI Commands

Start a normal interactive session:

```bash
codex
```

List local remote sessions:

```bash
codex sessions
```

Attach to a running session:

```bash
codex attach 7bfa
```

You can also set an explicit session title:

```bash
codex --title calendar-cli
```

Title priority is:

1. `--title`
2. first prompt
3. working directory name
4. `codex-session`

For `codex exec --register-remote-session`, the title is derived from the initial prompt and falls back to the working directory name.

## Socket Protocol

The session socket is newline-delimited JSON over a Unix domain socket:

```text
~/.codex/remote/<session_id>.sock
```

New clients first receive a snapshot:

```json
{
  "event": "session_snapshot",
  "title": "calendar-cli",
  "cwd": "/Users/example/projects/calendar",
  "status": "running",
  "phase": "executing",
  "recent_actions": ["user: add tests", "run cargo test"],
  "currentModel": "gpt-5.2-codex",
  "currentReasoningEffort": "medium"
}
```

After that, clients receive live NDJSON events such as:

```json
{"seq":41,"event":"assistant_message","content":"Analyzing repository"}
{"seq":42,"event":"tool_call","tool":"shell","command":"cargo test"}
{"seq":43,"event":"tool_result","output":"tests passed"}
{"seq":44,"event":"task_complete"}
{"seq":45,"event":"model_changed","model":"gpt-5.2-codex","reasoningEffort":"medium"}
```

Oversized text payloads are truncated before emission so a single very large
assistant message or tool output does not tear down attached clients. Truncated
events include:

```json
{"event":"tool_result","output":"...","truncated":true,"originalBytes":131072}
```

## Commands

Clients can send NDJSON commands to the same socket.

Supported commands:

- `user_message`
- `interrupt`
- `approve`
- `list_models`
- `set_model`
- `reset`
- `stop`

Examples:

```json
{"type":"user_message","content":"Add tests for calendar.py"}
{"type":"interrupt"}
{"type":"approve","id":"approval_123","decision":"approved"}
{"type":"approve","id":"patch_123","kind":"patch","decision":"approved"}
{"type":"list_models"}
{"type":"set_model","model":"gpt-5.2-codex","effort":"medium"}
{"type":"reset"}
{"type":"stop"}
```

Notes:

- `interrupt` maps to the same in-session turn interrupt as pressing `Esc` locally.
- `interrupt` is a no-op when there is no cancellable work; clients receive `interrupt_ignored`.
- `approve` defaults to exec approvals.
- Use `"kind":"patch"` for patch approvals.
- `list_models` emits a `model_list` event with the current model, current reasoning effort, and the currently available model presets.
- `set_model` validates the requested model and optional reasoning effort against the live model catalog before switching the session.
- Multiple clients may attach at the same time.
- Remote commands are queued and dispatched one at a time.
- `codex exec --register-remote-session` sessions are observe-only; attached clients receive an error event if they try to send commands.
