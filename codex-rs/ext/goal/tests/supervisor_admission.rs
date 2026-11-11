//! Durable reservation recovery refuses unknown admission and reconciles exact host turn receipts.
#![allow(dead_code)]
#[path = "../src/supervisor_admission.rs"]
mod admission;
use admission::Cursor;
use admission::cursor;
use admission::reconcile_history;
use admission::save_cursor;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_rollout::RolloutItem;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn post_admission_write_failure_survives_restart_and_requires_exact_turn_receipt()
-> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let thread = "11111111-1111-4111-8111-111111111111";
    let prepared = Cursor {
        reservation: Some("reserved-1".into()),
        admission_token: Some("reserved-1".into()),
        action_key: 42,
        ..Cursor::default()
    };
    save_cursor(home.path(), thread, &prepared).map_err(anyhow::Error::msg)?;
    let mut started = Cursor {
        generation: 1,
        attempts: 1,
        reservation: None,
        admitted_turn_id: Some("turn-1".into()),
        in_flight: true,
        action_key: 42,
        ..Cursor::default()
    };
    let path = home
        .path()
        .join("supervisor-inbox-cursors")
        .join(format!("{thread}.json"));
    let retained = path.with_extension("prepared");
    // Force atomic replacement to fail after admission; retain the original durable intent.
    std::fs::rename(&path, &retained)?;
    std::fs::create_dir(&path)?;
    assert!(save_cursor(home.path(), thread, &started).is_err());
    std::fs::remove_dir(&path)?;
    std::fs::rename(retained, &path)?;
    started = cursor(home.path(), thread).map_err(anyhow::Error::msg)?;
    assert_eq!(started, prepared);
    assert!(reconcile_history(&mut started, &[]).is_err());
    assert_eq!(
        started, prepared,
        "absence of a receipt cannot authorize another inference"
    );
    let start: TurnStartedEvent = serde_json::from_value(
        json!({"turn_id":"turn-1","turn_attribution":{"turn_id":"turn-1","turn_trigger":"supervisor_inbox:reserved-1","parent_turn_id":null,"initiating_agent_path":null,"root_turn_id":null}}),
    )?;
    let completed: TurnCompleteEvent = serde_json::from_value(
        json!({"turn_id":"turn-1","last_agent_message":"Owner disposition verified"}),
    )?;
    let history = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(start)),
        RolloutItem::EventMsg(EventMsg::TurnComplete(completed)),
    ];
    reconcile_history(&mut started, &history).map_err(anyhow::Error::msg)?;
    assert_eq!(
        started,
        Cursor {
            action_key: 42,
            attempts: 1,
            admitted_turn_id: Some("turn-1".into()),
            admission_token: Some("reserved-1".into()),
            ..Cursor::default()
        }
    );
    save_cursor(home.path(), thread, &started).map_err(anyhow::Error::msg)?;
    assert_eq!(
        cursor(home.path(), thread).map_err(anyhow::Error::msg)?,
        started
    );
    Ok(())
}

#[path = "../src/supervisor_resolution.rs"]
mod resolution;

#[cfg(unix)]
#[test]
fn operator_resolution_is_bound_private_and_requires_exact_evidence() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir()?;
    let thread = "11111111-1111-4111-8111-111111111111";
    let root = home.path().join("supervisor-admission-resolutions");
    let directory = root.join(thread);
    std::fs::create_dir_all(&directory)?;
    for p in [&root, &directory] {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
    }
    let path = directory.join("reservation-1.json");
    let mut record = json!({"version":1,"thread_id":thread,"reservation":"reservation-1","decision":"not_admitted","evidence":["verified no admitted turn or external effects"],"authority":"explicit synthetic operator decision","created_at":1.0});
    std::fs::write(&path, serde_json::to_vec(&record)?)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    assert!(
        resolution::load(home.path(), thread, "reservation-1")
            .map_err(anyhow::Error::msg)?
            .is_some()
    );
    record["thread_id"] = json!("other-thread");
    std::fs::write(&path, serde_json::to_vec(&record)?)?;
    assert!(resolution::load(home.path(), thread, "reservation-1").is_err());
    record["thread_id"] = json!(thread);
    record["authority"] = json!("");
    std::fs::write(&path, serde_json::to_vec(&record)?)?;
    assert!(resolution::load(home.path(), thread, "reservation-1").is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
    assert!(resolution::load(home.path(), thread, "reservation-1").is_err());
    Ok(())
}

#[test]
fn completed_previous_turn_is_not_proof_for_a_new_reservation() -> anyhow::Result<()> {
    let previous: TurnStartedEvent = serde_json::from_value(
        json!({"turn_id":"turn-a","turn_attribution":{"turn_id":"turn-a","turn_trigger":"supervisor_inbox:reservation-a","parent_turn_id":null,"initiating_agent_path":null,"root_turn_id":null}}),
    )?;
    let mut history = vec![RolloutItem::EventMsg(EventMsg::TurnStarted(previous))];
    let mut pending = Cursor {
        reservation: Some("reservation-b".into()),
        admission_token: Some("reservation-b".into()),
        admitted_turn_id: Some("turn-a".into()),
        ..Cursor::default()
    };
    assert_eq!(
        admission::admitted_turn_id(&history, "reservation-b"),
        None,
        "previous turn must not reject an evidenced not-admitted decision for the new reservation"
    );
    assert!(reconcile_history(&mut pending, &history).is_err());
    let current: TurnStartedEvent = serde_json::from_value(
        json!({"turn_id":"turn-b","turn_attribution":{"turn_id":"turn-b","turn_trigger":"supervisor_inbox:reservation-b","parent_turn_id":null,"initiating_agent_path":null,"root_turn_id":null}}),
    )?;
    history.push(RolloutItem::EventMsg(EventMsg::TurnStarted(current)));
    assert_eq!(
        admission::admitted_turn_id(&history, "reservation-b"),
        Some("turn-b"),
        "current-token admission must reject not-admitted resolution"
    );
    Ok(())
}
