# Remote Control

`codex --remote-control` starts Codex in a newline-delimited JSON mode over `stdin` and `stdout`.

## Usage

```bash
codex --remote-control
```

Send commands as one JSON object per line:

```bash
echo '{"type":"user_message","content":"Explain this codebase"}' | codex --remote-control
```

## Commands

- `{"type":"user_message","content":"..."}`
- `{"type":"approve","id":"..."}`
- `{"type":"approve","id":"...","decision":"denied"}`
- `{"type":"approve","id":"...","decision":"abort"}`
- `{"type":"reset"}`
- `{"type":"shutdown"}`

## Events

Events are emitted as single-line JSON objects and flushed after every line.

- `session_started`
- `assistant_message`
- `thinking`
- `tool_call`
- `tool_result`
- `approval_request`
- `subagent_start`
- `subagent_complete`
- `task_complete`
- `error`

Example output:

```json
{"event":"session_started","thread_id":"..."}
{"event":"assistant_message","content":"Analyzing repo"}
{"event":"tool_call","tool":"shell","command":"pytest"}
{"event":"tool_result","tool":"shell","output":"tests passed","exit_code":0}
{"event":"task_complete"}
```

`approval_request` events include the request `id` to send back with an `approve` command.
