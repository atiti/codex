//! Provider image admission and lossless attachment custody during summarizing compaction.
//!
//! Count materialized occurrences, including repeated payloads and tool images.
//! Only an already successful checkpoint may replace historical image parts in
//! active context; their original response items remain in the saved rollout.

use codex_history::ResponseItemEnvelope;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;

/// Resolve after routing. Unknown providers keep an explicit unknown limit.
pub(crate) fn input_image_limit(provider: &ModelProviderInfo) -> Option<usize> {
    provider
        .capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.max_input_images)
        .map(|limit| limit as usize)
        .or_else(|| {
            codex_api::is_azure_responses_provider(&provider.name, provider.base_url.as_deref())
                .then_some(50)
        })
}

pub(crate) fn image_count(item: &ResponseItem) -> usize {
    match item {
        ResponseItem::Message { content, .. } => content
            .iter()
            .filter(|part| matches!(part, ContentItem::InputImage { .. }))
            .count(),
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output
            .content_items()
            .map(|parts| {
                parts
                    .iter()
                    .filter(|part| matches!(part, FunctionCallOutputContentItem::InputImage { .. }))
                    .count()
            })
            .unwrap_or_default(),
        _ => 0,
    }
}

/// Called on the final formatted input before every HTTP/WebSocket submission.
pub(crate) fn validate_input(input: &[ResponseItem], limit: Option<usize>) -> CodexResult<()> {
    if let Some(limit) = limit {
        let count = input.iter().map(image_count).sum::<usize>();
        if count > limit {
            return Err(CodexErrorDetails::ImageLimitExceeded { count, limit }.into());
        }
    }
    Ok(())
}

/// Keep current user input and the latest complete tool exchange verbatim. Replace older
/// images only after their content was included in a successful checkpoint.
/// Instructions and explicitly retained owner exchanges are never shortened.
pub(crate) fn bound_checkpoint_images(
    history: &mut [ResponseItemEnvelope],
    limit: Option<usize>,
) -> CodexResult<()> {
    let Some(limit) = limit else {
        return Ok(());
    };
    let latest_user = history.iter().rposition(
        |envelope| matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "user"),
    );
    let items = history
        .iter()
        .map(|envelope| envelope.item.clone())
        .collect::<Vec<_>>();
    let live_boundary = crate::compact_remote_v2::safe_compaction_boundaries(&items)
        .last()
        .copied()
        .unwrap_or_default();
    let protected = |index: usize, envelope: &ResponseItemEnvelope| {
        latest_user == Some(index)
            || index >= live_boundary
            || latest_user.is_none()
            || envelope.is_always_retained()
            || matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "developer" || role == "system")
    };
    let protected_count = history
        .iter()
        .enumerate()
        .filter(|(index, envelope)| protected(*index, envelope))
        .map(|(_, envelope)| image_count(&envelope.item))
        .sum::<usize>();
    if protected_count > limit {
        return Err(CodexErrorDetails::ImageLimitExceeded {
            count: protected_count,
            limit,
        }
        .into());
    }
    let mut remaining = limit - protected_count;
    for (index, envelope) in history.iter_mut().enumerate().rev() {
        if protected(index, envelope) {
            continue;
        }
        let count = image_count(&envelope.item);
        if count <= remaining {
            remaining -= count;
            continue;
        }
        const NOTICE: &str = "[Earlier image summarized in the checkpoint; original attachment retained in saved history.]";
        match &mut envelope.item {
            ResponseItem::Message { content, .. } => {
                for part in content {
                    if matches!(part, ContentItem::InputImage { .. }) {
                        *part = ContentItem::InputText {
                            text: NOTICE.to_owned(),
                        };
                    }
                }
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                if let Some(parts) = output.content_items_mut() {
                    for part in parts {
                        if matches!(part, FunctionCallOutputContentItem::InputImage { .. }) {
                            *part = FunctionCallOutputContentItem::InputText {
                                text: NOTICE.to_owned(),
                            };
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Lossless current-input check before any staged request can summarize it away.
pub(crate) fn validate_current_input(
    input: &[ResponseItem],
    limit: Option<usize>,
) -> CodexResult<()> {
    let start = input
        .iter()
        .rposition(|item| matches!(item, ResponseItem::Message { role, .. } if role == "user"));
    if let Some(start) = start {
        validate_input(&input[start..=start], limit)?;
    }
    Ok(())
}

/// Restore the latest accepted user item from the successful compactor input.
/// Token-based retained selection must not silently remove its image parts.
pub(crate) fn preserve_current_user_images(
    original: &[ResponseItem],
    original_metadata: &[Option<codex_history::CodexHarnessMetadata>],
    checkpoint: &mut Vec<ResponseItemEnvelope>,
) -> CodexResult<()> {
    let Some(index) = original
        .iter()
        .rposition(|item| matches!(item, ResponseItem::Message { role, .. } if role == "user"))
    else {
        return Ok(());
    };
    let user = &original[index];
    if image_count(user) == 0 {
        return Ok(());
    }
    let id = user.id().ok_or_else(|| {
        codex_protocol::error::CodexErr::InvalidRequest(
            "Current image input has no stable identity; saved history is preserved.".to_owned(),
        )
    })?;
    let source = ResponseItemEnvelope {
        item: user.clone(),
        metadata: original_metadata.get(index).cloned().flatten(),
    };
    if let Some(retained) = checkpoint
        .iter_mut()
        .find(|envelope| envelope.item.id() == Some(id))
    {
        *retained = source;
    } else {
        let position = checkpoint
            .iter()
            .position(|envelope| {
                matches!(
                    envelope.item,
                    ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
                )
            })
            .unwrap_or(checkpoint.len());
        checkpoint.insert(position, source);
    }
    Ok(())
}
