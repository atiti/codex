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
