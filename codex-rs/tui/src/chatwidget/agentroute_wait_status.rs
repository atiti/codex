use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum WaitStatus {
    None,
    Unknown,
    Unavailable,
    Waiting(String),
}

pub(super) fn agentroute_wait_status(thread_id: &str) -> WaitStatus {
    let home = std::env::var_os("AGENTROUTE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".agentroute")));
    let Some(home) = home else {
        return WaitStatus::None;
    };
    wait_status_from_dir(&home.join("state/waits"), thread_id)
}

fn wait_status_from_dir(dir: &std::path::Path, thread_id: &str) -> WaitStatus {
    use std::io::Read;
    let path = dir.join("waits.json");
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return WaitStatus::None,
        _ => return WaitStatus::Unknown,
    };
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return WaitStatus::None,
        Err(_) => return WaitStatus::Unknown,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(opened) = file.metadata() else {
            return WaitStatus::Unknown;
        };
        if opened.ino() != metadata.ino() || opened.dev() != metadata.dev() {
            return WaitStatus::Unknown;
        }
    }
    let mut raw = Vec::new();
    if file.take(4_000_001).read_to_end(&mut raw).is_err() || raw.len() > 4_000_000 {
        return WaitStatus::Unknown;
    }
    let Ok(data) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return WaitStatus::Unknown;
    };
    if data.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
        return WaitStatus::Unknown;
    }
    let Some(items) = data.get("waits").and_then(|v| v.as_array()) else {
        return WaitStatus::Unknown;
    };
    if items.iter().any(|w| {
        w.get("thread_id").and_then(|v| v.as_str()) == Some(thread_id)
            && !matches!(
                w.get("state").and_then(|v| v.as_str()),
                Some("pending" | "fired" | "delivered" | "cancelled" | "undeliverable")
            )
    }) {
        return WaitStatus::Unknown;
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
            return WaitStatus::Waiting("Wake delivery failed".into());
        }
        return WaitStatus::None;
    }
    if relevant.iter().any(|w| {
        w.get("state").and_then(|v| v.as_str()) == Some("fired")
            && w.get("delivery")
                .and_then(|v| v.get("state"))
                .and_then(|v| v.as_str())
                == Some("unknown")
    }) {
        return WaitStatus::Waiting("Wake delivery uncertain".into());
    }
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
    }
    #[cfg(not(unix))]
    {
        return WaitStatus::Unknown;
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
        return WaitStatus::Waiting(match delivery {
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
    WaitStatus::Waiting(format!(
        "Waiting {label}{}{}",
        deadline.map(|v| format!(" until {v}")).unwrap_or_default(),
        extra
    ))
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
}
