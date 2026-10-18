//! Bounded replay-buffer policy for per-thread app-server events.

use super::ServerNotification;
use super::ThreadBufferedEvent;
use super::ThreadEventStore;
use std::borrow::Cow;

// Keep merged text finite so continued streaming still reaches bounded replay eviction.
const MAX_COALESCED_AGENT_MESSAGE_DELTA_BYTES: usize = 4 * 1024;
const MAX_BUFFERED_AGENT_MESSAGE_DELTA_BYTES: usize = 256 * 1024;
const MAX_BUFFERED_EVENT_BYTES: usize = 8 * 1024 * 1024;

/// Count serialized payload bytes without allocating another copy of a tool result.
fn event_bytes(event: &ThreadBufferedEvent) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 > MAX_BUFFERED_EVENT_BYTES {
                return Err(std::io::Error::other("replay byte budget exceeded"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(std::mem::size_of::<ThreadBufferedEvent>());
    let result = match event {
        ThreadBufferedEvent::Notification(value) => serde_json::to_writer(&mut counter, value),
        ThreadBufferedEvent::Request(value) => serde_json::to_writer(&mut counter, value),
        ThreadBufferedEvent::HistoryEntryResponse(value) => {
            match value {
                crate::app_event::HistoryLookupResponse::Entry { entry, .. } => {
                    counter.0 = counter
                        .0
                        .saturating_add(entry.as_ref().map_or(0, String::len));
                }
                crate::app_event::HistoryLookupResponse::Batch { entries, .. } => {
                    for entry in entries {
                        counter.0 = counter.0.saturating_add(std::mem::size_of_val(entry));
                        counter.0 = counter
                            .0
                            .saturating_add(entry.entry.as_ref().map_or(0, String::len));
                    }
                }
                crate::app_event::HistoryLookupResponse::BatchError { .. } => {}
            }
            Ok(())
        }
        ThreadBufferedEvent::FeedbackSubmission(value) => {
            counter.0 = counter
                .0
                .saturating_add(value.result.as_ref().map_or_else(String::len, String::len));
            Ok(())
        }
    };
    if result.is_err() {
        MAX_BUFFERED_EVENT_BYTES + 1
    } else {
        counter.0
    }
}

impl ThreadEventStore {
    pub(super) fn push_replay_notification(&mut self, notification: Cow<'_, ServerNotification>) {
        if let ServerNotification::AgentMessageDelta(delta) = notification.as_ref()
            && delta.delta.len() > MAX_BUFFERED_AGENT_MESSAGE_DELTA_BYTES
        {
            return;
        }

        if let ServerNotification::AgentMessageDelta(delta) = notification.as_ref()
            && let Some(ThreadBufferedEvent::Notification(previous)) = self.buffer.back_mut()
            && let ServerNotification::AgentMessageDelta(previous) = previous.as_mut()
            && previous.thread_id == delta.thread_id
            && previous.turn_id == delta.turn_id
            && previous.item_id == delta.item_id
            && previous.delta.len().saturating_add(delta.delta.len())
                <= MAX_COALESCED_AGENT_MESSAGE_DELTA_BYTES
        {
            previous.delta.push_str(&delta.delta);
            if let Some(bytes) = self.buffered_event_sizes.back_mut() {
                // JSON escaping can use six bytes for one input byte.
                let added = delta.delta.len().saturating_mul(6);
                *bytes = bytes.saturating_add(added);
                self.buffered_event_bytes = self.buffered_event_bytes.saturating_add(added);
            }
            self.buffered_agent_message_delta_bytes = self
                .buffered_agent_message_delta_bytes
                .saturating_add(delta.delta.len());
            self.evict_overflowing_events();
            return;
        }

        self.push_buffered_event(ThreadBufferedEvent::Notification(Box::new(
            notification.into_owned(),
        )));
    }

    pub(super) fn push_buffered_event(&mut self, event: ThreadBufferedEvent) {
        if let ThreadBufferedEvent::Notification(notification) = &event
            && let ServerNotification::AgentMessageDelta(delta) = notification.as_ref()
        {
            self.buffered_agent_message_delta_bytes = self
                .buffered_agent_message_delta_bytes
                .saturating_add(delta.delta.len());
        }
        let bytes = event_bytes(&event);
        self.buffered_event_bytes = self.buffered_event_bytes.saturating_add(bytes);
        self.buffered_event_sizes.push_back(bytes);
        self.buffer.push_back(event);
        self.evict_overflowing_events();
    }

    pub(super) fn recount_buffer_bytes(&mut self) {
        self.buffered_event_sizes = self.buffer.iter().map(event_bytes).collect();
        self.buffered_event_bytes = self.buffered_event_sizes.iter().sum();
        self.evict_overflowing_events();
    }

    fn evict_overflowing_events(&mut self) {
        while self.buffer.len() > self.capacity
            || self.buffered_event_bytes > MAX_BUFFERED_EVENT_BYTES
            || self.buffered_agent_message_delta_bytes > MAX_BUFFERED_AGENT_MESSAGE_DELTA_BYTES
        {
            let Some(removed) = self.buffer.pop_front() else {
                break;
            };
            self.buffered_event_bytes = self
                .buffered_event_bytes
                .saturating_sub(self.buffered_event_sizes.pop_front().unwrap_or_default());
            match removed {
                ThreadBufferedEvent::Notification(notification) => {
                    if let ServerNotification::AgentMessageDelta(delta) = notification.as_ref() {
                        self.buffered_agent_message_delta_bytes = self
                            .buffered_agent_message_delta_bytes
                            .saturating_sub(delta.delta.len());
                    }
                }
                ThreadBufferedEvent::Request(request) => self
                    .pending_interactive_replay
                    .note_evicted_server_request(request.as_ref()),
                ThreadBufferedEvent::HistoryEntryResponse(_)
                | ThreadBufferedEvent::FeedbackSubmission(_) => {}
            }
        }
    }
}

#[cfg(test)]
#[path = "thread_event_buffer_tests.rs"]
mod tests;
