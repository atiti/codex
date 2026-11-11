//! Safe-boundary supervision; inbox data never supplies execution authority.
use super::GoalRuntimeHandle;
use super::admission::Cursor;
use super::admission::cursor;
use super::admission::save_cursor;
use crate::supervisor_inbox;
use codex_core::StartIfIdleSubmission;
use codex_core::TurnInput;
use codex_core::TurnInputRequest;
use codex_core::TurnStartOptions;
use codex_core::context::ContextualUserFragment;
use codex_core::context::InternalContextSource;
use codex_core::context::InternalModelContextFragment;
use std::sync::Arc;
use std::time::Duration;

impl GoalRuntimeHandle {
    pub(crate) async fn supervisor_user_resumed(&self) {
        self.inner
            .supervisor_receipt_failed
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let home = self.supervisor_home().await;
        if supervisor_inbox::load(&home, &self.thread_id().to_string())
            .ok()
            .flatten()
            .is_some()
            && let Ok(mut value) = cursor(&home, &self.thread_id().to_string())
            && value.suspended
        {
            // Settle the cancelled admission before applying the user's resume.
            // Otherwise its durable TurnAborted receipt would suspend us again.
            if value.reservation.is_some() || value.in_flight {
                let Some(manager) = self.inner.thread_manager.upgrade() else {
                    return;
                };
                let Ok(thread) = manager.get_thread(self.thread_id()).await else {
                    return;
                };
                if super::admission::reconcile(&thread, &mut value)
                    .await
                    .is_err()
                    || value.reservation.is_some()
                    || value.in_flight
                {
                    return;
                }
            }
            value.suspended = false;
            if let Err(error) = save_cursor(&home, &self.thread_id().to_string(), &value) {
                tracing::warn!(%error, "failed to clear supervisor suspension");
            }
        }
    }

    pub(crate) async fn finish_supervisor_turn(&self, turn_id: &str) {
        let Ok(_permit) = self.goal_state_permit().await else {
            return;
        };
        let home = self.supervisor_home().await;
        if let Ok(mut value) = cursor(&home, &self.thread_id().to_string())
            && value.in_flight
            && value.admitted_turn_id.as_deref() == Some(turn_id)
        {
            value.in_flight = false;
            if let Err(error) = save_cursor(&home, &self.thread_id().to_string(), &value) {
                tracing::warn!(%error, "failed to finish supervisor receipt");
            }
        }
    }

    pub(crate) async fn suspend_supervisor(&self) -> bool {
        let Ok(_permit) = self.goal_state_permit().await else {
            return true;
        };
        let home = self.supervisor_home().await;
        if supervisor_inbox::load(&home, &self.thread_id().to_string())
            .ok()
            .flatten()
            .is_some()
        {
            let mut value = cursor(&home, &self.thread_id().to_string()).unwrap_or_default();
            value.suspended = true;
            if let Err(error) = save_cursor(&home, &self.thread_id().to_string(), &value) {
                tracing::warn!(%error, "failed to persist supervisor suspension");
            }
            return true;
        }
        false
    }

