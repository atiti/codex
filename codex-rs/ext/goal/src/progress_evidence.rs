//! Bounded and untrusted evidence for independent progress review.
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde::Deserialize;
const EVIDENCE_LIMIT: usize = 16_000;

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Verdict {
    pub(crate) decision: String,
    pub(crate) reason: String,
    #[serde(default)]
    pub(crate) model_tier: Option<String>,
}

pub(crate) fn parse_verdict(raw: &str) -> Result<Verdict, &'static str> {
    // Bound parsing work and never search arbitrary prose for an actionable object.
    if raw.len() > 16_000 {
        return Err("response exceeds 16000 bytes");
    }
    let raw = raw.trim();
    // Some routed models wrap an otherwise valid object in a Markdown code block.
    // Accept only a single block enclosing the entire response, with no surrounding prose.
    let json = if let Some(block) = raw.strip_prefix("```") {
        let (language, body) = block.split_once('\n').ok_or("invalid JSON code block")?;
        if !matches!(language.trim(), "" | "json") {
            return Err("invalid JSON code block");
        }
        body.strip_suffix("```")
            .ok_or("invalid JSON code block")?
            .trim()
    } else {
        raw
    };
    let verdict: Verdict =
        serde_json::from_str(json).map_err(|_| "invalid JSON or verdict fields")?;
    if !matches!(verdict.decision.as_str(), "continue" | "adjust" | "replan") {
        return Err("unknown decision");
    }
    if verdict
        .model_tier
        .as_deref()
        .is_some_and(|tier| !matches!(tier, "keep" | "fast" | "normal" | "smart" | "max"))
    {
        return Err("unknown model tier");
    }
    if verdict.reason.trim().is_empty() {
        return Err("empty reason");
    }
    if verdict.reason.len() > 1000 {
        return Err("reason exceeds 1000 UTF-8 bytes");
    }
    Ok(verdict)
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
                            Some(clipped(text, /*limit*/ 2000))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("{role}: {text}")
            }
            ResponseItem::FunctionCall {
                name, arguments, ..
            } => format!("tool {name}: {}", clipped(arguments, /*limit*/ 2000)),
            ResponseItem::CustomToolCall { name, input, .. } => {
                format!("tool {name}: {}", clipped(input, /*limit*/ 2000))
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                format!(
                    "tool result: {}",
                    clipped(
                        output.text_content().unwrap_or("[non-text output omitted]"),
                        /*limit*/ 2000
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
        "First retained user message (may precede the current task): {}\nCurrent turn request (unavailable when provenance is missing): {}\nActive or stored goal (if any): {}\nLatest user request/correction: {}\nPrevious review: {}\nRecent trajectory (bounded; older content may be omitted):\n{}\nAssess whether the next work advances the user's complete request. Identify neglected requirements and unnecessary detours. Return JSON with decision continue, adjust, or replan and a nonempty evidence-based reason of at most 1000 UTF-8 bytes covering observed progress, what remains, and the next useful step when supported by the evidence. Explicitly state uncertainty where evidence is missing. Also return model_tier: keep, fast, normal, smart, or max. Recommend a change only when evidence indicates model capability is limiting progress, or the remaining work is routine and verified. A tool outage, missing permission, slow operation, or unclear requirements alone does not justify escalation. Prefer keep when uncertain.",
        &original[..original_end],
        clipped(turn_request.unwrap_or("unavailable"), /*limit*/ 4000),
        clipped(goal.unwrap_or("none"), /*limit*/ 4000),
        &user[..user_end],
        clipped(prior.unwrap_or("unavailable"), /*limit*/ 1200),
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
