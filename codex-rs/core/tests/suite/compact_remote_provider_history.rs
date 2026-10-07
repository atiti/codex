use super::*;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(false; "automatic")]
#[test_case(true; "manual")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_provider_switch_filters_encrypted_state_before_compaction(
    manual: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = wiremock::MockServer::start().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                json!({"type": "response.output_item.done", "item": {
                    "type": "reasoning", "id": "rs_azure_saved", "summary": [],
                    "encrypted_content": "AZURE_ONLY_CIPHERTEXT"
                }}),
                responses::ev_assistant_message("msg-before", "saved answer"),
                responses::ev_completed_with_tokens("resp-before", /*total_tokens*/ 500),
            ]),
            sse(vec![
                json!({"type": "response.output_item.done", "item": {
                    "type": "compaction", "id": "cmp_openai_new",
                    "encrypted_content": "OPENAI_CHECKPOINT"
                }}),
                responses::ev_completed_with_tokens("resp-compact", /*total_tokens*/ 80),
            ]),
            sse(vec![responses::ev_completed("resp-after")]),
        ],
    )
    .await;
    let initial = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_provider_id = "agentroute-azure".to_string();
        })
        .build_with_auto_env(&server)
        .await?;
    initial.submit_turn("retain this user context").await?;
    let home = initial.home.clone();
    let path = initial
        .session_configured
        .rollout_path
        .clone()
        .context("rollout path")?;
    initial.codex.shutdown_and_wait().await?;
    let resumed = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(move |config| {
            if !manual {
                config.model_auto_compact_token_limit = Some(200);
            }
        })
        .resume(&server, home, path)
        .await?;
    if manual {
        resumed.codex.submit(Op::Compact).await?;
        wait_for_turn_complete(&resumed.codex).await;
    }
    resumed.submit_turn("continue safely").await?;
    resumed.codex.shutdown_and_wait().await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert!(!request.input().iter().any(|item| {
            item.get("encrypted_content") == Some(&json!("AZURE_ONLY_CIPHERTEXT"))
        }));
    }
    assert!(
        requests[1]
            .input()
            .iter()
            .any(|item| item["type"] == "compaction_trigger")
    );
    assert!(
        requests[1]
            .body_json()
            .to_string()
            .contains("retain this user context")
    );
    assert!(
        requests[2]
            .input()
            .iter()
            .any(|item| { item.get("encrypted_content") == Some(&json!("OPENAI_CHECKPOINT")) })
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_warmup_filters_foreign_reasoning_before_sampling() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let http = wiremock::MockServer::start().await;
    let _initial_response = responses::mount_sse_once(
        &http,
        sse(vec![
            json!({"type": "response.output_item.done", "item": {
                "type": "reasoning", "id": "rs_azure_saved", "summary": [],
                "encrypted_content": "AZURE_ONLY_CIPHERTEXT"
            }}),
            responses::ev_assistant_message("msg-initial", "saved answer"),
            responses::ev_completed("resp-initial"),
        ]),
    )
    .await;
    let initial = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_provider_id = "agentroute-azure".to_string();
        })
        .build_with_auto_env(&http)
        .await?;
    initial.submit_turn("keep this context").await?;
    let home = initial.home.clone();
    let path = initial
        .session_configured
        .rollout_path
        .clone()
        .context("rollout path")?;
    initial.codex.shutdown_and_wait().await?;
    let ws = responses::start_websocket_server(vec![
        vec![vec![
            responses::ev_response_created("warm-1"),
            responses::ev_completed("warm-1"),
        ]],
        vec![vec![
            responses::ev_response_created("warm-2"),
            responses::ev_completed("warm-2"),
        ]],
    ])
    .await;
    let base_url = format!("{}/v1", ws.uri());
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let resumed = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_extensions(Arc::new(extensions.build()))
        .with_config(move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.supports_websockets = true;
        })
        .resume(&http, home, path)
        .await?;
    tokio::time::timeout(
        Duration::from_secs(10),
        ws.wait_for_request(/*connection_index*/ 0, /*request_index*/ 0),
    )
    .await?;
    let warmup = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            resumed.codex.prewarm_with_history().await;
            tokio::select! {
                request = ws.wait_for_request(/*connection_index*/ 1, /*request_index*/ 0) => break request,
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
    })
    .await?;
    assert_eq!(warmup.body_json()["generate"], false);
    assert!(
        !warmup
            .body_json()
            .to_string()
            .contains("AZURE_ONLY_CIPHERTEXT")
    );
    assert!(warmup.body_json().to_string().contains("keep this context"));
    assert!(warmup.body_json().get("previous_response_id").is_none());
    resumed.codex.shutdown_and_wait().await?;
    ws.shutdown().await;
    Ok(())
}

#[test_case(true; "provider switch")]
#[test_case(false; "same provider")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_sample_filters_only_foreign_provider_state(switch_provider: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = wiremock::MockServer::start().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                json!({"type": "response.output_item.done", "item": {
                    "type": "reasoning", "id": "rs_azure_resume", "summary": [],
                    "encrypted_content": "AZURE_ONLY_RESUME_STATE"
                }}),
                responses::ev_assistant_message("msg-saved", "saved answer"),
                responses::ev_completed("resp-saved"),
            ]),
            sse(vec![
                responses::ev_assistant_message("msg-resumed", "continued answer"),
                responses::ev_completed("resp-resumed"),
            ]),
        ],
    )
    .await;
    let initial = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_provider_id = "agentroute-azure".to_string();
        })
        .build_with_auto_env(&server)
        .await?;
    initial
        .submit_turn("keep this resumed user context")
        .await?;
    let home = initial.home.clone();
    let path = initial
        .session_configured
        .rollout_path
        .clone()
        .context("rollout path")?;
    initial.codex.shutdown_and_wait().await?;
    let resumed = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(move |config| {
            if !switch_provider {
                config.model_provider_id = "agentroute-azure".to_string();
            }
        })
        .resume(&server, home, path)
        .await?;

    resumed.submit_turn("first sample after resume").await?;
    resumed.codex.shutdown_and_wait().await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let resumed_request = requests[1].body_json();
    assert!(
        resumed_request
            .to_string()
            .contains("first sample after resume")
    );
    assert_eq!(
        resumed_request
            .to_string()
            .contains("AZURE_ONLY_RESUME_STATE"),
        !switch_provider,
    );
    Ok(())
}
