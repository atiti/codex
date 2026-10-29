//! Narrow host bridge for the Goal extension's independent completion reviewer.

use std::sync::Arc;
use std::time::Duration;

use codex_features::Feature;
use codex_history::InitialHistory;
use codex_history::RolloutItem;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::BaseInstructionsProvenance;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HasLegacyEvent;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use tokio_util::sync::CancellationToken;

use crate::codex_delegate::run_codex_thread_one_shot;
use crate::codex_thread::CodexThread;
use crate::config::Constrained;
use crate::context::ContextualUserFragment;
use crate::context::GoalCompletionReviewEvidence;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::turn_timing::now_unix_timestamp_ms;

impl CodexThread {
    /// Deliver fallible progress advice only to the reviewed active turn and revision.
    pub async fn deliver_progress_review(
        &self,
        turn_id: &str,
        user_revision: u64,
        item: codex_protocol::models::ResponseItem,
    ) -> bool {
        self.session
            .inject_progress_review(turn_id, user_revision, item)
            .await
    }

    /// Publish bounded reviewer activity through the normal client and rollout lifecycle.
    /// These display items do not add messages to the model's conversation history.
    pub async fn report_goal_completion_review(&self, turn_id: &str, mut message: String) {
        let ctx = {
            let active = self.session.active_turn.lock().await;
            let Some(task) = active.as_ref().and_then(|active| active.task.as_ref()) else {
                return;
            };
            if task.turn_context.sub_id != turn_id || task.cancellation_token.is_cancelled() {
                return;
            }
            Arc::clone(&task.turn_context)
        };
        let mut end = message.len().min(1_600);
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        let item = TurnItem::AgentMessage(AgentMessageItem {
            id: format!("goal-review-{}", uuid::Uuid::new_v4()),
            content: vec![AgentMessageContent::Text { text: message }],
            phase: Some(MessagePhase::Commentary),
            memory_citation: None,
            delivery: None,
            questions: None,
        });
        let started_at_ms = now_unix_timestamp_ms();
        let started = EventMsg::ItemStarted(ItemStartedEvent {
            thread_id: self.session.thread_id,
            turn_id: ctx.sub_id.clone(),
            item: item.clone(),
            started_at_ms,
        });
        let completed = EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id: self.session.thread_id,
            turn_id: ctx.sub_id.clone(),
            item,
            started_at_ms: Some(started_at_ms),
            completed_at_ms: now_unix_timestamp_ms(),
        });
        // Display-only events must bypass progress accounting and realtime handoffs.
        // Preserve the legacy fanout so older rollouts retain the same visible history.
        for message in [started, completed] {
            let legacy = message.as_legacy_events(/*show_raw_agent_reasoning*/ false);
            self.session
                .send_event_raw(Event {
                    id: ctx.sub_id.clone(),
                    msg: message,
                })
                .await;
            for message in legacy {
                self.session
                    .send_event_raw(Event {
                        id: ctx.sub_id.clone(),
                        msg: message,
                    })
                    .await;
            }
        }
    }
}

const REVIEW_INSTRUCTIONS: &str = "Independently audit whether the user's goal is complete. Use available read-only evidence. Never assume a claim or prior message proves an outcome. Return only a JSON object with decision (complete, continue, or blocked) and reason (a concrete, short explanation). A missing check, inaccessible evidence, or uncertainty means continue. Blocked requires an exact external dependency.";

pub(crate) async fn run(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    cancellation_token: CancellationToken,
    goal_id: &str,
    objective: &str,
    history: &[ResponseItem],
) -> Result<String, String> {
    let evidence =
        GoalCompletionReviewEvidence::new(goal_id, objective, history).ok_or_else(|| {
            "goal objective exceeds the safe completion review context limit".to_string()
        })?;
    let initial_history = Some(InitialHistory::Forked(vec![RolloutItem::ResponseItem(
        ContextualUserFragment::into(evidence).into(),
    )]));
    run_with_instructions(
        session,
        ctx,
        cancellation_token,
        "Independently verify whether the supplied goal is complete.".to_string(),
        REVIEW_INSTRUCTIONS,
        /*progress_route*/ false,
        initial_history,
    )
    .await
    .map(|(text, _, _, _)| text)
}

