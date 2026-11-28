use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

/// Registries larger than this are reported as unknown rather than parsed.
const MAX_REGISTRY_BYTES: usize = 4_000_000;
/// The status line and terminal title ask for the wait status on every frame while a
/// turn runs. Re-check the registry and daemon at most this often per thread.
const RECHECK_INTERVAL: Duration = Duration::from_secs(1);
/// Bound the per-thread cache; a TUI normally shows only a handful of threads.
const MAX_CACHED_THREADS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum WaitStatus {
    None,
    Unknown,
    Unavailable,
    Waiting(String),
}

pub(super) fn agentroute_wait_status(thread_id: &str) -> WaitStatus {
    static CACHE: Mutex<Option<WaitStatusCache>> = Mutex::new(None);
    let home = std::env::var_os("AGENTROUTE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".agentroute")));
    let Some(home) = home else {
        return WaitStatus::None;
    };
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.get_or_insert_with(WaitStatusCache::default).status(
        &home.join("state/waits"),
        thread_id,
        Instant::now(),
    )
}

/// Identity and freshness of the registry file. A cached parse is reused only while
/// all of these are unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileKey {
    dev: u64,
    ino: u64,
    mtime: Option<SystemTime>,
    len: u64,
}

impl FileKey {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        let (dev, ino) = {
            use std::os::unix::fs::MetadataExt;
            (metadata.dev(), metadata.ino())
        };
        #[cfg(not(unix))]
        let (dev, ino) = (0, 0);
        Self {
            dev,
            ino,
            mtime: metadata.modified().ok(),
            len: metadata.len(),
        }
    }
}

/// Result of evaluating the registry for one thread, before the daemon liveness check.
/// It depends only on the registry contents, so it can be cached by [`FileKey`].
#[derive(Clone, Debug, PartialEq, Eq)]
enum Classified {
    Final(WaitStatus),
    /// An active wait; it is shown with this text only while the daemon is alive.
    NeedsDaemon(String),
}

struct CacheEntry {
    dir: PathBuf,
    /// `None` when the result must not be reused for a later check (missing file,
    /// stat error, or a file that changed while it was being read).
    key: Option<FileKey>,
    classified: Classified,
    status: WaitStatus,
    checked_at: Instant,
}

/// Per-thread wait status cache. The registry is re-parsed only when its path,
/// device/inode, mtime, or length changes, and is stat'ed at most once per
/// [`RECHECK_INTERVAL`]. The daemon liveness check is repeated on every re-check
/// because it does not depend on the registry contents.
#[derive(Default)]
struct WaitStatusCache {
    entries: HashMap<String, CacheEntry>,
    /// Number of registry reads, observable by tests.
    #[cfg(test)]
    parses: u64,
}

impl WaitStatusCache {
    fn status(&mut self, dir: &Path, thread_id: &str, now: Instant) -> WaitStatus {
        let previous = self
            .entries
            .get(thread_id)
            .filter(|entry| entry.dir.as_path() == dir);
        if let Some(entry) = previous
            && now.saturating_duration_since(entry.checked_at) < RECHECK_INTERVAL
        {
            return entry.status.clone();
        }
        let path = dir.join("waits.json");
        let (key, classified) = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                let current = FileKey::from_metadata(&metadata);
                match previous {
                    Some(entry) if entry.key == Some(current) => {
                        (entry.key, entry.classified.clone())
                    }
                    _ => {
                        #[cfg(test)]
                        {
                            self.parses += 1;
                        }
                        read_and_classify(&path, &metadata, thread_id)
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                (None, Classified::Final(WaitStatus::None))
            }
            _ => (None, Classified::Final(WaitStatus::Unknown)),
        };
        let status = finish(dir, &classified);
        if self.entries.len() >= MAX_CACHED_THREADS && !self.entries.contains_key(thread_id) {
            self.entries.clear();
        }
        self.entries.insert(
            thread_id.to_string(),
            CacheEntry {
                dir: dir.to_path_buf(),
                key,
                classified,
                status: status.clone(),
                checked_at: now,
            },
        );
        status
    }
}

#[cfg(test)]
fn wait_status_from_dir(dir: &Path, thread_id: &str) -> WaitStatus {
    let path = dir.join("waits.json");
    let classified = match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => read_and_classify(&path, &metadata, thread_id).1,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Classified::Final(WaitStatus::None),
        _ => Classified::Final(WaitStatus::Unknown),
    };
    finish(dir, &classified)
}

