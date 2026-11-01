//! Exercises the narrow Goal reviewer bridge with an active root turn and mocked model.

use std::time::Duration;

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::context::ContextualUserFragment;
use codex_core::context::InternalContextSource;
use codex_core::context::InternalModelContextFragment;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_reviewer_uses_active_provider_with_isolated_read_only_context() -> Result<()> {
    let server = start_mock_server().await;
    let root = mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .is_ok_and(|body| body.to_string().contains("root work stays active"))
        },
        sse_response(sse(vec![ev_response_created("root"), ev_completed("root")]))
            .set_delay(Duration::from_secs(10)),
    )
    .await;
    let reviewer = mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .is_ok_and(|body| body.to_string().contains("independent review evidence"))
        },
        sse_response(sse(vec![
            ev_response_created("review"),
            ev_assistant_message(
                "decision",
                r#"{"decision":"continue","reason":"No release evidence"}"#,
            ),
            ev_completed("review"),
        ])),
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let started = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "root work stays active".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let codex_core::TurnInputSubmission::Started { turn_id, .. } = started else {
        panic!("root turn did not start");
    };
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnStarted(_))
    })
    .await;
    let result = test
        .codex
        .review_goal_completion(&turn_id, "goal-1", "independent review evidence", &[])
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        result,
        r#"{"decision":"continue","reason":"No release evidence"}"#
    );
    let request = reviewer
        .requests()
        .into_iter()
        .find(|request| request.body_contains_text("independent review evidence"))
        .expect("review request");
    let root_request = root
        .requests()
        .into_iter()
        .find(|request| request.body_contains_text("root work stays active"))
        .expect("root request");
    assert_eq!(
        request.body_json()["model"],
        root_request.body_json()["model"]
    );
    assert!(request.instructions_text().contains("Independently audit"));
    assert!(!request.body_contains_text("root work stays active"));
    let body = request.body_json();
    assert!(body["tools"].is_array(), "review tools");
    let tool_surface = body["tools"].to_string();
    assert!(!tool_surface.contains("web_search"));
    assert!(!tool_surface.contains("mcp__"));
    assert!(!tool_surface.contains("agentroute_collaboration"));
    assert!(
        request
            .message_input_texts("developer")
            .join("\n")
            .contains("sandbox_mode` is `read-only`")
    );
    assert!(
        request
            .message_input_texts("user")
            .iter()
            .any(|text| text.contains("independent review evidence"))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_reviewer_runs_for_an_ordinary_active_turn() -> Result<()> {
    let server = start_mock_server().await;
    let review_server = start_mock_server().await;
    let root = mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            String::from_utf8_lossy(&request.body).contains("ordinary task stays active")
        },
        sse_response(sse(vec![ev_response_created("root"), ev_completed("root")]))
            .set_delay(Duration::from_secs(10)),
    )
    .await;
    let reviewer = mount_response_once_match(
        &review_server,
        |request: &wiremock::Request| {
            String::from_utf8_lossy(&request.body).contains("review recent trajectory")
        },
        sse_response(sse(vec![
            ev_response_created("review"),
            ev_assistant_message(
                "decision",
                r#"{"decision":"adjust","reason":"Return to the requested feature"}"#,
            ),
            ev_completed_with_tokens("review", /*total_tokens*/ 123),
        ])),
    )
    .await;
    let review_url = format!("{}/v1", review_server.uri());
    let test = test_codex()
        .with_pre_build_hook(|home| {
            let script = home.join("review_hook.py");
            std::fs::write(
                &script,
                "import json, sys\nfrom pathlib import Path\np = json.load(sys.stdin)\nwith Path(__file__).with_suffix('.log').open('a') as f: f.write(p.get('prompt', '') + '\\n')\nprint('{}')\n",
            )
            .expect("write hook");
            let python = if cfg!(windows) { "python" } else { "python3" };
            let hooks = serde_json::json!({"hooks": {"UserPromptSubmit": [{"hooks": [{"type": "command", "command": format!("{python} \"{}\"", script.display())}]}]}});
            std::fs::write(home.join("hooks.json"), hooks.to_string()).expect("write hooks");
        })
        .with_config(move |config| {
            core_test_support::hooks::trust_discovered_hooks(config);
            config.progress_review_model = Some("independent-review-model".to_string());
            config.progress_review_provider = Some("review-provider".to_string());
            let mut provider = config.model_provider.clone();
            provider.base_url = Some(review_url);
            config.model_providers.insert("review-provider".to_string(), provider);
        })
        .build_with_auto_env(&server)
        .await?;
    let started = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "ordinary task stays active".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let codex_core::TurnInputSubmission::Started { turn_id, .. } = started else {
        panic!("turn did not start");
    };
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnStarted(_))
    })
    .await;
    let (result, usage, model, provider) = test
        .codex
        .review_progress(&turn_id, "review recent trajectory".into())
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(result.contains("adjust"));
    assert_eq!(model, "independent-review-model");
    assert_eq!(provider, "review-provider");
    let hook_log = std::fs::read_to_string(test.codex_home_path().join("review_hook.log"))?;
    assert!(hook_log.contains("ordinary task stays active"));
    assert!(!hook_log.contains("review recent trajectory"));
    assert_eq!(usage.map(|usage| usage.input_tokens), Some(123));
    let request = reviewer
        .requests()
        .into_iter()
        .find(|request| request.body_contains_text("review recent trajectory"))
        .expect("review request");
    assert!(
        request
            .instructions_text()
            .contains("Independently assess progress")
    );
    assert!(!request.body_contains_text("ordinary task stays active"));
    assert_eq!(request.body_json()["model"], "independent-review-model");
    assert_ne!(
        request.body_json()["model"],
        root.requests()[0].body_json()["model"]
    );
    let revision = test
        .codex
        .conversation_history_snapshot()
        .await
        .user_message_revision();
    let advice: ResponseItem = ContextualUserFragment::into(InternalModelContextFragment::new(
        InternalContextSource::from_static("progress_review"),
        "Return to the requested feature".to_string(),
    ));
    assert!(
        !test
            .codex
            .deliver_progress_review("wrong-turn", revision, advice.clone())
            .await
    );
    assert!(
        !test
            .codex
            .deliver_progress_review(&turn_id, revision + 1, advice.clone())
            .await
    );
    assert!(
        test.codex
            .deliver_progress_review(&turn_id, revision, advice)
            .await
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn reviewer_github_tool_is_exposed_and_runs_fixed_query() -> Result<()> {
    core_test_support::test_codex::run_test_with_large_stack("reviewer_github", || {
        reviewer_github_tool_test(true, false, false)
    })
}

#[cfg(unix)]
#[test]
fn reviewer_github_tool_denies_without_allow_rule() -> Result<()> {
    core_test_support::test_codex::run_test_with_large_stack("reviewer_github_denied", || {
        reviewer_github_tool_test(false, false, false)
    })
}

#[cfg(unix)]
#[test]
fn reviewer_github_tool_disappears_with_denied_reads() -> Result<()> {
    core_test_support::test_codex::run_test_with_large_stack("reviewer_github_restricted", || {
        reviewer_github_tool_test(true, true, false)
    })
}

#[cfg(unix)]
#[test]
fn reviewer_github_tool_limits_large_output() -> Result<()> {
    core_test_support::test_codex::run_test_with_large_stack("reviewer_github_large", || {
        reviewer_github_tool_test(true, false, true)
    })
}

#[cfg(unix)]
async fn reviewer_github_tool_test(
    allow: bool,
    deny_reads: bool,
    large_output: bool,
) -> Result<()> {
    use codex_features::Feature;
    use core_test_support::responses::ev_function_call;
    use core_test_support::responses::mount_sse_sequence;
    core_test_support::skip_if_no_network!(Ok(()));
    use std::os::unix::fs::PermissionsExt;
    let server = start_mock_server().await;
    let review_server = start_mock_server().await;
    let _root = mount_response_once_match(
        &server,
        |r: &wiremock::Request| {
            String::from_utf8_lossy(&r.body).contains("root stays active for github")
        },
        sse_response(sse(vec![ev_response_created("root"), ev_completed("root")]))
            .set_delay(Duration::from_secs(10)),
    )
    .await;
    let reviewer = mount_sse_sequence(
        &review_server,
        vec![
            sse(vec![
                ev_response_created("review1"),
                ev_function_call(
                    "gh1",
                    "review_github_pull_request",
                    r#"{"repository":"atiti/agent-router","pull_request":136}"#,
                ),
                ev_completed("review1"),
            ]),
            sse(vec![
                ev_response_created("review2"),
                ev_assistant_message("decision", r#"{"decision":"continue","reason":"test"}"#),
                ev_completed("review2"),
            ]),
        ],
    )
    .await;
    let tool_dir = tempfile::tempdir()?;
    let gh = tool_dir.path().join("gh");
    std::fs::write(
        &gh,
        if large_output {
            "#!/bin/sh\n/usr/bin/yes x | /usr/bin/head -c 260000\n"
        } else {
            "#!/bin/sh\nprintf '%s\\n' '{\"state\":\"OPEN\",\"headRefOid\":\"testhead\"}'\n"
        },
    )?;
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755))?;
    let path = tool_dir.path().to_string_lossy().into_owned();
    let review_url = format!("{}/v1", review_server.uri());
    let test = test_codex()
        .with_pre_build_hook(move |home| {
            std::fs::write(home.join("config.toml"), "# load user rules\n").unwrap();
            std::fs::create_dir_all(home.join("rules")).unwrap();
            std::fs::write(
                home.join("rules/default.rules"),
                if allow {
                    r#"prefix_rule(pattern=["gh", "pr", "view"], decision="allow")"#
                } else {
                    r#"prefix_rule(pattern=["gh", "pr", "view"], decision="forbidden")"#
                },
            )
            .unwrap();
        })
        .with_config(move |config| {
            if deny_reads {
                use codex_protocol::models::PermissionProfile;
                use codex_protocol::permissions::FileSystemAccessMode;
                use codex_protocol::permissions::FileSystemSandboxEntry;
                use codex_protocol::permissions::NetworkSandboxPolicy;
                use codex_utils_absolute_path::AbsolutePathBuf;
                let mut fs = PermissionProfile::read_only().file_system_sandbox_policy();
                fs.entries.push(FileSystemSandboxEntry::new(
                    AbsolutePathBuf::try_from("/private/tmp/reviewer-denied")
                        .unwrap()
                        .into(),
                    FileSystemAccessMode::Deny,
                ));
                config
                    .permissions
                    .set_permission_profile(PermissionProfile::from_runtime_permissions(
                        &fs,
                        NetworkSandboxPolicy::Restricted,
                    ))
                    .unwrap();
            }
            config
                .features
                .enable(Feature::ReviewGithubEvidence)
                .unwrap();
            config
                .permissions
                .shell_environment_policy
                .r#set
                .insert("PATH".into(), path);
            config.progress_review_provider = Some("review-provider".into());
            let mut provider = config.model_provider.clone();
            provider.base_url = Some(review_url);
            config
                .model_providers
                .insert("review-provider".into(), provider);
        })
        .build_with_auto_env(&server)
        .await?;
    let started = test
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "root stays active for github".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let codex_core::TurnInputSubmission::Started { turn_id, .. } = started else {
        panic!("turn did not start")
    };
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnStarted(_))
    })
    .await;
    test.codex
        .review_progress(&turn_id, "inspect PR 136".into())
        .await
        .map_err(anyhow::Error::msg)?;
    let requests = reviewer.requests();
    if deny_reads {
        assert!(
            !requests[0].body_json()["tools"]
                .to_string()
                .contains("review_github_pull_request")
        );
        return Ok(());
    }
    assert!(
        requests[0].body_json()["tools"]
            .to_string()
            .contains("review_github_pull_request")
    );
    assert!(
        requests[1].body_contains_text(if large_output {
            "GitHub response too large"
        } else if allow {
            "testhead"
        } else {
            "requires an explicit gh pr view"
        }),
        "tool output must reach reviewer: {}",
        requests[1].body_json()["input"]
    );
    Ok(())
}
