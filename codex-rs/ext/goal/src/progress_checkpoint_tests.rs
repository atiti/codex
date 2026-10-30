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
    assert!(parse_acknowledgement(r#"{"review_id":"review-1","decision":"accepted","reason":"Aligned","next_action":"Run the audit"}"#).is_ok());
    assert!(
        parse_acknowledgement(
            r#"{"review_id":"","decision":"accepted","reason":"Aligned","next_action":"Run"}"#
        )
        .is_err()
    );
    assert!(
        parse_acknowledgement(
            r#"{"review_id":"review-1","decision":"done","reason":"Aligned","next_action":"Run"}"#
        )
        .is_err()
    );
    assert!(
        parse_acknowledgement(
            r#"{"review_id":"review-1","decision":"declined","reason":"","next_action":"Run"}"#
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
        r#"{"review_id":"old","decision":"accepted","reason":"Aligned","next_action":"Audit"}"#,
    )
    .unwrap();
    assert!(record_acknowledgement(&previous, &stale, 1).is_err());
    let current = parse_acknowledgement(r#"{"review_id":"review-1","decision":"accepted","reason":"Aligned","next_action":"Audit"}"#).unwrap();
    assert!(record_acknowledgement(&previous, &current, 1).is_ok());
    assert!(record_acknowledgement(&previous, &current, 1).is_err());
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
        r#"{"review_id":"id","decision":"accepted","reason":"Aligned","next_action":"Audit"}"#,
    )
    .unwrap();
    assert!(record_acknowledgement(&previous, &receipt, 5).is_err());
    assert!(record_acknowledgement(&previous, &receipt, 4).is_ok());
}
