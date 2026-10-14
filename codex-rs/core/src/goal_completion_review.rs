//! Narrow host bridge for the Goal extension's independent completion reviewer.

use std::sync::Arc;
use std::time::Duration;

use codex_features::Feature;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::models::BaseInstructionsProvenance;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use tokio_util::sync::CancellationToken;

use crate::codex_delegate::run_codex_thread_one_shot;
use crate::config::Constrained;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

const REVIEW_INSTRUCTIONS: &str = "Independently audit whether the user's goal is complete. Use available read-only evidence. Never assume a claim or prior message proves an outcome. Return only a JSON object with decision (complete, continue, or blocked) and reason (a concrete, short explanation). A missing check, inaccessible evidence, or uncertainty means continue. Blocked requires an exact external dependency.";

pub(crate) async fn run(
    session: Arc<Session>,
    ctx: Arc<TurnContext>,
    cancellation_token: CancellationToken,
    prompt: String,
) -> Result<String, String> {
    let mut config = ctx.config.as_ref().clone();
    let model = ctx.capture_current_model_info().slug.clone();
    let provider_id = ctx.model_provider_id();
    config.model = Some(model);
    config.model_provider_id = provider_id;
    config.model_provider = ctx.model_provider().info().clone();
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
    config
        .mcp_servers
        .set(std::collections::HashMap::new())
        .map_err(|err| format!("MCP isolation unavailable: {err}"))?;
    config.base_instructions = Some(REVIEW_INSTRUCTIONS.to_string());
    config.base_instructions_provenance = Some(BaseInstructionsProvenance::Custom);
    let review_token = cancellation_token.child_token();
    let (_, io) = run_codex_thread_one_shot(
        config,
        Arc::clone(&session.services.auth_manager),
        Arc::clone(&session.services.models_manager),
        vec![UserInput::Text {
            text: prompt,
            text_elements: Vec::new(),
        }],
        session,
        ctx,
        review_token.clone(),
        SubAgentSource::Review,
        /*final_output_json_schema*/ None,
        /*initial_history*/ None,
        codex_extension_api::SessionIsolation::Isolated,
    )
    .await
    .map_err(|err| format!("goal reviewer could not start: {err}"))?;
    let result = tokio::time::timeout(Duration::from_secs(/*secs*/ 120), async {
        while let Ok(event) = io.rx_event.recv().await {
            match event.msg {
                EventMsg::TurnComplete(done) if done.error.is_none() => {
                    return done
                        .last_agent_message
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
