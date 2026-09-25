use std::path::PathBuf;

use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookOutputEntry;
use codex_protocol::protocol::HookOutputEntryKind;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::HookRunSummary;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_utils_absolute_path::AbsolutePathBuf;

use super::common;
use crate::engine::ClaudeHooksEngine;
use crate::engine::ConfiguredHandler;
use crate::engine::HandlerRunResult;
use crate::engine::dispatcher;
use crate::engine::output_parser;
use crate::output_spill::AdditionalContext;
use crate::schema::NullableString;
use crate::schema::SubagentCommandInputFields;
use crate::schema::UserPromptSubmitCommandInput;

#[derive(Debug, Clone)]
pub struct UserPromptSubmitRequest {
    pub session_id: ThreadId,
    pub turn_id: String,
    pub subagent: Option<common::SubagentHookContext>,
    pub cwd: AbsolutePathBuf,
    pub transcript_path: Option<PathBuf>,
    pub model: String,
    pub model_provider: String,
    pub inherited_model_provider: Option<String>,
    pub requested_backend: Option<String>,
    pub spawn_model_explicit: bool,
    pub account_id: Option<String>,
    pub rate_limits: Option<RateLimitSnapshot>,
    pub ordinary_usage_allowed: Option<bool>,
    pub permission_mode: String,
    pub prompt: String,
}

