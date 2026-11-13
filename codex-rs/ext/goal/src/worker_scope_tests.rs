use super::worker_binding_required;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use std::ffi::OsStr;

#[test]
fn worker_flag_is_scoped_to_the_root_thread() {
    assert!(worker_binding_required(
        &SessionSource::Cli,
        Some(OsStr::new("1"))
    ));
    assert!(!worker_binding_required(&SessionSource::Cli, None));
    let child = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: ThreadId::new(),
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });
    assert!(!worker_binding_required(&child, Some(OsStr::new("1"))));
    assert!(!worker_binding_required(
        &SessionSource::SubAgent(SubAgentSource::Review),
        Some(OsStr::new("1"))
    ));
}
