//! Optional directional review for long-running root threads, including ordinary prompts.
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::progress_evidence::evidence_packet;
use crate::progress_evidence::parse_verdict;
use crate::progress_evidence::user_text;
use crate::progress_persistence::SavedReview;
use crate::progress_persistence::{self};
use codex_core::ThreadManager;
use codex_core::context::ContextualUserFragment;
use codex_core::context::InternalContextSource;
use codex_core::context::InternalModelContextFragment;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::TokenUsageContributor;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::TurnAbortInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_extension_api::TurnStopInput;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TokenUsageInfo;

const INTERVAL: Duration = Duration::from_secs(600);

type EnableCheck<C> = dyn Fn(&C) -> Option<PathBuf> + Send + Sync;

struct ProgressReview<C> {
    manager: Weak<ThreadManager>,
    enabled: Arc<EnableCheck<C>>,
    state_db: Option<Arc<codex_state::StateRuntime>>,
}

struct Enabled {
    path: Option<PathBuf>,
    write_lock: Mutex<()>,
}

#[derive(Default)]
pub(crate) struct Signals {
    failures: AtomicUsize,
    high_context: AtomicBool,
    notify: tokio::sync::Notify,
    checkpoint: Mutex<Option<String>>,
}

#[derive(Default)]
pub(crate) struct PreviousVerdict(pub(crate) Mutex<Option<(u64, String)>>);

#[derive(Default)]
struct ActiveClock(Mutex<Clock>);

#[derive(Default)]
struct Clock {
    active: Option<(String, std::time::Instant)>,
    accumulated: Duration,
    report_accumulated: Duration,
}

impl Clock {
    fn start(&mut self, turn: &str, now: std::time::Instant) {
        if let Some((_, started)) = self.active.take() {
            let elapsed = now.saturating_duration_since(started);
            self.accumulated += elapsed;
            self.report_accumulated += elapsed;
        }
        self.active = Some((turn.to_string(), now));
    }

    fn stop(&mut self, turn: &str, now: std::time::Instant) {
        if self.active.as_ref().is_some_and(|(id, _)| id == turn)
            && let Some((_, started)) = self.active.take()
        {
            let elapsed = now.saturating_duration_since(started);
            self.accumulated += elapsed;
            self.report_accumulated += elapsed;
        }
    }

    fn remaining(&self, now: std::time::Instant) -> Duration {
        let active = self.active.as_ref().map_or(Duration::ZERO, |(_, started)| {
            now.saturating_duration_since(*started)
        });
        INTERVAL.saturating_sub(self.accumulated + active)
    }

    fn elapsed(&self, now: std::time::Instant) -> Duration {
        INTERVAL.saturating_sub(self.remaining(now))
    }

    fn report_remaining(&self, now: std::time::Instant) -> Duration {
        let active = self.active.as_ref().map_or(Duration::ZERO, |(_, started)| {
            now.saturating_duration_since(*started)
        });
        INTERVAL.saturating_sub(self.report_accumulated + active)
    }

    fn reported(&mut self, turn: &str, now: std::time::Instant) {
        if let Some((id, started)) = &mut self.active
            && id == turn
        {
            self.accumulated += now.saturating_duration_since(*started);
            self.report_accumulated = Duration::ZERO;
            *started = now;
        }
    }

    fn reviewed(&mut self, turn: &str, now: std::time::Instant) {
        if let Some((id, started)) = &mut self.active
            && id == turn
        {
            self.report_accumulated += now.saturating_duration_since(*started);
            self.accumulated = Duration::ZERO;
            *started = now;
        }
    }
}

impl Signals {
    pub(crate) fn request(&self, checkpoint: String) {
        if let Ok(mut pending) = self.checkpoint.lock() {
            *pending = Some(checkpoint);
        }
        self.notify.notify_one();
    }

    fn observe_context(&self, input: i64, window: Option<i64>) -> bool {
        let high = window.is_some_and(|window| {
            window > 0 && input > 0 && i128::from(input) * 5 >= i128::from(window) * 4
        });
        let previous = self.high_context.swap(high, Ordering::Relaxed);
        high && !previous
    }

