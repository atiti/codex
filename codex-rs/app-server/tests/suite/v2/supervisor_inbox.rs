//! Black-box supervisor continuation and stopping through public app-server RPC.
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_sequence_unchecked;
use codex_app_server_protocol::ThreadGoalGetResponse;
use codex_app_server_protocol::ThreadGoalSetResponse;
use codex_app_server_protocol::ThreadGoalStatus;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::timeout;

fn publish(home: &std::path::Path, thread: &str, value: &serde_json::Value) -> Result<()> {
    let directory = home.join("supervisor-inbox");
    std::fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let path = directory.join(format!("{thread}.json"));
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, serde_json::to_vec(value)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn final_response(id: &str) -> String {
    responses::sse(vec![
        responses::ev_response_created(id),
        responses::ev_assistant_message(id, "Coordination still needs a disposition."),
        responses::ev_completed(id),
    ])
}

#[tokio::test]
async fn supervisor_inbox_wakes_after_final_rejects_stops_and_bounds_duplicate_replans()
-> Result<()> {
    let server = create_mock_responses_server_sequence_unchecked(vec![
        final_response("parent-final"),
        responses::sse(vec![
            responses::ev_response_created("blocked"),
            responses::ev_function_call("block", "update_goal", r#"{"status":"blocked"}"#),
            responses::ev_completed("blocked"),
        ]),
        responses::sse(vec![
            responses::ev_response_created("complete"),
            responses::ev_function_call("complete", "update_goal", r#"{"status":"complete"}"#),
            responses::ev_completed("complete"),
        ]),
        final_response("first-attempt"),
        final_response("bounded-replan"),
        final_response("resumed-wait"),
        final_response("resumed-replan"),
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
    let request = app
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".into()),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } = app.read_response(request).await?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64();
    let mut snapshot = json!({"version":1,"role":"supervisor","thread_id":thread.id,"team":"team","member":"supervisor","generation":1,"live_children":[{"id":"worker","state":"waiting","observed_at":now}],"events":[]});
    publish(home.path(), &thread.id, &snapshot)?;
    let request = app.send_raw_request("thread/goal/set",Some(json!({"threadId":thread.id,"objective":"Resolve worker overlap without retrying unknown operations"}))).await?;
    let _: ThreadGoalSetResponse = app.read_response(request).await?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: thread.id.clone(),
        input: vec![UserInput::Text {
            text: "Supervise the worker".into(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    // Enrollment after thread/start is picked up at the real turn boundary.
    snapshot["generation"] = json!(2);
    snapshot["events"] = json!([{"id":"overlap-1","state":"persisted","actionable":true,"kind":"coordination","message":"Owner overlap; delivery unknown. Reconcile before worker retry."}]);
    publish(home.path(), &thread.id, &snapshot)?;
    timeout(Duration::from_secs(30), async {
        loop {
            if server.received_requests().await.expect("requests").len() >= 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(
        requests.len(),
        5,
        "stable unresolved work must not spin inference"
    );
    let bodies = requests
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        bodies.contains("unresolved coordination"),
        "model stop rejection must be returned to the model"
    );
    assert!(bodies.contains("supervisor_inbox"));
    let request = app
        .send_raw_request("thread/goal/get", Some(json!({"threadId":thread.id})))
        .await?;
    let result: ThreadGoalGetResponse = app.read_response(request).await?;
    assert_eq!(result.goal.expect("goal").status, ThreadGoalStatus::Active);
    // Acknowledgement/transport generation changes do not create fresh work.
    snapshot["generation"] = json!(50);
    snapshot["events"][0]["state"] = json!("acked");
    publish(home.path(), &thread.id, &snapshot)?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(server.received_requests().await.expect("requests").len(), 5);
    snapshot["generation"] = json!(51);
    snapshot["live_children"] = json!([]);
    snapshot["events"][0] = json!({"id":"overlap-1","state":"dispositioned","actionable":false,"kind":"coordination","message":"Await reviewed-plan approval","wait":{"owner":"user","delivery_state":"delivered","question_ref":"user-message-7","resume_condition":"User approves reviewed plan"}});
    publish(home.path(), &thread.id, &snapshot)?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(server.received_requests().await.expect("requests").len(), 5);
    let request = app
        .send_raw_request(
            "thread/goal/set",
            Some(json!({"threadId":thread.id,"status":"paused"})),
        )
        .await?;
    let _: ThreadGoalSetResponse = app.read_response(request).await?;
    snapshot["generation"] = json!(52);
    snapshot["events"] = json!([{"id":"wait-resolved-1","state":"persisted","actionable":true,"kind":"wait_resolved","message":"User response delivered; reconcile exact approval scope"}]);
    publish(home.path(), &thread.id, &snapshot)?;
    app.shutdown_gracefully().await?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized_with_timeout(Duration::from_secs(30))
        .await?;
    let request = app
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread.id.clone(),
            ..Default::default()
        })
        .await?;
    let _: ThreadResumeResponse = app.read_response(request).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        5,
        "manual pause survives restart and pending inbox"
    );
    let request = app
        .send_raw_request(
            "thread/goal/set",
            Some(json!({"threadId":thread.id,"status":"active"})),
        )
        .await?;
    let _: ThreadGoalSetResponse = app.read_response(request).await?;
    timeout(Duration::from_secs(30), async {
        loop {
            if server.received_requests().await.expect("requests").len() >= 7 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(server.received_requests().await.expect("requests").len(), 7);
    app.shutdown_gracefully().await?;
    Ok(())
}
