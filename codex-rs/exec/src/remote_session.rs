use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandBeginEvent;
use codex_protocol::protocol::ExecCommandEndEvent;
use codex_protocol::protocol::StreamErrorEvent;
use codex_protocol::protocol::ThreadNameUpdatedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::process::Stdio;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RemoteSessionMetadata {
    session_id: String,
    cwd: PathBuf,
    title: String,
    status: String,
    pid: u32,
    created_at: String,
}

#[derive(Debug)]
struct SharedState {
    metadata: RemoteSessionMetadata,
    metadata_path: PathBuf,
    socket_path: PathBuf,
    phase: String,
    recent_actions: VecDeque<String>,
    next_seq: u64,
    clients: Vec<std::sync::mpsc::Sender<String>>,
}

pub(crate) struct ExecRemoteSession {
    #[cfg(unix)]
    shared: std::sync::Arc<std::sync::Mutex<SharedState>>,
    #[cfg(unix)]
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ExecRemoteSession {
    pub(crate) fn start(
        codex_home: &Path,
        cwd: &Path,
        title: Option<&str>,
    ) -> io::Result<Option<Self>> {
        #[cfg(not(unix))]
        {
            let _ = (codex_home, cwd, title);
            Ok(None)
        }

        #[cfg(unix)]
        {
            use std::io::BufRead;
            use std::io::BufReader;
            use std::io::Write;
            use std::os::unix::net::UnixListener;
            use std::sync::Arc;
            use std::sync::Mutex;
            use std::sync::atomic::AtomicBool;
            use std::sync::atomic::Ordering;
            use std::thread;
            use std::time::Duration;

            cleanup_remote_session_dir(codex_home)?;
            fs::create_dir_all(remote_session_dir(codex_home))?;
            let (session_id, metadata_path, socket_path) = allocate_session_paths(codex_home)?;
            let metadata = RemoteSessionMetadata {
                session_id,
                cwd: cwd.to_path_buf(),
                title: select_session_title(cwd, title),
                status: "running".to_string(),
                pid: std::process::id(),
                created_at: created_at_timestamp()?,
            };
            write_metadata(&metadata_path, &metadata)?;

            let listener = UnixListener::bind(&socket_path)?;
            listener.set_nonblocking(true)?;

            let shared = Arc::new(Mutex::new(SharedState {
                metadata,
                metadata_path,
                socket_path,
                phase: "idle".to_string(),
                recent_actions: VecDeque::new(),
                next_seq: 0,
                clients: Vec::new(),
            }));
            let shutdown = Arc::new(AtomicBool::new(false));

            let shared_for_accept = Arc::clone(&shared);
            let shutdown_for_accept = Arc::clone(&shutdown);
            thread::spawn(move || {
                while !shutdown_for_accept.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let (tx, rx) = std::sync::mpsc::channel::<String>();
                            let snapshot = {
                                let Ok(mut state) = shared_for_accept.lock() else {
                                    continue;
                                };
                                let snapshot = encode_snapshot(&state);
                                state.clients.push(tx.clone());
                                snapshot
                            };
                            let _ = tx.send(snapshot);

                            let write_stream = match stream.try_clone() {
                                Ok(stream) => stream,
                                Err(_) => continue,
                            };
                            thread::spawn(move || {
                                let mut write_stream = write_stream;
                                while let Ok(line) = rx.recv() {
                                    if writeln!(write_stream, "{line}").is_err() {
                                        break;
                                    }
                                    if write_stream.flush().is_err() {
                                        break;
                                    }
                                }
                            });

                            thread::spawn(move || {
                                let mut stream = stream;
                                let reader = BufReader::new(&mut stream);
                                for line in reader.lines() {
                                    let Ok(line) = line else {
                                        break;
                                    };
                                    if line.trim().is_empty() {
                                        continue;
                                    }
                                    let _ = tx.send(encode_named_event(
                                        "error",
                                        json!({
                                            "message": "remote commands are not supported for codex exec sessions"
                                        }),
                                    ));
                                }
                            });
                        }
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(100));
                        }
                        Err(_) => break,
                    }
                }
            });

            Ok(Some(Self { shared, shutdown }))
        }
    }

    pub(crate) fn observe_event(&self, event: &Event) {
        #[cfg(unix)]
        {
            let Ok(mut state) = self.shared.lock() else {
                return;
            };
            if let EventMsg::ThreadNameUpdated(ThreadNameUpdatedEvent {
                thread_name: Some(thread_name),
                ..
            }) = &event.msg
                && let Some(title) = normalized_title(Some(thread_name))
            {
                state.metadata.title = title;
                let _ = write_metadata(&state.metadata_path, &state.metadata);
            }

            let Some(line) = encode_protocol_event(&mut state, event) else {
                return;
            };
            state
                .clients
                .retain(|client| client.send(line.clone()).is_ok());
        }
    }

    pub(crate) fn close(&self) {
        #[cfg(unix)]
        {
            use std::sync::atomic::Ordering;

            self.shutdown.store(true, Ordering::Relaxed);
            let Ok(mut state) = self.shared.lock() else {
                return;
            };
            let _ = fs::remove_file(&state.metadata_path);
            let _ = fs::remove_file(&state.socket_path);
            state.clients.clear();
        }
    }
}

