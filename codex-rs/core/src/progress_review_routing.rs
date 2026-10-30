//! AgentRoute-specific allowlist bridge for reviewer-driven active-turn escalation.
use crate::codex_thread::CodexThread;

pub(crate) struct ManualReviewRouteLock;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProgressRoute {
    model: String,
    provider: String,
    effort: Option<codex_protocol::openai_models::ReasoningEffort>,
}

impl CodexThread {
    /// AgentRoute supplies an allowlist; the reviewer can select a tier, never an arbitrary model.
    /// Publication uses the existing routed settings path, preserving captured tool/model steps.
    pub async fn apply_progress_review_route(
        &self,
        turn_id: &str,
        revision: u64,
        tier: &str,
        reviewed_model: Option<&str>,
    ) -> String {
        let config = {
            let active = self.session.active_turn.lock().await;
            let Some(task) = active.as_ref().and_then(|turn| turn.task.as_ref()) else {
                return "Live review route skipped: turn ended.".into();
            };
            if task.turn_context.sub_id != turn_id || task.cancellation_token.is_cancelled() {
                return "Live review route skipped: turn changed.".into();
            }
            if task
                .turn_context
                .extension_data
                .get::<ManualReviewRouteLock>()
                .is_some()
            {
                return "Live review route skipped: explicit user route.".into();
            }
            task.turn_context.config.clone()
        };
        if self
            .conversation_history_snapshot()
            .await
            .user_message_revision()
            != revision
        {
            return "Live review route skipped: new user input.".into();
        }
        let Some(raw) = config
            .progress_review_routes
            .as_deref()
            .filter(|raw| raw.len() <= 16384)
        else {
            return "Live review route: advice only (automatic switching disabled).".into();
        };
        let Ok(routes) =
            serde_json::from_str::<std::collections::HashMap<String, ProgressRoute>>(raw)
        else {
            return "Live review route skipped: invalid route configuration.".into();
        };
        let Some(route) = routes.get(tier) else {
            return "Live review route skipped: tier is not authorized.".into();
        };
        // Initial rollout permits escalation only. Downgrades remain advice so a
        // reviewer cannot undercut the current task's risk/capability floor.
        let order = ["fast", "normal", "smart", "max"];
        let Some(current_model) = self.current_turn_model(turn_id).await else {
            return "Live review route skipped: turn ended.".into();
        };
        if reviewed_model != Some(current_model.as_str()) {
            return "Live review route skipped: model changed during review.".into();
        }
        let current_rank = order.iter().rposition(|key| {
            routes
                .get(*key)
                .is_some_and(|route| route.model == current_model)
        });
        let requested_rank = order.iter().position(|key| *key == tier);
        if !matches!((current_rank, requested_rank), (Some(current), Some(requested)) if requested > current)
        {
            return "Live review route: kept current model (no authorized escalation).".into();
        }
        let outcome = self
            .session
            .apply_review_turn_settings(
                turn_id,
                revision,
                codex_protocol::protocol::TurnSettingsUpdate {
                    model: Some(route.model.clone()),
                    effort: route.effort.clone().map(Some),
                    ..Default::default()
                },
                route.provider.clone(),
            )
            .await;
        format!(
            "Live review route: {outcome:?}; tier={tier}, provider={}, model={}. Applies to subsequent steps of this turn.",
            route.provider, route.model
        )
    }
}
