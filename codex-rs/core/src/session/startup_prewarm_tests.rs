use super::STARTUP_PREWARM_HISTORY_MAX_BYTES;
use super::truncate_startup_prewarm_history;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

fn message(role: &str, text: impl Into<String>) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content: vec![ContentItem::InputText { text: text.into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn startup_prewarm_history_keeps_a_bounded_suffix_of_complete_turns() {
    let history = vec![
        message("user", "old ".repeat(STARTUP_PREWARM_HISTORY_MAX_BYTES * 4)),
        message("assistant", "old answer"),
        message("user", "recent request"),
        message("assistant", "recent answer"),
    ];

    let retained = truncate_startup_prewarm_history(history);

    assert!(serde_json::to_vec(&retained).unwrap().len() <= STARTUP_PREWARM_HISTORY_MAX_BYTES);
    assert_eq!(
        retained,
        vec![
            message("user", "recent request"),
            message("assistant", "recent answer")
        ]
    );
}

#[test]
fn startup_prewarm_history_omits_a_single_turn_over_the_budget() {
    let history = vec![message(
        "user",
        "oversized ".repeat(STARTUP_PREWARM_HISTORY_MAX_BYTES * 4),
    )];

    assert!(truncate_startup_prewarm_history(history).is_empty());
}
