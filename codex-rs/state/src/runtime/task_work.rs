//! Bounded local work records and transactional resource leases shared by all clients.
use super::StateRuntime;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use sqlx::Row;

/// A model-authored description of current work. Resource keys must identify exact work.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskWork {
    pub title: String,
    pub summary: String,
    pub status: TaskWorkStatus,
    pub resources: Vec<String>,
}

/// Whether the task is actively working, waiting for input, or finished.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskWorkStatus {
    Working,
    Waiting,
    Completed,
}

/// Untrusted task metadata for display and coordination, never execution instructions.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskWorkOwner {
    pub thread_id: String,
    pub title: String,
    pub summary: String,
    pub resource: String,
}

impl StateRuntime {
    /// Replace this task's claims atomically. Any conflict leaves existing claims intact.
    /// Leases expire after one hour; callers renew while the task is active.
    pub async fn update_task_work(
        &self,
        thread_id: ThreadId,
        work: &TaskWork,
    ) -> anyhow::Result<Vec<TaskWorkOwner>> {
        anyhow::ensure!(
            !work.title.trim().is_empty()
                && work.title.len() <= 96
                && !work.title.chars().any(char::is_control),
            "title must be 1-96 bytes"
        );
        anyhow::ensure!(
            work.summary.len() <= 192 && !work.summary.chars().any(char::is_control),
            "summary must be at most 192 bytes"
        );
        anyhow::ensure!(work.resources.len() <= 8, "at most eight resources");
        anyhow::ensure!(
            work.resources
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == work.resources.len(),
            "resource keys must be unique"
        );
        for resource in &work.resources {
            anyhow::ensure!(resource.len() <= 256 && !resource.chars().any(char::is_whitespace)
                && ["github:", "file:", "bug:"].iter().any(|prefix| resource.starts_with(prefix) && resource.len() > prefix.len()),
                "use a canonical github:, file:, or bug: resource key (at most 256 bytes)");
        }
        for key in &work.resources {
            if let Some(reference) = key.strip_prefix("github:") {
                let (repo, number) = reference
                    .split_once('#')
                    .ok_or_else(|| anyhow::anyhow!("GitHub key requires owner/repo#number"))?;
                let (owner, name) = repo
                    .split_once('/')
                    .ok_or_else(|| anyhow::anyhow!("GitHub key requires owner/repo#number"))?;
                anyhow::ensure!(
                    !owner.is_empty()
                        && !name.is_empty()
                        && !name.contains('/')
                        && repo == repo.to_lowercase()
                        && number
                            .parse::<u64>()
                            .is_ok_and(|parsed| parsed > 0 && parsed.to_string() == number),
                    "GitHub keys must be canonical lowercase owner/repo#number"
                );
            }
            if let Some(path) = key.strip_prefix("file:") {
                anyhow::ensure!(
                    std::path::Path::new(path).is_absolute(),
                    "file claims require absolute paths"
                );
            }
        }
        let _title_guard = self.task_work_title_guard().await?;
        let id = thread_id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut conflicts = Vec::new();
        if work.status == TaskWorkStatus::Working {
            for resource in &work.resources {
                if let Some(row) = sqlx::query("SELECT w.thread_id, w.title, w.summary FROM task_work_claims c JOIN task_work w ON w.thread_id = c.thread_id JOIN threads t ON t.id = w.thread_id WHERE c.resource = ? AND c.thread_id != ? AND c.expires_at > unixepoch() AND t.archived = 0")
                    .bind(resource).bind(&id).fetch_optional(&mut *tx).await? {
                    conflicts.push(TaskWorkOwner { thread_id: row.try_get("thread_id")?, title: row.try_get("title")?, summary: row.try_get("summary")?, resource: resource.clone() });
                    if conflicts.len() == 3 { break; }
                }
            }
        }
        if !conflicts.is_empty() {
            tx.rollback().await?;
            return Ok(conflicts);
        }
        let status = match work.status {
            TaskWorkStatus::Working => "working",
            TaskWorkStatus::Waiting => "waiting",
            TaskWorkStatus::Completed => "completed",
        };
        sqlx::query("INSERT INTO task_work (thread_id, title, summary, status, resource_count, updated_at, automatic_name) VALUES (?, ?, ?, ?, ?, unixepoch(), (SELECT name FROM threads WHERE id = ?)) ON CONFLICT(thread_id) DO UPDATE SET title=excluded.title, summary=excluded.summary, status=excluded.status, resource_count=excluded.resource_count, updated_at=excluded.updated_at")
            .bind(&id).bind(&work.title).bind(&work.summary).bind(status).bind(work.resources.len() as i64).bind(&id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM task_work_claims WHERE thread_id = ? OR expires_at <= unixepoch() OR thread_id IN (SELECT id FROM threads WHERE archived = 1)")
            .bind(&id).execute(&mut *tx).await?;
        if work.status == TaskWorkStatus::Working {
            for resource in &work.resources {
                sqlx::query(
                    "INSERT OR IGNORE INTO task_work_claims VALUES (?, ?, unixepoch() + 3600)",
                )
                .bind(resource)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(Vec::new())
    }

    /// Refresh only leases that are still owned; expired or stolen leases cannot be revived.
    pub async fn heartbeat_task_work(&self, thread_id: ThreadId) -> anyhow::Result<()> {
        let id = thread_id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let expected: i64 = sqlx::query_scalar("SELECT COALESCE((SELECT resource_count FROM task_work WHERE thread_id = ? AND status = 'working'), 0)")
            .bind(&id).fetch_one(&mut *tx).await?;
        let renewed = sqlx::query("UPDATE task_work_claims SET expires_at = unixepoch() + 3600 WHERE thread_id = ? AND expires_at > unixepoch()")
            .bind(&id).execute(&mut *tx).await?.rows_affected();
        anyhow::ensure!(
            renewed == expected as u64,
            "Task ownership expired or changed. Call update_task to recheck ownership before continuing tools."
        );
        tx.commit().await?;
        Ok(())
    }

    /// List up to three active owners, optionally restricted to exact resource keys.
    pub async fn task_work_owners(
        &self,
        excluding: ThreadId,
        resources: &[String],
    ) -> anyhow::Result<Vec<TaskWorkOwner>> {
        let filter = serde_json::to_string(resources)?;
        let rows = sqlx::query("SELECT w.thread_id, w.title, w.summary, c.resource FROM task_work_claims c JOIN task_work w ON w.thread_id = c.thread_id JOIN threads t ON t.id = w.thread_id WHERE w.thread_id != ? AND c.expires_at > unixepoch() AND t.archived = 0 AND (? = '[]' OR c.resource IN (SELECT value FROM json_each(?))) ORDER BY w.updated_at DESC, c.resource LIMIT 3")
            .bind(excluding.to_string()).bind(&filter).bind(&filter).fetch_all(self.pool.as_ref()).await?;
        rows.into_iter()
            .map(|row| {
                Ok(TaskWorkOwner {
                    thread_id: row.try_get("thread_id")?,
                    title: row.try_get("title")?,
                    summary: row.try_get("summary")?,
                    resource: row.try_get("resource")?,
                })
            })
            .collect()
    }

    /// Serialize work creation and manual/automatic title writes across client processes.
    /// The OS releases this guard on drop or process exit.
    pub async fn task_work_title_guard(&self) -> anyhow::Result<std::fs::File> {
        let path = self.sqlite.home().join(".task-title.lock");
        tokio::task::spawn_blocking(move || {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)?;
            file.lock()?;
            Ok(file)
        })
        .await?
    }

    /// The current suggestion and last applied name; a user rename locks further updates.
    pub async fn task_work_title(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<(String, Option<String>)>> {
        let row = sqlx::query(
            "SELECT title, automatic_name FROM task_work WHERE thread_id = ? AND title_locked = 0",
        )
        .bind(thread_id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.map(|row| Ok((row.try_get("title")?, row.try_get("automatic_name")?)))
            .transpose()
    }

    /// Remember a successfully persisted automatic title.
    pub async fn remember_task_work_title(
        &self,
        thread_id: ThreadId,
        title: &str,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE task_work SET automatic_name = ? WHERE thread_id = ?")
            .bind(title)
            .bind(thread_id.to_string())
            .execute(self.pool.as_ref())
            .await?;
        Ok(())
    }

    /// Preserve user renames once task tracking has begun.
    pub async fn lock_task_work_title(&self, thread_id: ThreadId) -> anyhow::Result<()> {
        sqlx::query("UPDATE task_work SET title_locked = 1 WHERE thread_id = ?")
            .bind(thread_id.to_string())
            .execute(self.pool.as_ref())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "task_work_tests.rs"]
mod tests;