    pub(crate) async fn start_supervisor_poll(&self) {
        if !self.tools_available() {
            return;
        }
        let Ok(mut task) = self.inner.supervisor_poll.lock() else {
            return;
        };
        if task.is_some() {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        *task = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let runtime = GoalRuntimeHandle { inner };
                if let Err(error) = runtime.continue_supervisor_if_idle().await {
                    tracing::warn!(%error, "supervisor inbox continuation deferred");
                }
            }
        }));
    }

    pub(crate) fn stop_supervisor_poll(&self) {
        if let Ok(mut task) = self.inner.supervisor_poll.lock()
            && let Some(task) = task.take()
        {
            task.abort();
        }
    }

    // true means supervisor waiting/coordination owns this boundary; ordinary goal continuation must wait.
    pub(crate) async fn continue_supervisor_if_idle(&self) -> Result<bool, String> {
        if !self.tools_available() {
            return Ok(false);
        }
        let _permit = self.goal_state_permit().await?;
        let home = self.supervisor_home().await;
        let loaded =
            supervisor_inbox::load(&home, &self.thread_id().to_string()).and_then(|inbox| {
                if inbox.is_none() && self.supervisor_registered(&home) {
                    Err("Registered supervisor inbox is missing; repair its projection.".into())
                } else {
                    Ok(inbox)
                }
            });
        let inbox = match loaded {
            Ok(Some(inbox)) => inbox,
            Ok(None) => return Ok(false),
            Err(error) => {
                if !self
                    .inner
                    .supervisor_notice_sent
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    self.inner.event_emitter.supervisor_notice(
                        self.thread_id(),
                        format!(
                            "Supervisor inbox needs repair before automatic continuation: {error}"
                        ),
                    );
                }
                return Ok(true);
            }
        };
        let Some(manager) = self.inner.thread_manager.upgrade() else {
            return Ok(true);
        };
        let Ok(thread) = manager.get_thread(self.thread_id()).await else {
            return Ok(true);
        };
        // Discovery alone never registers ordinary threads. Only a validated bound
        // snapshot establishes enrollment, and its marker precedes any admission.
        self.inner
            .supervisor_enrolled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if !home
            .join("supervisor-inbox-cursors")
            .join(format!("{}.json", self.thread_id()))
            .exists()
        {
            let initial = Cursor {
                suspended: thread.agent_status().await
                    == codex_protocol::protocol::AgentStatus::Interrupted,
                ..Cursor::default()
            };
            save_cursor(&home, &self.thread_id().to_string(), &initial)?;
        }
        let mut value = match cursor(&home, &self.thread_id().to_string()) {
            Ok(value) => value,
            Err(error) => {
                if !self
                    .inner
                    .supervisor_notice_sent
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    self.inner.event_emitter.supervisor_notice(self.thread_id(), format!("Supervisor delivery receipt needs repair before automatic continuation: {error}"));
                }
                return Ok(true);
            }
        };
        match super::resolution::consume(&home, self.thread_id(), &thread, &mut value).await {
            Ok(true) => {
                save_cursor(&home, &self.thread_id().to_string(), &value)?;
                self.inner
                    .supervisor_receipt_failed
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(false) => {}
            Err(error) => {
                if !self
                    .inner
                    .supervisor_notice_sent
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    self.inner.event_emitter.supervisor_notice(
                        self.thread_id(),
                        format!("Operator admission resolution could not be accepted: {error}"),
                    );
                }
                return Ok(true);
            }
        }
        if self
            .inner
            .supervisor_receipt_failed
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(true);
        }
        let goal = self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.thread_id())
            .await
            .map_err(|e| e.to_string())?;
        if value.suspended
            || goal.as_ref().is_some_and(|g| {
                matches!(
                    g.status,
                    codex_state::ThreadGoalStatus::Paused
                        | codex_state::ThreadGoalStatus::BudgetLimited
                        | codex_state::ThreadGoalStatus::UsageLimited
                        | codex_state::ThreadGoalStatus::Complete
                )
            })
            || self
                .inner
                .state_dbs
                .thread_goals()
                .has_thread_goal_continuation_deferral(self.thread_id())
                .await
                .map_err(|e| e.to_string())?
        {
            return Ok(true);
        }
        if value.reservation.is_some() || value.in_flight {
            if let Err(error) = super::admission::reconcile(&thread, &mut value).await {
                self.inner
                    .supervisor_receipt_failed
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                self.inner
                    .event_emitter
                    .supervisor_notice(self.thread_id(), error);
                return Ok(true);
            }
            save_cursor(&home, &self.thread_id().to_string(), &value)?;
            if value.in_flight || value.suspended {
                return Ok(true);
            }
        }
        if !inbox.actionable() {
            self.inner
                .supervisor_notice_sent
                .store(false, std::sync::atomic::Ordering::Relaxed);
            if value.attempts != 0 || value.action_key != 0 {
                value.attempts = 0;
                value.action_key = 0;
                save_cursor(&home, &self.thread_id().to_string(), &value)?;
            }
            return Ok(inbox.quiet());
        }
        let key = inbox.action_key();
        if key != value.action_key {
            if let Some(goal) = goal
                .as_ref()
                .filter(|goal| goal.status == codex_state::ThreadGoalStatus::Blocked)
                && inbox.resumes_goal()
            {
                if let Some(resumed) = self
                    .inner
                    .state_dbs
                    .thread_goals()
                    .update_thread_goal(
                        self.thread_id(),
                        codex_state::GoalUpdate {
                            objective: None,
                            status: Some(codex_state::ThreadGoalStatus::Active),
                            token_budget: None,
                            expected_goal_id: Some(goal.goal_id.clone()),
                        },
                    )
                    .await
                    .map_err(|e| e.to_string())?
                {
                    self.inner.accounting_state.reset_empty_responses();
                    self.inner
                        .accounting_state
                        .mark_idle_goal_active(resumed.goal_id.clone());
                    self.inner.metrics.record_resumed();
                    self.inner.analytics.status_changed(
                        &resumed,
                        Some(codex_state::ThreadGoalStatus::Blocked),
                        crate::analytics::GoalEventAttribution::NoTurn,
                    );
                    self.inner.event_emitter.thread_goal_updated(
                        format!("{}:supervisor-resume:{key}", self.thread_id()),
                        /*turn_id*/ None,
                        crate::tool::protocol_goal_from_state(resumed),
                    );
                }
            } else if goal
                .as_ref()
                .is_some_and(|g| g.status == codex_state::ThreadGoalStatus::Blocked)
            {
                self.inner.event_emitter.supervisor_notice(self.thread_id(),"New coordination will be handled while the original goal remains blocked. Only a bound wait resolution, canonical ready step, or explicit user goal resume reactivates that goal; existing approvals and operation holds remain.");
            }
            value.action_key = key;
            value.attempts = 0;
            self.inner
                .supervisor_notice_sent
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
        if value.attempts >= 2 {
            if !self
                .inner
                .supervisor_notice_sent
                .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                self.inner.event_emitter.supervisor_notice(self.thread_id(), "Supervisor coordination remains unresolved after two attempts. The goal and inbox remain open. Record an evidenced disposition or new coordination event to continue; pending approvals and unknown operations remain held.");
            }
            return Ok(true);
        }
        let item = ContextualUserFragment::into(InternalModelContextFragment::new(
            InternalContextSource::from_static("supervisor_inbox"),
            inbox.notice(),
        ));
        let start_options = thread
            .thread_extension_data()
            .get::<TurnStartOptions>()
            .map(|o| o.as_ref().clone())
            .unwrap_or_default();
        let reservation = format!(
            "{}-{}-{}",
            self.thread_id(),
            key,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        value.reservation = Some(reservation.clone());
        value.admission_token = Some(reservation.clone());
        value.admitted_turn_id = None;
        value.in_flight = false;
        if let Err(error) = save_cursor(&home, &self.thread_id().to_string(), &value) {
            self.inner
                .supervisor_receipt_failed
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.inner.event_emitter.supervisor_notice(self.thread_id(), format!("Supervisor admission intent could not be persisted; automatic work is held until storage is repaired: {error}"));
            return Ok(true);
        }
        let submission = thread
            .start_turn_if_idle(
                TurnInputRequest::new(TurnInput::ResponseItem(item)).on_start(TurnStartOptions {
                    turn_trigger: Some(format!("supervisor_inbox:{reservation}")),
                    ..start_options
                }),
            )
            .await;
        match submission {
            Ok(StartIfIdleSubmission::Started { turn_id, .. }) => {
                // Never advance the durable receipt before admission. Crash redelivery has the same stable IDs.
                value.generation = inbox.generation;
                value.attempts += 1;
                value.in_flight = true;
                value.reservation = None;
                value.admitted_turn_id = Some(turn_id);
                if let Err(error) = save_cursor(&home, &self.thread_id().to_string(), &value) {
                    self.inner
                        .supervisor_receipt_failed
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    self.inner.event_emitter.supervisor_notice(self.thread_id(), format!("Supervisor turn started but its delivery receipt could not be persisted. Automatic work is held; reconcile this admitted turn before retry: {error}"));
                }
            }
            Ok(StartIfIdleSubmission::NotSubmitted { .. }) => {
                // Explicit host rejection proves no turn was admitted for this reservation.
                value.reservation = None;
                save_cursor(&home, &self.thread_id().to_string(), &value)?;
            }
            Err(error) => {
                self.inner
                    .supervisor_receipt_failed
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                self.inner.event_emitter.supervisor_notice(self.thread_id(), format!("Supervisor admission outcome is unknown; reconcile its durable reservation before retrying: {error}"));
            }
        }

        Ok(true)
    }
}
