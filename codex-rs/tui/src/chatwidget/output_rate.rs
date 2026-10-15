use std::time::Duration;
use std::time::Instant;

/// Estimates visible output throughput from streamed Unicode characters.
/// Provider token boundaries are unavailable while text is streaming, so the
/// display is deliberately marked approximate (four characters per token).
#[derive(Debug, Default)]
pub(super) struct OutputRate {
    started_at: Option<Instant>,
    last_refresh_at: Option<Instant>,
    output_bytes: usize,
}

impl OutputRate {
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Record visible text and return true when the status line should redraw.
    pub(super) fn observe(&mut self, text: &str, now: Instant) -> bool {
        if text.is_empty() {
            return false;
        }
        let started_at = *self.started_at.get_or_insert(now);
        self.output_bytes = self.output_bytes.saturating_add(text.len());
        if self
            .last_refresh_at
            .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_millis(500))
        {
            self.last_refresh_at = Some(now);
            return now.saturating_duration_since(started_at) >= Duration::from_millis(250);
        }
        false
    }

    pub(super) fn display(&self, now: Instant) -> Option<String> {
        let elapsed = now.saturating_duration_since(self.started_at?);
        if elapsed < Duration::from_millis(250) {
            return None;
        }
        let estimated_tokens = self.output_bytes / 4;
        let tokens_per_second = (estimated_tokens as f64 / elapsed.as_secs_f64()).round() as u64;
        Some(format!("~{tokens_per_second} out tok/s"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_streamed_output_rate_and_throttles_refreshes() {
        let start = Instant::now();
        let mut rate = OutputRate::default();
        assert!(!rate.observe("", start));
        assert!(!rate.observe("abcdefgh", start));
        assert_eq!(rate.display(start + Duration::from_millis(200)), None);
        assert!(rate.observe("ijklmnop", start + Duration::from_millis(500)));
        assert_eq!(
            rate.display(start + Duration::from_millis(500)).as_deref(),
            Some("~8 out tok/s")
        );
        assert!(!rate.observe("qrst", start + Duration::from_millis(700)));
        assert!(rate.observe("uvwx", start + Duration::from_secs(1)));
        assert_eq!(
            rate.display(start + Duration::from_secs(1)).as_deref(),
            Some("~6 out tok/s")
        );
        rate.reset();
        assert_eq!(rate.display(start + Duration::from_secs(2)), None);
    }
}