#[derive(Debug)]
pub struct UserPromptSubmitOutcome {
    pub hook_events: Vec<HookCompletedEvent>,
    pub should_stop: bool,
    pub stop_reason: Option<String>,
    pub additional_contexts: Vec<String>,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub route_message: Option<String>,
    pub strip_prompt_prefix_bytes: Option<usize>,
    pub strip_provider_state: bool,
    pub chatgpt_profile_home: Option<String>,
    pub reviewer_profile_name: Option<String>,
    pub reviewer_fallback_profiles: Vec<ReviewerFallbackProfile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewerFallbackProfile {
    pub name: String,
    pub codex_home: String,
}

#[derive(Debug, PartialEq, Eq)]
struct UserPromptSubmitHandlerData {
    should_stop: bool,
    stop_reason: Option<String>,
    additional_contexts_for_model: Vec<AdditionalContext>,
    model: Option<String>,
    model_provider: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    route_message: Option<String>,
    strip_prompt_prefix_bytes: Option<usize>,
    strip_provider_state: bool,
    chatgpt_profile_home: Option<String>,
    reviewer_profile_name: Option<String>,
    reviewer_fallback_profiles: Vec<ReviewerFallbackProfile>,
}

pub(crate) fn preview(
    handlers: &[ConfiguredHandler],
    _request: &UserPromptSubmitRequest,
) -> Vec<HookRunSummary> {
    dispatcher::select_handlers(
        handlers,
        HookEventName::UserPromptSubmit,
        /*matcher_input*/ None,
    )
    .into_iter()
    .map(|handler| dispatcher::running_summary(&handler))
    .collect()
}

pub(crate) async fn run(
    engine: &ClaudeHooksEngine,
    request: UserPromptSubmitRequest,
) -> UserPromptSubmitOutcome {
    let matched = dispatcher::select_handlers(
        &engine.handlers,
        HookEventName::UserPromptSubmit,
        /*matcher_input*/ None,
    );
    if matched.is_empty() {
        return UserPromptSubmitOutcome {
            hook_events: Vec::new(),
            should_stop: false,
            stop_reason: None,
            additional_contexts: Vec::new(),
            model: None,
            model_provider: None,
            reasoning_effort: None,
            route_message: None,
            strip_prompt_prefix_bytes: None,
            strip_provider_state: false,
            chatgpt_profile_home: None,
            reviewer_profile_name: None,
            reviewer_fallback_profiles: Vec::new(),
        };
    }

    let subagent = SubagentCommandInputFields::from(request.subagent.as_ref());
    let input_json = match serde_json::to_string(&UserPromptSubmitCommandInput {
        session_id: request.session_id.to_string(),
        turn_id: request.turn_id.clone(),
        agent_id: subagent.agent_id,
        agent_type: subagent.agent_type,
        transcript_path: NullableString::from_path(request.transcript_path.clone()),
        cwd: request.cwd.display().to_string(),
        hook_event_name: "UserPromptSubmit".to_string(),
        model: request.model.clone(),
        model_provider: request.model_provider.clone(),
        inherited_model_provider: request.inherited_model_provider.clone(),
        requested_backend: request.requested_backend.clone(),
        spawn_model_explicit: request.spawn_model_explicit,
        account_id: request.account_id.clone(),
        rate_limits: request.rate_limits.clone(),
        ordinary_usage_allowed: request.ordinary_usage_allowed,
        permission_mode: request.permission_mode.clone(),
        prompt: request.prompt.clone(),
    }) {
        Ok(input_json) => input_json,
        Err(error) => {
            return serialization_failure_outcome(common::serialization_failure_hook_events(
                matched,
                Some(request.turn_id),
                format!("failed to serialize user prompt submit hook input: {error}"),
            ));
        }
    };

    let results = dispatcher::execute_handlers(
        engine,
        matched,
        input_json,
        request.cwd.as_path(),
        Some(request.turn_id),
        parse_completed,
    )
    .await;

    let should_stop = results.iter().any(|result| result.data.should_stop);
    let stop_reason = results
        .iter()
        .find_map(|result| result.data.stop_reason.clone());
    let additional_contexts = common::flatten_additional_contexts(
        results
            .iter()
            .map(|result| result.data.additional_contexts_for_model.as_slice()),
    );
    let additional_contexts = engine
        .command_runtime
        .output_spiller()
        .maybe_spill_additional_contexts(additional_contexts)
        .await;
    let model = results.iter().find_map(|result| result.data.model.clone());
    let model_provider = results
        .iter()
        .find_map(|result| result.data.model_provider.clone());
    let reasoning_effort = results
        .iter()
        .find_map(|result| result.data.reasoning_effort.clone());
    let route_message = results
        .iter()
        .find_map(|result| result.data.route_message.clone());
    let strip_prompt_prefix_bytes = results
        .iter()
        .find_map(|result| result.data.strip_prompt_prefix_bytes);
    let strip_provider_state = results
        .iter()
        .any(|result| result.data.strip_provider_state);
    let chatgpt_profile_home = results
        .iter()
        .find_map(|result| result.data.chatgpt_profile_home.clone());
    let reviewer_profile_name = results
        .iter()
        .find_map(|result| result.data.reviewer_profile_name.clone());
    let reviewer_fallback_profiles = results
        .iter()
        .find(|result| !result.data.reviewer_fallback_profiles.is_empty())
        .map(|result| result.data.reviewer_fallback_profiles.clone())
        .unwrap_or_default();

    UserPromptSubmitOutcome {
        hook_events: results.into_iter().map(|result| result.completed).collect(),
        should_stop,
        stop_reason,
        additional_contexts,
        model,
        model_provider,
        reasoning_effort,
        route_message,
        strip_prompt_prefix_bytes,
        strip_provider_state,
        chatgpt_profile_home,
        reviewer_profile_name,
        reviewer_fallback_profiles,
    }
}

fn parse_completed(
    handler: &ConfiguredHandler,
    run_result: HandlerRunResult,
    turn_id: Option<String>,
) -> dispatcher::ParsedHandler<UserPromptSubmitHandlerData> {
    let mut entries = Vec::new();
    let mut status = HookRunStatus::Completed;
    let mut should_stop = false;
    let mut stop_reason = None;
    let mut additional_contexts_for_model = Vec::new();
    let mut model = None;
    let mut model_provider = None;
    let mut reasoning_effort = None;
    let mut route_message = None;
    let mut strip_prompt_prefix_bytes = None;
    let mut strip_provider_state = false;
    let mut chatgpt_profile_home = None;
    let mut reviewer_profile_name = None;
    let mut reviewer_fallback_profiles = Vec::new();

    match run_result.error.as_deref() {
        Some(error) => {
            status = HookRunStatus::Failed;
            entries.push(HookOutputEntry {
                kind: HookOutputEntryKind::Error,
                text: error.to_string(),
            });
        }
        None => match run_result.exit_code {
            Some(0) => {
                let trimmed_stdout = run_result.stdout.trim();
                if trimmed_stdout.is_empty() {
                } else if let Some(parsed) =
                    output_parser::parse_user_prompt_submit(&run_result.stdout)
                {
                    if let Some(system_message) = parsed.universal.system_message {
                        entries.push(HookOutputEntry {
                            kind: HookOutputEntryKind::Warning,
                            text: system_message,
                        });
                    }
                    if (!handler.can_apply_control_effects()
                        || parsed.invalid_block_reason.is_none())
                        && let Some(additional_context) = parsed.additional_context
                    {
                        common::append_additional_context(
                            &mut entries,
                            &mut additional_contexts_for_model,
                            handler,
                            additional_context,
                        );
                    }
                    let _ = parsed.universal.suppress_output;
                    if handler.can_apply_control_effects() {
                        model = parsed.model;
                        model_provider = parsed.model_provider;
                        reasoning_effort = parsed.reasoning_effort;
                        route_message = parsed.route_message;
                        strip_prompt_prefix_bytes = parsed.strip_prompt_prefix_bytes;
                        strip_provider_state = parsed.strip_provider_state;
                        chatgpt_profile_home = parsed.chatgpt_profile_home;
                        reviewer_profile_name = parsed.reviewer_profile_name;
                        reviewer_fallback_profiles = parsed.reviewer_fallback_profiles;
                        if !parsed.universal.continue_processing {
                            status = HookRunStatus::Stopped;
                            should_stop = true;
                            stop_reason = parsed.universal.stop_reason.clone();
                            if let Some(stop_reason_text) = parsed.universal.stop_reason {
                                entries.push(HookOutputEntry {
                                    kind: HookOutputEntryKind::Stop,
                                    text: stop_reason_text,
                                });
                            }
                        } else if let Some(invalid_block_reason) = parsed.invalid_block_reason {
                            status = HookRunStatus::Failed;
                            entries.push(HookOutputEntry {
                                kind: HookOutputEntryKind::Error,
                                text: invalid_block_reason,
                            });
                        } else if parsed.should_block {
                            status = HookRunStatus::Blocked;
                            should_stop = true;
                            stop_reason = parsed.reason.clone();
                            if let Some(reason) = parsed.reason {
                                entries.push(HookOutputEntry {
                                    kind: HookOutputEntryKind::Feedback,
                                    text: reason,
                                });
                            }
                        }
                    }
                } else if output_parser::looks_like_json(&run_result.stdout) {
                    status = HookRunStatus::Failed;
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Error,
                        text: "hook returned invalid user prompt submit JSON output".to_string(),
                    });
                } else {
                    let additional_context = trimmed_stdout.to_string();
                    common::append_additional_context(
                        &mut entries,
                        &mut additional_contexts_for_model,
                        handler,
                        additional_context,
                    );
                }
            }
            Some(2) if handler.can_apply_control_effects() => {
                if let Some(reason) = common::trimmed_non_empty(&run_result.stderr) {
                    status = HookRunStatus::Blocked;
                    should_stop = true;
                    stop_reason = Some(reason.clone());
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Feedback,
                        text: reason,
                    });
                } else {
                    status = HookRunStatus::Failed;
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Error,
                        text: "UserPromptSubmit hook exited with code 2 but did not write a blocking reason to stderr".to_string(),
                    });
                }
            }
            Some(exit_code) => {
                status = HookRunStatus::Failed;
                entries.push(HookOutputEntry {
                    kind: HookOutputEntryKind::Error,
                    text: format!("hook exited with code {exit_code}"),
                });
            }
            None => {
                status = HookRunStatus::Failed;
                entries.push(HookOutputEntry {
                    kind: HookOutputEntryKind::Error,
                    text: "hook exited without a status code".to_string(),
                });
            }
        },
    }

    let completed = HookCompletedEvent {
        turn_id,
        run: dispatcher::completed_summary(handler, &run_result, status, entries),
    };

    dispatcher::ParsedHandler {
        completed,
        data: UserPromptSubmitHandlerData {
            should_stop,
            stop_reason,
            additional_contexts_for_model,
            model,
            model_provider,
            reasoning_effort,
            route_message,
            strip_prompt_prefix_bytes,
            strip_provider_state,
            chatgpt_profile_home,
            reviewer_profile_name,
            reviewer_fallback_profiles,
        },
        completion_order: 0,
    }
}

