//! Real agent tool dispatch persists work and produces a visible status item.
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
async fn task_update_persists_claim_and_surfaces_status_without_extra_inference() -> Result<()> {
    let server = start_mock_server().await;
    let args = json!({"title": "Fix SendJump proxy", "summary": "Checking the existing fix", "status": "working", "resources": ["github:markster-io/sendjump#27"]});
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("one"),
                ev_function_call("work", "update_task", &args.to_string()),
                ev_completed("one"),
            ]),
            sse(vec![
                ev_response_created("two"),
                ev_assistant_message("done", "Continuing the review"),
                ev_completed("two"),
            ]),
            sse(vec![
                ev_response_created("three"),
                ev_function_call("overlap", "update_task", &args.to_string()),
                ev_completed("three"),
            ]),
            sse(vec![
                ev_response_created("four"),
                ev_assistant_message("stop", "The other task owns this work"),
                ev_completed("four"),
            ]),
        ],
    )
    .await;
    let home = std::sync::Arc::new(tempfile::TempDir::new()?);
    let test = test_codex()
        .with_home(home.clone())
        .with_config(|config| {
            config
                .features
                .enable(codex_features::Feature::TaskAwareness)
                .expect("enable task awareness");
        })
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Review the proxy fix".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let notice = wait_for_event(&test.codex, |event| matches!(event, EventMsg::ItemCompleted(item) if format!("{:?}", item.item).contains("◆ TASK"))).await;
    let EventMsg::ItemCompleted(event) = notice else {
        panic!("expected status item");
    };
    let codex_protocol::items::TurnItem::AgentMessage(message) = event.item else {
        panic!("expected task message");
    };
    let codex_protocol::items::AgentMessageContent::Text { text } = &message.content[0];
    insta::assert_snapshot!("task_work_status", text, @r#"
    ◆ TASK · Fix SendJump proxy · Working
    Checking the existing fix
    "#);
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let db = test.codex.state_db().expect("persistent state");
    let owners = db
        .task_work_owners(
            codex_protocol::ThreadId::new(),
            &["github:markster-io/sendjump#27".into()],
        )
        .await?;
    assert_eq!(
        owners,
        vec![codex_state::TaskWorkOwner {
            thread_id: test.session_configured.session_id.to_string(),
            title: "Fix SendJump proxy".into(),
            summary: "Checking the existing fix".into(),
            resource: "github:markster-io/sendjump#27".into()
        }]
    );
    let second = test_codex()
        .with_home(home)
        .with_config(|config| {
            config
                .features
                .enable(codex_features::Feature::TaskAwareness)
                .expect("enable task awareness");
        })
        .build_with_auto_env(&server)
        .await?;
    second
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Fix https://github.com/markster-io/sendjump/pull/27.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&second.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[2]
            .input()
            .iter()
            .any(|item| item["role"] == "assistant"
                && item.to_string().contains("task_work_overlap"))
    );
    assert!(
        requests[3]
            .function_call_output("overlap")
            .to_string()
            .contains(&test.session_configured.session_id.to_string())
    );
    assert_eq!(
        db.task_work_owners(second.session_configured.session_id, &[])
            .await?,
        owners
    );
    assert!(
        requests[1]
            .function_call_output("work")
            .to_string()
            .contains("accepted")
    );
    Ok(())
}
