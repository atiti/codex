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
        let has_foreign_state = foreign_items.clone().any(|envelope| {
            matches!(
                &envelope.item,
                ResponseItem::Reasoning { .. }
                    | ResponseItem::Compaction { .. }
                    | ResponseItem::ContextCompaction { .. }
                    | ResponseItem::FunctionCall {
                        encrypted_function_args: Some(_),
                        ..
                    }
            )
        });
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
