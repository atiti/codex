//! Bounded host admission receipts over canonical paginated/legacy lineage segments.
use super::LocalThreadStore;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_rollout::RolloutItem;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Seek;
use std::io::SeekFrom;

fn read_error(error: impl std::fmt::Display) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: error.to_string(),
    }
}

pub(super) async fn load(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    trigger: String,
) -> ThreadStoreResult<Vec<RolloutItem>> {
    if trigger.is_empty() || trigger.len() > 256 {
        return Err(ThreadStoreError::InvalidRequest {
            message: "Invalid turn receipt trigger".into(),
        });
    }
    crate::ThreadStore::flush_thread(store, thread_id).await?;
    let lineage = store
        .resolve_rollout_lineage_for_history_page(thread_id)
        .await?;
    tokio::task::spawn_blocking(move || read(lineage, &trigger))
        .await
        .map_err(read_error)?
}

fn read(
    lineage: super::rollout_lineage::RolloutLineage,
    trigger: &str,
) -> ThreadStoreResult<Vec<RolloutItem>> {
    let mut result = Vec::new();
    let mut admitted = None::<String>;
    for segment in lineage.segments() {
        let mut file = codex_rollout::open_rollout_seekable_reader(&segment.rollout_path, || false)
            .map_err(read_error)?;
        let end = match segment.end {
            Some(end) => end.end_byte_offset,
            None => file.seek(SeekFrom::End(0)).map_err(read_error)?,
        };
        file.seek(SeekFrom::Start(0)).map_err(read_error)?;
        let mut reader = BufReader::new(file);
        let mut bytes = Vec::new();
        let mut legacy_ordinal = 0;
        while reader.stream_position().map_err(read_error)? < end {
            bytes.clear();
            reader.read_until(b'\n', &mut bytes).map_err(read_error)?;
            if !bytes.ends_with(b"\n") || reader.stream_position().map_err(read_error)? > end {
                return Err(read_error("Incomplete canonical turn receipt prefix"));
            }
            let raw = serde_json::from_slice(&bytes).map_err(read_error)?;
            let line = codex_rollout::decode_rollout_line(raw).map_err(read_error)?;
            let ordinal = line.ordinal.unwrap_or(legacy_ordinal);
            legacy_ordinal += 1;
            if ordinal < segment.start_ordinal {
                continue;
            }
            match &line.item {
                RolloutItem::EventMsg(EventMsg::TurnStarted(turn))
                    if turn
                        .turn_attribution
                        .as_ref()
                        .and_then(|a| a.turn_trigger.as_deref())
                        == Some(trigger) =>
                {
                    if admitted.is_some() {
                        return Err(ThreadStoreError::InvalidRequest {
                            message:
                                "Duplicate turn admissions for one trigger; reconciliation required"
                                    .into(),
                        });
                    }
                    admitted = Some(turn.turn_id.clone());
                    result.push(line.item);
                }
                RolloutItem::EventMsg(EventMsg::TurnComplete(turn))
                    if admitted.as_deref() == Some(turn.turn_id.as_str()) =>
                {
                    result.truncate(1);
                    result.push(line.item);
                }
                RolloutItem::EventMsg(EventMsg::TurnAborted(turn))
                    if admitted.is_some() && turn.turn_id.as_deref() == admitted.as_deref() =>
                {
                    result.truncate(1);
                    result.push(line.item);
                }
                _ => {}
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::super::rollout_lineage::RolloutLineage;
    use super::super::rollout_lineage::RolloutLineageSegment;
    use super::*;
    use codex_protocol::protocol::HistoryPosition;
    use serde_json::json;

    #[test]
    fn receipt_scan_respects_prefix_and_rejects_duplicate_or_incomplete_admission() {
        let home = tempfile::TempDir::new().expect("temp");
        let path = home.path().join("receipt.jsonl");
        let line = |id: &str, trigger: &str| {
            format!(
                "{}\n",
                json!({"timestamp":"2026-11-10T00:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":id,"model_context_window":null,"turn_attribution":{"turn_id":id,"turn_trigger":trigger,"parent_turn_id":null,"initiating_agent_path":null,"root_turn_id":null}}})
            )
        };
        let prior = line("a", "other");
        let target = line("b", "exact");
        std::fs::write(&path, format!("{prior}{target}")).expect("write");
        let id = ThreadId::default();
        let lineage = |end| RolloutLineage {
            segments: vec![RolloutLineageSegment {
                rollout_id: id,
                rollout_path: path.clone(),
                start_ordinal: 0,
                end,
            }],
        };
        let receipt = read(lineage(None), "exact").expect("exact receipt");
        assert_eq!(receipt.len(), 1);
        let prefix = Some(HistoryPosition {
            thread_id: id,
            end_ordinal_exclusive: 1,
            end_byte_offset: prior.len() as u64,
        });
        assert!(
            read(lineage(prefix), "exact")
                .expect("fork prefix")
                .is_empty()
        );
        std::fs::write(&path, format!("{target}{target}")).expect("write duplicate");
        assert!(read(lineage(None), "exact").is_err());
        std::fs::write(&path, format!("{prior}{{")).expect("write incomplete");
        assert!(read(lineage(None), "exact").is_err());
    }
}