    fn observe(&self, outcome: ToolCallOutcome) -> bool {
        match outcome {
            ToolCallOutcome::Failed {
                handler_executed: true,
            }
            | ToolCallOutcome::Completed { success: false } => {
                if self.failures.fetch_add(1, Ordering::Relaxed) + 1 >= 3 {
                    self.failures.store(0, Ordering::Relaxed);
                    return true;
                }
            }
            ToolCallOutcome::Completed { success: true } => {
                self.failures.store(0, Ordering::Relaxed)
            }
            ToolCallOutcome::Blocked
            | ToolCallOutcome::Failed {
                handler_executed: false,
            }
            | ToolCallOutcome::Aborted => {}
        }
        false
    }
}

impl<C: Send + Sync + 'static> TokenUsageContributor for ProgressReview<C> {
    fn on_token_usage<'a>(
        &'a self,
        _session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
        turn_store: &'a ExtensionData,
        usage: &'a TokenUsageInfo,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if thread_store
                .get::<Enabled>()
                .is_none_or(|enabled| enabled.path.is_none())
            {
                return;
            }
            if let Some(signals) = turn_store.get::<Signals>()
                && signals.observe_context(
                    usage.last_token_usage.input_tokens,
                    usage.model_context_window,
                )
            {
                signals.notify.notify_one();
            }
        })
    }
}

impl<C: Send + Sync + 'static> ThreadLifecycleContributor<C> for ProgressReview<C> {
    fn on_thread_start<'a>(&'a self, input: ThreadStartInput<'a, C>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let root = !matches!(
                input.session_source,
                SessionSource::SubAgent(_) | SessionSource::Internal(_)
            );
            let state_path = if root {
                (self.enabled)(input.config)
                    .map(|home| progress_persistence::path(&home, input.thread_store.level_id()))
            } else {
                None
            };
            let saved = state_path
                .as_deref()
                .map(progress_persistence::load)
                .unwrap_or_default();
            input.thread_store.insert(Enabled {
                path: state_path,
                write_lock: Mutex::new(()),
            });
            input
                .thread_store
                .insert(PreviousVerdict(Mutex::new(saved.previous)));
            input.thread_store.insert(ActiveClock(Mutex::new(Clock {
                accumulated: Duration::from_secs(saved.active_seconds),
                report_accumulated: Duration::from_secs(saved.report_seconds),
                active: None,
            })));
        })
    }
}

