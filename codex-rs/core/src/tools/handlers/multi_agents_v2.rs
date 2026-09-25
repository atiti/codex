//! Implements the MultiAgentV2 collaboration tool surface.

use crate::agent::AgentStatus;
use crate::agent::agent_resolver::resolve_agent_target;
use crate::agent::types::AgentMessage;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::multi_agents_common::*;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::SubAgentActivityKind;
use codex_tools::ToolName;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

pub(crate) use followup_task::Handler as FollowupTaskHandler;
pub(crate) use interrupt_agent::Handler as InterruptAgentHandler;
pub(crate) use list_agents::Handler as ListAgentsHandler;
pub(crate) use send_message::Handler as SendMessageHandler;
pub(crate) use spawn::Handler as SpawnAgentHandler;
pub(crate) use wait::Handler as WaitAgentHandler;

mod analytics;
mod followup_task;
mod interrupt_agent;
mod list_agents;
mod message_tool;
mod send_message;
mod spawn;
pub(crate) mod wait;

pub(crate) async fn emit_sub_agent_activity(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    item: SubAgentActivityItem,
) {
    let item = TurnItem::SubAgentActivity(item);
    session.emit_turn_item_started(turn, &item).await;
    session.emit_turn_item_completed(turn, item).await;
}

fn agent_message_from_tool(
    message: String,
    source: &crate::tools::context::ToolCallSource,
) -> AgentMessage {
    if matches!(
        source,
        crate::tools::context::ToolCallSource::DirectPlaintextMessage
    ) {
        AgentMessage::Plaintext(message)
    } else {
        AgentMessage::Encrypted(message)
    }
}

#[cfg(test)]
mod routing_prompt_tests {
    use super::*;
    use crate::agent::types::MessageDeliveryMode;
    use codex_protocol::AgentPath;

    #[test]
    fn encrypted_communication_keeps_ephemeral_routing_prompt_in_memory() {
        let communication = AgentMessage::Routed {
            message: Box::new(agent_message_from_tool(
                "encrypted payload".to_string(),
                &crate::tools::context::ToolCallSource::Direct,
            )),
            routing_prompt: Some("Analyze a high-risk database migration".to_string()),
            inherited_model_provider: Some("agentroute-azure".to_string()),
            requested_backend: Some("deepseek".to_string()),
            model_explicit: true,
        }
        .into_communication(
            AgentPath::root(),
            AgentPath::root().join("worker").expect("recipient path"),
            MessageDeliveryMode::TriggerTurn,
        );

        assert_eq!(communication.content, "");
        assert_eq!(
            communication.encrypted_content.as_deref(),
            Some("encrypted payload")
        );
        assert_eq!(
            communication.routing_prompt.as_deref(),
            Some("Analyze a high-risk database migration")
        );
        assert_eq!(
            communication.routing_inherited_model_provider.as_deref(),
            Some("agentroute-azure")
        );
        assert_eq!(
            communication.routing_requested_backend.as_deref(),
            Some("deepseek")
        );
        assert!(communication.routing_model_explicit);
    }

    #[test]
    fn plaintext_communication_renders_exact_task_for_cross_provider_delivery() {
        let communication = AgentMessage::Routed {
            message: Box::new(agent_message_from_tool(
                "Reply with exactly: deepseek child ok".to_string(),
                &crate::tools::context::ToolCallSource::DirectPlaintextMessage,
            )),
            routing_prompt: Some("Reply with exactly: deepseek child ok".to_string()),
            inherited_model_provider: Some("agentroute-azure".to_string()),
            requested_backend: Some("deepseek".to_string()),
            model_explicit: true,
        }
        .into_communication(
            AgentPath::root(),
            AgentPath::root().join("worker").expect("recipient path"),
            MessageDeliveryMode::TriggerTurn,
        );

        assert!(communication.encrypted_content.is_none());
        assert_eq!(
            communication.content,
            "Message Type: NEW_TASK\nTask name: /root/worker\nSender: /root\nPayload:\nReply with exactly: deepseek child ok"
        );
        assert_eq!(
            communication.routing_prompt.as_deref(),
            Some("Reply with exactly: deepseek child ok")
        );
        assert_eq!(
            communication.routing_inherited_model_provider.as_deref(),
            Some("agentroute-azure")
        );
        assert_eq!(
            communication.routing_requested_backend.as_deref(),
            Some("deepseek")
        );
        assert!(communication.routing_model_explicit);
    }
}
