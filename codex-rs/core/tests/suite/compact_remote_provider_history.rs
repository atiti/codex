use super::*;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_provider_switch_filters_foreign_reasoning_before_ordinary_sampling() -> Result<()>
{
    skip_if_no_network!(Ok(()));
    let server = wiremock::MockServer::start().await;
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                json!({"type": "response.output_item.done", "item": {
                    "type": "reasoning", "id": "rs_azure_ordinary", "summary": [],
                    "encrypted_content": "AZURE_ONLY_ORDINARY_CIPHERTEXT"
                }}),
                responses::ev_assistant_message("msg-before-ordinary", "saved answer"),
                responses::ev_completed("resp-before-ordinary"),
            ]),
            sse(vec![responses::ev_completed("resp-after-ordinary")]),
        ],
    )
    .await;
    let initial = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_provider_id = "agentroute-azure".to_string();
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&server)
        .await?;
    initial.submit_turn("retain ordinary user context").await?;
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
            config.model_provider.supports_websockets = false;
        })
        .resume(&server, home, path)
        .await?;
    resumed.submit_turn("continue without compaction").await?;
    resumed.codex.shutdown_and_wait().await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .body_json()
            .to_string()
            .contains("retain ordinary user context")
    );
    assert!(!requests[1].input().iter().any(|item| {
        item.get("encrypted_content") == Some(&json!("AZURE_ONLY_ORDINARY_CIPHERTEXT"))
    }));
    Ok(())
}

#[test_case(false; "automatic")]
#[test_case(true; "manual")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_provider_switch_compacts_to_portable_plaintext_summary(
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
                responses::ev_assistant_message("summary", "PORTABLE_ASSISTANT_AND_TOOL_FACT"),
                responses::ev_completed_with_tokens("resp-compact", /*total_tokens*/ 80),
            ]),
            sse(vec![responses::ev_completed("resp-after")]),
            sse(vec![responses::ev_completed("resp-third-provider")]),
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
    let third = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_provider_id = "third-provider".to_string();
        })
        .resume(
            &server,
            resumed.home.clone(),
            resumed
                .session_configured
                .rollout_path
                .clone()
                .context("rollout path")?,
        )
        .await?;
    third
        .submit_turn("continue after a second provider switch")
        .await?;
    third.codex.shutdown_and_wait().await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    for request in &requests[1..] {
        assert!(!request.input().iter().any(|item| {
            item.get("encrypted_content") == Some(&json!("AZURE_ONLY_CIPHERTEXT"))
        }));
    }
    assert!(
        !requests[1]
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
    for request in &requests[2..] {
        assert!(request.body_contains_text("PORTABLE_ASSISTANT_AND_TOOL_FACT"));
        assert!(
            !request
                .input()
                .iter()
                .any(|item| item["type"] == "compaction")
        );
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_compaction_preserves_new_owned_checkpoints() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = wiremock::MockServer::start().await;
    let compact = |summary: &str, id: &str| {
        sse(vec![
            json!({"type":"response.output_item.done","item":{"type":"compaction","encrypted_content":summary}}),
            responses::ev_completed(id),
        ])
    };
    let mock = responses::mount_sse_sequence(&server, vec![
        sse(vec![
            json!({"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_foreign","summary":[],"encrypted_content":"FOREIGN_SECRET"}}),
            responses::ev_assistant_message("msg-original", "FIRST_ANSWER"),
            responses::ev_completed("first"),
        ]),
        sse(vec![responses::ev_assistant_message("msg-second", "SECOND_ANSWER"), responses::ev_completed_with_tokens("second", /*total_tokens*/ 500)]),
        responses::sse_failed("too-large", "context_length_exceeded", "context too large"),
        compact("OWN_PREFIX", "prefix"),
        compact("OWN_FINAL", "final"),
        responses::sse_completed("after"),
    ]).await;
    let initial = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_provider_id = "agentroute-azure".to_string();
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&server)
        .await?;
    initial.submit_turn("FIRST_CONTEXT").await?;
    initial.submit_turn("SECOND_CONTEXT").await?;
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
            config.model_provider_id = "agentroute-azure".to_string();
            config.model_provider.supports_websockets = false;
            config.model_auto_compact_token_limit = Some(200);
        })
        .resume(&server, home, path)
        .await?;
    resumed.submit_turn("AFTER_SWITCH").await?;
    resumed.codex.shutdown_and_wait().await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 6);
    assert!(requests[3].body_contains_text("FIRST_CONTEXT"));
    assert!(requests[4].body_contains_text("OWN_PREFIX"));
    assert!(requests[4].body_contains_text("SECOND_CONTEXT"));
    assert!(requests[5].body_contains_text("OWN_FINAL"));
    assert!(requests[5].body_contains_text("AFTER_SWITCH"));
    Ok(())
}

