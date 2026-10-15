//! Prompt admission notices exact resources already owned by another task.
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
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
    Ok(())
}
