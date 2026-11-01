use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

pub struct ReviewGithubHandler;

pub(crate) struct ReviewerGithubPolicy(pub std::sync::Arc<codex_execpolicy::Policy>);
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    repository: String,
    pull_request: u64,
}
fn valid_repository(repository: &str) -> bool {
    let parts: Vec<_> = repository.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && *p != "."
                && *p != ".."
                && p.len() <= 100
                && p.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        })
}
impl ToolExecutor<ToolInvocation> for ReviewGithubHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("review_github_pull_request")
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool { name: "review_github_pull_request".into(), description: "Read current GitHub pull request state, merge commit, head and CI checks using gh. Supply owner/repository and pull request number.".into(), strict: false, defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([("repository".into(), JsonSchema::string(Some("GitHub owner/repository".into()))), ("pull_request".into(), JsonSchema::number(Some("Pull request number".into())))]), Some(vec!["repository".into(), "pull_request".into()]), Some(false.into())), output_schema: None })
    }
    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel("invalid payload".into()));
            };
            let args: Args = parse_arguments(&arguments)?;
            if !valid_repository(&args.repository) || args.pull_request == 0 {
                return Err(FunctionCallError::RespondToModel(
                    "invalid repository or pull request number".into(),
                ));
            }
            // Host execution requires an explicit existing allow rule; prompt and forbid fail closed.
            let policy_command = vec!["gh".to_string(), "pr".to_string(), "view".to_string(), args.pull_request.to_string(), "--repo".to_string(), format!("github.com/{}", args.repository), "--json".to_string(), "number,state,mergedAt,mergeCommit,headRefOid,baseRefName,reviewDecision,statusCheckRollup,url".to_string()];
            let policy = invocation
                .session
                .services
                .thread_extension_data
                .get::<ReviewerGithubPolicy>()
                .ok_or_else(|| {
                    FunctionCallError::RespondToModel("GitHub evidence policy unavailable".into())
                })?;
            let policy = &policy.0;
            let decision = policy.check(&policy_command, &|_| codex_execpolicy::Decision::Prompt);
            if decision.decision != codex_execpolicy::Decision::Allow {
                return Err(FunctionCallError::RespondToModel(
                    "GitHub evidence requires an explicit gh pr view command allow rule".into(),
                ));
            }
            let mut command = tokio::process::Command::new("gh");
            command.env_clear().envs(crate::exec_env::create_env(
                &invocation.turn.config.permissions.shell_environment_policy,
                Some(invocation.session.thread_id),
            ));
            command.args(["pr", "view", &args.pull_request.to_string(), "--repo", &format!("github.com/{}", args.repository), "--json", "number,state,mergedAt,mergeCommit,headRefOid,baseRefName,reviewDecision,statusCheckRollup,url"]).env("GH_PROMPT_DISABLED", "1").kill_on_drop(true);
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = command
                .spawn()
                .map_err(|e| FunctionCallError::RespondToModel(format!("Unable to run gh: {e}")))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| FunctionCallError::RespondToModel("stdout unavailable".into()))?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| FunctionCallError::RespondToModel("stderr unavailable".into()))?;
            let result = tokio::time::timeout(Duration::from_secs(30), async {
                let mut out = Vec::new();
                let mut err = Vec::new();
                let mut stdout = stdout.take(256_001);
                let mut stderr = stderr.take(16_385);
                let (a, b) =
                    tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
                a?;
                b?;
                if out.len() > 256_000 || err.len() > 16_384 {
                    return Err(std::io::Error::other("GitHub response too large"));
                }
                let status = child.wait().await?;
                Ok::<_, std::io::Error>((status, out, err))
            })
            .await
            .map_err(|_| {
                FunctionCallError::RespondToModel("GitHub evidence request timed out".into())
            })?
            .map_err(|e| {
                FunctionCallError::RespondToModel(format!("GitHub evidence failed: {e}"))
            })?;
            if !result.0.success() {
                return Err(FunctionCallError::RespondToModel(format!(
                    "gh failed: {}",
                    String::from_utf8_lossy(&result.2)
                )));
            }
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                String::from_utf8_lossy(&result.1).into_owned(),
                Some(true),
            )))
        })
    }
}
impl CoreToolRuntime for ReviewGithubHandler {}
#[cfg(test)]
mod tests {
    use super::valid_repository;
    #[test]
    fn accepts_only_owner_repo() {
        assert!(valid_repository("atiti/agent-router"));
        for p in [
            "-x",
            "a/b/c",
            "a/..",
            "a/b --help",
            "a/$(echo x)",
            "/repo",
            "a/b\n",
        ] {
            assert!(!valid_repository(p));
        }
    }
}