impl Drop for ExecRemoteSession {
    fn drop(&mut self) {
        self.close();
    }
}

fn select_session_title(cwd: &Path, title: Option<&str>) -> String {
    if let Some(title) = normalized_title(title) {
        return title;
    }
    if let Some(title) = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| normalized_title(Some(name)))
    {
        return title;
    }
    "codex-session".to_string()
}

fn normalized_title(value: Option<&str>) -> Option<String> {
    let value = value?;
    let line = value.lines().find(|line| !line.trim().is_empty())?.trim();
    if line.is_empty() {
        return None;
    }
    Some(line.chars().take(80).collect())
}

fn encode_protocol_event(state: &mut SharedState, event: &Event) -> Option<String> {
    match &event.msg {
        EventMsg::SessionConfigured(session) => {
            state.metadata.cwd = session.cwd.clone();
            if let Some(name) = session
                .thread_name
                .as_deref()
                .and_then(|name| normalized_title(Some(name)))
            {
                state.metadata.title = name;
            }
            let _ = write_metadata(&state.metadata_path, &state.metadata);
            Some(encode_sequenced_event(
                state,
                "session_configured",
                json!({
                    "sessionId": session.session_id.to_string(),
                    "cwd": session.cwd,
                    "title": state.metadata.title,
                    "status": state.metadata.status,
                }),
            ))
        }
        EventMsg::TurnStarted(TurnStartedEvent { .. }) => {
            state.phase = "executing".to_string();
            Some(encode_sequenced_event(
                state,
                "phase_change",
                json!({ "phase": state.phase }),
            ))
        }
        EventMsg::AgentMessage(AgentMessageEvent { message, .. }) => {
            push_recent_action(state, format!("assistant: {}", message.trim()));
            Some(encode_sequenced_event(
                state,
                "assistant_message",
                json!({ "content": message }),
            ))
        }
        EventMsg::ExecCommandBegin(ExecCommandBeginEvent { command, .. }) => {
            state.phase = "executing".to_string();
            let command = command.join(" ");
            push_recent_action(state, format!("run {command}"));
            Some(encode_sequenced_event(
                state,
                "tool_call",
                json!({ "tool": "shell", "command": command }),
            ))
        }
        EventMsg::ExecCommandEnd(ExecCommandEndEvent {
            formatted_output, ..
        }) => {
            let output = if formatted_output.trim().is_empty() {
                "(no output)".to_string()
            } else {
                formatted_output.clone()
            };
            Some(encode_sequenced_event(
                state,
                "tool_result",
                json!({ "output": output }),
            ))
        }
        EventMsg::TurnComplete(TurnCompleteEvent { .. }) => {
            state.phase = "idle".to_string();
            push_recent_action(state, "turn complete".to_string());
            Some(encode_sequenced_event(state, "task_complete", json!({})))
        }
        EventMsg::TurnAborted(_) => {
            state.phase = "idle".to_string();
            Some(encode_sequenced_event(
                state,
                "phase_change",
                json!({ "phase": state.phase }),
            ))
        }
        EventMsg::StreamError(StreamErrorEvent { message, .. }) => Some(encode_sequenced_event(
            state,
            "error",
            json!({ "message": message }),
        )),
        EventMsg::ShutdownComplete => Some(encode_sequenced_event(
            state,
            "phase_change",
            json!({ "phase": "closed" }),
        )),
        _ => None,
    }
}

fn push_recent_action(state: &mut SharedState, action: String) {
    let action = action.trim();
    if action.is_empty() {
        return;
    }
    state.recent_actions.push_back(action.to_string());
    while state.recent_actions.len() > 8 {
        state.recent_actions.pop_front();
    }
}

