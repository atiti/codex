use crate::ThreadEventEnvelope;
use crate::spawn_thread_listener;
use crate::transport::AgentEvent;
use crate::transport::JsonTransport;
use crate::transport::Transport;
use codex_core::AuthManager;
use codex_core::NewThread;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_core::git_info::get_git_repo_root;
use codex_core::models_manager::collaboration_mode_presets::CollaborationModesConfig;
use codex_core::models_manager::manager::RefreshStrategy;
use codex_protocol::approvals::ApplyPatchApprovalRequestEvent;
use codex_protocol::approvals::ExecApprovalRequestEvent;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::SessionSource;
use codex_protocol::user_input::UserInput;
use codex_utils_oss::ensure_oss_provider_ready;
use shlex::try_join;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::warn;

pub struct RemoteControlRunArgs {
    pub config: Config,
    pub dangerously_bypass_approvals_and_sandbox: bool,
    pub model_provider: Option<String>,
    pub oss: bool,
    pub skip_git_repo_check: bool,
}

struct SessionDefaults {
    cwd: PathBuf,
    approval_policy: AskForApproval,
    sandbox_policy: codex_protocol::protocol::SandboxPolicy,
    model: String,
    effort: Option<codex_protocol::openai_models::ReasoningEffort>,
}

struct SessionState {
    thread_id: codex_protocol::ThreadId,
    thread: Arc<codex_core::CodexThread>,
    pending_approvals: HashMap<String, PendingApproval>,
    turn_active: bool,
}

enum PendingApproval {
    Exec { id: String, turn_id: Option<String> },
    Patch { id: String },
}

enum RemoteControlCommand {
    UserMessage {
        content: String,
    },
    Approve {
        id: String,
        decision: ReviewDecision,
    },
    Reset,
    Shutdown,
}

enum CommandEnvelope {
    Command(RemoteControlCommand),
    Invalid(String),
}