#[inline(never)]
fn pin_compact<'a, T, F>(
    _stage: &'static str,
    build: impl FnOnce() -> F,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>
where
    F: std::future::Future<Output = T> + Send + 'a,
{
    let fut = build();
    Box::pin(fut)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owned_opaque_checkpoint_can_be_recovered_before_provider_switch() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let fixture = seed_owned_opaque_checkpoint().await?;
    prove_owned_opaque_checkpoint_recovery(fixture).await
}

#[inline(never)]
fn seed_owned_opaque_checkpoint()
-> std::pin::Pin<Box<dyn std::future::Future<Output = Result<OwnedOpaqueCheckpointFixture>> + Send>>
{
    Box::pin(async move {
        let (server, mock) = mount_owned_opaque_mock().await?;
        let owner = build_owned_opaque_session(&server).await?;
        let home = owner.home.clone();
        let rollout_path = owner
            .session_configured
            .rollout_path
            .clone()
            .context("rollout path")?;
        compact_owned_session(owner).await?;
        Ok(OwnedOpaqueCheckpointFixture {
            server,
            mock,
            home,
            rollout_path,
        })
    })
}

#[inline(never)]
fn mount_owned_opaque_mock() -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<(wiremock::MockServer, responses::ResponseMock)>>
            + Send,
    >,
> {
    Box::pin(async move {
        let server = wiremock::MockServer::start().await;
        let mock = responses::mount_sse_sequence(&server, owned_opaque_events()).await;
        Ok((server, mock))
    })
}

fn owned_opaque_events() -> Vec<String> {
    vec![
        sse(vec![
            responses::ev_assistant_message("fact", "PORTABLE_LEGACY_FACT"),
            responses::ev_completed("initial"),
        ]),
        sse(vec![
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "compaction",
                    "id": "cmp_legacy",
                    "encrypted_content": "OWNER_ONLY_CHECKPOINT"
                }
            }),
            responses::ev_completed("opaque"),
        ]),
        sse(vec![
            responses::ev_assistant_message("summary", "PORTABLE_LEGACY_FACT"),
            responses::ev_completed("portable"),
        ]),
        sse(vec![responses::ev_completed("continued")]),
    ]
}

