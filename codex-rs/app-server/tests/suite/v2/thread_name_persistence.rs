//! Naming persists an empty paginated thread for resume before and after a restart.

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadSetNameParams;
use codex_app_server_protocol::ThreadSetNameResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::timeout;

const READ_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
async fn named_empty_paginated_thread_resumes_after_restart() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let start_id = app_server
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            history_mode: Some(ThreadHistoryMode::Paginated),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } =
        timeout(READ_TIMEOUT, app_server.read_response(start_id)).await??;
    let thread_id = thread.id;
    let name = "Scheduled task";
    let set_name_id = app_server
        .send_thread_set_name_request(ThreadSetNameParams {
            thread_id: thread_id.clone(),
            name: name.to_string(),
        })
        .await?;
    let _: ThreadSetNameResponse =
        timeout(READ_TIMEOUT, app_server.read_response(set_name_id)).await??;

    // Run-now resumes immediately after naming, before any user turn exists.
    let resume_id = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread_id.clone(),
            exclude_turns: true,
            ..Default::default()
        })
        .await?;
    let ThreadResumeResponse { thread, .. } =
        timeout(READ_TIMEOUT, app_server.read_response(resume_id)).await??;
    assert_eq!(thread.id, thread_id);
    assert_eq!(thread.name.as_deref(), Some(name));
    assert!(
        timeout(READ_TIMEOUT, app_server.shutdown_gracefully())
            .await??
            .success()
    );
    drop(app_server);

    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let resume_id = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread_id.clone(),
            exclude_turns: true,
            ..Default::default()
        })
        .await?;
    let ThreadResumeResponse { thread, .. } =
        timeout(READ_TIMEOUT, app_server.read_response(resume_id)).await??;
    assert_eq!(thread.id, thread_id);
    assert_eq!(thread.name.as_deref(), Some(name));

    let input = vec![UserInput::Text {
        text: "Scheduled run".to_string(),
        text_elements: Vec::new(),
    }];
    let completed = timeout(
        READ_TIMEOUT,
        app_server.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread_id.clone(),
            input,
            ..Default::default()
        }),
    )
    .await??;
    assert_eq!(completed.thread_id, thread_id);
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    assert_eq!(completed.turn.error, None);
    Ok(())
}

#[tokio::test]
async fn task_titles_evolve_and_manual_rename_survives_later_turns() -> Result<()> {
    use codex_app_server_protocol::ThreadReadParams;
    use codex_app_server_protocol::ThreadReadResponse;
    use codex_state::SqliteConfig;
    use codex_state::StateRuntime;
    use codex_state::TaskWork;
    use codex_state::TaskWorkStatus;
    use codex_utils_absolute_path::test_support::PathExt;
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(codex_features::Feature::TaskAwareness)
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let start = app
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            history_mode: Some(ThreadHistoryMode::Paginated),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } =
        timeout(READ_TIMEOUT, app.read_response(start)).await??;
    let id = thread.id;
    let seed = app
        .send_thread_set_name_request(ThreadSetNameParams {
            thread_id: id.clone(),
            name: "Initial task".into(),
        })
        .await?;
    let _: ThreadSetNameResponse = timeout(READ_TIMEOUT, app.read_response(seed)).await??;
    let db = StateRuntime::init(
        SqliteConfig::new_for_testing(home.path().abs()),
        "mock_provider".into(),
    )
    .await?;
    let work = TaskWork {
        title: "Fix current task".into(),
        summary: "Implementing".into(),
        status: TaskWorkStatus::Working,
        resources: Vec::new(),
    };
    assert!(
        db.update_task_work(codex_protocol::ThreadId::from_string(&id)?, &work)
            .await?
            .is_empty()
    );
    let parsed_id = codex_protocol::ThreadId::from_string(&id)?;
    // Simulate metadata persistence succeeding before the automatic checkpoint finishes.
    db.prepare_task_work_title(parsed_id, "Interrupted automatic title")
        .await?;
    db.update_thread_name(parsed_id, Some("Interrupted automatic title"))
        .await?;
    let input = vec![UserInput::Text {
        text: "Continue".into(),
        text_elements: Vec::new(),
    }];
    timeout(
        READ_TIMEOUT,
        app.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: id.clone(),
            input: input.clone(),
            ..Default::default()
        }),
    )
    .await??;
    let read = app
        .send_thread_read_request(ThreadReadParams {
            thread_id: id.clone(),
            include_turns: false,
        })
        .await?;
    let read: ThreadReadResponse = timeout(READ_TIMEOUT, app.read_response(read)).await??;
    assert_eq!(read.thread.name.as_deref(), Some("Fix current task"));
    // A second task renamed after its work record exists but before any sync is locked too.
    let start = app
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            history_mode: Some(ThreadHistoryMode::Paginated),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread: early, .. } =
        timeout(READ_TIMEOUT, app.read_response(start)).await??;
    let early_id = codex_protocol::ThreadId::from_string(&early.id)?;
    db.update_task_work(early_id, &work).await?;
    let rename = app
        .send_thread_set_name_request(ThreadSetNameParams {
            thread_id: early.id,
            name: "Manual before first refresh".into(),
        })
        .await?;
    let _: ThreadSetNameResponse = timeout(READ_TIMEOUT, app.read_response(rename)).await??;
    assert_eq!(db.task_work_title(early_id).await?, None);
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(db.sqlite().state_db_path()),
    )
    .await?;
    sqlx::query("CREATE TRIGGER fail_title_pin BEFORE UPDATE OF title_locked ON task_work BEGIN SELECT RAISE(FAIL, 'simulated checkpoint failure'); END").execute(&pool).await?;
    let rename = app
        .send_thread_set_name_request(ThreadSetNameParams {
            thread_id: id.clone(),
            name: "My pinned title".into(),
        })
        .await?;
    let _: ThreadSetNameResponse = timeout(READ_TIMEOUT, app.read_response(rename)).await??;
    sqlx::query("DROP TRIGGER fail_title_pin")
        .execute(&pool)
        .await?;
    pool.close().await;
    db.update_task_work(
        codex_protocol::ThreadId::from_string(&id)?,
        &TaskWork {
            title: "Changed task scope".into(),
            ..work
        },
    )
    .await?;
    timeout(
        READ_TIMEOUT,
        app.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: id.clone(),
            input,
            ..Default::default()
        }),
    )
    .await??;
    let read = app
        .send_thread_read_request(ThreadReadParams {
            thread_id: id,
            include_turns: false,
        })
        .await?;
    let read: ThreadReadResponse = timeout(READ_TIMEOUT, app.read_response(read)).await??;
    assert_eq!(read.thread.name.as_deref(), Some("My pinned title"));
    Ok(())
}