pub async fn run_remote_control_session(args: RemoteControlRunArgs) -> anyhow::Result<()> {
    let RemoteControlRunArgs {
        config,
        dangerously_bypass_approvals_and_sandbox,
        model_provider,
        oss,
        skip_git_repo_check,
    } = args;

    if oss {
        let provider_id = model_provider
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("OSS provider not set but oss flag was used"))?;
        ensure_oss_provider_ready(provider_id, &config)
            .await
            .map_err(|err| anyhow::anyhow!("OSS setup failed: {err}"))?;
    }

    if !skip_git_repo_check
        && !dangerously_bypass_approvals_and_sandbox
        && get_git_repo_root(&config.cwd).is_none()
    {
        anyhow::bail!(
            "Not inside a trusted directory and --skip-git-repo-check was not specified."
        );
    }

    let required_mcp_servers: HashSet<String> = config
        .mcp_servers
        .get()
        .iter()
        .filter(|(_, server)| server.enabled && server.required)
        .map(|(name, _)| name.clone())
        .collect();
    let auth_manager = AuthManager::shared(
        config.codex_home.clone(),
        true,
        config.cli_auth_credentials_store_mode,
    );
    let thread_manager = Arc::new(ThreadManager::new(
        config.codex_home.clone(),
        auth_manager,
        SessionSource::Exec,
        config.model_catalog.clone(),
        CollaborationModesConfig {
            default_mode_request_user_input: config
                .features
                .enabled(codex_core::features::Feature::DefaultModeRequestUserInput),
        },
    ));
    let default_model = thread_manager
        .get_models_manager()
        .get_default_model(&config.model, RefreshStrategy::OnlineIfUncached)
        .await;
    let defaults = SessionDefaults {
        cwd: config.cwd.to_path_buf(),
        approval_policy: config.permissions.approval_policy.value(),
        sandbox_policy: config.permissions.sandbox_policy.get().clone(),
        model: default_model,
        effort: config.model_reasoning_effort,
    };

    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<CommandEnvelope>();
    spawn_command_reader(cmd_tx);

    let mut transport = JsonTransport::new(std::io::stdout());
    let (mut session, mut session_rx) =
        start_session(&thread_manager, &config, &mut transport).await?;

    loop {
        tokio::select! {
            maybe_cmd = cmd_rx.recv() => {
                match maybe_cmd {
                    Some(CommandEnvelope::Command(command)) => {
                        match command {
                            RemoteControlCommand::UserMessage { content } => {
                                if session.turn_active {
                                    transport.emit(AgentEvent::Error {
                                        message: "a turn is already in progress".to_string(),
                                    })?;
                                    continue;
                                }

                                session.thread.submit(Op::UserTurn {
                                    items: vec![UserInput::Text {
                                        text: content,
                                        text_elements: Vec::new(),
                                    }],
                                    cwd: defaults.cwd.clone(),
                                    approval_policy: defaults.approval_policy,
                                    sandbox_policy: defaults.sandbox_policy.clone(),
                                    model: defaults.model.clone(),
                                    effort: defaults.effort,
                                    summary: None,
                                    service_tier: None,
                                    final_output_json_schema: None,
                                    collaboration_mode: None,
                                    personality: None,
                                }).await?;
                                session.turn_active = true;
                            }
                            RemoteControlCommand::Approve { id, decision } => {
                                match session.pending_approvals.remove(&id) {
                                    Some(PendingApproval::Exec { id, turn_id }) => {
                                        session.thread.submit(Op::ExecApproval {
                                            id,
                                            turn_id,
                                            decision,
                                        }).await?;
                                    }
                                    Some(PendingApproval::Patch { id }) => {
                                        session.thread.submit(Op::PatchApproval {
                                            id,
                                            decision,
                                        }).await?;
                                    }
                                    None => {
                                        transport.emit(AgentEvent::Error {
                                            message: format!("no pending approval with id {id}"),
                                        })?;
                                    }
                                }
                            }
                            RemoteControlCommand::Reset => {
                                shutdown_session(&session).await;
                                (session, session_rx) =
                                    start_session(&thread_manager, &config, &mut transport).await?;
                            }
                            RemoteControlCommand::Shutdown => {
                                shutdown_session(&session).await;
                                break;
                            }
                        }
                    }
                    Some(CommandEnvelope::Invalid(message)) => {
                        transport.emit(AgentEvent::Error { message })?;
                    }
                    None => {
                        shutdown_session(&session).await;
                        break;
                    }
                }
            }
            maybe_event = session_rx.recv() => {
                let Some(envelope) = maybe_event else {
                    transport.emit(AgentEvent::Error {
                        message: "remote-control event stream closed".to_string(),
                    })?;
                    break;
                };
                handle_event(envelope, &required_mcp_servers, &mut session, &mut transport).await?;
            }
        }
    }

    Ok(())
}

async fn start_session(
    thread_manager: &Arc<ThreadManager>,
    config: &Config,
    transport: &mut impl Transport,
) -> anyhow::Result<(SessionState, mpsc::UnboundedReceiver<ThreadEventEnvelope>)> {
    let NewThread {
        thread_id,
        thread,
        session_configured: _session_configured,
    } = thread_manager.start_thread(config.clone()).await?;
    let (tx, rx) = mpsc::unbounded_channel::<ThreadEventEnvelope>();
    spawn_thread_listener(thread_id, Arc::clone(&thread), tx, false);
    transport.emit(AgentEvent::SessionStarted {
        thread_id: thread_id.to_string(),
    })?;
    Ok((
        SessionState {
            thread_id,
            thread,
            pending_approvals: HashMap::new(),
            turn_active: false,
        },
        rx,
    ))
}

async fn shutdown_session(session: &SessionState) {
    if let Err(err) = session.thread.submit(Op::Shutdown).await {
        warn!("failed to shut down remote-control session: {err}");
    }
}

