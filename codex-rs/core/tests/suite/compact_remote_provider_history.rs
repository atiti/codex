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
async fn resumed_provider_switch_filters_encrypted_state_before_local_compaction() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = wiremock::MockServer::start().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                json!({"type": "response.output_item.done", "item": {
                    "type": "reasoning", "id": "rs_azure_local", "summary": [],
                    "encrypted_content": "AZURE_ONLY_LOCAL_CIPHERTEXT"
                }}),
                responses::ev_assistant_message("msg-before-local-compact", "saved answer"),
                responses::ev_completed("resp-before-local-compact"),
            ]),
            sse(vec![
                responses::ev_assistant_message("msg-local-summary", "safe local summary"),
                responses::ev_completed("resp-local-compact"),
            ]),
            sse(vec![responses::ev_completed("resp-after-local-compact")]),
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
        .submit_turn("retain this local compaction context")
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
        .with_config(|config| {
            config.model_provider.name = "OpenAI-compatible local test provider".to_string();
            config.model_provider.capabilities =
                Some(codex_model_provider_info::ModelProviderCapabilities {
                    remote_compaction: Some(
                        codex_model_provider_info::RemoteCompactionSupport::Unsupported,
                    ),
                    ..Default::default()
                });
            config.model_provider.tool_compatibility = Some(
                codex_model_provider_info::ToolCompatibility::
                    FunctionsAndApplyPatchPreserveReasoning,
            );
        })
        .resume(&server, home, path)
        .await?;
    resumed.codex.submit(Op::Compact).await?;
    wait_for_turn_complete(&resumed.codex).await;
    resumed
        .submit_turn("continue after local compaction")
        .await?;
    resumed.codex.shutdown_and_wait().await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let compact_request = &requests[1];
    assert!(compact_request.input().iter().any(|item| {
        item.to_string()
            .contains("retain this local compaction context")
    }));
    assert!(
        !compact_request
            .input()
            .iter()
            .any(|item| item.to_string().contains("AZURE_ONLY_LOCAL_CIPHERTEXT"))
    );
    assert!(
        !compact_request
            .input()
            .iter()
            .any(|item| item["type"] == "compaction_trigger")
    );
    assert!(
        requests[2]
            .input()
            .iter()
            .any(|item| item.to_string().contains("safe local summary"))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_warmup_caps_history_and_keeps_latest_complete_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let http = wiremock::MockServer::start().await;
    let _initial_responses = responses::mount_sse_sequence(
        &http,
        vec![
            sse(vec![
                responses::ev_assistant_message("msg-old", "old turn response"),
                responses::ev_completed("resp-old"),
            ]),
            sse(vec![
                responses::ev_assistant_message("msg-middle", "middle turn response"),
                responses::ev_completed("resp-middle"),
            ]),
            sse(vec![
                responses::ev_assistant_message("msg-latest", "latest complete assistant turn"),
                responses::ev_completed("resp-latest"),
            ]),
        ],
    )
    .await;
    let initial = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .build_with_auto_env(&http)
        .await?;
    let older_turn = format!("older turn marker {}", "o".repeat(4_000));
    let middle_turn = format!("middle turn marker {}", "m".repeat(3_800));
    initial.submit_turn(&older_turn).await?;
    initial.submit_turn(&middle_turn).await?;
    initial.submit_turn("latest complete user turn").await?;
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

    let warmup_body = warmup.body_json();
    assert_eq!(warmup_body["generate"], false);
    let input = warmup_body["input"].as_array().context("prewarm input")?;
    let history = input
        .iter()
        .filter(|item| {
            item["type"] == "message" && matches!(item["role"].as_str(), Some("user" | "assistant"))
        })
        .cloned()
        .collect::<Vec<_>>();
    let history_text = serde_json::to_string(&history)?;
    assert!(serde_json::to_vec(&history)?.len() <= 8_000);
    assert!(!history_text.contains("older turn marker"));
    assert!(history_text.contains("middle turn marker"));
    assert!(history_text.contains("latest complete user turn"));
    assert!(history_text.contains("latest complete assistant turn"));

    resumed.codex.shutdown_and_wait().await?;
    ws.shutdown().await;
    Ok(())
}

#[test_case(true, false; "provider switch")]
#[test_case(false, false; "same provider")]
#[test_case(true, true; "legacy provider metadata absent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_sample_filters_only_foreign_provider_state(
    switch_provider: bool,
    legacy_rollout: bool,
) -> Result<()> {
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
    if legacy_rollout {
        let mut lines = std::fs::read_to_string(&path)?
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<serde_json::Result<Vec<_>>>()?;
        for line in &mut lines {
            if let Some(metadata) = line
                .get_mut("metadata")
                .and_then(serde_json::Value::as_object_mut)
            {
                metadata.remove("model_provider_id");
            }
        }
        let rollout = lines
            .iter()
            .map(serde_json::to_string)
            .collect::<serde_json::Result<Vec<_>>>()?
            .join("\n");
        std::fs::write(&path, format!("{rollout}\n"))?;
    }
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
