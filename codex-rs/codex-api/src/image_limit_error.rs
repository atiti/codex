//! Recognize only the provider's explicit image-count rejection, not generic HTTP 400s.

use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use serde_json::Value;

pub(super) fn provider_image_limit_error(body: &str) -> Option<CodexErr> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    let error = parsed.get("error")?;
    if error.get("type")?.as_str()? != "invalid_request_error"
        || error.get("param")?.as_str()? != "input"
    {
        return None;
    }
    let limit = error
        .get("message")?
        .as_str()?
        .strip_prefix("Exceeded maximum number of images (")?
        .strip_suffix(") allowed in the request.")?
        .parse::<usize>()
        .ok()?;
    if limit == 0 || limit > 10_000 {
        return None;
    }
    Some(
        CodexErrorDetails::ImageLimitExceeded {
            // The body supplies the cap, not the exact count. The failed request
            // was at least one over it; local preflight retains the exact count.
            count: limit + 1,
            limit,
        }
        .into(),
    )
}
