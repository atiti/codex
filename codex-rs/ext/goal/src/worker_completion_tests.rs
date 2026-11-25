use super::*;
use serde_json::json;
use tempfile::tempdir;

#[test]
fn bound_worker_cannot_complete_until_packet_has_all_evidence() {
    let root = tempdir().unwrap();
    let home = root.path().join("codex");
    let workspace = root.path().join("workspace");
    let bindings = home.join("worker-goal-bindings");
    let packets = workspace.join("tasks/packets/private");
    std::fs::create_dir_all(&bindings).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bindings, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::create_dir_all(&packets).unwrap();
    std::fs::write(bindings.join("thread.json"), json!({"version":1,"thread_id":"thread","namespace":"private","packet":"sample","item":"worker","session":"owner"}).to_string()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            bindings.join("thread.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    let path = packets.join("sample.json");
    let mut packet = json!({"schema_version":1,"namespace":"private","id":"sample","history":[{"type":"complete","item":"worker","session":"owner","item_revision":2}],"items":[{"id":"worker","revision":2,"status":"active","assignment":{"session":"owner"},"next_step":{"state":"active"},"outcome_wait":null,"criteria":[{"evidence":[]}],"operations":{},"gates":{},"repositories":[{"path":"repo","initial_status":"clean","commit":"c","pr":"p","merge":"m","checks":"passed","clean":true,"evidence":["receipt"]}]}]});
    std::fs::write(&path, packet.to_string()).unwrap();
    assert!(validate(&home, &workspace, "thread", true).is_err());
    packet["items"][0]["status"] = json!("done");
    packet["items"][0]["assignment"] = Value::Null;
    packet["items"][0]["next_step"]["state"] = json!("done");
    packet["items"][0]["criteria"][0]["evidence"] = json!(["test"]);
    for key in [
        "implementation",
        "checks",
        "merge",
        "deployment",
        "acceptance",
        "handoff",
        "cleanup",
    ] {
        packet["items"][0]["gates"][key] = json!({"status":"passed","evidence":["receipt"]});
    }
    packet["items"][0]["operations"] = json!({"action":{"state":"unknown"}});
    std::fs::write(&path, packet.to_string()).unwrap();
    assert!(validate(&home, &workspace, "thread", true).is_err());
    packet["items"][0]["operations"] = json!({"action":{"state":"verified"}});
    std::fs::write(&path, packet.to_string()).unwrap();
    assert!(validate(&home, &workspace, "thread", true).is_ok());
    // A later completion belongs to a different owner; old evidence must not transfer.
    packet["history"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"complete","item":"worker","session":"other"}));
    std::fs::write(&path, packet.to_string()).unwrap();
    assert!(validate(&home, &workspace, "thread", true).is_err());
}

#[test]
fn missing_required_binding_fails_closed() {
    let root = tempdir().unwrap();
    assert!(validate_with_requirement(root.path(), root.path(), "thread", true).is_err());
    assert!(validate_with_requirement(root.path(), root.path(), "thread", false).is_ok());
}

#[test]
fn claimed_worker_assignment_creates_exact_bound_goal() {
    let root = tempdir().unwrap();
    let home = root.path().join("codex");
    let workspace = root.path().join("workspace");
    let attempts = home.join("worker-goal-attempts");
    let packets = workspace.join("tasks/packets/private");
    std::fs::create_dir_all(&attempts).unwrap();
    std::fs::create_dir_all(&packets).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&attempts, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let attempt = "abcd-1234";
    let file = attempts.join(format!("{attempt}.json"));
    std::fs::write(&file, json!({"version":1,"attempt":attempt,"namespace":"private","packet":"sample","item":"worker","session":"owner","team":"team","member":"worker"}).to_string()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let path = packets.join("sample.json");
    let mut packet = json!({"schema_version":1,"namespace":"private","id":"sample","items":[{"id":"worker","status":"active","assignment":{"session":"owner"},"objective":"Build the feature","criteria":[{"text":"Verified acceptance"}]}]});
    std::fs::write(&path, packet.to_string()).unwrap();
    let goal = prepare_goal(&home, &workspace, "thread", attempt).unwrap();
    assert!(goal.contains("Build the feature") && goal.contains("Verified acceptance"));
    assert!(home.join("worker-goal-bindings/thread.json").exists());
    packet["items"][0]["assignment"]["session"] = json!("other");
    std::fs::write(&path, packet.to_string()).unwrap();
    assert!(prepare_goal(&home, &workspace, "other-thread", attempt).is_err());
    // A resumed worker must retain its exact original thread and owner.
    packet["items"][0]["assignment"]["session"] = json!("owner");
    std::fs::write(&path, packet.to_string()).unwrap();
    let resumed = attempts.join("dcba-5678.json");
    std::fs::write(&resumed, json!({"version":1,"attempt":"dcba-5678","namespace":"private","packet":"sample","item":"worker","session":"owner","team":"team","member":"worker","resume_thread":"thread"}).to_string()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&resumed, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert!(prepare_goal(&home, &workspace, "thread", "dcba-5678").is_ok());
    assert!(prepare_goal(&home, &workspace, "wrong-thread", "dcba-5678").is_err());
}
