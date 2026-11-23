//! Explicit operator decisions settle only one admission; they grant no external effect authority.
use super::admission::Cursor;
use serde::Deserialize;
use std::io::Read;
use std::path::Path;

#[derive(Deserialize)]
pub(super) struct Resolution {
    version: u8,
    thread_id: String,
    reservation: String,
    decision: Decision,
    evidence: Vec<String>,
    authority: String,
    created_at: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Decision {
    NotAdmitted,
    Completed,
    Interrupted,
}

pub(super) async fn consume(
    home: &Path,
    thread_id: codex_protocol::ThreadId,
    thread: &codex_core::CodexThread,
    value: &mut Cursor,
) -> Result<bool, String> {
    let Some(token) = value
        .reservation
        .as_ref()
        .or(value.admission_token.as_ref())
        .cloned()
    else {
        return Ok(false);
    };
    if value.last_resolution.as_ref() == Some(&token)
        || (value.reservation.is_none() && !value.in_flight)
    {
        return Ok(false);
    }
    let thread_id = thread_id.to_string();
    let Some(resolution) = load(home, &thread_id, &token)? else {
        return Ok(false);
    };
    let history = thread
        .load_turn_events(format!("supervisor_inbox:{token}"))
        .await
        .map_err(|e| e.to_string())?;
    let recorded_id = super::admission::admitted_turn_id(&history, &token);
    if let Some(id) = recorded_id
        && thread.current_turn_model(id).await.is_some()
    {
        return Err("Cannot reconcile an admission while its turn is still running".into());
    }
    match resolution.decision {
        Decision::Completed => {
            let suspended = value.suspended;
            let mut verified = value.clone();
            verified.suspended = false;
            super::admission::reconcile(thread, &mut verified).await?;
            if verified.in_flight || verified.suspended {
                return Err(
                    "Completed admission resolution requires a successful terminal turn receipt"
                        .into(),
                );
            }
            *value = verified;
            value.suspended = suspended;
        }
        Decision::NotAdmitted => {
            if recorded_id.is_some() {
                return Err("Not-admitted resolution contradicts a recorded turn admission".into());
            }
            value.admitted_turn_id = None;
            value.notice_event_ids.clear();
            value.in_flight = false;
            value.reservation = None;
        }
        Decision::Interrupted => {
            value.admitted_turn_id = recorded_id.map(str::to_string);
            value.in_flight = false;
            value.reservation = None;
            value.suspended = true;
        }
    }
    value.last_resolution = Some(token);
    Ok(true)
}

pub(super) fn load(
    home: &Path,
    thread_id: &str,
    token: &str,
) -> Result<Option<Resolution>, String> {
    if token.is_empty()
        || token.len() > 160
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("Invalid admission reservation identity".into());
    }
    let directory = home
        .join("supervisor-admission-resolutions")
        .join(thread_id);
    let path = directory.join(format!("{token}.json"));
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if !metadata.is_file() || metadata.len() > 8192 {
        return Err("Admission resolution must be a bounded regular file".into());
    }
    for component in [
        path.as_path(),
        directory.as_path(),
        directory.parent().ok_or("Missing resolution directory")?,
    ] {
        let entry = std::fs::symlink_metadata(component).map_err(|e| e.to_string())?;
        if entry.file_type().is_symlink() {
            return Err("Admission resolution must not follow symlinks".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let owner = std::fs::metadata(home).map_err(|e| e.to_string())?.uid();
            if entry.uid() != owner || entry.mode() & 0o077 != 0 {
                return Err("Admission resolution must be owner-only".into());
            }
        }
    }
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(/*limit*/ 8193)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 8192 {
        return Err("Admission resolution exceeds size limit".into());
    }
    let resolution: Resolution = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if resolution.version != 1
        || resolution.thread_id != thread_id
        || resolution.reservation != token
        || resolution.authority.trim().is_empty()
        || resolution.evidence.is_empty()
        || resolution.evidence.len() > 16
        || resolution.evidence.iter().any(|e| e.trim().is_empty())
        || !resolution.created_at.is_finite()
    {
        return Err(
            "Invalid bound admission resolution or missing exact operator decision/evidence".into(),
        );
    }
    Ok(Some(resolution))
}
