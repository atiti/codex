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

/// Provider-owned checkpoints must be summarized by their owner before changing routes.
/// Silently deleting one can discard the only copy of assistant and tool evidence.
pub(crate) fn validate_checkpoint_ownership(
    input: &[ResponseItem],
    foreign_ids: &std::collections::HashSet<codex_protocol::ResponseItemId>,
    strip_unattributed: bool,
) -> codex_protocol::error::Result<()> {
    if input.iter().any(|item| {
        matches!(
            item,
            ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
        ) && (item.id().is_some_and(|id| foreign_ids.contains(id))
            || (item.id().is_none() && strip_unattributed))
    }) {
        return Err(codex_protocol::error::CodexErr::UnsupportedOperation(
            r#"This history contains an opaque checkpoint from another or unknown provider/account. Resume with the original provider/account using --disable token_budget -c 'compact_prompt="Summarize the conversation in plaintext, preserving decisions and tool evidence."', then run /compact before switching. If ownership is unavailable, recover from a pre-compaction backup instead. The existing history has been preserved."#.to_string(),
        ));
    }
    Ok(())
}

/// Mixed and legacy model outputs need a portable summary. Homogeneous owned
/// checkpoints retain native compaction; an explicit compact_prompt can recover them.
pub(crate) fn requires_portable_compaction(
    history: &[ResponseItemEnvelope],
    provider_id: &str,
) -> bool {
    history.iter().any(|envelope| {
        let item = &envelope.item;
        // Native mail is harness context rather than sampled model output.
        // Metadata-free plaintext tool injection is portable host evidence, too.
        let model_output = !matches!(item, ResponseItem::AgentMessage { .. })
            && !matches!(item, ResponseItem::Message { role, .. } if role != "assistant")
            && !matches!(
                item,
                ResponseItem::AdditionalTools { .. } | ResponseItem::ConfigurationUpdate { .. }
            );
        let owner = envelope
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.model_provider_id.as_deref());
        model_output
            && match owner {
                Some(owner) => owner != provider_id,
                None => {
                    has_provider_owned_state(item)
                        || matches!(item, ResponseItem::Message { role, .. } if role == "assistant")
                }
            }
    })
}
