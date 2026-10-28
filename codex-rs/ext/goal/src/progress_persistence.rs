//! Small, private thread-scoped review state. Conversation contents stay in the rollout.
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde::Serialize;

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct SavedReview {
    pub active_seconds: u64,
    #[serde(default)]
    pub report_seconds: u64,
    pub previous: Option<(u64, String)>,
}

pub(crate) fn path(home: &Path, thread_id: &str) -> PathBuf {
    home.join("progress-review")
        .join(format!("{thread_id}.json"))
}

pub(crate) fn load(path: &Path) -> SavedReview {
    std::fs::read(path)
        .ok()
        .filter(|data| data.len() <= 4096)
        .and_then(|data| serde_json::from_slice::<SavedReview>(&data).ok())
        .filter(|state| {
            state.active_seconds <= 600
                && state.report_seconds <= 600
                && state
                    .previous
                    .as_ref()
                    .is_none_or(|(_, text)| text.len() <= 1200)
        })
        .unwrap_or_default()
}

pub(crate) fn save(path: &Path, state: &SavedReview) -> std::io::Result<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("missing review state directory"))?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let temporary = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        serde_json::to_writer(&mut file, state)?;
        file.flush()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
#[path = "progress_persistence_tests.rs"]
mod tests;
