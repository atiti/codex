//! Exercises the goal extension's empty-response breaker through the public API.

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_sequence;
use app_test_support::create_mock_responses_server_sequence_unchecked;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ThreadGoalGetResponse;
use codex_app_server_protocol::ThreadGoalSetResponse;
use codex_app_server_protocol::ThreadGoalStatus;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::timeout;

#[test_case::test_case(None; "empty")]
#[test_case::test_case(Some("final_answer"); "draft")]
#[test_case::test_case(Some("commentary"); "commentary")]
#[test_case::test_case(Some("tool"); "tool")]
#[tokio::test]
async fn empty_goal_continuations_block_after_three_without_activity(
    recovery: Option<&str>,
) -> Result<()> {
    let turns = if recovery.is_some() { 6 } else { 3 };
    let mut scripts = Vec::new();
    for turn in 1..=turns {
        let mut id = format!("response-{turn}");
        let mut events = vec![responses::ev_response_created(&id)];
        if turn == 3
            && let Some(recovery) = recovery
        {
            if recovery == "tool" {
                events.push(responses::ev_function_call("get-goal", "get_goal", "{}"));
                events.push(responses::ev_completed(&id));
                scripts.push(responses::sse(events));
                id.push_str("-after-tool");
                events = vec![responses::ev_response_created(&id)];
            } else {
                let mut added = responses::ev_assistant_message("progress", "");
                added["type"] = json!("response.output_item.added");
                events.push(added);
                events.push(responses::ev_output_text_delta("Useful progress"));
                let mut event = responses::ev_assistant_message("progress", "Useful progress");
                event["item"]["phase"] = json!(recovery);
                events.push(event);
            }
        }
        let mut empty_final = responses::ev_assistant_message(&format!("empty-final-{turn}"), "");
        empty_final["item"]["phase"] = json!("final_answer");
        events.push(empty_final);
        events.push(responses::ev_completed(&id));
        scripts.push(responses::sse(events));
    }
    let server = create_mock_responses_server_sequence(scripts).await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_model("gpt-5.4")
        .enable_feature(Feature::Goals)
        .write(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let request = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let ThreadStartResponse { thread, .. } = mcp.read_response(request).await?;
    let request = mcp
        .send_raw_request(
            "thread/goal/set",
            Some(json!({"threadId": thread.id, "objective": "Finish the work"})),
        )
        .await?;
    let _: ThreadGoalSetResponse = mcp.read_response(request).await?;

    for turn in 1..=turns {
        if turn == 3 && matches!(recovery, Some("final_answer" | "commentary")) {
            let delta: serde_json::Value = timeout(
                Duration::from_secs(30),
                mcp.read_notification("item/agentMessage/delta"),
            )
            .await??;
            assert_eq!(json!("Useful progress"), delta["delta"]);
        }
        let completed: TurnCompletedNotification = timeout(
            Duration::from_secs(30),
            mcp.read_notification("turn/completed"),
        )
        .await??;
        assert_eq!(TurnStatus::Completed, completed.turn.status);
        assert_eq!(None, completed.turn.error);
    }
    let request = mcp
        .send_raw_request("thread/goal/get", Some(json!({"threadId": thread.id})))
        .await?;
    let result: ThreadGoalGetResponse = mcp.read_response(request).await?;
    assert_eq!(
        ThreadGoalStatus::Blocked,
        result.goal.expect("goal exists").status
    );
    server.verify().await;
    Ok(())
}

#[tokio::test]
async fn completion_review_continue_keeps_goal_active_and_reports_activity() -> Result<()> {
    let server = create_mock_responses_server_sequence_unchecked(vec![
        responses::sse(vec![
            responses::ev_response_created("root-update"),
            responses::ev_function_call("update-goal", "update_goal", r#"{"status":"complete"}"#),
            responses::ev_completed("root-update"),
        ]),
        responses::sse(vec![
            responses::ev_response_created("review"),
            responses::ev_assistant_message(
                "review-decision",
                r#"{"decision":"continue","reason":"The release check is missing."}"#,
            ),
            responses::ev_completed("review"),
        ]),
        responses::sse(vec![
            responses::ev_response_created("root-finish"),
            responses::ev_assistant_message(
                "root-message",
                "The goal remains active while I add the missing release check.",
            ),
            responses::ev_completed("root-finish"),
        ]),
    ])
    .await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_model("mock-model")
        .enable_feature(Feature::Goals)
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized_with_timeout(Duration::from_secs(30))
        .await?;
    let start = app
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".into()),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } = app.read_response(start).await?;
    let set_goal = app
        .send_raw_request(
            "thread/goal/set",
            Some(json!({"threadId": thread.id, "objective": "Verify the release check"})),
        )
        .await?;
    let _: ThreadGoalSetResponse = app.read_response(set_goal).await?;

    let completed = app
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Mark the goal complete".into(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    assert_eq!(TurnStatus::Completed, completed.turn.status);
    assert_eq!(None, completed.turn.error);

    let mut saw_continue = false;
    for _ in 0..16 {
        let item: ItemCompletedNotification = timeout(
            Duration::from_secs(30),
            app.read_notification("item/completed"),
        )
        .await??;
        if let ThreadItem::AgentMessage { text, .. } = item.item
            && text.contains("Review agent · continue (attempt 1)")
        {
            saw_continue = true;
            break;
        }
    }
    assert!(saw_continue, "completion review activity was not visible");

    let get_goal = app
        .send_raw_request("thread/goal/get", Some(json!({"threadId": thread.id})))
        .await?;
    let result: ThreadGoalGetResponse = app.read_response(get_goal).await?;
    assert_eq!(
        ThreadGoalStatus::Active,
        result.goal.expect("goal exists").status
    );
    Ok(())
}
