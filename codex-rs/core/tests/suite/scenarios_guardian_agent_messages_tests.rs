//! Guardian retains native encrypted parent replies across incremental reviews.

use codex_config::test_support::CloudConfigBundleFixture;
use codex_core::config::Constrained;
use codex_protocol::AgentPath;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use core_test_support::context_snapshot;
use core_test_support::context_snapshot::ContextSnapshotOptions;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(false; "accepted")]
#[test_case(true; "blocked")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encrypted_parent_reply_survives_incremental_guardian_reviews(
    block_review: bool,
) -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let action = |id| {
        responses::sse(vec![
            responses::ev_function_call(
                id,
                "exec_command",
                r#"{"cmd":"exit 0","sandbox_permissions":"require_escalated","justification":"Check the requested change."}"#,
            ),
            responses::ev_completed(id),
        ])
    };
    let assessment = || {
        responses::sse(vec![
            // Keep wire text stable across Cargo and Bazel's serde_json feature sets.
            responses::ev_assistant_message(
                "assessment",
                r#"{"outcome":"deny","rationale":"Mock assessment; no command is executed.","risk_level":"high","user_authorization":"low"}"#,
            ),
            responses::ev_completed("review"),
        ])
    };
    let mut sequence = vec![
        responses::sse(vec![
            responses::ev_assistant_message(
                "waiting",
                "I will wait for the parent reply before checking the change.",
            ),
            responses::ev_completed("waiting"),
        ]),
        action("first-check"),
        assessment(),
        action("second-check"),
        assessment(),
        responses::sse(vec![
            responses::ev_assistant_message("done", "Reviews completed."),
            responses::ev_completed("done"),
        ]),
    ];
    if block_review {
        sequence.remove(4);
        sequence.remove(2);
    }
    let mock = responses::mount_sse_sequence(&server, sequence).await;
    let mut builder = test_codex()
        .with_pre_build_hook(move |home| {
            if block_review {
                let script = home.join("guardian_marker_hook.py");
                std::fs::write(&script, r#"import json, sys
from pathlib import Path
payload = json.load(sys.stdin)
if payload.get("prompt") == ">>> TRANSCRIPT START\n":
    with Path(__file__).with_suffix(".log").open("a") as log:
        log.write("blocked marker\n")
    print(json.dumps({"decision": "block", "reason": "Blocked review marker", "hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": "Guardian marker blocked exactly once"}}))
else:
    print("{}")
"#).expect("write Guardian marker hook");
                let python = if cfg!(windows) { "python" } else { "python3" };
                std::fs::write(home.join("hooks.json"), serde_json::json!({"hooks": {"UserPromptSubmit": [{"hooks": [{"type": "command", "command": format!("{python} \"{}\"", script.display())}]}]}}).to_string()).expect("write Guardian hook config");
            }
        })
        .with_model("gpt-5.5")
        .with_config(move |config| {
            if block_review { core_test_support::hooks::trust_discovered_hooks(config); }
            super::configure_scenario_catalog(config);
            config
                .features
                .enable(Feature::GuardianThreadContext)
                .expect("enable Guardian thread context");
            config.approvals_reviewer = ApprovalsReviewer::AutoReview;
            config.permissions.approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
            config
                .permissions
                .set_permission_profile(PermissionProfile::read_only())
                .expect("read-only scenario permissions");
        });
    if block_review {
        // Guardian normally disables user hooks. Managed requirements can retain them;
        // exercise that supported case so the marker's block applies to every expanded item.
        builder = builder.with_cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                "[features]\ncodex_hooks = true\n",
            ),
        );
    }
    let test = builder.build_with_auto_env(&server).await?;
    test.submit_text_turn("Check the change after the parent confirms the exact action.")
        .await?;
    let communication = InterAgentCommunication::new_encrypted(
        AgentPath::root(),
        AgentPath::root().join("worker").expect("worker path"),
        Vec::new(),
        "opaque-parent-approval".into(),
        /*trigger_turn*/ true,
    );
    let expected = serde_json::to_value(communication.to_model_input_item())?;
    // TurnComplete can arrive before the previous turn finishes teardown. Queue
    // the reply through the same mailbox path used by real agent messages.
    test.codex
        .submit(Op::InterAgentCommunication {
            communication,
            start_options: Default::default(),
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = mock.requests();
    if block_review {
        assert_eq!(
            requests.len(),
            4,
            "blocked reviewers must not request inference"
        );
        let markers =
            std::fs::read_to_string(test.config.codex_home.join("guardian_marker_hook.log"))?;
        assert_eq!(
            markers.lines().collect::<Vec<_>>(),
            vec!["blocked marker", "blocked marker"]
        );
        return Ok(());
    }
    assert_eq!(requests.len(), 6);
    for request in [&requests[2], &requests[4]] {
        let messages = request
            .input()
            .into_iter()
            .filter(|item| item["type"] == "agent_message")
            .map(|mut item| {
                item.as_object_mut()
                    .expect("agent message object")
                    .remove("id");
                item.as_object_mut()
                    .expect("agent message object")
                    .remove("internal_chat_message_metadata_passthrough");
                item
            })
            .collect::<Vec<_>>();
        assert_eq!(messages, vec![expected.clone()]);
    }
    let mut snapshot = context_snapshot::format_request_history_snapshot(
        "An encrypted parent reply is delivered as a native agent message. Both Guardian reviews contain it exactly once, in order, as agent evidence. Ciphertext and decisions are mocked; this scenario verifies transport, not backend decryption or authorization outcomes.",
        &requests,
        &ContextSnapshotOptions::default().rewrite_known_segments(),
    );
    for (pattern, replacement) in [
        (
            r#"(?m)^(\s*"environment_id": )"(?:local|remote)""#,
            "$1\"<ENVIRONMENT>\"",
        ),
        (
            r#"(The active permission profile for environment )"(?:local|remote)""#,
            "$1\"<ENVIRONMENT>\"",
        ),
        (r#"(?m)^(\s*"cwd": )"[^"]*""#, "$1\"<CWD>\""),
        (
            r#""command": \[\s*(?:"[^"]*",\s*)*"exit 0"\s*\]"#,
            "\"command\": [\"<SHELL>\", \"exit 0\"]",
        ),
    ] {
        snapshot = regex_lite::Regex::new(pattern)?
            .replace_all(&snapshot, replacement)
            .into_owned();
    }
    insta::assert_snapshot!(
        "encrypted_parent_reply_survives_incremental_guardian_reviews",
        snapshot
    );
    Ok(())
}
