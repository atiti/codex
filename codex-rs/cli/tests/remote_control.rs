use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use codex_core::auth::CODEX_API_KEY_ENV_VAR;
use codex_utils_cargo_bin::cargo_bin;
use pretty_assertions::assert_eq;
use serde_json::Value;
use tempfile::TempDir;

#[tokio::test]
async fn remote_control_emits_assistant_message_event() -> Result<()> {
    let server = core_test_support::responses::start_mock_server().await;
    let body = core_test_support::responses::sse(vec![
        core_test_support::responses::ev_response_created("resp-1"),
        core_test_support::responses::ev_assistant_message("msg-1", "Remote hello"),
        core_test_support::responses::ev_completed("resp-1"),
    ]);
    core_test_support::responses::mount_sse_once(&server, body).await;

    let codex_home = TempDir::new()?;
    let repo_root = std::env::current_dir()?;
    let mut child = Command::new(cargo_bin("codex")?)
        .arg("--remote-control")
        .current_dir(&repo_root)
        .env("CODEX_HOME", codex_home.path())
        .env(CODEX_API_KEY_ENV_VAR, "dummy")
        .env("OPENAI_BASE_URL", format!("{}/v1", server.uri()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawn codex --remote-control")?;

    let mut stdin = child.stdin.take().context("stdin unavailable")?;
    let stdout = child.stdout.take().context("stdout unavailable")?;
    let (tx, rx) = mpsc::channel::<Result<Value, String>>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let value = serde_json::from_str::<Value>(line.trim_end())
                        .map_err(|err| format!("invalid JSON event: {err}"));
                    if tx.send(value).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(format!("failed to read stdout: {err}")));
                    break;
                }
            }
        }
    });

    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "type": "user_message",
            "content": "say hello",
        })
    )?;
    stdin.flush()?;

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut saw_assistant_message = false;
    while Instant::now() < deadline {
        let timeout = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(timeout) {
            Ok(Ok(value)) => {
                if value.get("event").and_then(Value::as_str) == Some("assistant_message") {
                    assert_eq!(
                        value.get("content").and_then(Value::as_str),
                        Some("Remote hello")
                    );
                    saw_assistant_message = true;
                    break;
                }
            }
            Ok(Err(message)) => anyhow::bail!("{message}"),
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("remote-control stdout closed before assistant message arrived");
            }
        }
    }

    assert_eq!(saw_assistant_message, true);

    writeln!(stdin, "{}", serde_json::json!({ "type": "shutdown" }))?;
    stdin.flush()?;
    let status = wait_for_exit(&mut child, Duration::from_secs(10))?;
    assert_eq!(status.success(), true);

    Ok(())
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            child
                .kill()
                .context("kill timed-out remote-control process")?;
            let _ = child.wait();
            anyhow::bail!("timed out waiting for remote-control process to exit");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