/// Reads and classifies the registry whose `symlink_metadata` was `metadata`. Returns
/// the cache key only when the opened file still matches that metadata after reading.
fn read_and_classify(
    path: &Path,
    metadata: &std::fs::Metadata,
    thread_id: &str,
) -> (Option<FileKey>, Classified) {
    use std::io::Read;
    let unknown = (None, Classified::Final(WaitStatus::Unknown));
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (None, Classified::Final(WaitStatus::None));
        }
        Err(_) => return unknown,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(opened) = file.metadata() else {
            return unknown;
        };
        if opened.ino() != metadata.ino() || opened.dev() != metadata.dev() {
            return unknown;
        }
    }
    let mut raw = Vec::new();
    if (&file)
        .take(MAX_REGISTRY_BYTES as u64 + 1)
        .read_to_end(&mut raw)
        .is_err()
    {
        return unknown;
    }
    let expected = FileKey::from_metadata(metadata);
    let key = file
        .metadata()
        .ok()
        .map(|opened| FileKey::from_metadata(&opened))
        .filter(|opened| *opened == expected);
    if raw.len() > MAX_REGISTRY_BYTES {
        return (key, Classified::Final(WaitStatus::Unknown));
    }
    (key, classify(&raw, thread_id))
}

fn classify(raw: &[u8], thread_id: &str) -> Classified {
    let unknown = Classified::Final(WaitStatus::Unknown);
    let Ok(data) = serde_json::from_slice::<serde_json::Value>(raw) else {
        return unknown;
    };
    if data.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
        return unknown;
    }
    let Some(items) = data.get("waits").and_then(|v| v.as_array()) else {
        return unknown;
    };
    if items.iter().any(|w| {
        w.get("thread_id").and_then(|v| v.as_str()) == Some(thread_id)
            && !matches!(
                w.get("state").and_then(|v| v.as_str()),
                Some("pending" | "fired" | "delivered" | "cancelled" | "undeliverable")
            )
    }) {
        return unknown;
    }
    let relevant: Vec<_> = items
        .iter()
        .filter(|w| {
            w.get("thread_id").and_then(|v| v.as_str()) == Some(thread_id)
                && matches!(
                    w.get("state").and_then(|v| v.as_str()),
                    Some("pending" | "fired")
                )
        })
        .collect();
    if relevant.is_empty() {
        if items.iter().any(|w| {
            w.get("thread_id").and_then(|v| v.as_str()) == Some(thread_id)
                && w.get("state").and_then(|v| v.as_str()) == Some("undeliverable")
        }) {
            return Classified::Final(WaitStatus::Waiting("Wake delivery failed".into()));
        }
        return Classified::Final(WaitStatus::None);
    }
    if relevant.iter().any(|w| {
        w.get("state").and_then(|v| v.as_str()) == Some("fired")
            && w.get("delivery")
                .and_then(|v| v.get("state"))
                .and_then(|v| v.as_str())
                == Some("unknown")
    }) {
        return Classified::Final(WaitStatus::Waiting("Wake delivery uncertain".into()));
    }
    let w = relevant
        .iter()
        .find(|w| w.get("state").and_then(|v| v.as_str()) == Some("fired"))
        .copied()
        .unwrap_or(relevant[0]);
    let state = w.get("state").and_then(|v| v.as_str()).unwrap_or("unknown");
    if state == "fired" {
        let delivery = w
            .get("delivery")
            .and_then(|v| v.get("state"))
            .and_then(|v| v.as_str());
        return Classified::NeedsDaemon(match delivery {
            Some("unknown") => "Wake delivery uncertain".into(),
            Some("inbox_pending") => "Wake pending owner".into(),
            _ => "Wake pending delivery".into(),
        });
    }
    let kind = w.get("kind").and_then(|v| v.as_str()).unwrap_or("event");
    let label = match kind {
        "pr" | "run" => "CI",
        "packet" => "work packet",
        "timer" => "timer",
        "file" => "file",
        "event" => "event",
        _ => "condition",
    };
    let deadline = w
        .get("deadline_at")
        .and_then(serde_json::Value::as_f64)
        .and_then(|v| chrono::DateTime::from_timestamp(v as i64, 0))
        .map(|v| v.with_timezone(&chrono::Local).format("%H:%M").to_string());
    let extra = if relevant.len() > 1 {
        format!(" +{}", relevant.len() - 1)
    } else {
        String::new()
    };
    Classified::NeedsDaemon(format!(
        "Waiting {label}{}{}",
        deadline.map(|v| format!(" until {v}")).unwrap_or_default(),
        extra
    ))
}

