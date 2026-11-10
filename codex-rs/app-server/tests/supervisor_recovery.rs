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
