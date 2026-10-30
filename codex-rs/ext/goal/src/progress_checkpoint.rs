//! Explicit, bounded checkpoints requested by an agent or an invoked skill.
use std::collections::BTreeMap;
use std::sync::Weak;

use codex_core::ThreadManager;
use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_protocol::ThreadId;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use serde::Deserialize;
use serde_json::json;

pub(crate) struct AcknowledgementTool {
    pub manager: Weak<ThreadManager>,
    pub thread_id: ThreadId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Acknowledgement {
    review_id: String,
    decision: String,
    reason: String,
    next_action: String,
}

fn parse_acknowledgement(raw: &str) -> Result<Acknowledgement, FunctionCallError> {
    let value: Acknowledgement = serde_json::from_str(raw)
        .map_err(|_| FunctionCallError::RespondToModel("invalid acknowledgement".into()))?;
    if value.review_id.trim().is_empty()
        || value.review_id.len() > 128
        || !matches!(
            value.decision.as_str(),
            "accepted" | "partly_accepted" | "declined"
        )
        || value.reason.trim().is_empty()
        || value.reason.len() > 500
        || value.next_action.trim().is_empty()
        || value.next_action.len() > 500
    {
        return Err(FunctionCallError::RespondToModel(
            "invalid acknowledgement fields".into(),
        ));
    }
    Ok(value)
}

fn record_acknowledgement(
    previous: &crate::progress_review::PreviousVerdict,
    value: &Acknowledgement,
) -> Result<(), FunctionCallError> {
    let mut state = previous
        .0
        .lock()
        .map_err(|_| FunctionCallError::RespondToModel("review state unavailable".into()))?;
    let (_, message) = state
        .as_mut()
        .ok_or_else(|| FunctionCallError::RespondToModel("no review available".into()))?;
    if !message.starts_with(&format!(
        "Independent progress review [{}]:",
        value.review_id
    )) || message.contains("\nAcknowledgement:")
        || message.contains("]: continue.")
    {
        return Err(FunctionCallError::RespondToModel(
            "review ID is stale, already acknowledged, or does not require acknowledgement".into(),
        ));
    }
    message.push_str(&format!(
        "\nAcknowledgement: {}. {} Next action: {}",
        value.decision, value.reason, value.next_action
    ));
    Ok(())
}

impl<'call> ToolExecutor<ToolCall<'call>> for AcknowledgementTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("acknowledge_progress_review")
    }
    fn spec(&self) -> ToolSpec {
        let properties = BTreeMap::from([
            (
                "review_id".into(),
                JsonSchema::string(Some("Exact review ID".into())),
            ),
            (
                "decision".into(),
                JsonSchema::string_enum(
                    vec![
                        json!("accepted"),
                        json!("partly_accepted"),
                        json!("declined"),
                    ],
                    None,
                ),
            ),
            (
                "reason".into(),
                JsonSchema::string(Some("Reason, at most 500 bytes".into())),
            ),
            (
                "next_action".into(),
                JsonSchema::string(Some("Concrete next action, at most 500 bytes".into())),
            ),
        ]);
        ToolSpec::Function(ResponsesApiTool { name: "acknowledge_progress_review".into(), description: "Record your decision and next action for a progress review. This is a statement of intent, not evidence of completion.".into(), strict: false, defer_loading: None, parameters: JsonSchema::object(properties, Some(vec!["review_id".into(), "decision".into(), "reason".into(), "next_action".into()]), Some(false.into())), output_schema: None })
    }
    fn handle<'a>(
        &'a self,
        invocation: ToolCall<'call>,
    ) -> codex_extension_api::ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            let value = parse_acknowledgement(invocation.function_arguments()?)?;
            let manager = self.manager.upgrade().ok_or_else(|| {
                FunctionCallError::RespondToModel("thread manager unavailable".into())
            })?;
            let thread = manager
                .get_thread(self.thread_id)
                .await
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            let previous = thread
                .thread_extension_data()
                .get::<crate::progress_review::PreviousVerdict>()
                .ok_or_else(|| FunctionCallError::RespondToModel("no review available".into()))?;
            let revision = thread
                .conversation_history_snapshot()
                .await
                .user_message_revision();
            {
                let state = previous.0.lock().map_err(|_| {
                    FunctionCallError::RespondToModel("review state unavailable".into())
                })?;
                if state
                    .as_ref()
                    .is_none_or(|(review_revision, _)| *review_revision != revision)
                {
                    return Err(FunctionCallError::RespondToModel(
                        "review was superseded by new user input".into(),
                    ));
                }
            }
            record_acknowledgement(&previous, &value)?;
            crate::progress_review::persist_from_store(thread.thread_extension_data());
            Ok(Box::new(JsonToolOutput::new(
                json!({"recorded": true, "review_id": value.review_id, "note": "Intent recorded; follow-through requires separate evidence."}),
            )) as Box<dyn ToolOutput>)
        })
    }
}

pub(crate) struct CheckpointTool {
    manager: Weak<ThreadManager>,
    thread_id: ThreadId,
}

impl CheckpointTool {
    pub(crate) fn new(manager: Weak<ThreadManager>, thread_id: ThreadId) -> Self {
        Self { manager, thread_id }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    checkpoint: String,
}

fn parse_checkpoint(raw: &str) -> Result<String, FunctionCallError> {
    let args: Args = serde_json::from_str(raw).map_err(|error| {
        FunctionCallError::RespondToModel(format!("invalid checkpoint: {error}"))
    })?;
    if args.checkpoint.trim().is_empty() || args.checkpoint.len() > 1000 {
        return Err(FunctionCallError::RespondToModel(
            "checkpoint must contain 1 to 1000 bytes".to_string(),
        ));
    }
    Ok(args.checkpoint)
}

impl<'call> ToolExecutor<ToolCall<'call>> for CheckpointTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("request_progress_review")
    }

    fn spec(&self) -> ToolSpec {
        let properties = BTreeMap::from([("checkpoint".to_string(), JsonSchema::string(Some("The milestone, acceptance criteria, or specific review question. At most 1000 bytes.".to_string())))]);
        ToolSpec::Function(ResponsesApiTool {
            name: "request_progress_review".to_string(),
            description: "Request an independent, read-only progress review at a meaningful milestone. It runs asynchronously subject to a cooldown; continue useful work while waiting.".to_string(),
            strict: false, defer_loading: None,
            parameters: JsonSchema::object(properties, Some(vec!["checkpoint".to_string()]), Some(false.into())),
            output_schema: None,
        })
    }

    fn handle<'a>(
        &'a self,
        invocation: ToolCall<'call>,
    ) -> codex_extension_api::ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            let checkpoint = parse_checkpoint(invocation.function_arguments()?)?;
            let manager = self.manager.upgrade().ok_or_else(|| {
                FunctionCallError::RespondToModel("thread manager unavailable".to_string())
            })?;
            let thread = manager
                .get_thread(self.thread_id)
                .await
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            let signals = thread
                .current_turn_extension_data::<crate::progress_review::Signals>(&invocation.turn_id)
                .await
                .ok_or_else(|| {
                    FunctionCallError::RespondToModel(
                        "progress review is not active for this turn".to_string(),
                    )
                })?;
            signals.request(checkpoint);
            Ok(Box::new(JsonToolOutput::new(
                json!({"scheduled": true, "note": "Review is asynchronous and subject to a cooldown."}),
            )) as Box<dyn ToolOutput>)
        })
    }
}

#[cfg(test)]
#[path = "progress_checkpoint_tests.rs"]
mod tests;
