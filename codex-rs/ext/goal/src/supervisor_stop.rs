//! Validate model-requested stopping independently of model judgments.
use super::GoalRuntimeHandle;
use crate::supervisor_inbox;
use codex_protocol::protocol::AgentStatus;

impl GoalRuntimeHandle {
    pub(super) async fn supervisor_home(&self) -> std::path::PathBuf {
        if let Some(manager) = self.inner.thread_manager.upgrade()
            && let Ok(thread) = manager.get_thread(self.thread_id()).await
        {
            return thread.config().await.codex_home.to_path_buf();
        }
        self.inner.state_dbs.sqlite().home().to_path_buf()
    }

    pub(super) fn supervisor_registered(&self, home: &std::path::Path) -> bool {
        home.join("supervisor-inbox-cursors")
            .join(format!("{}.json", self.thread_id()))
            .exists()
            || self
                .inner
                .supervisor_poll
                .lock()
                .is_ok_and(|task| task.is_some())
    }

    pub(crate) async fn validate_supervisor_stop(
        &self,
        status: codex_protocol::protocol::ThreadGoalStatus,
    ) -> Result<(), String> {
        if status == codex_protocol::protocol::ThreadGoalStatus::Paused {
            return Ok(());
        }
        let home = self.supervisor_home().await;
        let Some(inbox) = supervisor_inbox::load(&home, &self.thread_id().to_string())? else {
            return if self.supervisor_registered(&home) {
                Err("Registered supervisor inbox is missing; repair its projection before stopping.".into())
            } else {
                Ok(())
            };
        };
        inbox.validate_stop(status)?;
        if let Some(manager) = self.inner.thread_manager.upgrade() {
            for id in manager
                .list_agent_subtree_thread_ids(self.thread_id())
                .await
                .map_err(|e| e.to_string())?
            {
                if id != self.thread_id()
                    && let Ok(child) = manager.get_thread(id).await
                    && matches!(
                        child.agent_status().await,
                        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted
                    )
                {
                    return Err(format!(
                        "Supervisor child {id} is still live; wait for its report and record its disposition before stopping."
                    ));
                }
            }
        }
        Ok(())
    }
}
