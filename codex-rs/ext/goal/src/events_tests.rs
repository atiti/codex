use super::GoalEventEmitter;
use codex_extension_api::ExtensionEventSink;
use codex_extension_api::ExtensionWarning;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Default)]
struct RecordingEventSink(Mutex<Vec<Event>>);

impl ExtensionEventSink for RecordingEventSink {
    fn emit(&self, event: Event) {
        self.0
            .lock()
            .expect("event mutex should not be poisoned")
            .push(event);
    }

    fn emit_warning(&self, _warning: ExtensionWarning) {
        panic!("unexpected extension warning");
    }
}

#[test]
fn user_notice_emits_visible_commentary() {
    let sink = Arc::new(RecordingEventSink::default());
    let emitter = GoalEventEmitter::new(sink.clone());

    emitter.user_notice("goal-context-too-large", "Shorten the goal to resume it.");

    let events = sink.0.lock().expect("event mutex should not be poisoned");
    assert!(matches!(
        &events[0].msg,
        EventMsg::AgentMessage(message)
            if message.message == "Shorten the goal to resume it."
                && message.phase == Some(MessagePhase::Commentary)
    ));
}
