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
