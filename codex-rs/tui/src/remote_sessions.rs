use chrono::Utc;
use codex_protocol::approvals::NetworkApprovalContext;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::ApplyPatchApprovalRequestEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecApprovalRequestEvent;
use codex_protocol::protocol::ExecCommandBeginEvent;
use codex_protocol::protocol::ExecCommandEndEvent;
use codex_protocol::protocol::PatchApplyBeginEvent;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::StreamErrorEvent;
use codex_protocol::protocol::ThreadNameUpdatedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use codex_protocol::request_permissions::RequestPermissionsEvent;
use rand::Rng;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

const MAX_REMOTE_TEXT_BYTES: usize = 64 * 1024;
const MAX_RECENT_ACTION_BYTES: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteSessionMetadata {
    pub session_id: String,
    pub cwd: PathBuf,
    pub title: String,
    pub status: String,
    pub pid: u32,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum RemoteSessionCommand {
    UserMessage {
        content: String,
    },
    Interrupt,
    Approve {
        id: String,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        decision: Option<ReviewDecision>,
        #[serde(default)]
        permissions: Option<PermissionProfile>,
    },
    ListModels,
    SetModel {
        model: String,
        #[serde(default)]
        effort: Option<ReasoningEffort>,
    },
    Reset,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TitleSource {
    Explicit,
    Prompt,
    Fallback,
}

#[derive(Debug, Clone)]
struct PendingRemoteApproval {
    kind: &'static str,
    id: String,
    turn_id: Option<String>,
    reason: Option<String>,
    command: Option<Vec<String>>,
    available_decisions: Option<Vec<ReviewDecision>>,
    network_approval_context: Option<NetworkApprovalContext>,
    additional_permissions: Option<PermissionProfile>,
    permissions: Option<PermissionProfile>,
    files: Option<Vec<String>>,
}

#[derive(Debug)]
struct SharedState {
    metadata: RemoteSessionMetadata,
    metadata_path: PathBuf,
    socket_path: PathBuf,
    phase: String,
    recent_actions: VecDeque<String>,
    current_model: Option<String>,
    current_reasoning_effort: Option<ReasoningEffort>,
    pending_approvals: Vec<PendingRemoteApproval>,
    next_seq: u64,
    clients: Vec<std::sync::mpsc::Sender<String>>,
    title_source: TitleSource,
}

pub(crate) struct RemoteSessionController {
    #[cfg(unix)]
    shared: std::sync::Arc<std::sync::Mutex<SharedState>>,
    #[cfg(unix)]
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl RemoteSessionController {
    pub(crate) fn start(
        codex_home: &Path,
        cwd: &Path,
        title_override: Option<&str>,
        initial_prompt: Option<&str>,
        app_event_tx: crate::app_event_sender::AppEventSender,
    ) -> io::Result<Option<Self>> {
        #[cfg(not(unix))]
        {
            let _ = (
                codex_home,
                cwd,
                title_override,
                initial_prompt,
                app_event_tx,
            );
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
            let (title, title_source) = select_session_title(cwd, title_override, initial_prompt);
            let metadata = RemoteSessionMetadata {
                session_id,
                cwd: cwd.to_path_buf(),
                title,
                status: "running".to_string(),
                pid: std::process::id(),
                created_at: Utc::now().to_rfc3339(),
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
                current_model: None,
                current_reasoning_effort: None,
                pending_approvals: Vec::new(),
                next_seq: 0,
                clients: Vec::new(),
                title_source,
            }));
            let shutdown = Arc::new(AtomicBool::new(false));

            let shared_for_accept = Arc::clone(&shared);
            let shutdown_for_accept = Arc::clone(&shutdown);
            thread::spawn(move || {
                while !shutdown_for_accept.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if configure_client_stream(&stream).is_err() {
                                continue;
                            }
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
                                Err(_) => {
                                    continue;
                                }
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

                            let app_event_tx = app_event_tx.clone();
                            thread::spawn(move || {
                                let mut stream = stream;
                                let reader = BufReader::new(&mut stream);
                                for line in reader.lines() {
                                    let line = match line {
                                        Ok(line) => line,
                                        Err(_) => break,
                                    };
                                    let trimmed = line.trim();
                                    if trimmed.is_empty() {
                                        continue;
                                    }
                                    match serde_json::from_str::<RemoteSessionCommand>(trimmed) {
                                        Ok(command) => {
                                            app_event_tx.send(
                                                crate::app_event::AppEvent::RemoteSessionCommand(
                                                    command,
                                                ),
                                            );
                                        }
                                        Err(err) => {
                                            let message = encode_named_event(
                                                "error",
                                                json!({ "message": format!("invalid command: {err}") }),
                                            );
                                            let _ = tx.send(message);
                                        }
                                    }
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
            if let Some(title) = title_from_event(event, state.title_source) {
                state.metadata.title = title;
                state.title_source = TitleSource::Prompt;
                let _ = write_metadata(&state.metadata_path, &state.metadata);
            }
            if let EventMsg::ThreadNameUpdated(ThreadNameUpdatedEvent {
                thread_name: Some(thread_name),
                ..
            }) = &event.msg
                && !thread_name.trim().is_empty()
            {
                state.metadata.title = thread_name.trim().to_string();
                state.title_source = TitleSource::Explicit;
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

    pub(crate) fn emit_named_event(&self, event_name: &str, payload: serde_json::Value) {
        #[cfg(unix)]
        {
            let Ok(mut state) = self.shared.lock() else {
                return;
            };
            let line = encode_sequenced_event(&mut state, event_name, payload);
            state
                .clients
                .retain(|client| client.send(line.clone()).is_ok());
        }
    }

    pub(crate) fn update_model_selection(
        &self,
        model: &str,
        reasoning_effort: Option<ReasoningEffort>,
    ) {
        #[cfg(unix)]
        {
            let Ok(mut state) = self.shared.lock() else {
                return;
            };
            state.current_model = Some(model.to_string());
            state.current_reasoning_effort = reasoning_effort;
        }
    }

    pub(crate) fn resolve_approval(&self, kind: &str, id: &str) {
        #[cfg(unix)]
        {
            let Ok(mut state) = self.shared.lock() else {
                return;
            };
            remove_pending_approval(&mut state, kind, id);
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

impl Drop for RemoteSessionController {
    fn drop(&mut self) {
        self.close();
    }
}

pub fn list_remote_sessions(codex_home: &Path) -> io::Result<Vec<RemoteSessionMetadata>> {
    let mut sessions = Vec::new();
    cleanup_remote_session_dir(codex_home)?;
    let dir = remote_session_dir(codex_home);
    if !dir.exists() {
        return Ok(sessions);
    }

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(metadata) = serde_json::from_str::<RemoteSessionMetadata>(&contents) else {
            continue;
        };
        sessions.push(metadata);
    }

    sessions.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    Ok(sessions)
}

pub fn remote_session_dir(codex_home: &Path) -> PathBuf {
    codex_home.join("remote")
}

pub fn remote_session_socket_path(codex_home: &Path, session_id: &str) -> PathBuf {
    remote_session_dir(codex_home).join(format!("{session_id}.sock"))
}

fn allocate_session_paths(codex_home: &Path) -> io::Result<(String, PathBuf, PathBuf)> {
    let mut rng = rand::rng();
    loop {
        let session_id = format!("{:04x}", rng.random::<u16>());
        let metadata_path = remote_session_dir(codex_home).join(format!("{session_id}.json"));
        let socket_path = remote_session_socket_path(codex_home, &session_id);
        if !metadata_path.exists() && !socket_path.exists() {
            return Ok((session_id, metadata_path, socket_path));
        }
    }
}

fn select_session_title(
    cwd: &Path,
    title_override: Option<&str>,
    prompt: Option<&str>,
) -> (String, TitleSource) {
    if let Some(title) = normalized_title(title_override) {
        return (title, TitleSource::Explicit);
    }
    if let Some(title) = normalized_title(prompt) {
        return (title, TitleSource::Prompt);
    }
    if let Some(title) = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| normalized_title(Some(name)))
    {
        return (title, TitleSource::Fallback);
    }
    ("codex-session".to_string(), TitleSource::Fallback)
}

fn normalized_title(value: Option<&str>) -> Option<String> {
    let value = value?;
    let line = value.lines().find(|line| !line.trim().is_empty())?.trim();
    if line.is_empty() {
        return None;
    }
    Some(line.chars().take(80).collect())
}

fn title_from_event(event: &Event, title_source: TitleSource) -> Option<String> {
    if title_source != TitleSource::Fallback {
        return None;
    }
    match &event.msg {
        EventMsg::UserMessage(UserMessageEvent { message, .. }) => normalized_title(Some(message)),
        _ => None,
    }
}

fn encode_protocol_event(state: &mut SharedState, event: &Event) -> Option<String> {
    match &event.msg {
        EventMsg::SessionConfigured(session) => {
            state.metadata.cwd = session.cwd.clone();
            state.current_model = Some(session.model.clone());
            state.current_reasoning_effort = session.reasoning_effort;
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
                    "currentModel": state.current_model,
                    "currentReasoningEffort": state.current_reasoning_effort,
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
        EventMsg::UserMessage(UserMessageEvent { message, .. }) => {
            push_recent_action(state, format!("user: {}", message.trim()));
            Some(encode_sequenced_event(
                state,
                "user_message",
                text_payload("content", message),
            ))
        }
        EventMsg::AgentMessage(AgentMessageEvent { message, .. }) => {
            push_recent_action(state, format!("assistant: {}", message.trim()));
            Some(encode_sequenced_event(
                state,
                "assistant_message",
                text_payload("content", message),
            ))
        }
        EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
            call_id, command, ..
        }) => {
            state.phase = "executing".to_string();
            let command = command.join(" ");
            remove_pending_approval(state, "exec", call_id);
            push_recent_action(state, format!("run {command}"));
            let mut payload = serde_json::Map::new();
            payload.insert("tool".to_string(), json!("shell"));
            insert_text_field(&mut payload, "command", &command);
            Some(encode_sequenced_event(
                state,
                "tool_call",
                serde_json::Value::Object(payload),
            ))
        }
        EventMsg::ExecApprovalRequest(ev) => {
            let approval = pending_exec_approval(ev);
            upsert_pending_approval(state, approval.clone());
            Some(encode_sequenced_event(
                state,
                "approval_request",
                approval_payload(&approval),
            ))
        }
        EventMsg::ApplyPatchApprovalRequest(ev) => {
            let approval = pending_patch_approval(ev);
            upsert_pending_approval(state, approval.clone());
            Some(encode_sequenced_event(
                state,
                "approval_request",
                approval_payload(&approval),
            ))
        }
        EventMsg::RequestPermissions(ev) => {
            let approval = pending_permissions_approval(ev);
            upsert_pending_approval(state, approval.clone());
            Some(encode_sequenced_event(
                state,
                "approval_request",
                approval_payload(&approval),
            ))
        }
        EventMsg::PatchApplyBegin(PatchApplyBeginEvent { call_id, .. }) => {
            remove_pending_approval(state, "patch", call_id);
            None
        }
        EventMsg::ExecCommandEnd(payload) => {
            let output = select_exec_output(payload);
            Some(encode_sequenced_event(
                state,
                "tool_result",
                text_payload("output", &output),
            ))
        }
        EventMsg::TurnComplete(TurnCompleteEvent { .. }) => {
            state.phase = "idle".to_string();
            state.pending_approvals.clear();
            push_recent_action(state, "turn complete".to_string());
            Some(encode_sequenced_event(state, "task_complete", json!({})))
        }
        EventMsg::TurnAborted(_) => {
            state.phase = "idle".to_string();
            state.pending_approvals.clear();
            Some(encode_sequenced_event(
                state,
                "phase_change",
                json!({ "phase": state.phase }),
            ))
        }
        EventMsg::StreamError(StreamErrorEvent { message, .. }) => Some(encode_sequenced_event(
            state,
            "error",
            text_payload("message", message),
        )),
        EventMsg::ShutdownComplete => {
            state.pending_approvals.clear();
            Some(encode_sequenced_event(
                state,
                "phase_change",
                json!({ "phase": "closed" }),
            ))
        }
        _ => None,
    }
}

fn push_recent_action(state: &mut SharedState, action: String) {
    let action = action.trim();
    if action.is_empty() {
        return;
    }
    state
        .recent_actions
        .push_back(truncate_text(action, MAX_RECENT_ACTION_BYTES).0.to_string());
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
            "pendingApprovals": state
                .pending_approvals
                .iter()
                .map(approval_payload)
                .collect::<Vec<_>>(),
            "currentModel": state.current_model,
            "currentReasoningEffort": state.current_reasoning_effort,
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

fn text_payload(field_name: &str, text: &str) -> serde_json::Value {
    let mut payload = serde_json::Map::new();
    insert_text_field(&mut payload, field_name, text);
    serde_json::Value::Object(payload)
}

fn insert_text_field(
    payload: &mut serde_json::Map<String, serde_json::Value>,
    field_name: &str,
    text: &str,
) {
    let (truncated, original_bytes) = truncate_text(text, MAX_REMOTE_TEXT_BYTES);
    payload.insert(field_name.to_string(), json!(truncated));
    if original_bytes.is_some() {
        payload.insert("truncated".to_string(), json!(true));
        payload.insert("originalBytes".to_string(), json!(text.len()));
    }
}

fn truncate_text(text: &str, max_bytes: usize) -> (&str, Option<usize>) {
    if text.len() <= max_bytes {
        return (text, None);
    }

    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], Some(text.len()))
}

fn select_exec_output(payload: &ExecCommandEndEvent) -> String {
    if !payload.formatted_output.is_empty() {
        payload.formatted_output.clone()
    } else if !payload.aggregated_output.is_empty() {
        payload.aggregated_output.clone()
    } else {
        match (
            payload.stdout.trim().is_empty(),
            payload.stderr.trim().is_empty(),
        ) {
            (false, false) => format!("{}\n{}", payload.stdout, payload.stderr),
            (false, true) => payload.stdout.clone(),
            (true, false) => payload.stderr.clone(),
            (true, true) => "(no output)".to_string(),
        }
    }
}

fn pending_exec_approval(ev: &ExecApprovalRequestEvent) -> PendingRemoteApproval {
    PendingRemoteApproval {
        kind: "exec",
        id: ev.effective_approval_id(),
        turn_id: Some(ev.turn_id.clone()),
        reason: ev.reason.clone(),
        command: Some(ev.command.clone()),
        available_decisions: Some(ev.effective_available_decisions()),
        network_approval_context: ev.network_approval_context.clone(),
        additional_permissions: ev.additional_permissions.clone(),
        permissions: None,
        files: None,
    }
}

fn pending_patch_approval(ev: &ApplyPatchApprovalRequestEvent) -> PendingRemoteApproval {
    PendingRemoteApproval {
        kind: "patch",
        id: ev.call_id.clone(),
        turn_id: Some(ev.turn_id.clone()),
        reason: ev.reason.clone(),
        command: None,
        available_decisions: Some(vec![ReviewDecision::Approved, ReviewDecision::Abort]),
        network_approval_context: None,
        additional_permissions: None,
        permissions: None,
        files: Some(
            ev.changes
                .keys()
                .map(|path: &PathBuf| path.display().to_string())
                .collect(),
        ),
    }
}

fn pending_permissions_approval(ev: &RequestPermissionsEvent) -> PendingRemoteApproval {
    PendingRemoteApproval {
        kind: "permissions",
        id: ev.call_id.clone(),
        turn_id: Some(ev.turn_id.clone()),
        reason: ev.reason.clone(),
        command: None,
        available_decisions: Some(vec![
            ReviewDecision::Approved,
            ReviewDecision::ApprovedForSession,
            ReviewDecision::Abort,
        ]),
        network_approval_context: None,
        additional_permissions: None,
        permissions: Some(ev.permissions.clone()),
        files: None,
    }
}

fn upsert_pending_approval(state: &mut SharedState, approval: PendingRemoteApproval) {
    if let Some(existing) = state
        .pending_approvals
        .iter_mut()
        .find(|existing| existing.kind == approval.kind && existing.id == approval.id)
    {
        *existing = approval;
        return;
    }
    state.pending_approvals.push(approval);
}

fn remove_pending_approval(state: &mut SharedState, kind: &str, id: &str) {
    state
        .pending_approvals
        .retain(|approval| !(approval.kind == kind && approval.id == id));
}

fn approval_payload(approval: &PendingRemoteApproval) -> serde_json::Value {
    let mut payload = serde_json::Map::new();
    payload.insert("kind".to_string(), json!(approval.kind));
    payload.insert("id".to_string(), json!(approval.id));
    if let Some(turn_id) = &approval.turn_id {
        payload.insert("turnId".to_string(), json!(turn_id));
    }
    if let Some(reason) = &approval.reason {
        insert_text_field(&mut payload, "reason", reason);
    }
    if let Some(command) = &approval.command {
        payload.insert("command".to_string(), json!(command));
        insert_text_field(&mut payload, "commandDisplay", &command.join(" "));
    }
    if let Some(available_decisions) = &approval.available_decisions {
        payload.insert("availableDecisions".to_string(), json!(available_decisions));
    }
    if let Some(network_approval_context) = &approval.network_approval_context {
        payload.insert(
            "networkApprovalContext".to_string(),
            json!(network_approval_context),
        );
    }
    if let Some(additional_permissions) = &approval.additional_permissions {
        payload.insert(
            "additionalPermissions".to_string(),
            json!(additional_permissions),
        );
    }
    if let Some(permissions) = &approval.permissions {
        payload.insert("permissions".to_string(), json!(permissions));
    }
    if let Some(files) = &approval.files {
        payload.insert("files".to_string(), json!(files));
        payload.insert("fileCount".to_string(), json!(files.len()));
    }
    serde_json::Value::Object(payload)
}

fn write_metadata(path: &Path, metadata: &RemoteSessionMetadata) -> io::Result<()> {
    let contents = serde_json::to_string_pretty(metadata)
        .map_err(|err| io::Error::other(format!("serialize metadata: {err}")))?;
    fs::write(path, contents)
}

#[cfg(unix)]
fn configure_client_stream(stream: &std::os::unix::net::UnixStream) -> io::Result<()> {
    stream.set_nonblocking(false)
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

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::RemoteSessionMetadata;
    use super::SharedState;
    use super::TitleSource;
    use super::configure_client_stream;
    use super::encode_protocol_event;
    use super::encode_snapshot;
    use super::list_remote_sessions;
    use super::select_exec_output;
    use super::select_session_title;
    use super::text_payload;
    use chrono::Utc;
    use codex_protocol::models::PermissionProfile;
    use codex_protocol::openai_models::ReasoningEffort;
    use codex_protocol::protocol::Event;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ExecCommandEndEvent;
    use codex_protocol::protocol::ExecCommandSource;
    use codex_protocol::protocol::ExecCommandStatus;
    use codex_protocol::request_permissions::RequestPermissionsEvent;
    use pretty_assertions::assert_eq;
    use std::collections::VecDeque;
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn prefers_explicit_title_then_prompt_then_directory_name() {
        let cwd = std::path::Path::new("/tmp/example-project");
        assert_eq!(
            select_session_title(cwd, Some("Manual Title"), Some("Prompt title")).0,
            "Manual Title"
        );
        assert_eq!(
            select_session_title(cwd, None, Some("Prompt title")).0,
            "Prompt title"
        );
        assert_eq!(select_session_title(cwd, None, None).0, "example-project");
    }

    #[test]
    fn stale_running_sessions_are_pruned() {
        let codex_home = TempDir::new().expect("tempdir");
        let remote_dir = codex_home.path().join("remote");
        fs::create_dir_all(&remote_dir).expect("create remote dir");
        let path = remote_dir.join("dead.json");
        let metadata = RemoteSessionMetadata {
            session_id: "dead".to_string(),
            cwd: std::path::PathBuf::from("/tmp/project"),
            title: "test".to_string(),
            status: "running".to_string(),
            pid: 999_999,
            created_at: Utc::now().to_rfc3339(),
        };
        fs::write(
            &path,
            serde_json::to_string(&metadata).expect("serialize metadata"),
        )
        .expect("write metadata");

        let sessions = list_remote_sessions(codex_home.path()).expect("list sessions");
        assert!(sessions.is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn text_payload_truncates_oversized_content() {
        let content = "x".repeat(70 * 1024);
        let payload = text_payload("output", &content);
        assert_eq!(payload["truncated"], true);
        assert_eq!(payload["originalBytes"], content.len() as u64);
        assert!(payload["output"].as_str().expect("string output").len() < content.len());
    }

    #[test]
    fn select_exec_output_falls_back_to_aggregated_output() {
        let payload = ExecCommandEndEvent {
            call_id: "call-1".to_string(),
            process_id: None,
            turn_id: "turn-1".to_string(),
            command: vec!["printf".to_string(), "hello".to_string()],
            cwd: PathBuf::from("/tmp"),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::Agent,
            interaction_input: None,
            stdout: String::new(),
            stderr: String::new(),
            aggregated_output: "hello from aggregate".to_string(),
            exit_code: 0,
            duration: std::time::Duration::from_millis(1),
            formatted_output: String::new(),
            status: ExecCommandStatus::Completed,
        };

        assert_eq!(select_exec_output(&payload), "hello from aggregate");
    }

    #[test]
    fn snapshot_includes_current_model_state() {
        let metadata = RemoteSessionMetadata {
            session_id: "dead".to_string(),
            cwd: PathBuf::from("/tmp/project"),
            title: "test".to_string(),
            status: "running".to_string(),
            pid: 123,
            created_at: Utc::now().to_rfc3339(),
        };
        let snapshot = encode_snapshot(&SharedState {
            metadata,
            metadata_path: PathBuf::from("/tmp/project.json"),
            socket_path: PathBuf::from("/tmp/project.sock"),
            phase: "idle".to_string(),
            recent_actions: VecDeque::new(),
            current_model: Some("gpt-5.2-codex".to_string()),
            current_reasoning_effort: Some(ReasoningEffort::Medium),
            pending_approvals: Vec::new(),
            next_seq: 0,
            clients: Vec::new(),
            title_source: TitleSource::Fallback,
        });

        let payload: serde_json::Value = serde_json::from_str(&snapshot).expect("parse snapshot");
        assert_eq!(payload["currentModel"], "gpt-5.2-codex");
        assert_eq!(payload["currentReasoningEffort"], "medium");
    }

    #[test]
    fn remote_command_deserializes_interrupt_and_set_model() {
        let interrupt: super::RemoteSessionCommand =
            serde_json::from_str(r#"{"type":"interrupt"}"#).expect("interrupt command");
        assert_eq!(interrupt, super::RemoteSessionCommand::Interrupt);

        let set_model: super::RemoteSessionCommand = serde_json::from_str(
            r#"{"type":"set_model","model":"gpt-5.2-codex","effort":"medium"}"#,
        )
        .expect("set model command");
        assert_eq!(
            set_model,
            super::RemoteSessionCommand::SetModel {
                model: "gpt-5.2-codex".to_string(),
                effort: Some(ReasoningEffort::Medium),
            }
        );
    }

    #[test]
    fn request_permissions_events_are_streamed_and_snapshotted() {
        let metadata = RemoteSessionMetadata {
            session_id: "dead".to_string(),
            cwd: PathBuf::from("/tmp/project"),
            title: "test".to_string(),
            status: "running".to_string(),
            pid: 123,
            created_at: Utc::now().to_rfc3339(),
        };
        let mut state = SharedState {
            metadata,
            metadata_path: PathBuf::from("/tmp/project.json"),
            socket_path: PathBuf::from("/tmp/project.sock"),
            phase: "idle".to_string(),
            recent_actions: VecDeque::new(),
            current_model: None,
            current_reasoning_effort: None,
            pending_approvals: Vec::new(),
            next_seq: 0,
            clients: Vec::new(),
            title_source: TitleSource::Fallback,
        };
        let permissions = PermissionProfile::default();
        let event = Event {
            id: "request-permissions".to_string(),
            msg: EventMsg::RequestPermissions(RequestPermissionsEvent {
                call_id: "perm-1".to_string(),
                turn_id: "turn-1".to_string(),
                reason: Some("Need broader access".to_string()),
                permissions: permissions.clone(),
            }),
        };

        let encoded = encode_protocol_event(&mut state, &event).expect("approval event");
        let payload: serde_json::Value = serde_json::from_str(&encoded).expect("parse event");
        assert_eq!(payload["event"], "approval_request");
        assert_eq!(payload["kind"], "permissions");
        assert_eq!(payload["id"], "perm-1");
        assert_eq!(
            payload["permissions"],
            serde_json::to_value(&permissions).expect("serialize permissions")
        );

        let snapshot: serde_json::Value =
            serde_json::from_str(&encode_snapshot(&state)).expect("parse snapshot");
        assert_eq!(
            snapshot["pendingApprovals"].as_array().map(Vec::len),
            Some(1)
        );
        assert_eq!(snapshot["pendingApprovals"][0]["kind"], "permissions");
        assert_eq!(snapshot["pendingApprovals"][0]["id"], "perm-1");
    }

    #[cfg(unix)]
    #[test]
    fn accepted_client_streams_are_forced_back_to_blocking_mode() {
        let (client, peer) = std::os::unix::net::UnixStream::pair().expect("create unix pair");
        client.set_nonblocking(true).expect("set nonblocking");
        assert!(stream_is_nonblocking(&client));

        configure_client_stream(&client).expect("restore blocking mode");

        assert!(!stream_is_nonblocking(&client));
        assert!(!stream_is_nonblocking(&peer));
    }

    #[cfg(unix)]
    fn stream_is_nonblocking(stream: &std::os::unix::net::UnixStream) -> bool {
        let flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1, "fcntl(F_GETFL) should succeed");
        flags & libc::O_NONBLOCK != 0
    }
}
