//! Durable admission journal: prepared intent precedes execution, unknown outcomes never retry.
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub(super) struct Cursor {
    pub(super) generation: u64,
    pub(super) suspended: bool,
    #[serde(default)]
    pub(super) action_key: u64,
    #[serde(default)]
    pub(super) attempts: u8,
    #[serde(default)]
    pub(super) in_flight: bool,
    #[serde(default)]
    pub(super) reservation: Option<String>,
    #[serde(default)]
    pub(super) admitted_turn_id: Option<String>,
    #[serde(default)]
    pub(super) admission_token: Option<String>,
    #[serde(default)]
    pub(super) last_resolution: Option<String>,
}

pub(super) fn cursor(home: &Path, thread_id: &str) -> Result<Cursor, String> {
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

pub(super) fn save_cursor(home: &Path, thread_id: &str, value: &Cursor) -> Result<(), String> {
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
        drop(file);
        std::fs::rename(&temporary, &path)?;
        // Match thread-store persistence: Unix syncs the directory entry. Windows
        // retains file fsync and atomic replace without unsupported directory opens.
        #[cfg(unix)]
        std::fs::File::open(&parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result.map_err(|e: std::io::Error| e.to_string())
}

pub(super) async fn reconcile(
    thread: &codex_core::CodexThread,
    value: &mut Cursor,
) -> Result<(), String> {
    if value.reservation.is_none() && !value.in_flight {
        return Ok(());
    }
    if value.reservation.is_none()
        && let Some(id) = value.admitted_turn_id.as_deref()
        && let Some(token) = value.admission_token.as_deref()
        && let Some(metadata) = thread.current_turn_extension_data::<std::sync::Arc<dyn codex_analytics::TurnAnalyticsMetadata>>(id).await
        && metadata.turn_trigger().as_deref() == Some(format!("supervisor_inbox:{token}").as_str())
        && thread.current_turn_model(id).await.is_some()
    {
        return Ok(());
    }
    let history = thread
        .load_history(/*include_archived*/ false)
        .await
        .map_err(|e| e.to_string())?;
    let result = reconcile_history(value, &history.items);
    if let Some(id) = value.admitted_turn_id.as_deref()
        && thread.current_turn_model(id).await.is_some()
    {
        return Ok(());
    }
    result
}

pub(super) fn reconcile_history(
    value: &mut Cursor,
    history: &[codex_rollout::RolloutItem],
) -> Result<(), String> {
    use codex_protocol::protocol::EventMsg;
    use codex_rollout::RolloutItem;
    let token = value
        .reservation
        .as_deref()
        .or(value.admission_token.as_deref())
        .ok_or_else(|| {
            "Supervisor admission has no reservation identity; reconcile before retrying."
                .to_string()
        })?;
    let recorded=admitted_turn_id(history,token).ok_or_else(||"Prepared supervisor admission has no verified turn receipt. Reconcile the reservation before retrying.".to_string())?;
    if value.reservation.is_some() {
        value.admitted_turn_id = Some(recorded.to_string());
        value.admission_token = value.reservation.take();
        value.in_flight = true;
        value.attempts = value.attempts.saturating_add(1);
    } else if value.admitted_turn_id.as_deref() != Some(recorded) {
        return Err("Recorded turn identity does not match this admission reservation; reconcile before retrying.".into());
    }
    let id = value.admitted_turn_id.as_deref().ok_or_else(|| {
        "Supervisor admission has no recorded turn identity; reconcile before retrying.".to_string()
    })?;
    for item in history.iter().rev() {
        match item {
            RolloutItem::EventMsg(EventMsg::TurnComplete(turn)) if turn.turn_id == id => {
                value.in_flight = false;
                value.suspended |= turn.error.is_some();
                return Ok(());
            }
            RolloutItem::EventMsg(EventMsg::TurnAborted(turn))
                if turn.turn_id.as_deref() == Some(id) =>
            {
                value.in_flight = false;
                value.suspended = true;
                return Ok(());
            }
            _ => {}
        }
    }
    Err(
        "Supervisor admission has no terminal turn receipt. Reconcile its outcome before retrying."
            .into(),
    )
}

pub(super) fn admitted_turn_id<'a>(
    history: &'a [codex_rollout::RolloutItem],
    token: &str,
) -> Option<&'a str> {
    use codex_protocol::protocol::EventMsg;
    use codex_rollout::RolloutItem;
    let trigger = format!("supervisor_inbox:{token}");
    history.iter().rev().find_map(|item| match item {
        RolloutItem::EventMsg(EventMsg::TurnStarted(turn))
            if turn
                .turn_attribution
                .as_ref()
                .and_then(|a| a.turn_trigger.as_deref())
                == Some(trigger.as_str()) =>
        {
            Some(turn.turn_id.as_str())
        }
        _ => None,
    })
}
