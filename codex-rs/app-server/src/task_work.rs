//! Synchronize model-maintained task titles through the existing naming notification.
use crate::outgoing_message::ThreadScopedOutgoingMessageSender;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadNameUpdatedNotification;
use codex_core::CodexThread;
use codex_core::ThreadManager;
use codex_protocol::ThreadId;
use codex_thread_store::ThreadMetadataPatch;
use std::sync::Arc;
use tokio::sync::Semaphore;

pub(crate) async fn sync_title(
    thread_id: ThreadId,
    thread: &CodexThread,
    manager: &ThreadManager,
    outgoing: &ThreadScopedOutgoingMessageSender,
    names: &Arc<Semaphore>,
) -> anyhow::Result<()> {
    if !thread
        .config()
        .await
        .features
        .get()
        .enabled(codex_features::Feature::TaskAwareness)
    {
        return Ok(());
    }
    let _permit = names.acquire().await?;
    let Some(db) = thread.state_db() else {
        return Ok(());
    };
    let title_guard = db.task_work_title_guard().await?;
    let Some(state) = db.task_work_title(thread_id).await? else {
        return Ok(());
    };
    let Some(metadata) = db.get_thread(thread_id).await? else {
        return Ok(());
    };
    if metadata.archived_at.is_some() {
        return Ok(());
    }
    if metadata.name != state.automatic_name
        && !(state.pending_name.is_some() && metadata.name == state.pending_name)
    {
        db.lock_task_work_title(thread_id).await?;
        return Ok(());
    }
    let title = state.title;
    let notify =
        metadata.name.as_deref() != Some(&title) || state.pending_name.as_deref() == Some(&title);
    if metadata.name.as_deref() != Some(&title) {
        db.prepare_task_work_title(thread_id, &title).await?;
        manager
            .update_thread_metadata(
                thread_id,
                ThreadMetadataPatch {
                    name: Some(Some(title.clone())),
                    ..Default::default()
                },
                /*include_archived*/ false,
            )
            .await?;
    }
    db.remember_task_work_title(thread_id, &title).await?;
    drop(title_guard);
    if notify {
        outgoing
            .send_global_server_notification(ServerNotification::ThreadNameUpdated(
                ThreadNameUpdatedNotification {
                    thread_id: thread_id.to_string(),
                    thread_name: Some(title.clone()),
                },
            ))
            .await;
    }
    Ok(())
}