/// Applies the daemon liveness check, which is never cached beyond one re-check.
fn finish(dir: &Path, classified: &Classified) -> WaitStatus {
    let detail = match classified {
        Classified::Final(status) => return status.clone(),
        Classified::NeedsDaemon(detail) => detail,
    };
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let Ok(lock) = std::fs::OpenOptions::new()
            .read(true)
            .open(dir.join("daemon.lock"))
        else {
            return WaitStatus::Unavailable;
        };
        // The daemon holds an exclusive flock for its lifetime; never block the UI.
        let acquired = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if acquired == 0 {
            unsafe {
                libc::flock(lock.as_raw_fd(), libc::LOCK_UN);
            }
            return WaitStatus::Unavailable;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::WouldBlock {
            return WaitStatus::Unknown;
        }
        WaitStatus::Waiting(detail.clone())
    }
    #[cfg(not(unix))]
    {
        let _ = (dir, detail);
        WaitStatus::Unknown
    }
}

#[cfg(test)]
mod agentroute_wait_status_tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use tempfile::tempdir;

    #[test]
    fn shows_wait_and_never_assumes_a_dead_daemon_will_wake_it() {
        let dir = tempdir().unwrap();
        let registry = dir.path().join("waits.json");
        let lock = std::fs::File::create(dir.path().join("daemon.lock")).unwrap();
        std::fs::write(&registry, r#"{"version":1,"waits":[{"thread_id":"a","state":"pending","kind":"run","deadline_at":1794589000}]}"#).unwrap();
        assert_eq!(wait_status_from_dir(dir.path(), "b"), WaitStatus::None);
        assert_eq!(
            wait_status_from_dir(dir.path(), "a"),
            WaitStatus::Unavailable
        );
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(
            matches!(wait_status_from_dir(dir.path(), "a"), WaitStatus::Waiting(s) if s.contains("CI") && s.contains("until"))
        );
        let mut file = std::fs::File::create(&registry).unwrap();
        write!(file, "{{bad").unwrap();
        assert_eq!(wait_status_from_dir(dir.path(), "a"), WaitStatus::Unknown);
        unsafe {
            libc::flock(lock.as_raw_fd(), libc::LOCK_UN);
        }
    }

    #[test]
    fn uncertain_delivery_is_not_misreported_as_ready() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("waits.json"),
            r#"{"version":1,"waits":[{"thread_id":"a","state":"fired","delivery":{"state":"unknown"}}]}"#,
        )
        .unwrap();
        assert_eq!(
            wait_status_from_dir(dir.path(), "a"),
            WaitStatus::Waiting("Wake delivery uncertain".into())
        );
    }

    const PENDING: &str =
        r#"{"version":1,"waits":[{"thread_id":"a","state":"pending","kind":"run"}]}"#;
    const PACKET: &str =
        r#"{"version":1,"waits":[{"thread_id":"a","state":"pending","kind":"packet"}]}"#;

    fn hold_daemon_lock(dir: &std::path::Path) -> std::fs::File {
        let lock = std::fs::File::create(dir.join("daemon.lock")).unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        lock
    }

    fn set_mtime(path: &std::path::Path, secs: u64) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    #[test]
    fn unchanged_registry_is_parsed_once() {
        let dir = tempdir().unwrap();
        let _lock = hold_daemon_lock(dir.path());
        std::fs::write(dir.path().join("waits.json"), PENDING).unwrap();
        let mut cache = WaitStatusCache::default();
        let start = Instant::now();
        for step in 0..10u64 {
            let now = start + Duration::from_millis(step * 700);
            assert_eq!(
                cache.status(dir.path(), "a", now),
                WaitStatus::Waiting("Waiting CI".into())
            );
        }
        assert_eq!(cache.parses, 1);
    }

    #[test]
    fn modified_registry_is_reparsed_after_the_interval() {
        let dir = tempdir().unwrap();
        let registry = dir.path().join("waits.json");
        let _lock = hold_daemon_lock(dir.path());
        std::fs::write(&registry, PENDING).unwrap();
        set_mtime(&registry, 1_000);
        let mut cache = WaitStatusCache::default();
        let start = Instant::now();
        assert_eq!(
            cache.status(dir.path(), "a", start),
            WaitStatus::Waiting("Waiting CI".into())
        );

        // New length: re-read.
        std::fs::write(&registry, PACKET).unwrap();
        set_mtime(&registry, 1_000);
        // Within the interval the cached value is served without touching the file.
        let soon = start + RECHECK_INTERVAL / 2;
        assert_eq!(
            cache.status(dir.path(), "a", soon),
            WaitStatus::Waiting("Waiting CI".into())
        );
        assert_eq!(cache.parses, 1);
        let later = start + RECHECK_INTERVAL;
        assert_eq!(
            cache.status(dir.path(), "a", later),
            WaitStatus::Waiting("Waiting work packet".into())
        );
        assert_eq!(cache.parses, 2);

        // Same length, new mtime: re-read.
        let same_len =
            r#"{"version":1,"waits":[{"thread_id":"a","state":"pending","kind":"file"}]}  "#;
        assert_eq!(same_len.len(), PACKET.len());
        std::fs::write(&registry, same_len).unwrap();
        set_mtime(&registry, 2_000);
        assert_eq!(
            cache.status(dir.path(), "a", later + RECHECK_INTERVAL),
            WaitStatus::Waiting("Waiting file".into())
        );
        assert_eq!(cache.parses, 3);
    }

    #[test]
    fn daemon_liveness_is_rechecked_without_reparsing() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("waits.json"), PENDING).unwrap();
        let lock = hold_daemon_lock(dir.path());
        let mut cache = WaitStatusCache::default();
        let start = Instant::now();
        assert_eq!(
            cache.status(dir.path(), "a", start),
            WaitStatus::Waiting("Waiting CI".into())
        );
        drop(lock);
        assert_eq!(
            cache.status(dir.path(), "a", start + RECHECK_INTERVAL),
            WaitStatus::Unavailable
        );
        assert_eq!(cache.parses, 1);
    }

    #[test]
    fn missing_registry_is_not_parsed_and_appears_after_the_interval() {
        let dir = tempdir().unwrap();
        let _lock = hold_daemon_lock(dir.path());
        let mut cache = WaitStatusCache::default();
        let start = Instant::now();
        assert_eq!(cache.status(dir.path(), "a", start), WaitStatus::None);
        assert_eq!(cache.parses, 0);
        std::fs::write(dir.path().join("waits.json"), PENDING).unwrap();
        assert_eq!(
            cache.status(dir.path(), "a", start + RECHECK_INTERVAL),
            WaitStatus::Waiting("Waiting CI".into())
        );
        assert_eq!(cache.parses, 1);
    }

    #[test]
    fn replaced_registry_with_same_size_and_mtime_is_reparsed() {
        let dir = tempdir().unwrap();
        let registry = dir.path().join("waits.json");
        let _lock = hold_daemon_lock(dir.path());
        std::fs::write(&registry, PENDING).unwrap();
        set_mtime(&registry, 1_000);
        let mut cache = WaitStatusCache::default();
        let start = Instant::now();
        cache.status(dir.path(), "a", start);
        // Atomic replace: same length and mtime, new inode.
        let staged = dir.path().join("waits.json.tmp");
        std::fs::write(&staged, PENDING.replace("\"a\"", "\"z\"")).unwrap();
        set_mtime(&staged, 1_000);
        std::fs::rename(&staged, &registry).unwrap();
        assert_eq!(
            cache.status(dir.path(), "a", start + RECHECK_INTERVAL),
            WaitStatus::None
        );
        assert_eq!(cache.parses, 2);
    }

    #[test]
    fn oversized_and_symlinked_registries_stay_unknown() {
        let dir = tempdir().unwrap();
        let registry = dir.path().join("waits.json");
        std::fs::write(&registry, vec![b' '; MAX_REGISTRY_BYTES + 1]).unwrap();
        let mut cache = WaitStatusCache::default();
        let start = Instant::now();
        assert_eq!(cache.status(dir.path(), "a", start), WaitStatus::Unknown);
        assert_eq!(
            cache.status(dir.path(), "a", start + RECHECK_INTERVAL),
            WaitStatus::Unknown
        );
        assert_eq!(cache.parses, 1);

        let other = tempdir().unwrap();
        let target = other.path().join("real.json");
        std::fs::write(&target, PENDING).unwrap();
        std::os::unix::fs::symlink(&target, other.path().join("waits.json")).unwrap();
        assert_eq!(cache.status(other.path(), "b", start), WaitStatus::Unknown);
    }
}
