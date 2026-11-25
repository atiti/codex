//! A reviewer may decline or fail; neither outcome approves completion.
#![allow(dead_code)]

#[path = "../src/completion_review.rs"]
mod completion_review;

use pretty_assertions::assert_eq;

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
