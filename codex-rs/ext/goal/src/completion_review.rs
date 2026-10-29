//! Bounded evidence passed to the independent Goal completion reviewer.

use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_state::ThreadGoal;
use serde::Deserialize;

const MAX_HISTORY_BYTES: usize = 8_000;
const MAX_HISTORY_MESSAGES: usize = 24;
const MAX_REASON_BYTES: usize = 1_000;

pub(crate) fn review_prompt(goal: &ThreadGoal, history: &[ResponseItem]) -> String {
    let mut evidence = Vec::new();
    let mut remaining = MAX_HISTORY_BYTES;
    for item in history.iter().rev() {
        let ResponseItem::Message { role, content, .. } = item else {
            continue;
        };
        if role != "assistant" && role != "user" {
            continue;
        }
        let text = content
            .iter()
            .filter_map(|part| match part {
                ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            continue;
        }
        let mut start = text.len().saturating_sub(remaining);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        let selected = &text[start..];
        remaining = remaining.saturating_sub(selected.len());
        evidence.push(format!("{role}: {selected}"));
        if remaining == 0 || evidence.len() == MAX_HISTORY_MESSAGES {
            break;
        }
    }
    evidence.reverse();
    let mut objective_end = goal.objective.len().min(1_000);
    while !goal.objective.is_char_boundary(objective_end) {
        objective_end -= 1;
    }
    format!(
        "Goal ID: {}\nObjective: {}\n\nRecent conversation (claims require verification):\n{}\n\nInspect authoritative state using read-only tools when possible. Reply as strict JSON: {{\"decision\":\"complete|continue|blocked\",\"reason\":\"specific evidence or gap\"}}. Do not approve completion unless every requested deliverable is verified.",
        goal.goal_id,
        &goal.objective[..objective_end],
        evidence.join("\n\n")
    )
}

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
