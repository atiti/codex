use std::sync::Arc;

use codex_extension_api::ExtensionEventSink;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadGoal;
use codex_protocol::protocol::ThreadGoalUpdatedEvent;

#[derive(Clone)]
pub(crate) struct GoalEventEmitter {
    sink: Arc<dyn ExtensionEventSink>,
}

impl GoalEventEmitter {
    pub(crate) fn new(sink: Arc<dyn ExtensionEventSink>) -> Self {
        Self { sink }
    }

    pub(crate) fn thread_goal_updated(
        &self,
        event_id: impl Into<String>,
        turn_id: Option<String>,
        goal: ThreadGoal,
    ) {
        self.sink.emit(Event {
            id: event_id.into(),
            msg: EventMsg::ThreadGoalUpdated(ThreadGoalUpdatedEvent {
                thread_id: goal.thread_id,
                turn_id,
                goal,
            }),
        });
    }

    pub(crate) fn user_notice(&self, event_id: impl Into<String>, message: impl Into<String>) {
        self.sink.emit(Event {
            id: event_id.into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: message.into(),
                phase: Some(MessagePhase::Commentary),
                memory_citation: None,
                delivery: None,
                questions: None,
            }),
        });
    }
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