fn serialization_failure_outcome(hook_events: Vec<HookCompletedEvent>) -> UserPromptSubmitOutcome {
    UserPromptSubmitOutcome {
        hook_events,
        should_stop: false,
        stop_reason: None,
        additional_contexts: Vec::new(),
        model: None,
        model_provider: None,
        reasoning_effort: None,
        route_message: None,
        strip_prompt_prefix_bytes: None,
        strip_provider_state: false,
        chatgpt_profile_home: None,
        reviewer_profile_name: None,
        reviewer_fallback_profiles: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use codex_protocol::openai_models::ReasoningEffort;
    use codex_protocol::protocol::HookEventName;
    use codex_protocol::protocol::HookOutputEntry;
    use codex_protocol::protocol::HookOutputEntryKind;
    use codex_protocol::protocol::HookRunStatus;
    use codex_utils_absolute_path::test_support::PathBufExt;
    use codex_utils_absolute_path::test_support::test_path_buf;
    use pretty_assertions::assert_eq;

    use super::ReviewerFallbackProfile;
    use super::UserPromptSubmitHandlerData;
    use super::parse_completed;
    use crate::engine::ConfiguredHandler;
    use crate::engine::HandlerRunResult;
    use crate::output_spill::AdditionalContext;

    #[test]
    fn continue_false_preserves_context_for_later_turns() {
        let parsed = parse_completed(
            &handler(),
            run_result(
                Some(0),
                r#"{"continue":false,"stopReason":"pause","hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"do not inject"}}"#,
                "",
            ),
            Some("turn-1".to_string()),
        );

        assert_eq!(
            parsed.data,
            UserPromptSubmitHandlerData {
                should_stop: true,
                stop_reason: Some("pause".to_string()),
                additional_contexts_for_model: vec![AdditionalContext {
                    text: "do not inject".to_string(),
                    limit: Default::default(),
                }],
                model: None,
                model_provider: None,
                reasoning_effort: None,
                route_message: None,
                strip_prompt_prefix_bytes: None,
                strip_provider_state: false,
                chatgpt_profile_home: None,
                reviewer_profile_name: None,
                reviewer_fallback_profiles: Vec::new(),
            }
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Stopped);
        assert_eq!(
            parsed.completed.run.entries,
            vec![
                HookOutputEntry {
                    kind: HookOutputEntryKind::Context,
                    text: "do not inject".to_string(),
                },
                HookOutputEntry {
                    kind: HookOutputEntryKind::Stop,
                    text: "pause".to_string(),
                },
            ]
        );
    }

    #[test]
    fn claude_block_decision_blocks_processing() {
        let parsed = parse_completed(
            &handler(),
            run_result(
                Some(0),
                r#"{"decision":"block","reason":"slow down","hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"do not inject"}}"#,
                "",
            ),
            Some("turn-1".to_string()),
        );

        assert_eq!(
            parsed.data,
            UserPromptSubmitHandlerData {
                should_stop: true,
                stop_reason: Some("slow down".to_string()),
                additional_contexts_for_model: vec![AdditionalContext {
                    text: "do not inject".to_string(),
                    limit: Default::default(),
                }],
                model: None,
                model_provider: None,
                reasoning_effort: None,
                route_message: None,
                strip_prompt_prefix_bytes: None,
                strip_provider_state: false,
                chatgpt_profile_home: None,
                reviewer_profile_name: None,
                reviewer_fallback_profiles: Vec::new(),
            }
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Blocked);
        assert_eq!(
            parsed.completed.run.entries,
            vec![
                HookOutputEntry {
                    kind: HookOutputEntryKind::Context,
                    text: "do not inject".to_string(),
                },
                HookOutputEntry {
                    kind: HookOutputEntryKind::Feedback,
                    text: "slow down".to_string(),
                },
            ]
        );
    }

    #[test]
    fn claude_block_decision_requires_reason() {
        let stdout = r#"{"decision":"block","hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"do not inject"}}"#;
        let parsed = parse_completed(
            &handler(),
            run_result(Some(0), stdout, ""),
            Some("turn-1".to_string()),
        );

        assert_eq!(
            parsed.data,
            UserPromptSubmitHandlerData {
                should_stop: false,
                stop_reason: None,
                additional_contexts_for_model: Vec::new(),
                model: None,
                model_provider: None,
                reasoning_effort: None,
                route_message: None,
                strip_prompt_prefix_bytes: None,
                strip_provider_state: false,
                chatgpt_profile_home: None,
                reviewer_profile_name: None,
                reviewer_fallback_profiles: Vec::new(),
            }
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Failed);
        assert_eq!(
            parsed.completed.run.entries,
            vec![HookOutputEntry {
                kind: HookOutputEntryKind::Error,
                text: "UserPromptSubmit hook returned decision:block without a non-empty reason"
                    .to_string(),
            }]
        );

        let async_handler = handler_with_async(/*async*/ true);
        let parsed = parse_completed(
            &async_handler,
            run_result(Some(0), stdout, ""),
            Some("turn-1".to_string()),
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Completed);
        assert_eq!(
            parsed.completed.run.entries,
            vec![HookOutputEntry {
                kind: HookOutputEntryKind::Context,
                text: "do not inject".to_string(),
            }],
        );
    }

    #[test]
    fn exit_code_two_blocks_processing() {
        let parsed = parse_completed(
            &handler(),
            run_result(Some(2), "", "blocked by policy\n"),
            Some("turn-1".to_string()),
        );

        assert_eq!(
            parsed.data,
            UserPromptSubmitHandlerData {
                should_stop: true,
                stop_reason: Some("blocked by policy".to_string()),
                additional_contexts_for_model: Vec::new(),
                model: None,
                model_provider: None,
                reasoning_effort: None,
                route_message: None,
                strip_prompt_prefix_bytes: None,
                strip_provider_state: false,
                chatgpt_profile_home: None,
                reviewer_profile_name: None,
                reviewer_fallback_profiles: Vec::new(),
            }
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Blocked);
        assert_eq!(
            parsed.completed.run.entries,
            vec![HookOutputEntry {
                kind: HookOutputEntryKind::Feedback,
                text: "blocked by policy".to_string(),
            }]
        );

        let async_handler = handler_with_async(/*async*/ true);
        let parsed = parse_completed(
            &async_handler,
            run_result(Some(2), "", "blocked by policy\n"),
            Some("turn-1".to_string()),
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Failed);
        assert!(!parsed.data.should_stop);
    }

    #[test]
    fn parses_model_and_reasoning_effort_override() {
        let parsed = parse_completed(
            &handler(),
            run_result(
                Some(0),
                r#"{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","model":"gpt-5.6-sol","reasoningEffort":"high","routeMessage":"route details","stripPromptPrefixBytes":7,"stripProviderState":true,"chatgptProfileHome":"/tmp/codex-work","reviewerProfileName":"markster","reviewerFallbackProfiles":[{"name":"personal","codexHome":"/tmp/codex-personal"}]}}"#,
                "",
            ),
            Some("turn-1".to_string()),
        );

        assert_eq!(parsed.data.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(parsed.data.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(parsed.data.route_message.as_deref(), Some("route details"));
        assert_eq!(parsed.data.strip_prompt_prefix_bytes, Some(7));
        assert!(parsed.data.strip_provider_state);
        assert_eq!(
            parsed.data.chatgpt_profile_home.as_deref(),
            Some("/tmp/codex-work")
        );
        assert_eq!(
            parsed.data.reviewer_profile_name.as_deref(),
            Some("markster")
        );
        assert_eq!(
            parsed.data.reviewer_fallback_profiles,
            vec![ReviewerFallbackProfile {
                name: "personal".to_string(),
                codex_home: "/tmp/codex-personal".to_string(),
            }]
        );
    }

    fn handler() -> ConfiguredHandler {
        handler_with_async(/*async*/ false)
    }

    fn handler_with_async(r#async: bool) -> ConfiguredHandler {
        ConfiguredHandler {
            builtin: false,
            event_name: HookEventName::UserPromptSubmit,
            matcher: None,
            timeout_sec: 5,
            status_message: None,
            additional_context_limit: Default::default(),
            source_path: test_path_buf("/tmp/hooks.json").abs().into(),
            source: codex_protocol::protocol::HookSource::User,
            display_order: 0,
            kind: crate::engine::ConfiguredHandlerKind::Command {
                command: "echo hook".to_string(),
                r#async,
                env: std::collections::HashMap::new(),
            },
        }
    }

    fn run_result(exit_code: Option<i32>, stdout: &str, stderr: &str) -> HandlerRunResult {
        HandlerRunResult {
            started_at: 1,
            completed_at: 2,
            duration_ms: 1,
            exit_code,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            error: None,
        }
    }
}
