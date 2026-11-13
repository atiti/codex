//! Fail closed when a packet-bound worker requests goal completion.
use std::io::Read;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

const MAX_BYTES: u64 = 2_000_000;

#[derive(Deserialize)]
struct Binding {
    version: u8,
    thread_id: String,
    namespace: String,
    packet: String,
    item: String,
    session: String,
}

fn slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err("worker completion data must be a bounded regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err("worker completion data must belong to the current user".into());
        }
    }
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = file.metadata().map_err(|e| e.to_string())?;
        if opened.ino() != metadata.ino() || opened.dev() != metadata.dev() {
            return Err("worker completion data changed during open".into());
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("worker completion data too large".into());
    }
    Ok(bytes)
}

pub(crate) fn validate(home: &Path, workspace: &Path, thread: &str) -> Result<(), String> {
    let required = std::env::var_os("AGENTROUTE_REQUIRE_WORKER_BINDING").as_deref()
        == Some(std::ffi::OsStr::new("1"));
    validate_with_requirement(home, workspace, thread, required)
}

fn validate_with_requirement(
    home: &Path,
    workspace: &Path,
    thread: &str,
    required: bool,
) -> Result<(), String> {
    let path = home
        .join("worker-goal-bindings")
        .join(format!("{thread}.json"));
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if required {
                return Err(
                    "worker goal binding is missing; register the packet owner first".into(),
                );
            }
            return Ok(());
        }
        Err(e) => return Err(e.to_string()),
        Ok(_) => {}
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let directory = std::fs::symlink_metadata(home.join("worker-goal-bindings"))
            .map_err(|e| e.to_string())?;
        let metadata = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if !directory.is_dir()
            || directory.uid() != unsafe { libc::geteuid() }
            || directory.mode() & 0o077 != 0
            || metadata.mode() & 0o077 != 0
        {
            return Err("worker goal binding must be owner-private".into());
        }
    }
    let binding: Binding =
        serde_json::from_slice(&read_bounded(&path)?).map_err(|e| e.to_string())?;
    if binding.version != 1
        || binding.thread_id != thread
        || !slug(&binding.namespace)
        || !slug(&binding.packet)
        || !slug(&binding.item)
        || binding.session.is_empty()
    {
        return Err("invalid worker goal binding".into());
    }
    for dir in [
        workspace.join("tasks/packets"),
        workspace.join("tasks/packets").join(&binding.namespace),
    ] {
        if std::fs::symlink_metadata(&dir)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_symlink()
        {
            return Err("worker packet directory may not be a symlink".into());
        }
    }
    let packet = workspace
        .join("tasks/packets")
        .join(&binding.namespace)
        .join(format!("{}.json", binding.packet));
    let data: Value = serde_json::from_slice(&read_bounded(&packet)?).map_err(|e| e.to_string())?;
    if data["schema_version"] != 1
        || data["namespace"] != binding.namespace
        || data["id"] != binding.packet
    {
        return Err("worker packet identity mismatch".into());
    }
    let item = data["items"]
        .as_array()
        .and_then(|items| items.iter().find(|i| i["id"] == binding.item))
        .ok_or("worker packet outcome missing")?;
    let completed_by_owner = data["history"]
        .as_array()
        .and_then(|events| {
            events
                .iter()
                .rev()
                .find(|event| event["type"] == "complete" && event["item"] == binding.item)
        })
        .is_some_and(|event| event["session"] == binding.session);

    if !completed_by_owner {
        return Err("worker packet completion owner does not match binding".into());
    }
    if item["status"] != "done" || !item["assignment"].is_null() {
        return Err("worker packet outcome is not complete and released".into());
    }
    if !item["outcome_wait"].is_null() || item["next_step"]["state"] != "done" {
        return Err("worker packet has an unresolved wait or step".into());
    }
    if item["criteria"].as_array().is_none_or(|criteria| {
        criteria.is_empty() || criteria.iter().any(|c| !strings(&c["evidence"]))
    }) {
        return Err("worker packet criteria lack evidence".into());
    }
    if item["operations"].as_object().is_none_or(|ops| {
        ops.values()
            .any(|o| !matches!(o["state"].as_str(), Some("verified" | "failed")))
    }) {
        return Err("worker packet operations unresolved".into());
    }
    for name in [
        "implementation",
        "checks",
        "merge",
        "deployment",
        "acceptance",
        "handoff",
        "cleanup",
    ] {
        let gate = &item["gates"][name];
        let valid = match gate["status"].as_str() {
            Some("passed") => strings(&gate["evidence"]),
            Some("not_applicable") => gate["reason"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty()),
            _ => false,
        };
        if !valid {
            return Err(format!("worker packet gate {name} is unresolved"));
        }
    }
    let repos = item["repositories"]
        .as_array()
        .ok_or("worker repository evidence missing")?;
    if item["gates"]["merge"]["status"] == "passed" && repos.is_empty() {
        return Err("worker merged repository evidence missing".into());
    }
    for repo in repos {
        for key in ["path", "initial_status", "commit", "pr", "merge"] {
            if repo[key].as_str().is_none_or(|s| s.trim().is_empty()) {
                return Err(format!("worker repository {key} missing"));
            }
        }
        if repo["checks"] != "passed" || repo["clean"] != true || !strings(&repo["evidence"]) {
            return Err("worker repository checks or cleanup incomplete".into());
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "worker_completion_tests.rs"]
mod tests;

fn strings(value: &Value) -> bool {
    value.as_array().is_some_and(|entries| {
        !entries.is_empty()
            && entries
                .iter()
                .all(|v| v.as_str().is_some_and(|s| !s.trim().is_empty()))
    })
}

pub(crate) fn prepare_goal(
    home: &Path,
    workspace: &Path,
    thread: &str,
    attempt: &str,
) -> Result<String, String> {
    if attempt.is_empty()
        || attempt.len() > 64
        || !attempt.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
    {
        return Err("invalid worker attempt identity".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let directory = std::fs::symlink_metadata(home.join("worker-goal-attempts"))
            .map_err(|e| e.to_string())?;
        if !directory.is_dir()
            || directory.uid() != unsafe { libc::geteuid() }
            || directory.mode() & 0o077 != 0
        {
            return Err("worker attempt directory must be owner-private".into());
        }
    }
    let source = home
        .join("worker-goal-attempts")
        .join(format!("{attempt}.json"));
    let raw = read_bounded(&source)?;
    let mut binding: Value = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
    if binding["version"] != 1 || binding["attempt"] != attempt {
        return Err("worker attempt mismatch".into());
    }
    if !binding["resume_thread"].is_null() && binding["resume_thread"] != thread {
        return Err("worker resume thread identity mismatch".into());
    }
    let namespace = binding["namespace"].as_str().ok_or("missing namespace")?;
    let packet_id = binding["packet"].as_str().ok_or("missing packet")?;
    let item_id = binding["item"].as_str().ok_or("missing item")?;
    let session = binding["session"].as_str().ok_or("missing owner")?;
    if !slug(namespace) || !slug(packet_id) || !slug(item_id) || session.is_empty() {
        return Err("invalid worker assignment identity".into());
    }
    let packet_path = workspace
        .join("tasks/packets")
        .join(namespace)
        .join(format!("{packet_id}.json"));
    let packet: Value =
        serde_json::from_slice(&read_bounded(&packet_path)?).map_err(|e| e.to_string())?;
    if packet["schema_version"] != 1
        || packet["namespace"] != namespace
        || packet["id"] != packet_id
    {
        return Err("worker packet identity mismatch".into());
    }
    let item = packet["items"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["id"] == item_id))
        .ok_or("worker outcome missing")?;
    if item["status"] != "active" || item["assignment"]["session"] != session {
        return Err("worker claim is not active for the bound owner".into());
    }
    let objective = item["objective"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or("worker objective missing")?;
    let criteria = item["criteria"]
        .as_array()
        .ok_or("worker criteria missing")?
        .iter()
        .map(|c| c["text"].as_str().unwrap_or(""))
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>();
    if criteria.is_empty() {
        return Err("worker criteria missing".into());
    }
    let goal = format!(
        "Complete work packet {namespace}/{packet_id}/{item_id}: {objective}. Acceptance: {}. Complete only after canonical criteria, gates and effects are verified; stop and ask on genuine blockers.",
        criteria.join("; ")
    );
    codex_protocol::protocol::validate_thread_goal_objective(&goal)?;
    binding["thread_id"] = Value::String(thread.to_string());
    if let Some(object) = binding.as_object_mut() {
        object.remove("resume_thread");
    }
    let directory = home.join("worker-goal-bindings");
    std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    let target = directory.join(format!("{thread}.json"));
    if target.exists() {
        let previous: Value =
            serde_json::from_slice(&read_bounded(&target)?).map_err(|e| e.to_string())?;
        for key in [
            "version",
            "thread_id",
            "namespace",
            "packet",
            "item",
            "session",
            "team",
            "member",
        ] {
            if previous[key] != binding[key] {
                return Err("worker goal binding conflict".into());
            }
        }
    } else {
        use std::io::Write;
        let temp = directory.join(format!(".{thread}.{attempt}.tmp"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp).map_err(|e| e.to_string())?;
        file.write_all(
            serde_json::to_string(&binding)
                .map_err(|e| e.to_string())?
                .as_bytes(),
        )
        .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        std::fs::rename(temp, target).map_err(|e| e.to_string())?;
    }
    Ok(goal)
}
