use crate::transport::AgentEvent;
use crate::transport::Transport;
use serde_json::json;
use std::io;
use std::io::Write;

pub struct JsonTransport<W> {
    writer: W,
}

impl<W> JsonTransport<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<W: Write> Transport for JsonTransport<W> {
    fn emit(&mut self, event: AgentEvent) -> io::Result<()> {
        let value = match event {
            AgentEvent::SessionStarted { thread_id } => {
                json!({ "event": "session_started", "thread_id": thread_id })
            }
            AgentEvent::AssistantMessage { content } => {
                json!({ "event": "assistant_message", "content": content })
            }
            AgentEvent::Thinking { content } => {
                json!({ "event": "thinking", "content": content })
            }
            AgentEvent::ToolCall { tool, command } => {
                json!({ "event": "tool_call", "tool": tool, "command": command })
            }
            AgentEvent::ToolResult {
                tool,
                output,
                exit_code,
            } => json!({
                "event": "tool_result",
                "tool": tool,
                "output": output,
                "exit_code": exit_code,
            }),
            AgentEvent::ApprovalRequest {
                id,
                kind,
                command,
                reason,
            } => json!({
                "event": "approval_request",
                "id": id,
                "kind": kind,
                "command": command,
                "reason": reason,
            }),
            AgentEvent::SubagentStart { task } => {
                json!({ "event": "subagent_start", "task": task })
            }
            AgentEvent::SubagentComplete => json!({ "event": "subagent_complete" }),
            AgentEvent::TaskComplete => json!({ "event": "task_complete" }),
            AgentEvent::Error { message } => json!({ "event": "error", "message": message }),
        };

        serde_json::to_writer(&mut self.writer, &value)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()
    }
}
