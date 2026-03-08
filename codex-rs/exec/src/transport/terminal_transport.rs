use crate::transport::AgentEvent;
use crate::transport::Transport;
use std::io;
use std::io::Write;

pub struct TerminalTransport<W> {
    writer: W,
}

impl<W> TerminalTransport<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<W: Write> Transport for TerminalTransport<W> {
    fn emit(&mut self, event: AgentEvent) -> io::Result<()> {
        let line = match event {
            AgentEvent::SessionStarted { thread_id } => format!("session started: {thread_id}"),
            AgentEvent::AssistantMessage { content } => format!("assistant: {content}"),
            AgentEvent::Thinking { content } => format!("thinking: {content}"),
            AgentEvent::ToolCall { tool, command } => format!("{tool}: {command}"),
            AgentEvent::ToolResult {
                tool,
                output,
                exit_code,
            } => format!("{tool} result (exit_code={exit_code:?}): {output}"),
            AgentEvent::ApprovalRequest {
                id,
                kind,
                command,
                reason,
            } => format!(
                "approval request [{kind}] {id}: {}{}",
                command.unwrap_or_default(),
                reason
                    .map(|message| format!(" ({message})"))
                    .unwrap_or_default()
            ),
            AgentEvent::SubagentStart { task } => format!("subagent start: {task}"),
            AgentEvent::SubagentComplete => "subagent complete".to_string(),
            AgentEvent::TaskComplete => "task complete".to_string(),
            AgentEvent::Error { message } => format!("error: {message}"),
        };

        writeln!(self.writer, "{line}")
    }
}
