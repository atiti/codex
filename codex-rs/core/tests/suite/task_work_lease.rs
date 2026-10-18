//! Expired ownership gates real tool dispatch, including nested Code Mode recovery.
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use codex_state::TaskWork;
use codex_state::TaskWorkStatus;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use test_case::test_case;

#[test_case("direct"; "ordinary_tools")]
#[test_case("exec"; "nested_code_mode_tools")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_claim_requires_recheck_before_tools(style: &'static str) -> Result<()> {
    let server = start_mock_server().await;
    let work = TaskWork {
        title: "Lease recovery".into(),
        summary: "Continuing".into(),
        status: TaskWorkStatus::Working,
        resources: vec!["bug:example:lease".into()],
    };
    let work_args = serde_json::to_string(&work)?;
    let plan_args = json!({"plan":[]}).to_string();
    let call = |id: &str, name: &str, args: &str| {
        if style == "exec" {
            ev_custom_tool_call(
                id,
                "exec",
                &format!("text(await tools.{name}({args})); text('finished-{id}');"),
            )
        } else {
            ev_function_call(id, name, args)
        }
    };
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("seed", "Ready"),
                ev_completed("seed"),
            ]),
            sse(vec![
                call("blocked", "update_plan", &plan_args),
                ev_completed("blocked"),
            ]),
            sse(vec![
                call("reclaim", "update_task", &work_args),
                ev_completed("reclaim"),
            ]),
            sse(vec![
                call("allowed", "update_plan", &plan_args),
                ev_completed("allowed"),
            ]),
            sse(vec![
                ev_assistant_message("done", "Done"),
                ev_completed("done"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_model("test-gpt-5.1-codex")
        .with_config(move |config| {
            config.update_plan_enabled = true;
            config
                .features
                .enable(Feature::TaskAwareness)
                .expect("enable task awareness");
            if style == "exec" {
                config
                    .features
                    .enable(Feature::CodeMode)
                    .expect("enable code mode");
                config
                    .features
                    .enable(Feature::CodeModeHost)
                    .expect("enable code mode host");
                config
                    .features
                    .enable(Feature::ExecutedToolCallMetadata)
                    .expect("enable executed tool call metadata");
            }
        });
    if style == "exec" {
        builder = builder
            .with_code_mode_host_program(codex_utils_cargo_bin::cargo_bin("codex-code-mode-host")?);
    }
    let test = builder.build_with_auto_env(&server).await?;
    test.submit_turn("Start").await?;
    let db = test.codex.state_db().expect("persistent state");
    assert!(
        db.update_task_work(test.session_configured.thread_id, &work)
            .await?
            .is_empty()
    );
    let pool = db
        .sqlite()
        .open_read_write_pool(&db.sqlite().state_db_path())
        .await?;
    sqlx::query("UPDATE task_work_claims SET expires_at = unixepoch() - 1")
        .execute(&pool)
        .await?;
    pool.close().await;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Continue".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let mut plan_updates = 0;
    wait_for_event(&test.codex, |event| {
        if matches!(event, EventMsg::PlanUpdate(_)) {
            plan_updates += 1;
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    let plan_result = if style == "exec" {
        "finished-allowed"
    } else {
        "Plan updated"
    };
    for (index, expected) in [(2, "ownership expired"), (3, "accepted"), (4, plan_result)] {
        let input = serde_json::to_string(&requests[index].input())?;
        assert!(input.contains(expected), "expected {expected}: {input}");
    }
    assert_eq!(plan_updates, 1);
    db.heartbeat_task_work(test.session_configured.thread_id)
        .await?;
    Ok(())
}
