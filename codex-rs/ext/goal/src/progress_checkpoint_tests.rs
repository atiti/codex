use super::parse_checkpoint;

#[test]
fn accepts_bounded_checkpoint_and_rejects_empty_unknown_or_oversized_input() {
    assert_eq!(
        parse_checkpoint(r#"{"checkpoint":"Check whether the feature is ready"}"#).unwrap(),
        "Check whether the feature is ready"
    );
    assert!(parse_checkpoint(r#"{"checkpoint":" "}"#).is_err());
    assert!(parse_checkpoint(r#"{"checkpoint":"check","extra":true}"#).is_err());
    assert!(parse_checkpoint(&format!("{{\"checkpoint\":\"{}\"}}", "a".repeat(1001))).is_err());
}

#[test]
fn acknowledgement_requires_bounded_structured_fields() {
    use super::parse_acknowledgement;
    assert!(parse_acknowledgement(r#"{"review_id":"review-1","decision":"accepted","reason":"Aligned","accepted":"Audit","disagreed":"None","next_action":"Run the audit"}"#).is_ok());
    assert!(
        parse_acknowledgement(
            r#"{"review_id":"","decision":"accepted","reason":"Aligned","accepted":"Audit","disagreed":"None","next_action":"Run"}"#
        )
        .is_err()
    );
    assert!(
        parse_acknowledgement(
            r#"{"review_id":"review-1","decision":"done","reason":"Aligned","accepted":"Audit","disagreed":"None","next_action":"Run"}"#
        )
        .is_err()
    );
    assert!(
        parse_acknowledgement(
            r#"{"review_id":"review-1","decision":"declined","reason":"","accepted":"Audit","disagreed":"None","next_action":"Run"}"#
        )
        .is_err()
    );
}

#[test]
fn receipt_rejects_stale_and_duplicate_reviews() {
    use super::parse_acknowledgement;
    use super::record_acknowledgement;
    use crate::progress_review::PreviousVerdict;
    use std::sync::Mutex;
    let previous = PreviousVerdict(Mutex::new(Some((
        1,
        "Independent progress review [review-1]: adjust. Verify outcome".into(),
    ))));
    let stale = parse_acknowledgement(
        r#"{"review_id":"old","decision":"accepted","reason":"Aligned","accepted":"Audit","disagreed":"None","next_action":"Audit"}"#,
    )
    .unwrap();
    assert!(record_acknowledgement(&previous, &stale, /*revision*/ 1).is_err());
    let current = parse_acknowledgement(r#"{"review_id":"review-1","decision":"accepted","reason":"Aligned","accepted":"Audit","disagreed":"None","next_action":"Audit"}"#).unwrap();
    assert!(record_acknowledgement(&previous, &current, /*revision*/ 1).is_ok());
    assert!(record_acknowledgement(&previous, &current, /*revision*/ 1).is_err());
    assert!(
        previous
            .0
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .1
            .contains("Next action: Audit")
    );
}

#[test]
fn receipt_rejects_superseding_user_revision() {
    use super::parse_acknowledgement;
    use super::record_acknowledgement;
    use crate::progress_review::PreviousVerdict;
    use std::sync::Mutex;
    let previous = PreviousVerdict(Mutex::new(Some((
        4,
        "Independent progress review [id]: adjust. Audit".into(),
    ))));
    let receipt = parse_acknowledgement(
        r#"{"review_id":"id","decision":"accepted","reason":"Aligned","accepted":"Audit","disagreed":"None","next_action":"Audit"}"#,
    )
    .unwrap();
    assert!(record_acknowledgement(&previous, &receipt, /*revision*/ 5).is_err());
    assert!(record_acknowledgement(&previous, &receipt, /*revision*/ 4).is_ok());
}

#[test]
fn continue_review_accepts_a_structured_receipt() {
    use super::parse_acknowledgement;
    use super::record_acknowledgement;
    use crate::progress_review::PreviousVerdict;
    use std::sync::Mutex;
    let previous = PreviousVerdict(Mutex::new(Some((
        1,
        "Independent progress review [id]: continue. Proceed".into(),
    ))));
    let receipt = parse_acknowledgement(r#"{"review_id":"id","decision":"accepted","reason":"Agreed","accepted":"Proceed","disagreed":"None","next_action":"Run tests"}"#).unwrap();
    assert!(record_acknowledgement(&previous, &receipt, /*revision*/ 1).is_ok());
    let state = previous.0.lock().unwrap();
    let message = &state.as_ref().unwrap().1;
    assert!(message.contains("Accepted: Proceed"));
    assert!(message.contains("Disagreed: None"));
    assert!(message.contains("Next action: Run tests"));
}
