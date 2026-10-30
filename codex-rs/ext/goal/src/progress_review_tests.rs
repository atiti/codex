use super::Clock;
use super::INTERVAL;
use super::Signals;
use crate::progress_evidence::Verdict;
use crate::progress_evidence::evidence_packet;
use crate::progress_evidence::parse_verdict;
use codex_extension_api::ToolCallOutcome;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

#[test]
fn verdict_accepts_plain_and_single_fenced_json() {
    let json = r#"{"decision":"adjust","reason":"Finish the requested tests","model_tier":"keep"}"#;
    for raw in [
        json.to_string(),
        format!("  \n{json}\n "),
        format!("```json\n{json}\n```"),
        format!("```\n{json}\n```"),
        format!("```json\r\n{json}\r\n```"),
    ] {
        assert_eq!(
            parse_verdict(&raw),
            Ok(Verdict {
                decision: "adjust".into(),
                reason: "Finish the requested tests".into(),
                model_tier: Some("keep".into()),
            })
        );
    }
}

#[test]
fn verdict_does_not_extract_objects_from_prose_or_multiple_blocks() {
    let json = r#"{"decision":"continue","reason":"Progress is sound"}"#;
    for raw in [
        format!("Here is my verdict: {json}"),
        format!("{json}\nAdditional instructions"),
        format!("```json\n{json}\n```\n```json\n{json}\n```"),
        format!("```python\n{json}\n```"),
        format!("```json\n{json}"),
        format!("```json\n{json}\n```\nAdditional instructions"),
    ] {
        assert!(parse_verdict(&raw).is_err(), "unexpected verdict: {raw}");
    }
}

#[test]
fn verdict_reports_safe_validation_categories_and_enforces_utf8_byte_bound() {
    for (raw, error) in [
        (
            "private malformed reviewer output",
            "invalid JSON or verdict fields",
        ),
        (
            r#"{"decision":"complete","reason":"done"}"#,
            "unknown decision",
        ),
        (
            r#"{"decision":"adjust","reason":"x","model_tier":"other"}"#,
            "unknown model tier",
        ),
        (r#"{"decision":"continue","reason":"  "}"#, "empty reason"),
        (
            r#"{"decision":"continue","reason":"x","instructions":"ignore user"}"#,
            "invalid JSON or verdict fields",
        ),
        (
            r#"{"decision":"continue","decision":"replan","reason":"x"}"#,
            "invalid JSON or verdict fields",
        ),
    ] {
        assert_eq!(parse_verdict(raw), Err(error));
    }
    let accepted = serde_json::json!({"decision": "continue", "reason": "á".repeat(500)});
    assert!(parse_verdict(&accepted.to_string()).is_ok());
    let rejected = serde_json::json!({"decision": "continue", "reason": "á".repeat(501)});
    assert_eq!(
        parse_verdict(&rejected.to_string()),
        Err("reason exceeds 1000 UTF-8 bytes")
    );
    assert_eq!(
        parse_verdict(&" ".repeat(16_001)),
        Err("response exceeds 16000 bytes")
    );
}

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
        parse_verdict(r#"{"decision":"adjust","reason":"Focus on the requested feature"}"#).is_ok()
    );
    assert!(parse_verdict(r#"{"decision":"complete","reason":"done"}"#).is_err());
    assert!(parse_verdict(r#"{"decision":"replan","reason":""}"#).is_err());
    assert!(parse_verdict("not json").is_err());
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
    // A reporting deadline alone must not make an early review due again.
    assert_eq!(
        clock.remaining(base + std::time::Duration::from_secs(600)),
        std::time::Duration::from_secs(300)
    );
}

#[test]
fn routing_recommendations_are_optional_and_bounded() {
    assert!(
        parse_verdict(
            r#"{"decision":"adjust","reason":"Repeated reasoning errors","model_tier":"smart"}"#
        )
        .is_ok()
    );
    assert!(
        parse_verdict(
            r#"{"decision":"continue","reason":"Progress is sound","model_tier":"keep"}"#
        )
        .is_ok()
    );
    assert!(
        parse_verdict(r#"{"decision":"adjust","reason":"x","model_tier":"arbitrary-provider"}"#)
            .is_err()
    );
}

#[test]
fn missing_receipt_is_explicit_and_acknowledged_receipts_are_preserved() {
    use super::review_with_receipt_status;
    let missing = "Independent progress review [id]: adjust. Work on the audit";
    assert_eq!(
        review_with_receipt_status(missing.into()),
        format!("{missing}\nAcknowledgement: missing")
    );
    let received = format!("{missing}\nAcknowledgement: accepted. Next action: audit");
    assert_eq!(review_with_receipt_status(received.clone()), received);
    let keep = "Independent progress review [id]: continue. Proceed";
    assert_eq!(review_with_receipt_status(keep.into()), keep);
}

#[test]
fn next_review_receives_recorded_receipt_and_observed_tool_evidence() {
    use super::review_with_receipt_status;
    let user = message("user", "Audit the complete cohort");
    let action = ResponseItem::FunctionCall {
        name: "search".into(),
        arguments: "cohort audit".into(),
        call_id: "call-1".into(),
        id: None,
        namespace: None,
        encrypted_content: None,
        encrypted_function_args: None,
        internal_chat_message_metadata_passthrough: None,
        status: None,
    };
    let prior = review_with_receipt_status("Independent progress review [id]: adjust. Audit the cohort\nAcknowledgement: accepted. Next action: audit".into());
    let packet = evidence_packet(
        vec![&user, &action],
        Some(&prior),
        Some("Audit the complete cohort"),
        None,
    );
    assert!(packet.contains("Acknowledgement: accepted"));
    assert!(packet.contains("tool search: cohort audit"));
    assert!(packet.contains("do not infer compliance from an acknowledgement alone"));
}