async fn handle_event(
    envelope: ThreadEventEnvelope,
    required_mcp_servers: &HashSet<String>,
    session: &mut SessionState,
    transport: &mut impl Transport,
) -> anyhow::Result<()> {
    let ThreadEventEnvelope {
        thread_id, event, ..
    } = envelope;
    if thread_id != session.thread_id {
        return Ok(());
    }

    match event.msg {
        EventMsg::SessionConfigured(_) | EventMsg::TurnStarted(_) | EventMsg::ShutdownComplete => {}
        EventMsg::AgentMessage(payload) => {
            transport.emit(AgentEvent::AssistantMessage {
                content: payload.message,
            })?;
        }
        EventMsg::AgentReasoning(payload) => {
            transport.emit(AgentEvent::Thinking {
                content: payload.text,
            })?;
        }
        EventMsg::ExecCommandBegin(payload) => {
            transport.emit(AgentEvent::ToolCall {
                tool: "shell".to_string(),
                command: stringify_command(&payload.command),
            })?;
        }
        EventMsg::ExecCommandEnd(payload) => {
            transport.emit(AgentEvent::ToolResult {
                tool: "shell".to_string(),
                output: command_output(&payload),
                exit_code: Some(payload.exit_code),
            })?;
        }
        EventMsg::PatchApplyBegin(payload) => {
            transport.emit(AgentEvent::ToolCall {
                tool: "apply_patch".to_string(),
                command: summarize_file_changes(&payload.changes),
            })?;
        }
        EventMsg::PatchApplyEnd(payload) => {
            transport.emit(AgentEvent::ToolResult {
                tool: "apply_patch".to_string(),
                output: combine_output(&payload.stdout, &payload.stderr),
                exit_code: None,
            })?;
        }
        EventMsg::ExecApprovalRequest(payload) => {
            remember_exec_approval(session, &payload);
            transport.emit(AgentEvent::ApprovalRequest {
                id: payload.effective_approval_id(),
                kind: "exec".to_string(),
                command: Some(stringify_command(&payload.command)),
                reason: payload.reason,
            })?;
        }
        EventMsg::ApplyPatchApprovalRequest(payload) => {
            remember_patch_approval(session, &payload);
            transport.emit(AgentEvent::ApprovalRequest {
                id: payload.call_id,
                kind: "patch".to_string(),
                command: Some(summarize_file_changes(&payload.changes)),
                reason: payload.reason,
            })?;
        }
        EventMsg::CollabAgentSpawnBegin(payload) => {
            transport.emit(AgentEvent::SubagentStart {
                task: payload.prompt,
            })?;
        }
        EventMsg::CollabAgentSpawnEnd(_) => {
            transport.emit(AgentEvent::SubagentComplete)?;
        }
        EventMsg::TurnComplete(_) => {
            session.turn_active = false;
            session.pending_approvals.clear();
            transport.emit(AgentEvent::TaskComplete)?;
        }
        EventMsg::TurnAborted(payload) => {
            session.turn_active = false;
            session.pending_approvals.clear();
            transport.emit(AgentEvent::Error {
                message: format!("turn aborted: {:?}", payload.reason),
            })?;
            transport.emit(AgentEvent::TaskComplete)?;
        }
        EventMsg::Error(payload) => {
            session.turn_active = false;
            transport.emit(AgentEvent::Error {
                message: payload.message,
            })?;
        }
        EventMsg::Warning(payload) => {
            transport.emit(AgentEvent::Error {
                message: payload.message,
            })?;
        }
        EventMsg::StreamError(payload) => {
            session.turn_active = false;
            let message = match payload.additional_details {
                Some(details) if !details.trim().is_empty() => {
                    format!("{} ({details})", payload.message)
                }
                _ => payload.message,
            };
            transport.emit(AgentEvent::Error { message })?;
        }
        EventMsg::McpStartupUpdate(update)
            if required_mcp_servers.contains(&update.server)
                && matches!(
                    &update.status,
                    codex_protocol::protocol::McpStartupStatus::Failed { .. }
                ) =>
        {
            transport.emit(AgentEvent::Error {
                message: format!("required MCP server {} failed to initialize", update.server),
            })?;
        }
        _ => {}
    }

    Ok(())
}

