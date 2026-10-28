use super::Clock;
use super::INTERVAL;
use super::Signals;
use crate::progress_evidence::evidence_packet;
use crate::progress_evidence::parse_verdict;
use codex_extension_api::ToolCallOutcome;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

fn message(role: &str, text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: role.into(),
        content: vec![ContentItem::InputText { text: text.into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
        encrypted_content: None,
        status: None,
    }
}

#[test]
fn evidence_retains_user_request_and_correction_with_bounded_trajectory() {
    let first = message("user", "Implement the complete feature");
    let large = message("assistant", &"á".repeat(30_000));
    let correction = message("user", "Include ordinary prompts");
    let packet = evidence_packet(
        vec![&first, &large, &correction],
        Some("prior review"),
        Some("current turn"),
        Some("stored goal"),
    );
    assert!(packet.contains("Implement the complete feature"));
    assert!(packet.contains("Include ordinary prompts"));
    assert!(packet.contains("prior review"));
    assert!(packet.contains("current turn"));
    assert!(packet.contains("stored goal"));
    assert!(packet.len() < 26_000);
}

#[test]
fn verdict_requires_known_decision_and_bounded_reason() {
    assert!(
        parse_verdict(r#"{"decision":"adjust","reason":"Focus on the requested feature"}"#)
            .is_some()
    );
    assert!(parse_verdict(r#"{"decision":"complete","reason":"done"}"#).is_none());
    assert!(parse_verdict(r#"{"decision":"replan","reason":""}"#).is_none());
    assert!(parse_verdict("not json").is_none());
}

#[test]
fn repeated_failures_trigger_once_and_success_resets_the_sequence() {
    let signals = Signals::default();
    let fail = ToolCallOutcome::Completed { success: false };
    assert!(!signals.observe(fail));
    assert!(!signals.observe(fail));
    assert!(!signals.observe(ToolCallOutcome::Completed { success: true }));
    assert!(!signals.observe(fail));
    assert!(!signals.observe(fail));
    assert!(signals.observe(fail));
    assert!(!signals.observe(fail));
}

#[test]
fn context_pressure_triggers_once_until_usage_recovers() {
    let signals = Signals::default();
    assert!(!signals.observe_context(/*input*/ 79, Some(100)));
    assert!(signals.observe_context(/*input*/ 80, Some(100)));
    assert!(!signals.observe_context(/*input*/ 90, Some(100)));
    assert!(!signals.observe_context(/*input*/ 10, Some(100)));
    assert!(signals.observe_context(/*input*/ 81, Some(100)));
    assert!(!signals.observe_context(/*input*/ 100, /*window*/ None));
}

#[test]
fn clock_counts_active_turns_and_excludes_idle_time() {
    let base = std::time::Instant::now();
    let mut clock = Clock::default();
    clock.start("a", base);
    clock.stop("a", base + std::time::Duration::from_secs(300));
    clock.start("b", base + std::time::Duration::from_secs(600));
    assert_eq!(
        clock.remaining(base + std::time::Duration::from_secs(700)),
        std::time::Duration::from_secs(200)
    );
    clock.reviewed("b", base + std::time::Duration::from_secs(700));
    assert_eq!(
        clock.remaining(base + std::time::Duration::from_secs(700)),
        INTERVAL
    );
    clock.stop("a", base + std::time::Duration::from_secs(710));
    assert_eq!(
        clock.remaining(base + std::time::Duration::from_secs(710)),
        INTERVAL - std::time::Duration::from_secs(10)
    );
}

#[test]
fn early_reviews_do_not_reset_reporting_cadence() {
    let base = std::time::Instant::now();
    let mut clock = Clock::default();
    clock.start("a", base);
    clock.reviewed("a", base + std::time::Duration::from_secs(300));
    assert_eq!(
        clock.report_remaining(base + std::time::Duration::from_secs(600)),
        std::time::Duration::ZERO
    );
    clock.reported("a", base + std::time::Duration::from_secs(600));
    assert_eq!(
        clock.report_remaining(base + std::time::Duration::from_secs(600)),
        INTERVAL
    );
}
