//! Captures a review on the host's original action and authorization state.
//! The extension chooses effects; this adapter supplies evidence, validation and publication.

use super::super::GuardianReviewerFallbackState;
use super::super::GuardianReviewerIdentity;
use super::super::GuardianReviewerProfiles;
use super::*;
use crate::codex_thread::GuardianAuthorizationVersion;
use codex_guardian_reviewer::ReviewHost;
use codex_protocol::approvals::GuardianReviewReason;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::WarningEvent;

pub(in crate::guardian) struct PreparedApproval {
    request: GuardianApprovalRequest,
    turn: Arc<crate::session::turn_context::TurnContext>,
    root_authorization_version: Option<GuardianAuthorizationVersion>,
    user_message_revision: u64,
    review_evidence: Option<(
        Arc<GuardianReviewEvidence>,
        String,
        GuardianAuthorizationVersion,
        Option<GuardianAuthorizationVersion>,
    )>,
    reviewer_profiles: tokio::sync::Mutex<GuardianReviewerFallbackState>,
}

impl ReviewHost for super::super::runtime::ReviewRuntime {
    type Prepared = PreparedApproval;

    async fn servicing_turn(
        &self,
    ) -> Option<(String, Arc<codex_protocol::openai_models::ModelInfo>)> {
        let active = self.session.active_turn.lock().await;
        let turn = &active.as_ref()?.task.as_ref()?.turn_context;
        Some((turn.sub_id.clone(), Arc::clone(turn.model_info())))
    }

    async fn prepare(
        &self,
        review_id: &str,
        review_reason: GuardianReviewReason,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<(PreparedApproval, codex_guardian_reviewer::ReviewReport), ReviewDecision> {
        let super::super::runtime::ReviewRuntime {
            session,
            history_reset: _,
            context,
            request,
            reasons: _,
            options,
        } = self.clone();
        let request = match request.validate(&context) {
            Ok(request) => request.clone(),
            Err(decision) => return Err(decision),
        };
        let model_context = context.model_context();
        let turn = Arc::clone(context.turn());
        let GuardianReviewOptions {
            plugin_attribution_override,
            approval_request_source,
            external_cancel: _,
            require_synchronous_review: _,
            require_guardian: _,
        } = options;
        let target_item_id = guardian_request_target_item_id(&request).map(str::to_string);
        let assessment_turn_id = guardian_request_turn_id(&request, &turn.sub_id).to_string();
        let plugin_attribution = match plugin_attribution_override {
            Some(attribution) => Some(attribution),
            None if matches!(&request, GuardianApprovalRequest::ExecCommand { .. }) => {
                let attribution_deadline = std::cmp::min(
                    deadline,
                    Instant::now() + GUARDIAN_PLUGIN_ATTRIBUTION_TIMEOUT,
                );
                let attribution = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(ReviewDecision::Abort),
                    attribution = tokio::time::timeout_at(
                        attribution_deadline,
                        plugin_attribution_for_guardian_request(&context, &request),
                    ) => attribution,
                };
                match attribution {
                    Ok(attribution) => attribution,
                    Err(_) => {
                        tracing::warn!(
                            timeout_ms = GUARDIAN_PLUGIN_ATTRIBUTION_TIMEOUT.as_millis(),
                            "Guardian plugin attribution timed out"
                        );
                        None
                    }
                }
            }
            None => plugin_attribution_for_guardian_request(&context, &request).await,
        };
        let (plugin_id, script_path) = plugin_attribution
            .as_ref()
            .map(PluginCommandAttribution::serialized_fields)
            .unzip();
        let report =
            codex_guardian_reviewer::ReviewReport::new(codex_guardian_reviewer::ReviewMetadata {
                thread_id: session.thread_id.to_string(),
                turn_id: assessment_turn_id,
                review_id: review_id.to_owned(),
                target_item_id,
                plugin_id,
                script_path,
                approval_request_source,
                reviewed_action: guardian_reviewed_action(&request),
                action: guardian_assessment_action(&request),
                review_reason,
                model_context,
            });
        let root_authorization_version = session
            .services
            .agent_control
            .root_user_authorization(session.thread_id)
            .await
            .map(|snapshot| snapshot.authorization_version);
        // Keep the authorization revision even when no cacheable review evidence exists.
        let history = session.conversation_history_snapshot().await;
        let user_message_revision = history.user_message_revision();
        let review_evidence = if let Some(evidence) = session
            .services
            .thread_extension_data
            .get::<GuardianReviewEvidence>()
        {
            let authorization_version = evidence.authorization_version(history.as_ref());
            format_guardian_action_pretty(&request).ok().map(|action| {
                (
                    evidence,
                    action,
                    authorization_version,
                    root_authorization_version,
                )
            })
        } else {
            None
        };
        drop(history);
        Ok((
            PreparedApproval {
                request,
                turn: Arc::clone(&turn),
                root_authorization_version,
                user_message_revision,
                review_evidence,
                reviewer_profiles: tokio::sync::Mutex::new(GuardianReviewerFallbackState {
                    active: GuardianReviewerIdentity {
                        name: turn
                            .extension_data
                            .get::<GuardianReviewerProfiles>()
                            .and_then(|profiles| profiles.current_name.clone()),
                        auth_manager: turn
                            .model_provider()
                            .auth_manager()
                            .or_else(|| Some(Arc::clone(&session.services.auth_manager))),
                    },
                    fallbacks: turn
                        .extension_data
                        .get::<GuardianReviewerProfiles>()
                        .map(|profiles| profiles.fallbacks.iter().cloned().collect())
                        .unwrap_or_default(),
                }),
            },
            report,
        ))
    }

