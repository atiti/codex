//! Prompt admission notices exact resources already owned by another task.
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::compact::SUMMARIZATION_PROMPT;
use codex_login::CodexAuth;
use codex_model_provider_info::built_in_model_providers;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_active_pr_prompt_shows_overlap_without_claiming_it() -> Result<()> {
    let server = start_mock_server().await;
    let args = json!({
        "title": "Fix SendJump proxy",
        "summary": "Reviewing the existing fix",
        "status": "working",
        "resources": ["github:markster-io/sendjump#27"]
    });
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("owner-1"),
                ev_function_call("work", "update_task", &args.to_string()),
                ev_completed("owner-1"),
            ]),
            sse(vec![
                ev_response_created("owner-2"),
                ev_assistant_message("owner-done", "I am reviewing the existing fix"),
                ev_completed("owner-2"),
            ]),
            sse(vec![
                ev_response_created("prompt"),
                ev_assistant_message("prompt-done", "I found another task on this PR"),
                ev_completed("prompt"),
            ]),
            sse(vec![
                ev_response_created("feature-disabled"),
                ev_assistant_message("feature-disabled-done", "Task awareness is disabled"),
                ev_completed("feature-disabled"),
            ]),
        ],
    )
    .await;
    let home = std::sync::Arc::new(tempfile::TempDir::new()?);
    let owner = test_codex()
        .with_home(home.clone())
        .with_config(|config| {
            config
                .features
                .enable(codex_features::Feature::TaskAwareness)
                .expect("enable task awareness");
        })
        .build_with_auto_env(&server)
        .await?;
    owner
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Review the proxy fix".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&owner.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let db = owner.codex.state_db().expect("persistent state");
    let owners = db
        .task_work_owners(
            codex_protocol::ThreadId::new(),
            &["github:markster-io/sendjump#27".into()],
        )
        .await?;
    assert_eq!(
        owners,
        vec![codex_state::TaskWorkOwner {
            thread_id: owner.session_configured.session_id.to_string(),
            title: "Fix SendJump proxy".into(),
            summary: "Reviewing the existing fix".into(),
            resource: "github:markster-io/sendjump#27".into()
        }]
    );

    let prompt_task = test_codex()
        .with_home(home.clone())
        .with_model("gpt-5.5")
        .with_pre_build_hook(|home| {
            let script = home.join("task-route.py");
            std::fs::write(
                &script,
                r#"import json
print(json.dumps({"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","model":"gpt-5.5","routeMessage":"Task coordination test route"}}))
"#,
            )
            .expect("write route hook");
            std::fs::write(
                home.join("hooks.json"),
                json!({"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":format!("python3 {}", script.display())}]}]}}).to_string(),
            )
            .expect("write hook configuration");
        })
        .with_config(|config| {
            core_test_support::hooks::trust_discovered_hooks(config);
            config
                .features
                .enable(codex_features::Feature::StepModelSwitching)
                .expect("enable routing");
            config
                .features
                .enable(codex_features::Feature::TaskAwareness)
                .expect("enable task awareness");
        })
        .build_with_auto_env(&server)
        .await?;
    prompt_task
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Fix https://github.com/markster-io/sendjump/pull/27.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let mut notice_ids = std::collections::BTreeSet::new();
    wait_for_event(&prompt_task.codex, |event| {
        if let EventMsg::ItemCompleted(item) = event
            && let codex_protocol::items::TurnItem::AgentMessage(message) = &item.item
            && message.id.starts_with("agentroute-")
        {
            assert!(
                notice_ids.insert(message.id.clone()),
                "notice identities must be distinct"
            );
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(notice_ids.len(), 2);
    assert!(
        notice_ids
            .iter()
            .any(|id| id.starts_with("agentroute-route-"))
    );
    assert!(
        notice_ids
            .iter()
            .any(|id| id.starts_with("agentroute-task-overlap-"))
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].input().iter().any(|item| {
        item["role"] == "assistant" && item.to_string().contains("task_work_overlap")
    }));
    assert_eq!(
        db.task_work_owners(prompt_task.session_configured.session_id.into(), &[])
            .await?,
        owners
    );

    let task_awareness_disabled = test_codex()
        .with_home(home)
        .with_model("gpt-5.5")
        .with_pre_build_hook(|home| {
            let script = home.join("task-route.py");
            std::fs::write(
                &script,
                r#"import json
print(json.dumps({"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","model":"gpt-5.5","routeMessage":"Task coordination test route"}}))
"#,
            )
            .expect("write route hook");
            std::fs::write(
                home.join("hooks.json"),
                json!({"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":format!("python3 {}", script.display())}]}]}}).to_string(),
            )
            .expect("write hook configuration");
        })
        .with_config(|config| {
            core_test_support::hooks::trust_discovered_hooks(config);
            config
                .features
                .enable(codex_features::Feature::StepModelSwitching)
                .expect("enable routing");
            config
                .features
                .disable(codex_features::Feature::TaskAwareness)
                .expect("disable task awareness");
        })
        .build_with_auto_env(&server)
        .await?;
    task_awareness_disabled
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Fix https://github.com/markster-io/sendjump/pull/27.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let mut disabled_notice_ids = std::collections::BTreeSet::new();
    wait_for_event(&task_awareness_disabled.codex, |event| {
        if let EventMsg::ItemCompleted(item) = event
            && let codex_protocol::items::TurnItem::AgentMessage(message) = &item.item
            && message.id.starts_with("agentroute-task-overlap-")
        {
            disabled_notice_ids.insert(message.id.clone());
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert!(disabled_notice_ids.is_empty());
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    assert!(!requests[3].input().iter().any(|item| {
        item["role"] == "assistant" && item.to_string().contains("task_work_overlap")
    }));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlap_context_is_recorded_after_pre_sampling_compaction() -> Result<()> {
    let server = start_mock_server().await;
    let home = std::sync::Arc::new(tempfile::TempDir::new()?);
    let previous_model = "gpt-5.4";
    let next_model = "gpt-5.2";
    let mut model_provider = built_in_model_providers(/*openai_base_url*/ None)["openai"].clone();
    model_provider.name = "OpenAI-compatible overlap test provider".into();
    model_provider.base_url = Some(format!("{}/v1", server.uri()));
    model_provider.supports_websockets = false;
    let owner_args = json!({
        "title": "Fix SendJump proxy",
        "summary": "Reviewing the existing fix",
        "status": "working",
        "resources": ["github:markster-io/sendjump#27"]
    });
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("owner-claim"),
                ev_function_call("owner-work", "update_task", &owner_args.to_string()),
                ev_completed("owner-claim"),
            ]),
            sse(vec![
                ev_response_created("owner-finish"),
                ev_assistant_message("owner-finish-message", "Claim recorded"),
                ev_completed("owner-finish"),
            ]),
            sse(vec![
                ev_response_created("seed"),
                ev_assistant_message("seed-message", "Seed history"),
                ev_completed_with_tokens("seed", /*total_tokens*/ 120_000),
            ]),
            sse(vec![
                ev_response_created("compact"),
                ev_assistant_message("compact-summary", "PRETURN_COMPACT_SUMMARY"),
                ev_completed_with_tokens("compact", /*total_tokens*/ 20),
            ]),
            sse(vec![
                ev_response_created("overlap"),
                ev_assistant_message("overlap-message", "I saw the existing task claim"),
                ev_completed_with_tokens("overlap", /*total_tokens*/ 40),
            ]),
        ],
    )
    .await;

    let owner = test_codex()
        .with_home(home.clone())
        .with_config(|config| {
            config
                .features
                .enable(codex_features::Feature::TaskAwareness)
                .expect("enable task awareness");
        })
        .build_with_auto_env(&server)
        .await?;
    owner
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Review the SendJump proxy fix".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&owner.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let task = test_codex()
        .with_home(home)
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_model(previous_model)
        .with_model_info_override(previous_model, |model_info| {
            model_info.context_window = Some(273_000);
            model_info.max_context_window = Some(273_000);
        })
        .with_model_info_override(next_model, |model_info| {
            model_info.context_window = Some(125_000);
            model_info.max_context_window = Some(125_000);
        })
        .with_config(move |config| {
            config.model_provider = model_provider;
            config.compact_prompt = Some(SUMMARIZATION_PROMPT.to_string());
            config
                .features
                .enable(codex_features::Feature::TaskAwareness)
                .expect("enable task awareness");
        })
        .build(&server)
        .await
        .expect("build prompt task");
    task.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "seed context phrase ".repeat(100),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: previous_model.into(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            }),
        )
        .await?;
    wait_for_event(&task.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    task.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Fix https://github.com/markster-io/sendjump/pull/27.".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: next_model.into(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            }),
        )
        .await?;
    wait_for_event(&task.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = mock.requests();
    let request_models = requests
        .iter()
        .map(|request| {
            request.body_json()["model"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 5, "model sequence: {request_models:?}");
    let compact_request = requests[3].body_json().to_string();
    assert!(requests[3].body_contains_text(SUMMARIZATION_PROMPT));
    assert!(!compact_request.contains("task_work_overlap"));
    let follow_up_request = requests[4].body_json().to_string();
    assert!(follow_up_request.contains("PRETURN_COMPACT_SUMMARY"));
    assert!(requests[4].input().iter().any(|item| {
        item["role"] == "assistant" && item.to_string().contains("task_work_overlap")
    }));
    Ok(())
}