impl<C: Send + Sync + 'static> TurnLifecycleContributor for ProgressReview<C> {
    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input.token_usage_at_turn_start.is_none()
                || input
                    .thread_store
                    .get::<Enabled>()
                    .is_none_or(|enabled| enabled.path.is_none())
                || input.collaboration_mode.mode == codex_protocol::config_types::ModeKind::Plan
            {
                return;
            }
            let Ok(thread_id) = ThreadId::from_string(input.thread_store.level_id()) else {
                return;
            };
            let Some(manager) = self.manager.upgrade() else {
                return;
            };
            let turn_id = input.turn_id.to_string();
            let state_db = self.state_db.clone();
            let signals = input.turn_store.get_or_init::<Signals>(Signals::default);
            let Some(previous_verdict) = input.thread_store.get::<PreviousVerdict>() else {
                return;
            };
            let Some(clock) = input.thread_store.get::<ActiveClock>() else {
                return;
            };
            let Some(enabled) = input.thread_store.get::<Enabled>() else {
                return;
            };
            if let Ok(mut value) = clock.0.lock() {
                value.start(&turn_id, std::time::Instant::now());
            }
            tokio::spawn(async move {
                let mut last_review = tokio::time::Instant::now();
                loop {
                    let remaining = clock
                        .0
                        .lock()
                        .map(|value| {
                            value
                                .remaining(std::time::Instant::now())
                                .min(value.report_remaining(std::time::Instant::now()))
                        })
                        .unwrap_or(INTERVAL);
                    let signalled = tokio::select! {
                        _ = tokio::time::sleep(remaining) => false,
                        _ = signals.notify.notified() => {
                            if last_review.elapsed() < Duration::from_secs(120) {
                                let report_remaining = clock.0.lock()
                                    .map(|value| value.report_remaining(std::time::Instant::now()))
                                    .unwrap_or(INTERVAL);
                                tokio::select! {
                                    _ = tokio::time::sleep_until(last_review + Duration::from_secs(120)) => true,
                                    _ = tokio::time::sleep(report_remaining) => {
                                        // Preserve the event for after the report and cooldown.
                                        signals.notify.notify_one();
                                        false
                                    },
                                }
                            } else {
                                true
                            }
                        },
                    };
                    let Ok(thread) = manager.get_thread(thread_id).await else {
                        break;
                    };
                    if thread.current_turn_model(&turn_id).await.is_none() {
                        break;
                    }
                    let history = thread.conversation_history_snapshot().await;
                    let revision = history.user_message_revision();
                    let now = std::time::Instant::now();
                    let report_due = clock
                        .0
                        .lock()
                        .map(|value| value.report_remaining(now).is_zero())
                        .unwrap_or(false);
                    if report_due {
                        let reminder: ResponseItem = ContextualUserFragment::into(
                            InternalModelContextFragment::new(
                                InternalContextSource::from_static("progress_review"),
                                "At the next safe boundary, give the user a concise progress update: the overall objective, evidence of progress, remaining work or blockers, and the next useful step. Continue working; do not interrupt a running operation just to report.".to_string(),
                            ),
                        );
                        let delivered = thread
                            .deliver_progress_review(&turn_id, revision, reminder)
                            .await;
                        if !delivered {
                            // A new user revision can invalidate the snapshot. Retry without
                            // resetting the reporting clock or spinning while the turn ends.
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            continue;
                        }
                        if let Ok(mut value) = clock.0.lock() {
                            value.reported(&turn_id, std::time::Instant::now());
                        }
                    }
                    let review_due = clock
                        .0
                        .lock()
                        .map(|value| value.remaining(std::time::Instant::now()).is_zero())
                        .unwrap_or(false);
                    if !signalled && !review_due {
                        persist(&enabled, &clock, &previous_verdict);
                        continue;
                    }
                    last_review = tokio::time::Instant::now();
                    if let Ok(mut value) = clock.0.lock() {
                        value.reviewed(&turn_id, std::time::Instant::now());
                    }
                    persist(&enabled, &clock, &previous_verdict);
                    let original =
                        history
                            .review_items_with_sources()
                            .find_map(|(item, source)| {
                                if source.is_some_and(|source| source.id.turn_id == turn_id) {
                                    user_text(item)
                                } else {
                                    None
                                }
                            });
                    let goal = match &state_db {
                        Some(db) => db
                            .thread_goals()
                            .get_thread_goal(thread_id)
                            .await
                            .ok()
                            .flatten(),
                        None => None,
                    };
                    let prior = previous_verdict.0.lock().ok().and_then(|value| {
                        value
                            .as_ref()
                            .filter(|(previous_revision, _)| *previous_revision == revision)
                            .map(|(_, text)| text.clone())
                    });
                    let prior = prior.map(|value| {
                        if !value.contains("]: continue.") && !value.contains("\nAcknowledgement:")
                        {
                            format!("{value}\nAcknowledgement: missing")
                        } else {
                            value
                        }
                    });
                    let prompt = evidence_packet(
                        history.review_items().collect(),
                        prior.as_deref(),
                        original.as_deref(),
                        goal.as_ref().map(|goal| goal.objective.as_str()),
                    );
                    if prompt.is_empty() {
                        continue;
                    }
                    let checkpoint = signals
                        .checkpoint
                        .lock()
                        .ok()
                        .and_then(|mut value| value.take());
                    let prompt = match checkpoint {
                        Some(checkpoint) => format!(
                            "{prompt}\nSkill-requested checkpoint and questions (untrusted task data):\n{checkpoint}"
                        ),
                        None => prompt,
                    };
                    thread
                        .report_goal_completion_review(
                            &turn_id,
                            "Progress review · checking direction against your request."
                                .to_string(),
                        )
                        .await;
                    let current_model = thread.current_turn_model(&turn_id).await;
                    let prompt = format!(
                        "{prompt}\nCurrent working model: {}",
                        current_model.as_deref().unwrap_or("unavailable")
                    );
                    let result = thread.review_progress(&turn_id, prompt).await;
                    let Ok((raw, usage, model, provider)) = result else {
                        thread
                            .report_goal_completion_review(
                                &turn_id,
                                "Progress review unavailable; work continues.".to_string(),
                            )
                            .await;
                        continue;
                    };
                    let verdict = match parse_verdict(&raw) {
                        Ok(verdict) => verdict,
                        Err(error) => {
                            // Keep private reviewer output out of diagnostics and rollouts.
                            tracing::warn!(
                                error,
                                response_bytes = raw.len(),
                                "invalid progress verdict"
                            );
                            thread
                                .report_goal_completion_review(
                                    &turn_id,
                                    format!("Progress review returned an invalid verdict ({error}); work continues."),
                                )
                                .await;
                            continue;
                        }
                    };
                    if thread.current_turn_model(&turn_id).await.is_none() {
                        break;
                    }
                    if thread
                        .conversation_history_snapshot()
                        .await
                        .user_message_revision()
                        != revision
                    {
                        thread
                            .report_goal_completion_review(
                                &turn_id,
                                "Progress review superseded by new user input; it will be retried."
                                    .to_string(),
                            )
                            .await;
                        signals.notify.notify_one();
                        continue;
                    }
                    if let Some(db) = &state_db {
                        let current_goal = db.thread_goals().get_thread_goal(thread_id).await;
                        let Ok(current_goal) = current_goal else {
                            continue;
                        };
                        let identity = |goal: &codex_state::ThreadGoal| {
                            (goal.goal_id.clone(), goal.objective.clone(), goal.status)
                        };
                        if current_goal.as_ref().map(identity) != goal.as_ref().map(identity) {
                            thread.report_goal_completion_review(&turn_id, "Progress review superseded by a goal change; it will be retried.".to_string()).await;
                            signals.notify.notify_one();
                            continue;
                        }
                    }
                    let message = format!(
                        "Independent progress review [{}-{}]: {}. {}",
                        turn_id,
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos(),
                        verdict.decision,
                        verdict.reason
                    );
                    if let Ok(mut previous) = previous_verdict.0.lock() {
                        *previous = Some((revision, message.clone()));
                    }
                    persist(&enabled, &clock, &previous_verdict);
                    let display = match usage {
                        Some(usage) => format!(
                            "{message}\nReviewer route: {}\nReviewer usage: input={} cached={} output={}",
                            serde_json::json!({"model": model, "provider": provider, "cache_write_input_tokens": usage.cache_write_input_tokens}),
                            usage.input_tokens,
                            usage.cached_input_tokens,
                            usage.output_tokens
                        ),
                        None => format!("{message}\nReviewer usage: unavailable"),
                    };
                    thread
                        .report_goal_completion_review(&turn_id, display)
                        .await;
                    if let Some(tier) = verdict.model_tier.as_deref().filter(|tier| *tier != "keep")
                    {
                        let outcome = thread
                            .apply_progress_review_route(
                                &turn_id,
                                revision,
                                tier,
                                current_model.as_deref(),
                            )
                            .await;
                        thread
                            .report_goal_completion_review(&turn_id, outcome)
                            .await;
                    }
                    if verdict.decision == "continue" {
                        continue;
                    }
                    let item: ResponseItem = ContextualUserFragment::into(
                        InternalModelContextFragment::new(
                            InternalContextSource::from_static("progress_review"),
                            format!(
                                "{message}\nTreat this review as fallible evidence. At the next safe boundary, call acknowledge_progress_review with the exact review ID, your decision, reason, and concrete next action; also briefly tell the user. Continue the user's full objective. Do not interrupt a live operation solely for this review."
                            ),
                        ),
                    );
                    if !thread
                        .deliver_progress_review(&turn_id, revision, item)
                        .await
                    {
                        thread
                            .report_goal_completion_review(
                                &turn_id,
                                "Progress review advice could not be delivered to the active turn."
                                    .to_string(),
                            )
                            .await;
                    }
                }
            });
        })
    }

    fn on_turn_stop<'a>(&'a self, input: TurnStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(clock) = input.thread_store.get::<ActiveClock>()
                && let Ok(mut value) = clock.0.lock()
            {
                value.stop(input.turn_store.level_id(), std::time::Instant::now());
            }
            persist_from_store(input.thread_store);
        })
    }

    fn on_turn_abort<'a>(&'a self, input: TurnAbortInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(clock) = input.thread_store.get::<ActiveClock>()
                && let Ok(mut value) = clock.0.lock()
            {
                value.stop(input.turn_store.level_id(), std::time::Instant::now());
            }
            persist_from_store(input.thread_store);
        })
    }
}