#[inline(never)]
fn build_owned_opaque_session(
    server: &wiremock::MockServer,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<core_test_support::test_codex::TestCodex>>
            + Send
            + '_,
    >,
> {
    Box::pin(async move {
        test_codex()
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(|config| {
                config.model_provider.supports_websockets = false;
            })
            .build_with_auto_env(server)
            .await
    })
}

#[inline(never)]
fn compact_owned_session(
    owner: core_test_support::test_codex::TestCodex,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> {
    Box::pin(async move {
        let owner = remember_tool_evidence(owner).await?;
        let owner = compact_current_thread(owner).await?;
        shutdown_test_codex(owner).await
    })
}

#[inline(never)]
fn remember_tool_evidence(
    owner: core_test_support::test_codex::TestCodex,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<core_test_support::test_codex::TestCodex>> + Send>,
> {
    Box::pin(async move {
        pin_compact("owned_submit_turn", || {
            owner.submit_turn("Remember the tool evidence")
        })
        .await?;
        Ok(owner)
    })
}

#[inline(never)]
fn compact_current_thread(
    owner: core_test_support::test_codex::TestCodex,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<core_test_support::test_codex::TestCodex>> + Send>,
> {
    Box::pin(async move {
        pin_compact("compact_submit", || owner.codex.submit(Op::Compact)).await?;
        pin_compact("compact_wait", || wait_for_turn_complete(&owner.codex)).await;
        Ok(owner)
    })
}

#[inline(never)]
fn shutdown_test_codex(
    owner: core_test_support::test_codex::TestCodex,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> {
    Box::pin(async move {
        pin_compact("shutdown_and_wait", || owner.codex.shutdown_and_wait()).await?;
        Ok(())
    })
}

struct OwnedOpaqueCheckpointFixture {
    server: wiremock::MockServer,
    mock: responses::ResponseMock,
    home: std::sync::Arc<tempfile::TempDir>,
    rollout_path: std::path::PathBuf,
}

#[inline(never)]
fn prove_owned_opaque_checkpoint_recovery(
    fixture: OwnedOpaqueCheckpointFixture,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> {
    Box::pin(async move {
        let OwnedOpaqueCheckpointFixture {
            server,
            mock,
            home,
            rollout_path,
        } = fixture;
        let home = recover_owned_checkpoint(&server, home, rollout_path.clone()).await?;
        recall_after_plaintext_compaction(&server, home.clone(), rollout_path.clone()).await?;
        let requests = mock.requests();
        assert_eq!(requests.len(), 4);
        assert!(
            requests[1]
                .input()
                .iter()
                .any(|item| item["type"] == "compaction_trigger")
        );
        assert!(
            !requests[2]
                .input()
                .iter()
                .any(|item| item["type"] == "compaction_trigger")
        );
        assert!(requests[2].body_contains_text("OWNER_ONLY_CHECKPOINT"));
        assert!(!requests[3].body_contains_text("OWNER_ONLY_CHECKPOINT"));
        assert!(requests[3].body_contains_text("PORTABLE_LEGACY_FACT"));
        assert_plaintext_compaction_replacement(&rollout_path)?;
        drop(home);
        drop(mock);
        Ok(())
    })
}

#[inline(never)]
fn recover_owned_checkpoint(
    server: &wiremock::MockServer,
    home: std::sync::Arc<tempfile::TempDir>,
    rollout_path: std::path::PathBuf,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<std::sync::Arc<tempfile::TempDir>>> + Send + '_>,
> {
    Box::pin(async move {
        let recovery = resume_with_plaintext_prompt(server, home.clone(), rollout_path).await?;
        let recovery = compact_current_thread(recovery).await?;
        shutdown_test_codex(recovery).await?;
        Ok(home)
    })
}

#[inline(never)]
fn resume_with_plaintext_prompt(
    server: &wiremock::MockServer,
    home: std::sync::Arc<tempfile::TempDir>,
    rollout_path: std::path::PathBuf,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<core_test_support::test_codex::TestCodex>>
            + Send
            + '_,
    >,
> {
    Box::pin(async move {
        test_codex()
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(|config| {
                config.model_provider.supports_websockets = false;
                config.compact_prompt = Some(
                    "Summarize the conversation in plaintext, preserving decisions and tool evidence."
                        .to_string(),
                );
            })
            .resume(server, home, rollout_path)
            .await
    })
}

#[inline(never)]
fn recall_after_plaintext_compaction(
    server: &wiremock::MockServer,
    home: std::sync::Arc<tempfile::TempDir>,
    rollout_path: std::path::PathBuf,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
    Box::pin(async move {
        let next = resume_on_switched_provider(server, home, rollout_path).await?;
        submit_recall_and_shutdown(next).await
    })
}

#[inline(never)]
fn resume_on_switched_provider(
    server: &wiremock::MockServer,
    home: std::sync::Arc<tempfile::TempDir>,
    rollout_path: std::path::PathBuf,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<core_test_support::test_codex::TestCodex>>
            + Send
            + '_,
    >,
> {
    Box::pin(async move {
        test_codex()
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(|config| {
                config.model_provider_id = "new-provider".to_string();
                config.model_provider.supports_websockets = false;
            })
            .resume(server, home, rollout_path)
            .await
    })
}

#[inline(never)]
fn submit_recall_and_shutdown(
    next: core_test_support::test_codex::TestCodex,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> {
    Box::pin(async move {
        pin_compact("recall_submit", || next.submit_turn("Recall the evidence")).await?;
        pin_compact("recall_shutdown", || next.codex.shutdown_and_wait()).await?;
        Ok(())
    })
}

fn assert_plaintext_compaction_replacement(rollout_path: &std::path::Path) -> Result<()> {
    let rollout = std::fs::read_to_string(rollout_path)?;
    let replacement = rollout.lines().rev().find_map(|line| {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        (value.get("type").and_then(serde_json::Value::as_str) == Some("compacted"))
            .then_some(value)
    });
    let replacement = replacement.context("plaintext compaction replacement")?;
    let message = replacement
        .pointer("/payload/message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        !message.trim().is_empty(),
        "prompted compaction must install a nonempty plaintext summary"
    );
    assert!(
        !rollout_contains_encrypted_replacement(&replacement),
        "prompted compaction must not retain encrypted_content"
    );
    Ok(())
}

fn rollout_contains_encrypted_replacement(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            map.get("encrypted_content")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|content| !content.is_empty())
                || map.values().any(rollout_contains_encrypted_replacement)
        }
        serde_json::Value::Array(items) => items.iter().any(rollout_contains_encrypted_replacement),
        _ => false,
    }
}

#[test_case(false; "known foreign owner")]
#[test_case(true; "unknown legacy owner")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_opaque_checkpoint_fails_closed_without_sending_or_rewriting_history(
    legacy: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = wiremock::MockServer::start().await;
    let mock = responses::mount_sse_sequence(&server, vec![
        sse(vec![responses::ev_assistant_message("fact", "LEGACY_TOOL_FACT"), responses::ev_completed("initial")]),
        sse(vec![json!({"type":"response.output_item.done","item":{"type":"compaction","id":"cmp_legacy","encrypted_content":"OWNER_ONLY_CHECKPOINT"}}), responses::ev_completed("opaque")]),
    ]).await;
    let owner = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&server)
        .await?;
    owner.submit_turn("Remember the tool evidence").await?;
    owner.codex.submit(Op::Compact).await?;
    wait_for_turn_complete(&owner.codex).await;
    owner.codex.shutdown_and_wait().await?;
    let path = owner
        .session_configured
        .rollout_path
        .clone()
        .context("rollout path")?;
    if legacy {
        fn remove_owner(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(map) => {
                    if let Some(metadata) = map
                        .get_mut("metadata")
                        .and_then(serde_json::Value::as_object_mut)
                    {
                        metadata.remove("model_provider_id");
                    }
                    for value in map.values_mut() {
                        remove_owner(value);
                    }
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        remove_owner(item);
                    }
                }
                _ => {}
            }
        }
        let mut lines = std::fs::read_to_string(&path)?
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<serde_json::Result<Vec<_>>>()?;
        for line in &mut lines {
            remove_owner(line);
        }
        std::fs::write(
            &path,
            lines
                .iter()
                .map(serde_json::to_string)
                .collect::<serde_json::Result<Vec<_>>>()?
                .join("\n")
                + "\n",
        )?;
    }
    let next = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_provider_id = "new-provider".to_string();
            config.model_provider.supports_websockets = false;
        })
        .resume(&server, owner.home.clone(), path.clone())
        .await?;
    next.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Recall the evidence".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let error = wait_for_event(&next.codex, |event| matches!(event, EventMsg::Error(_))).await;
    assert!(matches!(error, EventMsg::Error(error) if error.message.contains("opaque checkpoint")));
    wait_for_turn_complete(&next.codex).await;
    next.codex.shutdown_and_wait().await?;
    assert_eq!(
        mock.requests().len(),
        2,
        "foreign checkpoint must never leave the client"
    );
    assert!(std::fs::read_to_string(path)?.contains("OWNER_ONLY_CHECKPOINT"));
    Ok(())
}
