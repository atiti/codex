//! Exercises the narrow Goal reviewer bridge with an active root turn and mocked model.

use std::time::Duration;

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_reviewer_uses_active_provider_with_isolated_read_only_context() -> Result<()> {
    let server = start_mock_server().await;
    let root = mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .is_ok_and(|body| body.to_string().contains("root work stays active"))
        },
        sse_response(sse(vec![ev_response_created("root"), ev_completed("root")]))
            .set_delay(Duration::from_secs(10)),
    )
    .await;
    let reviewer = mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .is_ok_and(|body| body.to_string().contains("independent review evidence"))
        },
        sse_response(sse(vec![
            ev_response_created("review"),
            ev_assistant_message(
                "decision",
                r#"{"decision":"continue","reason":"No release evidence"}"#,
            ),
            ev_completed("review"),
        ])),
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let started = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "root work stays active".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let codex_core::TurnInputSubmission::Started { turn_id, .. } = started else {
        panic!("root turn did not start");
    };
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnStarted(_))
    })
    .await;
    let result = test
        .codex
        .review_goal_completion(&turn_id, "goal-1", "independent review evidence", &[])
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        result,
        r#"{"decision":"continue","reason":"No release evidence"}"#
    );
    let request = reviewer
        .requests()
        .into_iter()
        .find(|request| request.body_contains_text("independent review evidence"))
        .expect("review request");
    let root_request = root
        .requests()
        .into_iter()
        .find(|request| request.body_contains_text("root work stays active"))
        .expect("root request");
    assert_eq!(
        request.body_json()["model"],
        root_request.body_json()["model"]
    );
    assert!(request.instructions_text().contains("Independently audit"));
    assert!(!request.body_contains_text("root work stays active"));
    let body = request.body_json();
    assert!(body["tools"].is_array(), "review tools");
    let tool_surface = body["tools"].to_string();
    assert!(!tool_surface.contains("web_search"));
    assert!(!tool_surface.contains("mcp__"));
    assert!(!tool_surface.contains("agentroute_collaboration"));
    assert!(
        request
            .message_input_texts("developer")
            .join("\n")
            .contains("sandbox_mode` is `read-only`")
    );
    assert!(
        request
            .message_input_texts("user")
            .iter()
            .any(|text| text.contains("independent review evidence"))
    );
    Ok(())
}