pub(crate) async fn run_with_instructions(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    cancellation_token: CancellationToken,
    prompt: String,
    instructions: &'static str,
    progress_route: bool,
    initial_history: Option<InitialHistory>,
) -> Result<
    (
        String,
        Option<codex_protocol::protocol::TokenUsage>,
        String,
        String,
    ),
    String,
> {

    let mut config = ctx.config.as_ref().clone();
    let model = ctx.capture_current_model_info().slug.clone();
    let provider_id = ctx.model_provider_id();
    config.model = Some(model);
    config.model_provider_id = provider_id;
    config.model_provider = ctx.model_provider().info().clone();
    if progress_route {
        if let Some(provider_id) = config.progress_review_provider.clone() {
            if config
                .config_layer_stack
                .required_model_provider()
                .is_some_and(|required| required != provider_id)
            {
                return Err(
                    "progress review provider conflicts with managed requirements".to_string(),
                );
            }
            config.model_provider = config
                .model_providers
                .get(&provider_id)
                .cloned()
                .ok_or_else(|| format!("progress review provider `{provider_id}` not found"))?;
            config.model_provider_id = provider_id;
        }
        if let Some(model) = config.progress_review_model.clone() {
            config.model = Some(model);
        }
    }
    config.permissions.approval_policy = Constrained::allow_only(AskForApproval::Never);
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .map_err(|err| format!("read-only review permissions unavailable: {err}"))?;
    config
        .web_search_mode
        .set(WebSearchMode::Disabled)
        .map_err(|err| format!("web-disabled review unavailable: {err}"))?;
    let _ = config.features.disable(Feature::Collab);
    let _ = config.features.disable(Feature::MultiAgentV2);
    let _ = config.features.disable(Feature::Goals);
    // Hooks execute outside the reviewer's model sandbox. Do not run inherited
    // commands or notify handlers as part of a read-only isolated review.
    config
        .features
        .disable(Feature::CodexHooks)
        .map_err(|err| format!("hook isolation unavailable: {err}"))?;
    config.notify = None;
    config
        .mcp_servers
        .set(std::collections::HashMap::new())
        .map_err(|err| format!("MCP isolation unavailable: {err}"))?;
    config.base_instructions = Some(instructions.to_string());
    config.base_instructions_provenance = Some(BaseInstructionsProvenance::Custom);
    let review_auth_manager = if config.model_provider_id == ctx.model_provider_id() {
        ctx.model_provider().auth_manager().unwrap_or_else(|| Arc::clone(&session.services.auth_manager))
    } else {
        Arc::clone(&session.services.auth_manager)
    };
    let review_token = cancellation_token.child_token();
    let (review_session, io) = run_codex_thread_one_shot(
        config,
        review_auth_manager,
        Arc::clone(&session.services.models_manager),
        vec![UserInput::Text {
            text: "Independently verify whether the supplied goal is complete.".to_string(),
            text_elements: Vec::new(),
        }],
        session,
        ctx,
        review_token.clone(),
        SubAgentSource::Review,
        /*final_output_json_schema*/ None,
        initial_history,
        codex_extension_api::SessionIsolation::Isolated,
    )
    .await
    .map_err(|err| format!("goal reviewer could not start: {err}"))?;
    let result = tokio::time::timeout(Duration::from_secs(/*secs*/ 120), async {
        while let Ok(event) = io.rx_event.recv().await {
            match event.msg {
                EventMsg::TurnComplete(done) if done.error.is_none() => {
                    // Delegate event forwarding intentionally filters TokenCount;
                    // read the isolated session's recorded usage directly.
                    let usage = review_session.total_token_usage().await;
                    let route = review_session.thread_config_snapshot().await;
                    return done
                        .last_agent_message
                        .map(|text| (text, usage, route.model, route.model_provider_id))
                        .ok_or_else(|| "goal reviewer returned no decision".to_string());
                }
                EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) => {
                    return Err("goal reviewer did not complete successfully".to_string());
                }
                _ => {}
            }
        }
        Err("goal reviewer ended without a decision".to_string())
    })
    .await;
    // Timeout or consumer failure must stop this one-shot reviewer as well.
    review_token.cancel();
    result.map_err(|_| "goal reviewer timed out".to_string())?
}
