//! A reviewer may decline or fail; neither outcome approves completion.
#![allow(dead_code)]

#[path = "../src/completion_review.rs"]
mod completion_review;

use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_state::ThreadGoal;
use codex_state::ThreadGoalStatus;
use pretty_assertions::assert_eq;

#[test]
fn reviewer_prompt_is_bounded_and_includes_recent_evidence() {
    let goal = ThreadGoal {
        thread_id: ThreadId::new(),
        goal_id: "goal-1".into(),
        objective: "Ship a release".into(),
        status: ThreadGoalStatus::Active,
        token_budget: None,
        tokens_used: 0,
        time_used_seconds: 0,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let history = vec![
        ResponseItem::Message {
            id: None,
            role: "assistant".into(),
            content: vec![ContentItem::OutputText {
                text: "older".repeat(20_000),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputText {
                text: "check release tag".into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let prompt = completion_review::review_prompt(&goal, &history);
    assert!(prompt.len() < 10_000);
    assert!(prompt.contains("check release tag"));
    assert!(prompt.contains("Ship a release"));
}

#[test]
fn multibyte_history_and_objective_stay_within_byte_cap() {
    let goal = ThreadGoal {
        thread_id: ThreadId::new(),
        goal_id: "goal-unicode".into(),
        objective: "á".repeat(10_000),
        status: ThreadGoalStatus::Active,
        token_budget: None,
        tokens_used: 0,
        time_used_seconds: 0,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let history = vec![ResponseItem::Message {
        id: None,
        role: "assistant".into(),
        content: vec![ContentItem::OutputText {
            text: "ü".repeat(10_000),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];
    let prompt = completion_review::review_prompt(&goal, &history);
    assert!(prompt.len() < 10_000);
}

#[test]
fn invalid_or_unavailable_review_never_completes() {
    for raw in [
        Err("timeout".to_string()),
        Ok("not json".to_string()),
        Ok(r#"{"decision":"complete","reason":""}"#.to_string()),
    ] {
        let verdict = completion_review::parse_review(raw);
        assert_eq!(verdict.decision(), "continue");
    }
    let verdict = completion_review::parse_review(Ok(
        r#"{"decision":"complete","reason":"Tag and deployment verified"}"#.to_string(),
    ));
    assert_eq!(verdict.decision(), "complete");
}
