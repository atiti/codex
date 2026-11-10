//! Default paginated backend and actual supported operator hold/settlement notifications.
#![allow(clippy::expect_used)]
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_sequence_unchecked;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_app_server_protocol::WarningNotification;
use codex_features::Feature;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::timeout;
fn write_private(path: &Path, value: &serde_json::Value) -> Result<()> {
    let directory = path.parent().expect("parent");
    std::fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        if let Some(parent) = directory.parent().filter(|p| {
            p.file_name()
                .is_some_and(|s| s == "supervisor-admission-resolutions")
        }) {
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(value)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(tmp, path)?;
    Ok(())
}
#[tokio::test]
async fn default_paginated_recovery_warns_and_consumes_exact_not_admitted_record() -> Result<()> {
    let script = |id: &str| {
        responses::sse(vec![
            responses::ev_response_created(id),
            responses::ev_assistant_message(id, "Synthetic coordination checkpoint"),
            responses::ev_completed(id),
        ])
    };
    let server = create_mock_responses_server_sequence_unchecked(vec![
        script("previous-a"),
        script("recovery-b"),
        script("bounded-replan"),
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
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let ThreadStartResponse { thread, .. } = app.read_response(request).await?;
    let completed = app
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Synthetic user turn A".into(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    app.shutdown_gracefully().await?;
    let inbox = home
        .path()
        .join("supervisor-inbox")
        .join(format!("{}.json", thread.id));
    write_private(
        &inbox,
        &json!({"version":1,"role":"supervisor","thread_id":thread.id,"team":"team","member":"supervisor","generation":2,"live_children":[],"events":[{"id":"debt","state":"persisted","actionable":true,"kind":"coordination","message":"Only synthetic local fixture effects"}]}),
    )?;
    let cursor = home
        .path()
        .join("supervisor-inbox-cursors")
        .join(format!("{}.json", thread.id));
    write_private(
        &cursor,
        &json!({"generation":1,"suspended":false,"reservation":"synthetic-b","admission_token":"synthetic-b","admitted_turn_id":completed.turn.id,"in_flight":false,"attempts":0}),
    )?;
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
    let warning: WarningNotification =
        timeout(Duration::from_secs(15), app.read_notification("warning")).await??;
    assert!(
        warning.message.contains("verified turn receipt"),
        "must expose the real held reservation, not an unsupported backend call: {}",
        warning.message
    );
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    let record = home
        .path()
        .join("supervisor-admission-resolutions")
        .join(&thread.id)
        .join("synthetic-b.json");
    write_private(
        &record,
        &json!({"version":1,"thread_id":thread.id,"reservation":"synthetic-b","decision":"not_admitted","evidence":["Synthetic B was never admitted; A is a different known turn"],"authority":"Explicit synthetic test operator decision; no real pending effects","created_at":1.0}),
    )?;
    timeout(Duration::from_secs(20), async {
        loop {
            if server.received_requests().await.expect("requests").len() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(&cursor)?)?;
    assert_eq!(state["last_resolution"], json!("synthetic-b"));
    app.shutdown_gracefully().await?;
    Ok(())
}

fn final_response(id: &str) -> String {
    responses::sse(vec![
        responses::ev_response_created(id),
        responses::ev_assistant_message(id, "Synthetic discovery checkpoint"),
        responses::ev_completed(id),
    ])
}

async fn wait_requests(server: &wiremock::MockServer, expected: usize) -> Result<()> {
    timeout(Duration::from_secs(20), async {
        while server.received_requests().await.expect("requests").len() < expected {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn late_enrollment_discovers_idle_thread_and_retains_missing_projection_hold() -> Result<()> {
    let server = create_mock_responses_server_sequence_unchecked(vec![
        final_response("ordinary"),
        final_response("late"),
        final_response("bounded-replan"),
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
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let ThreadStartResponse { thread, .. } = app.read_response(request).await?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: thread.id.clone(),
        input: vec![UserInput::Text {
            text: "Ordinary turn before enrollment".into(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    let cursor = home
        .path()
        .join("supervisor-inbox-cursors")
        .join(format!("{}.json", thread.id));
    assert!(
        !cursor.exists(),
        "discovery cannot register an ordinary thread"
    );
    let inbox = home
        .path()
        .join("supervisor-inbox")
        .join(format!("{}.json", thread.id));
    let mut snapshot = json!({"version":1,"role":"supervisor","thread_id":thread.id,"team":"team","member":"supervisor","generation":1,"live_children":[],"events":[]});
    write_private(&inbox, &snapshot)?;
    timeout(Duration::from_secs(10), async {
        while !cursor.exists() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    std::fs::remove_file(&inbox)?;
    let warning = timeout(Duration::from_secs(10), async {
        loop {
            let warning: WarningNotification = app.read_notification("warning").await?;
            if warning
                .message
                .contains("Registered supervisor inbox is missing")
            {
                return Ok::<_, anyhow::Error>(warning);
            }
        }
    })
    .await??;
    assert_eq!(warning.thread_id, Some(thread.id.clone()));
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    snapshot["generation"] = json!(2);
    snapshot["events"] = json!([{"id":"late-report","state":"persisted","actionable":true,"kind":"coordination","message":"Late worker overlap requires disposition"}]);
    write_private(&inbox, &snapshot)?;
    wait_requests(&server, 3).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(server.received_requests().await.expect("requests").len(), 3);
    app.shutdown_gracefully().await?;
    Ok(())
}

#[tokio::test]
async fn cancellation_before_enrollment_seeds_suspension_without_ordinary_cursor() -> Result<()> {
    use codex_app_server_protocol::ThreadGoalSetResponse;
    use codex_app_server_protocol::TurnCompletedNotification;
    use codex_app_server_protocol::TurnInterruptParams;
    use codex_app_server_protocol::TurnInterruptResponse;
    use codex_app_server_protocol::TurnStartResponse;
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
            if n == 0 {
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
    let request = app
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Cancel before supervisor enrollment".into(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let TurnStartResponse { turn } = app.read_response(request).await?;
    timeout(Duration::from_secs(15), arrived.notified()).await?;
    let request = app
        .send_turn_interrupt_request(TurnInterruptParams {
            thread_id: thread.id.clone(),
            turn_id: turn.id,
        })
        .await?;
    let _: TurnInterruptResponse = app.read_response(request).await?;
    let completed: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(completed.turn.status, TurnStatus::Interrupted);
    let cursor = home
        .path()
        .join("supervisor-inbox-cursors")
        .join(format!("{}.json", thread.id));
    assert!(
        !cursor.exists(),
        "ordinary cancellation must not create supervisor state"
    );
    // The generic native interruption survives a restart before enrollment.
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
    assert!(!cursor.exists());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let inbox = home
        .path()
        .join("supervisor-inbox")
        .join(format!("{}.json", thread.id));
    write_private(
        &inbox,
        &json!({"version":1,"role":"supervisor","thread_id":thread.id,"team":"team","member":"supervisor","generation":1,"live_children":[],"events":[{"id":"late","state":"persisted","actionable":true,"kind":"coordination","message":"Preserve earlier user cancellation"}]}),
    )?;
    timeout(Duration::from_secs(10), async {
        while !cursor.exists() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(&cursor)?)?;
    assert_eq!(state["suspended"], json!(true));
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
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let request = app.send_raw_request("thread/goal/set",Some(json!({"threadId":thread.id,"objective":"Explicitly resume enrolled supervisor","status":"active"}))).await?;
    let _: ThreadGoalSetResponse = app.read_response(request).await?;
    wait_requests(&server, 2).await?;
    app.shutdown_gracefully().await?;
    Ok(())
}
