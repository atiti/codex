//! Untrusted evidence for an isolated directional review, bounded including markers.
use super::ContextualUserFragment;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::input_text;

pub(crate) struct ProgressReviewEvidence(String);
impl ProgressReviewEvidence {
    pub(crate) fn new(packet: String) -> Option<Self> {
        let escaped = packet
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let evidence = Self(escaped);
        let rendered = evidence.render().into_text()?;
        (rendered.len() <= 48_000 && codex_utils_string::approx_token_count(&rendered) <= 9_000)
            .then_some(evidence)
    }
}
impl ContextualUserFragment for ProgressReviewEvidence {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("goal.progress_review_evidence".into())
    }
    fn role(&self) -> &'static str {
        "user"
    }
    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }
    fn type_markers() -> (&'static str, &'static str) {
        ("<progress_review_evidence>", "</progress_review_evidence>")
    }
    fn body(&self) -> ContentItem {
        input_text(self.0.clone())
    }
}
#[cfg(test)]
#[path = "progress_review_evidence_tests.rs"]
mod tests;
