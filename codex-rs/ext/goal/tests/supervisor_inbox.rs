//! Native consumer accepts the producer schema and conservatively handles debt.
#![allow(dead_code)]
#[path = "../src/supervisor_inbox.rs"]
mod inbox;
use codex_protocol::protocol::ThreadGoalStatus;
use pretty_assertions::assert_eq;
use serde_json::json;

fn snapshot(event: serde_json::Value) -> serde_json::Value {
    json!({"version":1,"role":"supervisor","thread_id":"11111111-1111-4111-8111-111111111111","team":"team","member":"supervisor","generation":1,"live_children":[],"events":[event]})
}

#[test]
fn delivered_waits_transport_receipts_and_unknown_debt() -> anyhow::Result<()> {
    let mut value = snapshot(
        json!({"id":"e","kind":"coordination","state":"persisted","actionable":true,"message":"Overlap"}),
    );
    let initial: inbox::Inbox = serde_json::from_value(value.clone())?;
    assert!(initial.actionable());
    for state in ["delivered", "acked"] {
        value["events"][0]["state"] = json!(state);
        value["generation"] = json!(5);
        let receipt: inbox::Inbox = serde_json::from_value(value.clone())?;
        assert_eq!(initial.action_key(), receipt.action_key());
    }
    value["events"][0] = json!({"id":"e","kind":"coordination","state":"dispositioned","actionable":false,"message":"Approval held","wait":{"owner":"user","delivery_state":"delivered","question_ref":"user-message-5","resume_condition":"User approval"}});
    let wait: inbox::Inbox = serde_json::from_value(value.clone())?;
    assert!(wait.quiet());
    assert!(!wait.actionable());
    assert!(wait.validate_stop(ThreadGoalStatus::Complete).is_err());
    assert_eq!(wait.validate_stop(ThreadGoalStatus::Blocked), Ok(()));
    value["events"][0]["wait"]["delivery_state"] = json!("unknown");
    assert!(serde_json::from_value::<inbox::Inbox>(value.clone())?.actionable());
    value["events"] = json!([]);
    value["live_children"] = json!([{"id":"worker","state":"running","observed_at":1.0}]);
    let stale: inbox::Inbox = serde_json::from_value(value.clone())?;
    assert!(stale.actionable());
    assert!(stale.notice().contains("worker"));
    value["live_children"] = json!([]);
    value["overflow"] = json!({"events":0,"children":0,"waiting_events":129});
    let omitted_waits: inbox::Inbox = serde_json::from_value(value.clone())?;
    assert!(omitted_waits.quiet());
    assert!(!omitted_waits.actionable());
    assert!(
        omitted_waits
            .validate_stop(ThreadGoalStatus::Complete)
            .is_err()
    );
    value["overflow"] = json!({"events":2,"children":0});
    assert!(
        serde_json::from_value::<inbox::Inbox>(value)?
            .validate_stop(ThreadGoalStatus::Complete)
            .is_err()
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn reader_rejects_symlinks_public_files_and_wrong_thread() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir()?;
    let directory = home.path().join("supervisor-inbox");
    std::fs::create_dir(&directory)?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    let id = "11111111-1111-4111-8111-111111111111";
    let path = directory.join(format!("{id}.json"));
    std::fs::write(
        &path,
        serde_json::to_vec(&snapshot(
            json!({"id":"e","state":"persisted","actionable":true,"kind":"coordination","message":"data"}),
        ))?,
    )?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    assert!(
        inbox::load(home.path(), id)
            .map_err(anyhow::Error::msg)?
            .is_some()
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
    assert!(inbox::load(home.path(), id).is_err());
    std::fs::remove_file(&path)?;
    std::os::unix::fs::symlink(home.path().join("other"), &path)?;
    assert!(inbox::load(home.path(), id).is_err());
    Ok(())
}
