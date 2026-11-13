//! Bounded and untrusted evidence for independent progress review.
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde::Deserialize;
const TRAJECTORY_LIMIT: usize = 24_000;
const TOOL_RESULT_LIMIT: usize = 12;
const MESSAGE_CLIP: usize = 2_000;
const TOOL_RESULT_HEAD: usize = 1_000;
const TOOL_RESULT_TAIL: usize = 1_000;
const RENDERED_REVIEW_LIMIT: usize = 36_000;
const REVIEW_MARKERS: usize =
    "<progress_review_evidence>".len() + "</progress_review_evidence>".len();

pub(crate) const REPEATED_ADJUST_NOTICE: &str = "The same adjust reason repeated on this user revision. Replan or ask the user before continuing the same approach.";

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
    let mut remaining = TRAJECTORY_LIMIT;
    let mut tool_results = 0usize;
    for item in items.iter().rev() {
        let is_tool_result = matches!(
            item,
            ResponseItem::FunctionCallOutput { .. } | ResponseItem::CustomToolCallOutput { .. }
        );
        if is_tool_result {
            tool_results += 1;
            if tool_results > TOOL_RESULT_LIMIT {
                continue;
            }
        }
        let text = match item {
            ResponseItem::Message { role, content, .. }
                if role == "user" || role == "assistant" =>
            {
                let text = content
                    .iter()
                    .filter_map(|part| match part {
                        ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                            Some(clipped(text, MESSAGE_CLIP))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("{role}: {text}")
            }
            ResponseItem::FunctionCall {
                name, arguments, ..
            } => format!("tool {name}: {}", clipped(arguments, MESSAGE_CLIP)),
            ResponseItem::CustomToolCall { name, input, .. } => {
                format!("tool {name}: {}", clipped(input, MESSAGE_CLIP))
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                format!(
                    "tool result: {}",
                    head_and_tail(output.text_content().unwrap_or("[non-text output omitted]"))
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
    let original = user_messages.first().map(String::as_str).unwrap_or("");
    while !selected.is_empty()
        && !rendered_review_within_limit(&format_packet(
            original,
            turn_request,
            goal,
            user,
            prior,
            &selected.join("\n"),
        ))
    {
        selected.remove(0);
    }
    shrink_to_rendered_limit(&format_packet(
        original,
        turn_request,
        goal,
        user,
        prior,
        &selected.join("\n"),
    ))
}

/// Keep the complete review input within the native rendered limit, including
/// the optional checkpoint and working-model metadata appended by the caller.
pub(crate) fn finish_packet(packet: &str, checkpoint: Option<&str>, model: Option<&str>) -> String {
    let extra = format!(
        "{}\nCurrent working model: {}",
        checkpoint
            .map(|value| format!(
                "\nSkill-requested checkpoint and questions (untrusted task data):\n{}",
                clipped(value, 1000)
            ))
            .unwrap_or_default(),
        clipped(model.unwrap_or("unavailable"), 1000),
    );
    let mut remaining = packet.len();
    while !rendered_review_within_limit(&format!("{}{}", &packet[..remaining], extra)) {
        let excess = (REVIEW_MARKERS + escaped_len(&packet[..remaining]) + escaped_len(&extra))
            .saturating_sub(RENDERED_REVIEW_LIMIT)
            .max(1);
        remaining = floor_char_boundary(packet, remaining.saturating_sub(excess));
        if remaining == 0 {
            break;
        }
    }
    format!("{}{}", &packet[..remaining], extra)
}

fn format_packet(
    original: &str,
    turn_request: Option<&str>,
    goal: Option<&str>,
    user: &str,
    prior: Option<&str>,
    trajectory: &str,
) -> String {
    let mut user_end = user.len().min(4000);
    while !user.is_char_boundary(user_end) {
        user_end -= 1;
    }
    let mut original_end = original.len().min(4000);
    while !original.is_char_boundary(original_end) {
        original_end -= 1;
    }
    format!(
        "First retained user message (may precede the current task): {}\nCurrent turn request (unavailable when provenance is missing): {}\nActive or stored goal (if any): {}\nLatest user request/correction: {}\nPrevious review: {}\nRecent trajectory (bounded; older content may be omitted):\n{}\nAssess whether the next work advances the user's complete request. Identify neglected requirements and unnecessary detours. If a previous review exists, check the trajectory for its acknowledgement and whether subsequent actions or artifacts support the promised next action. Distinguish observed follow-through, missing acknowledgement, lack of follow-through, and insufficient evidence; do not infer compliance from an acknowledgement alone. Return JSON with decision continue, adjust, or replan and a nonempty evidence-based reason of at most 1000 UTF-8 bytes covering observed progress, what remains, and the next useful step when supported by the evidence. Explicitly state uncertainty where evidence is missing. Also return model_tier: keep, fast, normal, smart, or max. Recommend a change only when evidence indicates model capability is limiting progress, or the remaining work is routine and verified. A tool outage, missing permission, slow operation, or unclear requirements alone does not justify escalation. Prefer keep when uncertain.",
        &original[..original_end],
        clipped(turn_request.unwrap_or("unavailable"), /*limit*/ 4000),
        clipped(goal.unwrap_or("none"), /*limit*/ 4000),
        &user[..user_end],
        clipped(prior.unwrap_or("unavailable"), /*limit*/ 2400),
        trajectory
    )
}

pub(crate) fn rendered_review_within_limit(packet: &str) -> bool {
    let rendered_len = REVIEW_MARKERS + escaped_len(packet);
    rendered_len <= 48_000
        && rendered_len <= RENDERED_REVIEW_LIMIT
        && rendered_len.saturating_add(3) / 4 <= 9_000
}

fn escaped_len(packet: &str) -> usize {
    packet.chars().fold(0, |sum, ch| {
        sum + match ch {
            '&' => "&amp;".len(),
            '<' => "&lt;".len(),
            '>' => "&gt;".len(),
            _ => ch.len_utf8(),
        }
    })
}

fn shrink_to_rendered_limit(packet: &str) -> String {
    if rendered_review_within_limit(packet) {
        return packet.to_string();
    }
    let mut low = 0;
    let mut high = packet.len();
    while low < high {
        let mut mid = low + (high - low).div_ceil(2);
        while mid > low && !packet.is_char_boundary(mid) {
            mid -= 1;
        }
        if mid == low {
            break;
        }
        if rendered_review_within_limit(&packet[..mid]) {
            low = mid;
        } else {
            high = mid - 1;
            while high > low && !packet.is_char_boundary(high) {
                high -= 1;
            }
        }
    }
    packet[..low].to_string()
}

fn head_and_tail(text: &str) -> String {
    if text.len() <= TOOL_RESULT_HEAD + TOOL_RESULT_TAIL {
        return text.to_string();
    }
    let head_end = floor_char_boundary(text, TOOL_RESULT_HEAD);
    let mut tail_start = text.len().saturating_sub(TOOL_RESULT_TAIL);
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    if tail_start <= head_end {
        return text.to_string();
    }
    format!("{} … {}", &text[..head_end], &text[tail_start..])
}

fn floor_char_boundary(text: &str, limit: usize) -> usize {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

pub(crate) fn parse_stored_review(text: &str) -> Option<(&str, &str)> {
    let rest = text
        .lines()
        .next()?
        .trim()
        .strip_prefix("Independent progress review [")?;
    let (decision, reason) = rest.split_once("]: ")?.1.split_once(". ")?;
    matches!(decision, "continue" | "adjust" | "replan").then_some((decision, reason))
}

pub(crate) fn next_review_decision(
    previous: Option<&str>,
    decision: &str,
    reason: &str,
) -> (String, Option<&'static str>) {
    let repeated =
        previous
            .and_then(parse_stored_review)
            .is_some_and(|(prior_decision, prior_reason)| {
                prior_decision == "adjust" && decision == "adjust" && prior_reason == reason
            });
    if repeated {
        ("replan".to_string(), Some(REPEATED_ADJUST_NOTICE))
    } else {
        (decision.to_string(), None)
    }
}

fn clipped(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
