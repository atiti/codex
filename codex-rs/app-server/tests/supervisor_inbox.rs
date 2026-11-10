//! Black-box supervisor continuation and stopping through public app-server RPC.
#![allow(clippy::expect_used)]
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
        final_response("explicit-resume"),
        final_response("explicit-resume-replan"),
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
            Some(json!({"threadId":thread.id,"status":"blocked"})),
        )
        .await?;
    let _: ThreadGoalSetResponse = app.read_response(request).await?;
    snapshot["generation"] = json!(52);
    snapshot["events"] = json!([{"id":"wait-resolved-1","state":"persisted","actionable":true,"kind":"wait_resolved","resolves_event":"overlap-1","message":"Delivered response resolves the exact prior wait; preserve authority boundaries"}]);
    publish(home.path(), &thread.id, &snapshot)?;
    timeout(Duration::from_secs(30), async {
        loop {
            if server.received_requests().await.expect("requests").len() >= 7 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    let request = app
        .send_raw_request("thread/goal/get", Some(json!({"threadId":thread.id})))
        .await?;
    let resumed: ThreadGoalGetResponse = app.read_response(request).await?;
    assert_eq!(
        resumed.goal.expect("goal").status,
        ThreadGoalStatus::Active,
        "fresh bound resolution must reactivate the original blocked goal"
    );
    let request = app
        .send_raw_request(
            "thread/goal/set",
            Some(json!({"threadId":thread.id,"status":"paused"})),
        )
        .await?;
    let _: ThreadGoalSetResponse = app.read_response(request).await?;
    snapshot["generation"] = json!(53);
    snapshot["events"] = json!([{"id":"after-user-pause","state":"persisted","actionable":true,"kind":"coordination","message":"Pending follow-up must preserve manual pause"}]);
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
        7,
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
            if server.received_requests().await.expect("requests").len() >= 9 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(server.received_requests().await.expect("requests").len(), 9);
    app.shutdown_gracefully().await?;
    Ok(())
}

#[tokio::test]
async fn supervisor_inbox_user_interrupt_survives_new_event_and_restart() -> Result<()> {
    use codex_app_server_protocol::TurnCompletedNotification;
    use codex_app_server_protocol::TurnInterruptParams;
    use codex_app_server_protocol::TurnInterruptResponse;
    use codex_app_server_protocol::TurnStartedNotification;
    use codex_app_server_protocol::TurnStatus;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use tokio::sync::Notify;
    use wiremock::Mock;
    use wiremock::ResponseTemplate;
    let server = responses::start_mock_server().await;
    let arrived = Arc::new(Notify::new());
    let notify = Arc::clone(&arrived);
    let count = Arc::new(AtomicUsize::new(0));
    let requests = Arc::clone(&count);
    Mock::given(wiremock::matchers::method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            let n = requests.fetch_add(1, Ordering::SeqCst);
            let response = ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(final_response(&format!("cancel-{n}")));
            if n == 1 {
                notify.notify_one();
                response.set_delay(Duration::from_secs(30))
            } else {
                response
            }
        })
        .mount(&server)
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
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let ThreadStartResponse { thread, .. } = app.read_response(request).await?;
    let mut snapshot = json!({"version":1,"role":"supervisor","thread_id":thread.id,"team":"team","member":"supervisor","generation":1,"live_children":[],"events":[]});
    publish(home.path(), &thread.id, &snapshot)?;
    let initial = app
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Supervise".into(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    snapshot["generation"] = json!(2);
    snapshot["events"] = json!([{"id":"admitted","kind":"coordination","state":"persisted","actionable":true,"message":"Controlled inbox turn to interrupt"}]);
    publish(home.path(), &thread.id, &snapshot)?;
    let started = timeout(Duration::from_secs(30), async {
        loop {
            let started: TurnStartedNotification = app.read_notification("turn/started").await?;
            if started.turn.id != initial.turn.id {
                return Ok::<_, anyhow::Error>(started);
            }
        }
    })
    .await??;
    timeout(Duration::from_secs(30), arrived.notified()).await?;
    let request = app
        .send_turn_interrupt_request(TurnInterruptParams {
            thread_id: thread.id.clone(),
            turn_id: started.turn.id,
        })
        .await?;
    let _: TurnInterruptResponse = app.read_response(request).await?;
    let completed: TurnCompletedNotification = timeout(
        Duration::from_secs(30),
        app.read_notification("turn/completed"),
    )
    .await??;
    assert_eq!(completed.turn.status, TurnStatus::Interrupted);
    snapshot["generation"] = json!(3);
    snapshot["events"] = json!([{"id":"after-cancel","kind":"coordination","state":"persisted","actionable":true,"message":"New worker report must preserve user cancellation"}]);
    publish(home.path(), &thread.id, &snapshot)?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
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
    assert_eq!(count.load(Ordering::SeqCst), 2);
    let request = app.send_raw_request("thread/goal/set", Some(json!({"threadId":thread.id,"objective":"Resume synthetic coordination","status":"active"}))).await?;
    let _: ThreadGoalSetResponse = app.read_response(request).await?;
    timeout(Duration::from_secs(20), async {
        while count.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    app.shutdown_gracefully().await?;
    Ok(())
}

#[tokio::test]
async fn supervisor_inbox_own_admitted_turn_can_complete_after_disposition() -> Result<()> {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    let server = MockServer::start().await;
    let sequence = Arc::new(AtomicUsize::new(0));
    let context = Arc::new(Mutex::new(
        None::<(std::path::PathBuf, String, serde_json::Value)>,
    ));
    let fixture = Arc::clone(&context);
    let count = Arc::clone(&sequence);
    Mock::given(wiremock::matchers::method("POST")).respond_with(move |_: &wiremock::Request| {
        let n = count.fetch_add(1,Ordering::SeqCst);
        let body = match n {
            1 => responses::sse(vec![responses::ev_function_call("inspect","get_goal","{}"),responses::ev_completed("inspect")]),
            2 => {
                let guard = fixture.lock().expect("fixture lock");
                let (home,thread,resolved) = guard.as_ref().expect("resolved fixture");
                publish(home,thread,resolved).expect("publish coordinator disposition");
                responses::sse(vec![responses::ev_function_call("finish","update_goal",r#"{"status":"complete"}"#),responses::ev_completed("finish")])
            }
            3 => responses::sse(vec![responses::ev_assistant_message("review",r#"{"decision":"complete","reason":"Controlled worker result and owner disposition verified."}"#),responses::ev_completed("review")]),
            _ => final_response(&format!("final-{n}")),
        };
        ResponseTemplate::new(200).insert_header("content-type","text/event-stream").set_body_string(body)
    }).mount(&server).await;
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
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let ThreadStartResponse { thread, .. } = app.read_response(request).await?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64();
    let mut snapshot = json!({"version":1,"role":"supervisor","thread_id":thread.id,"team":"team","member":"supervisor","generation":1,"live_children":[{"id":"worker","state":"waiting","observed_at":now}],"events":[]});
    publish(home.path(), &thread.id, &snapshot)?;
    let request = app.send_raw_request("thread/goal/set",Some(json!({"threadId":thread.id,"objective":"Verify controlled worker result and disposition"}))).await?;
    let _: ThreadGoalSetResponse = app.read_response(request).await?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: thread.id.clone(),
        input: vec![UserInput::Text {
            text: "Supervise controlled worker".into(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;
    snapshot["generation"] = json!(2);
    snapshot["events"] = json!([{"id":"result","state":"persisted","actionable":true,"kind":"coordination","message":"Controlled worker returned result; accept and disposition"}]);
    let mut resolved = snapshot.clone();
    resolved["generation"] = json!(3);
    resolved["events"][0]["state"] = json!("dispositioned");
    resolved["events"][0]["actionable"] = json!(false);
    resolved["live_children"][0]["state"] = json!("complete");
    *context.lock().expect("fixture lock") =
        Some((home.path().to_path_buf(), thread.id.clone(), resolved));
    publish(home.path(), &thread.id, &snapshot)?;
    timeout(Duration::from_secs(30), async {
        loop {
            let request = app
                .send_raw_request("thread/goal/get", Some(json!({"threadId":thread.id})))
                .await?;
            let result: ThreadGoalGetResponse = app.read_response(request).await?;
            if result
                .goal
                .is_some_and(|goal| goal.status == ThreadGoalStatus::Complete)
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await??;
    assert!(
        sequence.load(Ordering::SeqCst) >= 4,
        "independent completion review must run"
    );
    app.shutdown_gracefully().await?;
    Ok(())
}
