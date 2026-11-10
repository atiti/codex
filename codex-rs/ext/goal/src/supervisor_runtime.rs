//! Safe-boundary supervision; inbox data never supplies execution authority.
use super::GoalRuntimeHandle;
use crate::supervisor_inbox;
use codex_core::StartIfIdleSubmission;
use codex_core::TurnInput;
use codex_core::TurnInputRequest;
use codex_core::TurnStartOptions;
use codex_core::context::ContextualUserFragment;
use codex_core::context::InternalContextSource;
use codex_core::context::InternalModelContextFragment;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[derive(Default, Deserialize, Serialize)]
struct Cursor {
    generation: u64,
    suspended: bool,
    #[serde(default)]
    action_key: u64,
    #[serde(default)]
    attempts: u8,
    #[serde(default)]
    in_flight: bool,
}

fn cursor(home: &Path, thread_id: &str) -> Result<Cursor, String> {
    let path = home
        .join("supervisor-inbox-cursors")
        .join(format!("{thread_id}.json"));
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        if !metadata.is_file() {
            return Err("supervisor cursor must be a regular file".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let owner = std::fs::metadata(home).map_err(|e| e.to_string())?.uid();
            if metadata.uid() != owner || metadata.mode() & 0o077 != 0 {
                return Err("supervisor cursor must be owner-only".into());
            }
        }
    }
    match std::fs::read(path) {
        Ok(data) if data.len() <= 1024 => serde_json::from_slice(&data).map_err(|e| e.to_string()),
        Ok(_) => Err("supervisor cursor exceeds size limit".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Cursor::default()),
        Err(error) => Err(error.to_string()),
    }
}

fn save_cursor(home: &Path, thread_id: &str, value: &Cursor) -> Result<(), String> {
    use std::io::Write;
    let parent = home.join("supervisor-inbox-cursors");
    std::fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
    if !std::fs::symlink_metadata(&parent)
        .map_err(|e| e.to_string())?
        .is_dir()
    {
        return Err("supervisor cursor directory must not be a symlink".into());
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        options.mode(0o600);
    }
    let path = parent.join(format!("{thread_id}.json"));
    let temporary = parent.join(format!(
        "{thread_id}.{}.{:?}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let mut file = options.open(&temporary).map_err(|e| e.to_string())?;
    let result = (|| {
        serde_json::to_writer(&mut file, value)?;
        file.flush()?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)?;
        std::fs::File::open(&parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result.map_err(|e: std::io::Error| e.to_string())
}

impl GoalRuntimeHandle {
    pub(crate) async fn supervisor_turn_started(&self) {
        let home = self.supervisor_home().await;
        if supervisor_inbox::load(&home, &self.thread_id().to_string())
            .ok()
            .flatten()
            .is_some()
            && let Ok(mut value) = cursor(&home, &self.thread_id().to_string())
            && value.suspended
        {
            value.suspended = false;
            if let Err(error) = save_cursor(&home, &self.thread_id().to_string(), &value) {
                tracing::warn!(%error, "failed to clear supervisor suspension");
            }
        }
    }

    pub(crate) async fn finish_supervisor_turn(&self) {
        let Ok(_permit) = self.goal_state_permit().await else {
            return;
        };
        let home = self.supervisor_home().await;
        if let Ok(mut value) = cursor(&home, &self.thread_id().to_string())
            && value.in_flight
        {
            value.in_flight = false;
            if let Err(error) = save_cursor(&home, &self.thread_id().to_string(), &value) {
                tracing::warn!(%error, "failed to finish supervisor receipt");
            }
        }
    }

    pub(crate) async fn resume_supervisor_receipt(&self) {
        let Ok(_permit) = self.goal_state_permit().await else {
            return;
        };
        let home = self.supervisor_home().await;
        if let Ok(mut value) = cursor(&home, &self.thread_id().to_string())
            && value.in_flight
            && !value.suspended
        {
            value.in_flight = false;
            value.attempts = value.attempts.saturating_sub(1);
            if let Err(error) = save_cursor(&home, &self.thread_id().to_string(), &value) {
                tracing::warn!(%error, "failed to recover supervisor receipt");
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
        let home = self.supervisor_home().await;
        if supervisor_inbox::load(&home, &self.thread_id().to_string())
            .ok()
            .flatten()
            .is_none()
        {
            return;
        }
        let Ok(mut task) = self.inner.supervisor_poll.lock() else {
            return;
        };
        if task.is_some() {
            return;
        }
        if !home
            .join("supervisor-inbox-cursors")
            .join(format!("{}.json", self.thread_id()))
            .exists()
            && let Err(error) =
                save_cursor(&home, &self.thread_id().to_string(), &Cursor::default())
        {
            tracing::warn!(%error, "failed to persist supervisor registration receipt");
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
                    self.inner.event_emitter.user_notice(
                        format!("{}:supervisor-inbox-invalid", self.thread_id()),
                        format!(
                            "Supervisor inbox needs repair before automatic continuation: {error}"
                        ),
                    );
                }
                return Ok(true);
            }
        };
        let mut value = match cursor(&home, &self.thread_id().to_string()) {
            Ok(value) => value,
            Err(error) => {
                if !self
                    .inner
                    .supervisor_notice_sent
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    self.inner.event_emitter.user_notice(format!("{}:supervisor-cursor-invalid", self.thread_id()), format!("Supervisor delivery receipt needs repair before automatic continuation: {error}"));
                }
                return Ok(true);
            }
        };
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
                self.inner.event_emitter.user_notice(format!("{}:supervisor-coordination-stalled", self.thread_id()), "Supervisor coordination remains unresolved after two attempts. The goal and inbox remain open. Record an evidenced disposition or new coordination event to continue; pending approvals and unknown operations remain held.");
            }
            return Ok(true);
        }
        let Some(manager) = self.inner.thread_manager.upgrade() else {
            return Ok(true);
        };
        let Ok(thread) = manager.get_thread(self.thread_id()).await else {
            return Ok(true);
        };
        let item = ContextualUserFragment::into(InternalModelContextFragment::new(
            InternalContextSource::from_static("supervisor_inbox"),
            inbox.notice(),
        ));
        let start_options = thread
            .thread_extension_data()
            .get::<TurnStartOptions>()
            .map(|o| o.as_ref().clone())
            .unwrap_or_default();
        let submission = thread
            .start_turn_if_idle(
                TurnInputRequest::new(TurnInput::ResponseItem(item)).on_start(TurnStartOptions {
                    turn_trigger: Some("supervisor_inbox".into()),
                    ..start_options
                }),
            )
            .await
            .map_err(|e| e.to_string())?;
        if matches!(submission, StartIfIdleSubmission::Started { .. }) {
            // Never advance the durable receipt before admission. Crash redelivery has the same stable IDs.
            value.generation = inbox.generation;
            value.attempts += 1;
            value.in_flight = true;
            save_cursor(&home, &self.thread_id().to_string(), &value)?;
        }
        Ok(true)
    }
}