impl<C: Send + Sync + 'static> ToolContributor for ProgressReview<C> {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        if thread_store
            .get::<Enabled>()
            .is_none_or(|enabled| enabled.path.is_none())
        {
            return Vec::new();
        }
        let Ok(thread_id) = ThreadId::from_string(thread_store.level_id()) else {
            return Vec::new();
        };
        vec![
            Arc::new(crate::progress_checkpoint::CheckpointTool::new(
                self.manager.clone(),
                thread_id,
            )),
            Arc::new(crate::progress_checkpoint::AcknowledgementTool {
                manager: self.manager.clone(),
                thread_id,
            }),
        ]
    }
}

impl<C: Send + Sync + 'static> ToolLifecycleContributor for ProgressReview<C> {
    fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            if input
                .thread_store
                .get::<Enabled>()
                .is_none_or(|enabled| enabled.path.is_none())
            {
                return;
            }
            let Some(signals) = input.turn_store.get::<Signals>() else {
                return;
            };
            if signals.observe(input.outcome) {
                signals.notify.notify_one();
            }
        })
    }
}

#[cfg(test)]
#[path = "progress_review_tests.rs"]
mod tests;

pub fn install_progress_review<C: Send + Sync + 'static>(
    registry: &mut ExtensionRegistryBuilder<C>,
    manager: Weak<ThreadManager>,
    state_db: Option<Arc<codex_state::StateRuntime>>,
    enabled: impl Fn(&C) -> Option<PathBuf> + Send + Sync + 'static,
) {
    let extension = Arc::new(ProgressReview {
        manager,
        enabled: Arc::new(enabled),
        state_db,
    });
    registry.thread_lifecycle_contributor(extension.clone());
    registry.turn_lifecycle_contributor(extension.clone());
    registry.token_usage_contributor(extension.clone());
    registry.tool_lifecycle_contributor(extension.clone());
    registry.tool_contributor(extension);
}

pub(crate) fn persist_from_store(store: &codex_extension_api::ExtensionData) {
    if let (Some(enabled), Some(clock), Some(previous)) = (
        store.get::<Enabled>(),
        store.get::<ActiveClock>(),
        store.get::<PreviousVerdict>(),
    ) {
        persist(&enabled, &clock, &previous);
    }
}

fn persist(enabled: &Enabled, clock: &ActiveClock, previous: &PreviousVerdict) {
    let Some(path) = enabled.path.as_deref() else {
        return;
    };
    let Ok(_writer) = enabled.write_lock.lock() else {
        return;
    };
    let Ok(clock) = clock.0.lock() else {
        return;
    };
    let Ok(previous) = previous.0.lock() else {
        return;
    };
    let state = SavedReview {
        active_seconds: clock.elapsed(std::time::Instant::now()).as_secs(),
        report_seconds: INTERVAL
            .saturating_sub(clock.report_remaining(std::time::Instant::now()))
            .as_secs(),
        previous: previous.clone(),
    };
    if let Err(error) = progress_persistence::save(path, &state) {
        tracing::warn!("could not persist progress review state: {error}");
    }
}
