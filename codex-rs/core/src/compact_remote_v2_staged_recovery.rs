//! Transactional bounded-prefix recovery for an oversized foreground compaction.
use super::CompactionRequestTransport;
use super::ModelClientSession;
use super::Prompt;
use super::RemoteCompactionV2Output;
use super::Session;
use super::StepContext;
use super::run_remote_compaction_request_v2;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use std::collections::HashSet;

/// Compact successive prefixes into a checkpoint, committing nothing until all stages succeed.
pub(super) async fn recover_oversized_history(
    sess: &Session,
    step_context: &StepContext,
    prompt: &Prompt,
    metadata: &crate::responses_metadata::CodexResponsesMetadata,
    client: &mut ModelClientSession,
    original_error: CodexErr,
) -> CodexResult<RemoteCompactionV2Output> {
    let mut remaining = prompt.input.clone();
    client.sanitize_compaction_recovery_input(
        &mut remaining,
        step_context.turn.model_provider().info(),
    );
    // The old client deliberately removes unattributed foreign ciphertext. After sanitizing
    // the snapshot once, use a new client so checkpoints produced by this provider survive.
    let mut recovery_client = client.sanitized_compaction_recovery_session();
    if !matches!(remaining.pop(), Some(ResponseItem::CompactionTrigger {})) {
        return Err(original_error);
    }
    // Initial tool declarations and instructions must remain available to each
    // staged request. Keep their original order instead of reconstructing them.
    let preamble_end = remaining
        .iter()
        .position(|item| {
            !matches!(item, ResponseItem::AdditionalTools { .. } | ResponseItem::ConfigurationUpdate { .. })
                && !matches!(item, ResponseItem::Message { role, .. } if role == "developer" || role == "system")
        })
        .unwrap_or(remaining.len());
    let preamble = if remaining[..preamble_end]
        .iter()
        .any(|item| matches!(item, ResponseItem::AdditionalTools { .. }))
    {
        remaining.drain(..preamble_end).collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut checkpoint: Option<ResponseItem> = None;
    let mut checkpoint_configuration: Option<ResponseItem> = None;
    let mut replayed_updates = Vec::new();
    for _ in 0..32 {
        // Once the prefix preceding an incremental update has been summarized,
        // carry the update itself verbatim after the checkpoint on future requests.
        // Consecutive developer items include removal and namespace notices.
        if checkpoint.is_some() {
            let update_end = remaining.iter().take_while(|item| {
                matches!(item, ResponseItem::AdditionalTools { .. } | ResponseItem::ConfigurationUpdate { .. })
                    || matches!(item, ResponseItem::Message { role, .. } if role == "developer" || role == "system")
            }).count();
            for item in remaining.drain(..update_end) {
                if matches!(item, ResponseItem::ConfigurationUpdate { .. }) {
                    checkpoint_configuration = Some(item);
                } else {
                    replayed_updates.push(item);
                }
            }
        }
        // Keep tool declarations and retained outputs in the unmodified suffix:
        // their state cannot safely be reconstructed from an opaque checkpoint.
        // Search calls must remain with their outputs, including failed searches.
        let protected = remaining
            .iter()
            .position(|item| {
                matches!(item, ResponseItem::AdditionalTools { .. })
                    || matches!(item, ResponseItem::ToolSearchCall { .. })
                    || matches!(item, ResponseItem::ToolSearchOutput { .. })
                    || (sess.enabled(codex_features::Feature::RetainClientToolOutputs)
                        && matches!(
                            item,
                            ResponseItem::FunctionCallOutput {
                                call_id: None,
                                name: Some(_),
                                ..
                            }
                        ))
            })
            .unwrap_or(remaining.len());
        let boundaries = safe_compaction_boundaries(&remaining)
            .into_iter()
            .filter(|boundary| *boundary <= protected)
            .collect::<Vec<_>>();
        if boundaries.is_empty() {
            return Err(original_error);
        }
        let mut candidate = boundaries.len() / 2;
        loop {
            let split = boundaries[candidate];
            let mut partial = prompt.clone();
            partial.input = preamble
                .iter()
                .cloned()
                .chain(checkpoint.iter().cloned())
                .chain(checkpoint_configuration.iter().cloned())
                .chain(replayed_updates.iter().cloned())
                .chain(remaining[..split].iter().cloned())
                .collect();
            partial.input.push(ResponseItem::CompactionTrigger {});
            match run_remote_compaction_request_v2(
                sess,
                step_context,
                &partial,
                metadata,
                CompactionRequestTransport::Foreground(&mut recovery_client),
            )
            .await
            {
                Ok(output) => {
                    if let Some(usage) = output.token_usage.as_ref() {
                        sess.record_token_usage_info(
                            &step_context.turn,
                            &step_context.settings,
                            Some(usage),
                            crate::session::TokenUsageContext::SpendingOnly,
                        )
                        .await?;
                    }
                    checkpoint_configuration = remaining[..split]
                        .iter()
                        .rev()
                        .find(|item| matches!(item, ResponseItem::ConfigurationUpdate { .. }))
                        .cloned()
                        .or(checkpoint_configuration);
                    checkpoint = Some(output.compaction_output);
                    remaining.drain(..split);
                    break;
                }
                Err(error)
                    if matches!(error.details(), CodexErrorDetails::ContextWindowExceeded)
                        && candidate > 0 =>
                {
                    candidate /= 2
                }
                Err(error) => return Err(error),
            }
        }
        let mut combined = prompt.clone();
        combined.input = preamble
            .iter()
            .cloned()
            .chain(checkpoint.iter().cloned())
            .chain(checkpoint_configuration.iter().cloned())
            .chain(replayed_updates.iter().cloned())
            .chain(remaining.iter().cloned())
            .collect();
        combined.input.push(ResponseItem::CompactionTrigger {});
        match run_remote_compaction_request_v2(
            sess,
            step_context,
            &combined,
            metadata,
            CompactionRequestTransport::Foreground(&mut recovery_client),
        )
        .await
        {
            Err(error) if matches!(error.details(), CodexErrorDetails::ContextWindowExceeded) => {}
            result => return result,
        }
    }
    Err(original_error)
}

/// Split at a message boundary only after all known tool calls have completed.
fn safe_compaction_boundaries(input: &[ResponseItem]) -> Vec<usize> {
    let mut pending = HashSet::<String>::new();
    let mut boundaries = Vec::new();
    let mut unknown_call = false;
    for (index, item) in input.iter().enumerate() {
        if index > 0
            && pending.is_empty()
            && !unknown_call
            && (matches!(item, ResponseItem::Message { role, .. } if role == "user" || role == "assistant")
                || matches!(
                    item,
                    ResponseItem::FunctionCall { .. }
                        | ResponseItem::CustomToolCall { .. }
                        | ResponseItem::AdditionalTools { .. }
                        | ResponseItem::ToolSearchCall { .. }
                ))
        {
            boundaries.push(index);
        }
        match item {
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::CustomToolCall { call_id, .. } => {
                if call_id.is_empty() || !pending.insert(call_id.clone()) {
                    unknown_call = true;
                }
            }
            ResponseItem::LocalShellCall { call_id, .. }
            | ResponseItem::ToolSearchCall { call_id, .. } => {
                if let Some(id) = call_id {
                    if id.is_empty() || !pending.insert(id.clone()) {
                        unknown_call = true;
                    }
                } else {
                    unknown_call = true;
                }
            }
            ResponseItem::FunctionCallOutput { call_id, .. }
            | ResponseItem::ToolSearchOutput { call_id, .. } => {
                if let Some(id) = call_id {
                    if !pending.remove(id) {
                        unknown_call = true;
                    }
                } else {
                    unknown_call = true;
                }
            }
            ResponseItem::CustomToolCallOutput { call_id, .. } if !pending.remove(call_id) => {
                unknown_call = true;
            }
            _ => {}
        }
    }
    boundaries
}

#[cfg(test)]
mod staged_recovery_tests {
    use super::*;
    use serde_json::json;

    fn item(value: serde_json::Value) -> ResponseItem {
        serde_json::from_value(value).expect("response item")
    }

    #[test]
    fn safe_boundaries_never_split_parallel_tool_calls() {
        let history = vec![
            item(
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]}),
            ),
            item(json!({"type":"function_call","name":"a","arguments":"{}","call_id":"a"})),
            item(json!({"type":"function_call","name":"b","arguments":"{}","call_id":"b"})),
            item(json!({"type":"function_call_output","call_id":"a","output":"done"})),
            item(
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"interrupt"}]}),
            ),
            item(json!({"type":"function_call_output","call_id":"b","output":"done"})),
            item(
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"second"}]}),
            ),
            item(
                json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}),
            ),
        ];
        assert_eq!(safe_compaction_boundaries(&history), vec![1, 6, 7]);
    }

    #[test]
    fn missing_tool_id_stops_future_splits() {
        let history = vec![
            item(
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]}),
            ),
            item(json!({"type":"function_call_output","output":"unknown"})),
            item(
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"second"}]}),
            ),
        ];
        assert!(safe_compaction_boundaries(&history).is_empty());
    }

    #[test]
    fn repeated_pending_call_id_never_creates_a_false_boundary() {
        let history = vec![
            item(json!({"type":"function_call","name":"a","arguments":"{}","call_id":"same"})),
            item(json!({"type":"function_call","name":"b","arguments":"{}","call_id":"same"})),
            item(json!({"type":"function_call_output","call_id":"same","output":"done"})),
            item(
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"later"}]}),
            ),
        ];
        assert!(safe_compaction_boundaries(&history).is_empty());
    }
    #[test]
    fn safe_boundaries_keep_search_call_and_output_together() {
        let history = vec![
            item(
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]}),
            ),
            item(
                json!({"type":"tool_search_call","call_id":"search","execution":"server","arguments":{}}),
            ),
            item(
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"while searching"}]}),
            ),
            item(
                json!({"type":"tool_search_output","call_id":"search","status":"completed","execution":"server","tools":[]}),
            ),
            item(
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"after"}]}),
            ),
        ];
        assert_eq!(safe_compaction_boundaries(&history), vec![1, 4]);
    }
}
