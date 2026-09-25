use super::*;
use crate::client::normalize_response_items_for_provider;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnItemContributor;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::ToolCompatibility;
use codex_protocol::ResponseItemId;
use codex_protocol::items::AgentMessageContent;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tracing_subscriber::prelude::*;

struct RewriteAgentMessageContributor;

impl TurnItemContributor for RewriteAgentMessageContributor {
    fn contribute<'a>(
        &'a self,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
        item: &'a mut TurnItem,
    ) -> codex_extension_api::ExtensionFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if let TurnItem::AgentMessage(agent_message) = item {
                agent_message.content = vec![AgentMessageContent::Text {
                    text: "plan contributed assistant text".to_string(),
                }];
            }
            Ok(())
        })
    }
}

fn assistant_output_text(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(ResponseItemId::with_suffix("msg", "1")),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn post_sampling_token_estimate_is_disabled_by_always_on_sinks() {
    let feedback = codex_feedback::CodexFeedback::new();
    let subscriber = tracing_subscriber::registry()
        .with(feedback.logger_layer())
        .with(tracing_subscriber::fmt::layer().with_filter(codex_state::log_db::default_filter()));

    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        assert!(!tracing::event_enabled!(
            target: POST_SAMPLING_TOKEN_ESTIMATE_TARGET,
            tracing::Level::TRACE,
            turn_id,
            estimated_token_count,
            message
        ));
    });
}

#[tokio::test]
async fn plan_mode_uses_contributed_turn_item_for_last_agent_message() {
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_item_contributor(Arc::new(RewriteAgentMessageContributor));
    session.services.extensions = Arc::new(builder.build());
    let turn_store = ExtensionData::new(turn_context.sub_id.clone());
    let mut state = PlanModeStreamState::new(&turn_context.sub_id);
    let mut last_agent_message = None;
    let item = assistant_output_text("original assistant text");

    let step_context = StepContext::for_test(Arc::new(turn_context));
    let handled = handle_assistant_item_done_in_plan_mode(
        &session,
        &step_context,
        &turn_store,
        &item,
        &mut state,
        /*previously_active_item*/ None,
        &mut last_agent_message,
    )
    .await;

    assert!(handled);
    assert_eq!(
        last_agent_message.as_deref(),
        Some("plan contributed assistant text")
    );
}

#[test]
fn realtime_user_verification_notice_excludes_request_payload() {
    let event = EventMsg::ElicitationRequest(codex_protocol::approvals::ElicitationRequestEvent {
        turn_id: None,
        server_name: "private-server-name".to_string(),
        id: codex_protocol::mcp::RequestId::String("private-request-id".to_string()),
        request: codex_protocol::approvals::ElicitationRequest::UserVerification {
            meta: None,
            title: "private-title".to_string(),
            description: "private-description".to_string(),
            challenge: "private-challenge".to_string(),
        },
    });
    assert_eq!(
        realtime_text_for_event(&event),
        Some(RealtimeEventText::Handoff(
            "<user_verification_notice>User verification is required. Please respond in the app.</user_verification_notice>".to_string(),
            None,
        )),
    );
}

#[test]
fn openai_prompt_drops_third_party_plaintext_reasoning() {
    let provider = ModelProviderInfo::create_openai_provider(/*base_url*/ None);
    let native = ResponseItem::Reasoning {
        id: Some(ResponseItemId::with_suffix("rs", "native")),
        summary: Vec::new(),
        content: None,
        encrypted_content: Some("native encrypted reasoning".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    let foreign = ResponseItem::Reasoning {
        id: Some(ResponseItemId::from_server("foreign-id".to_string())),
        summary: Vec::new(),
        content: Some(vec![
            codex_protocol::models::ReasoningItemContent::ReasoningText {
                text: "third-party plaintext".to_string(),
            },
        ]),
        encrypted_content: Some("third-party marker".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    let mut input = vec![native.clone(), foreign, assistant_output_text("answer")];

    normalize_response_items_for_provider(&mut input, &provider, &HashSet::new(), false);

    assert_eq!(input, vec![native, assistant_output_text("answer")]);
}

#[test]
fn restricted_provider_prompt_keeps_only_portable_custom_tools() {
    let mut provider = ModelProviderInfo::default();
    provider.tool_compatibility = Some(ToolCompatibility::FunctionsAndApplyPatch);
    let custom_call = |name: &str, call_id: &str| ResponseItem::CustomToolCall {
        id: Some(ResponseItemId::with_suffix("ctc", call_id)),
        status: Some("completed".to_string()),
        call_id: call_id.to_string(),
        name: name.to_string(),
        namespace: None,
        input: "input".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let apply_patch = custom_call("apply_patch", "patch-call");
    let mut input = vec![
        custom_call("exec", "exec-call"),
        apply_patch.clone(),
        ResponseItem::Reasoning {
            id: Some(ResponseItemId::from_server("foreign-id".to_string())),
            summary: Vec::new(),
            content: None,
            encrypted_content: None,
            internal_chat_message_metadata_passthrough: None,
        },
        assistant_output_text("answer"),
    ];

    normalize_response_items_for_provider(&mut input, &provider, &HashSet::new(), false);

    let mut expected_apply_patch = apply_patch;
    expected_apply_patch.set_id(None);
    let mut expected_message = assistant_output_text("answer");
    expected_message.set_id(None);
    assert_eq!(input, vec![expected_apply_patch, expected_message]);
}

#[test]
fn compatible_third_party_provider_drops_encrypted_provider_state() {
    let provider = ModelProviderInfo {
        name: "compatible-cloud".to_string(),
        base_url: Some("https://example.invalid/v1".to_string()),
        ..Default::default()
    };
    let function_call = ResponseItem::FunctionCall {
        id: Some(ResponseItemId::with_suffix("fc", "portable")),
        name: "lookup".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        encrypted_function_args: Some(vec!["provider-bound".to_string()]),
        call_id: "call-portable".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let mut input = vec![
        ResponseItem::Reasoning {
            id: Some(ResponseItemId::with_suffix("rs", "foreign")),
            summary: Vec::new(),
            content: None,
            encrypted_content: Some("provider-bound".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Compaction {
            id: Some(ResponseItemId::with_suffix("cmp", "foreign")),
            encrypted_content: "provider-bound".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::ContextCompaction {
            id: Some(ResponseItemId::with_suffix("cmp", "foreign-context")),
            encrypted_content: Some("provider-bound".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
        function_call.clone(),
        assistant_output_text("answer"),
    ];

    let foreign_ids = input.iter().filter_map(|item| item.id().cloned()).collect();
    normalize_response_items_for_provider(&mut input, &provider, &foreign_ids, true);

    let mut expected_function_call = function_call;
    expected_function_call.set_id(None);
    if let ResponseItem::FunctionCall {
        encrypted_function_args,
        ..
    } = &mut expected_function_call
    {
        *encrypted_function_args = None;
    }
    let mut expected_message = assistant_output_text("answer");
    expected_message.set_id(None);
    assert_eq!(input, vec![expected_function_call, expected_message]);
}