fn spawn_command_reader(tx: mpsc::UnboundedSender<CommandEnvelope>) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let locked = stdin.lock();
        for line in locked.lines() {
            let envelope = match line {
                Ok(line) => match parse_command(&line) {
                    Ok(command) => CommandEnvelope::Command(command),
                    Err(err) => CommandEnvelope::Invalid(err),
                },
                Err(err) => CommandEnvelope::Invalid(format!("failed to read stdin: {err}")),
            };
            if tx.send(envelope).is_err() {
                break;
            }
        }
    });
}

fn parse_command(line: &str) -> Result<RemoteControlCommand, String> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|err| format!("invalid JSON command: {err}"))?;
    let command_type = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "remote-control command is missing string field `type`".to_string())?;

    match command_type {
        "user_message" => Ok(RemoteControlCommand::UserMessage {
            content: required_string_field(&value, "content")?,
        }),
        "approve" => {
            let decision = match value
                .get("decision")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("approved")
            {
                "approved" => ReviewDecision::Approved,
                "denied" => ReviewDecision::Denied,
                "abort" => ReviewDecision::Abort,
                other => {
                    return Err(format!(
                        "unsupported approval decision `{other}`; use approved, denied, or abort"
                    ));
                }
            };
            Ok(RemoteControlCommand::Approve {
                id: required_string_field(&value, "id")?,
                decision,
            })
        }
        "reset" => Ok(RemoteControlCommand::Reset),
        "shutdown" => Ok(RemoteControlCommand::Shutdown),
        other => Err(format!("unsupported remote-control command type `{other}`")),
    }
}

fn required_string_field(value: &serde_json::Value, field: &str) -> Result<String, String> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("remote-control command is missing string field `{field}`"))
}

fn remember_exec_approval(session: &mut SessionState, payload: &ExecApprovalRequestEvent) {
    let turn_id = (!payload.turn_id.is_empty()).then_some(payload.turn_id.clone());
    session.pending_approvals.insert(
        payload.effective_approval_id(),
        PendingApproval::Exec {
            id: payload.effective_approval_id(),
            turn_id,
        },
    );
}

fn remember_patch_approval(session: &mut SessionState, payload: &ApplyPatchApprovalRequestEvent) {
    session.pending_approvals.insert(
        payload.call_id.clone(),
        PendingApproval::Patch {
            id: payload.call_id.clone(),
        },
    );
}

fn stringify_command(command: &[String]) -> String {
    try_join(command.iter().map(String::as_str)).unwrap_or_else(|_| command.join(" "))
}

fn command_output(payload: &codex_protocol::protocol::ExecCommandEndEvent) -> String {
    if !payload.formatted_output.is_empty() {
        payload.formatted_output.clone()
    } else if !payload.aggregated_output.is_empty() {
        payload.aggregated_output.clone()
    } else {
        combine_output(&payload.stdout, &payload.stderr)
    }
}

fn combine_output(stdout: &str, stderr: &str) -> String {
    match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
        (false, false) => format!("{stdout}\n{stderr}"),
        (false, true) => stdout.to_string(),
        (true, false) => stderr.to_string(),
        (true, true) => String::new(),
    }
}

fn summarize_file_changes(changes: &HashMap<PathBuf, FileChange>) -> String {
    let mut paths: Vec<String> = changes
        .keys()
        .map(|path| path.display().to_string())
        .collect();
    paths.sort_unstable();
    if paths.is_empty() {
        "no file changes".to_string()
    } else {
        format!("files: {}", paths.join(", "))
    }
}
