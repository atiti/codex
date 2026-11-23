//! Bounded evidence passed to the independent Goal completion reviewer.

use serde::Deserialize;

const MAX_REASON_BYTES: usize = 1_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewDecision {
    decision: String,
    reason: String,
}

pub(crate) struct Verdict {
    decision: &'static str,
    reason: String,
}

impl Verdict {
    pub(crate) fn decision(&self) -> &'static str {
        self.decision
    }
    pub(crate) fn reason(&self) -> &str {
        &self.reason
    }
}

pub(crate) fn parse_review(result: Result<String, String>) -> Verdict {
    let Ok(raw) = result else {
        return Verdict {
            decision: "continue",
            reason: "Independent review unavailable; completion is unverified".to_string(),
        };
    };
    let parsed: Result<ReviewDecision, _> = serde_json::from_str(raw.trim());
    match parsed {
        Ok(review) if matches!(review.decision.as_str(), "complete" | "continue" | "blocked")
            && !review.reason.trim().is_empty() && review.reason.len() <= MAX_REASON_BYTES =>
            Verdict { decision: match review.decision.as_str() { "complete" => "complete", "blocked" => "blocked", _ => "continue" }, reason: review.reason },
        _ => Verdict { decision: "continue", reason: "Independent review returned an invalid or unbounded decision; completion is unverified".to_string() },
    }
}
