//! Bounded and untrusted evidence for independent progress review.
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde::Deserialize;
const EVIDENCE_LIMIT: usize = 16_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Verdict {
    pub(crate) decision: String,
    pub(crate) reason: String,
}

pub(crate) fn parse_verdict(raw: &str) -> Option<Verdict> {
    let verdict: Verdict = serde_json::from_str(raw.trim()).ok()?;
    if matches!(verdict.decision.as_str(), "continue" | "adjust" | "replan")
        && !verdict.reason.trim().is_empty()
        && verdict.reason.len() <= 1000
    {
        Some(verdict)
    } else {
        None
    }
}

pub(crate) fn user_text(item: &ResponseItem) -> Option<String> {
    match item {
        ResponseItem::Message { role, content, .. } if role == "user" => Some(
            content
                .iter()
                .filter_map(|part| match part {
                    ContentItem::InputText { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

pub(crate) fn evidence_packet(
    items: Vec<&ResponseItem>,
    prior: Option<&str>,
    turn_request: Option<&str>,
    goal: Option<&str>,
) -> String {
    let user_messages: Vec<String> = items
        .iter()
        .filter_map(|item| user_text(item))
        .filter(|text| !text.is_empty())
        .collect();
    let Some(user) = user_messages.last() else {
        return String::new();
    };
    if user.is_empty() {
        return String::new();
    }
    let mut selected = Vec::new();
    let mut remaining = EVIDENCE_LIMIT;
    for item in items.iter().rev() {
        let text = match item {
            ResponseItem::Message { role, content, .. }
                if role == "user" || role == "assistant" =>
            {
                let text = content
                    .iter()
                    .filter_map(|part| match part {
                        ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                            Some(clipped(text, 2000))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("{role}: {text}")
            }
            ResponseItem::FunctionCall {
                name, arguments, ..
            } => format!("tool {name}: {}", clipped(arguments, 2000)),
            ResponseItem::CustomToolCall { name, input, .. } => {
                format!("tool {name}: {}", clipped(input, 2000))
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                format!(
                    "tool result: {}",
                    clipped(
                        output.text_content().unwrap_or("[non-text output omitted]"),
                        2000
                    )
                )
            }
            _ => continue,
        };
        let mut start = text.len().saturating_sub(remaining);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        selected.push(text[start..].to_string());
        remaining = remaining.saturating_sub(text.len() - start);
        if remaining == 0 || selected.len() == 40 {
            break;
        }
    }
    selected.reverse();
    let mut user_end = user.len().min(4000);
    while !user.is_char_boundary(user_end) {
        user_end -= 1;
    }
    let original = user_messages.first().map(String::as_str).unwrap_or("");
    let mut original_end = original.len().min(4000);
    while !original.is_char_boundary(original_end) {
        original_end -= 1;
    }
    format!(
        "First retained user message (may precede the current task): {}\nCurrent turn request (unavailable when provenance is missing): {}\nActive or stored goal (if any): {}\nLatest user request/correction: {}\nPrevious review: {}\nRecent trajectory (bounded; older content may be omitted):\n{}\nAssess whether the next work advances the user's complete request. Identify neglected requirements and unnecessary detours. Return JSON with decision continue, adjust, or replan and a concise evidence-based reason covering observed progress, what remains, and the next useful step when supported by the evidence. Explicitly state uncertainty where evidence is missing.",
        &original[..original_end],
        clipped(turn_request.unwrap_or("unavailable"), 4000),
        clipped(goal.unwrap_or("none"), 4000),
        &user[..user_end],
        clipped(prior.unwrap_or("unavailable"), 1200),
        selected.join("\n")
    )
}

fn clipped(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
