use super::*;
use codex_history::CodexHarnessMetadata;

#[test]
fn rewritten_output_preserves_harness_metadata() {
    let envelope = ResponseItemEnvelope {
        item: ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("call-1".to_string()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("large output".repeat(100)),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: Some(CodexHarnessMetadata::default()),
    };

    let rewritten = rewritten_output_for_context_window(&envelope)
        .expect("function output should be rewritten");

    assert_eq!(rewritten.metadata, envelope.metadata);
    assert_ne!(rewritten.item, envelope.item);
}

#[test]
fn discovered_tool_definitions_are_not_trimmed() {
    let item: ResponseItem = serde_json::from_value(serde_json::json!({
        "type": "tool_search_output",
        "call_id": "search-1",
        "status": "completed",
        "execution": "client",
        "tools": [{"type": "function", "name": "discovered", "parameters": {"type": "object"}}]
    }))
    .expect("search output");
    assert!(rewritten_output_for_context_window(&ResponseItemEnvelope::new(item)).is_none());
}