    async fn attempt(
        &self,
        prepared: &PreparedApproval,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> (GuardianReviewOutcome, GuardianReviewAnalyticsResult) {
        let reviewer = prepared.reviewer_profiles.lock().await.active.clone();
        let (mut outcome, analytics) = run_guardian_review_session_before_deadline(
            Arc::clone(&self.session),
            self.context.clone(),
            reviewer,
            prepared.request.clone(),
            self.reasons.clone(),
            guardian_output_schema(),
            Some(cancellation.clone()),
            deadline,
        )
        .await;
        let session = &self.session;
        let root_authorization_version = prepared.root_authorization_version;
        let user_message_revision = prepared.user_message_revision;
        if matches!(&outcome, GuardianReviewOutcome::Completed(assessment) if assessment.outcome == GuardianAssessmentOutcome::Allow)
            && ((session.guardian_context_mode == GuardianContextMode::ThreadOwned
                && (root_authorization_version
                    != session
                        .services
                        .agent_control
                        .root_user_authorization(session.thread_id)
                        .await
                        .map(|snapshot| snapshot.authorization_version)
                    || user_message_revision
                        != session
                            .conversation_history_snapshot()
                            .await
                            .user_message_revision()))
                || self.history_reset.is_cancelled()
                || cancellation.is_cancelled())
        {
            outcome = GuardianReviewOutcome::Error(GuardianReviewError::Cancelled);
        }
        if !matches!(
            &outcome,
            GuardianReviewOutcome::Error(GuardianReviewError::Session {
                error_info: Some(CodexErrorInfo::UsageLimitExceeded),
                ..
            })
        ) {
            return (outcome, analytics);
        }
        let mut reviewer_profiles = prepared.reviewer_profiles.lock().await;
        while let Some(fallback) = reviewer_profiles.fallbacks.pop_front() {
            let auth_manager = match codex_login::AuthManager::shared_from_config_for_codex_home(
                prepared.turn.config.as_ref(),
                fallback.codex_home,
                /*enable_codex_api_key_env*/ false,
            )
            .await
            {
                Ok(auth_manager) => auth_manager,
                Err(error) => {
                    tracing::warn!(profile = %fallback.name, %error, "could not load reviewer fallback profile");
                    continue;
                }
            };
            if auth_manager
                .auth()
                .await
                .is_none_or(|auth| !auth.is_chatgpt_auth())
            {
                tracing::warn!(profile = %fallback.name, "reviewer fallback profile is not signed in");
                continue;
            }
            let from_profile = reviewer_profiles
                .active
                .name
                .clone()
                .unwrap_or_else(|| "current".to_string());
            reviewer_profiles.active = GuardianReviewerIdentity {
                name: Some(fallback.name.clone()),
                auth_manager: Some(auth_manager),
            };
            self.session
                .send_event(
                    prepared.turn.as_ref(),
                    EventMsg::GuardianWarning(WarningEvent {
                        message: format!(
                            "◆ APPROVAL REVIEWER FALLBACK · {from_profile} → {} · current subscription usage limit reached",
                            fallback.name
                        ),
                    }),
                )
                .await;
            return (
                GuardianReviewOutcome::Error(GuardianReviewError::ReviewerFallbackReady {
                    from_profile,
                    to_profile: fallback.name,
                }),
                analytics,
            );
        }
        (outcome, analytics)
    }

    fn validate_action(&self) -> Result<(&str, Option<&str>), ReviewDecision> {
        let request = self.request.validate(&self.context)?;
        Ok((
            guardian_request_turn_id(request, &self.context.turn().sub_id),
            guardian_request_target_item_id(request),
        ))
    }

    async fn emit(&self, event: EventMsg) {
        self.session.send_event(self.context.turn(), event).await;
    }

    async fn record_evidence(
        &self,
        prepared: &PreparedApproval,
        event: &codex_protocol::protocol::GuardianAssessmentEvent,
    ) {
        if let Some((evidence, action, authorization_version, root_authorization_version)) =
            &prepared.review_evidence
        {
            evidence.record(
                event,
                action,
                *authorization_version,
                *root_authorization_version,
            );
        }
    }

    async fn interrupt(&self, turn_id: &str, warning: EventMsg) {
        self.session
            .interrupt_turn_with_warning(turn_id, warning)
            .await;
    }
}
