//! Provider ownership checks for model requests made outside an ordinary routed turn.

use crate::client::ModelClient;
use crate::client::ModelClientSession;
use codex_history::ResponseItemEnvelope;
use codex_model_provider::SharedModelProvider;
use codex_protocol::models::ResponseItem;

impl ModelClient {
    pub(crate) fn new_session_for_foreign_provider_history(
        &self,
        provider_id: &str,
        provider: SharedModelProvider,
        history: &[ResponseItemEnvelope],
    ) -> Option<ModelClientSession> {
        let foreign_items = history.iter().filter(|envelope| {
            envelope
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.model_provider_id.as_deref())
                != Some(provider_id)
        });
        let has_foreign_state = foreign_items
            .clone()
            .any(|envelope| has_provider_owned_state(&envelope.item));
        if has_foreign_state {
            Some(
                self.new_session_for_mixed_provider_history(
                    provider,
                    foreign_items
                        .filter_map(|envelope| envelope.item.id().cloned())
                        .collect(),
                ),
            )
        } else {
            None
        }
    }
}

/// Opaque payloads are owned by the provider/account that produced them.
pub(crate) fn has_provider_owned_state(item: &ResponseItem) -> bool {
    use codex_protocol::models::AgentMessageInputContent;
    use codex_protocol::models::FunctionCallOutputContentItem;
    match item {
        ResponseItem::Reasoning { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction { .. } => true,
        ResponseItem::FunctionCall {
            encrypted_content,
            encrypted_function_args,
            ..
        } => encrypted_content.is_some() || encrypted_function_args.is_some(),
        ResponseItem::Message {
            encrypted_content, ..
        }
        | ResponseItem::LocalShellCall {
            encrypted_content, ..
        }
        | ResponseItem::ToolSearchCall {
            encrypted_content, ..
        }
        | ResponseItem::CustomToolCall {
            encrypted_content, ..
        }
        | ResponseItem::WebSearchCall {
            encrypted_content, ..
        }
        | ResponseItem::ImageGenerationCall {
            encrypted_content, ..
        } => encrypted_content.is_some(),
        ResponseItem::AgentMessage { content, .. } => content
            .iter()
            .any(|part| matches!(part, AgentMessageInputContent::EncryptedContent { .. })),
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            output.content_items().is_some_and(|parts| {
                parts.iter().any(|part| {
                    matches!(part, FunctionCallOutputContentItem::EncryptedContent { .. })
                })
            })
        }
        _ => false,
    }
}
