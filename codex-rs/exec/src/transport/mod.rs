mod json_transport;
#[allow(dead_code)]
mod terminal_transport;

pub use json_transport::JsonTransport;

use std::io;

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    SessionStarted {
        thread_id: String,
    },
    AssistantMessage {
        content: String,
    },
    Thinking {
        content: String,
    },
    ToolCall {
        tool: String,
        command: String,
    },
    ToolResult {
        tool: String,
        output: String,
        exit_code: Option<i32>,
    },
    ApprovalRequest {
        id: String,
        kind: String,
        command: Option<String>,
        reason: Option<String>,
    },
    SubagentStart {
        task: String,
    },
    SubagentComplete,
    TaskComplete,
    Error {
        message: String,
    },
}

pub trait Transport {
    fn emit(&mut self, event: AgentEvent) -> io::Result<()>;
}