fn encode_snapshot(state: &SharedState) -> String {
    encode_named_event(
        "session_snapshot",
        json!({
            "title": state.metadata.title,
            "cwd": state.metadata.cwd,
            "status": state.metadata.status,
            "phase": state.phase,
            "recent_actions": state.recent_actions,
        }),
    )
}

fn encode_sequenced_event(
    state: &mut SharedState,
    event_name: &str,
    payload: serde_json::Value,
) -> String {
    state.next_seq += 1;
    let mut value = payload;
    if let serde_json::Value::Object(ref mut object) = value {
        object.insert("event".to_string(), json!(event_name));
        object.insert("seq".to_string(), json!(state.next_seq));
    }
    value.to_string()
}

fn encode_named_event(event_name: &str, payload: serde_json::Value) -> String {
    let mut value = payload;
    if let serde_json::Value::Object(ref mut object) = value {
        object.insert("event".to_string(), json!(event_name));
    }
    value.to_string()
}

fn write_metadata(path: &Path, metadata: &RemoteSessionMetadata) -> io::Result<()> {
    let contents = serde_json::to_string_pretty(metadata)
        .map_err(|err| io::Error::other(format!("serialize metadata: {err}")))?;
    fs::write(path, contents)
}

fn cleanup_remote_session_dir(codex_home: &Path) -> io::Result<()> {
    let dir = remote_session_dir(codex_home);
    if !dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }

        let remove_entry = match fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str::<RemoteSessionMetadata>(&contents) {
                Ok(metadata) => metadata.status != "running" || !process_exists(metadata.pid),
                Err(_) => true,
            },
            Err(_) => true,
        };

        if remove_entry {
            let _ = fs::remove_file(&path);
            if let Some(session_id) = path.file_stem().and_then(|stem| stem.to_str()) {
                let _ = fs::remove_file(remote_session_socket_path(codex_home, session_id));
            }
        }
    }

    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("sock") {
            continue;
        }

        let Some(session_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            let _ = fs::remove_file(&path);
            continue;
        };
        let metadata_path = dir.join(format!("{session_id}.json"));
        if !metadata_path.exists() {
            let _ = fs::remove_file(&path);
        }
    }

    Ok(())
}

fn remote_session_dir(codex_home: &Path) -> PathBuf {
    codex_home.join("remote")
}

fn remote_session_socket_path(codex_home: &Path, session_id: &str) -> PathBuf {
    remote_session_dir(codex_home).join(format!("{session_id}.sock"))
}

fn allocate_session_paths(codex_home: &Path) -> io::Result<(String, PathBuf, PathBuf)> {
    loop {
        let session_id = Uuid::new_v4().simple().to_string()[..4].to_string();
        let metadata_path = remote_session_dir(codex_home).join(format!("{session_id}.json"));
        let socket_path = remote_session_socket_path(codex_home, &session_id);
        if !metadata_path.exists() && !socket_path.exists() {
            return Ok((session_id, metadata_path, socket_path));
        }
    }
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    ProcessCommand::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    true
}

fn created_at_timestamp() -> io::Result<String> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| io::Error::other(format!("timestamp: {err}")))?
        .as_secs();
    Ok(seconds.to_string())
}

#[cfg(test)]
mod tests {
    use super::ExecRemoteSession;
    use super::encode_snapshot;
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use std::fs;
    use tempfile::TempDir;

    #[cfg(unix)]
    #[test]
    fn startup_prunes_stale_metadata() {
        let codex_home = TempDir::new().expect("tempdir");
        let remote_dir = codex_home.path().join("remote");
        fs::create_dir_all(&remote_dir).expect("create remote dir");
        let path = remote_dir.join("dead.json");
        fs::write(
            &path,
            serde_json::json!({
                "session_id": "dead",
                "cwd": "/tmp/project",
                "title": "test",
                "status": "running",
                "pid": 999_999,
                "created_at": "0",
            })
            .to_string(),
        )
        .expect("write metadata");

        let session = ExecRemoteSession::start(
            codex_home.path(),
            std::path::Path::new("/tmp/project"),
            None,
        )
        .expect("start remote session");
        drop(session);

        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_uses_explicit_title() {
        let codex_home = TempDir::new().expect("tempdir");
        let session = ExecRemoteSession::start(
            codex_home.path(),
            std::path::Path::new("/tmp/project"),
            Some("exec smoke test"),
        )
        .expect("start remote session")
        .expect("unix remote session");
        let shared = session.shared.lock().expect("lock shared state");
        let snapshot: Value =
            serde_json::from_str(&encode_snapshot(&shared)).expect("parse snapshot");
        assert_eq!(snapshot["title"], "exec smoke test");
    }
}
