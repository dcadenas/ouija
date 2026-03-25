//! Workflow actor executor.
//!
//! A workflow is an external executable (Python, Ruby, bash, etc.) that guides
//! an LLM session through a deterministic process. Communication uses a simple
//! JSON-over-stdin/stdout protocol; the workflow manages its own state (typically
//! a JSON file) and can call ouija's REST API for async push operations.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::state::AppState;

const WORKFLOW_TIMEOUT: Duration = Duration::from_secs(30);
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// Returned by the workflow on registration.
#[derive(Debug, Deserialize)]
pub struct WorkflowRegistration {
    /// LLM-facing interface description. Prepended to the session prompt.
    pub instructions: String,
    /// First nudge text injected after session starts. Also used as the reminder.
    #[serde(default)]
    pub inject_on_start: Option<String>,
    /// Maximum workflow calls allowed before the daemon refuses further calls.
    /// Prevents unbounded looping. Enforced by the daemon, not the workflow.
    #[serde(default)]
    pub max_calls: Option<u64>,
}

/// Generic workflow response for runtime actions.
#[derive(Debug, Deserialize)]
struct WorkflowResponse {
    message: Option<String>,
    #[serde(default)]
    error: Option<String>,
    /// Machine-checkable success criteria for the current step.
    /// When present, appended to the message so the LLM knows how to verify its work.
    #[serde(default)]
    verify: Option<String>,
}

/// Input envelope sent to the workflow on stdin.
#[derive(Debug, Serialize)]
struct WorkflowInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<String>,
    session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<serde_json::Value>,
}

/// Resolve a workflow path, making relative paths relative to `working_dir`.
fn resolve_path(workflow_path: &str, working_dir: Option<&str>) -> PathBuf {
    let p = Path::new(workflow_path);
    if p.is_absolute() {
        p.to_path_buf()
    } else if let Some(dir) = working_dir {
        Path::new(dir).join(p)
    } else {
        p.to_path_buf()
    }
}

/// Spawn a workflow executable, pass JSON on stdin, read JSON from stdout.
async fn execute_workflow(
    workflow_path: &Path,
    input: &WorkflowInput,
    timeout: Duration,
    working_dir: Option<&str>,
    port: u16,
) -> Result<serde_json::Value, String> {
    if !workflow_path.exists() {
        return Err(format!(
            "workflow not found: {}. Check that the path is correct and the file exists.",
            workflow_path.display()
        ));
    }

    let input_json =
        serde_json::to_string(input).map_err(|e| format!("failed to serialize input: {e}"))?;

    let cwd = working_dir
        .map(Path::new)
        .unwrap_or_else(|| workflow_path.parent().unwrap_or(Path::new(".")));

    let mut child = tokio::process::Command::new(workflow_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .current_dir(cwd)
        .env("OUIJA_API", format!("http://127.0.0.1:{port}"))
        .env("OUIJA_SESSION_ID", &input.session_id)
        .spawn()
        .map_err(|e| format!("failed to spawn workflow: {e}"))?;

    // Write input to stdin
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(input_json.as_bytes())
            .await
            .map_err(|e| format!("failed to write to workflow stdin: {e}"))?;
        // Drop stdin to close it
    }

    // Wait with timeout. wait_with_output takes ownership, but tokio drops the
    // future (and the child) on timeout, which closes pipes and reaps the process.
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| "workflow timed out. The script may be hanging. Call workflow(action='status') to retry, or check the workflow script for issues.".to_string())?
        .map_err(|e| format!("workflow process error: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "workflow crashed (exit {}): {}\nCall workflow(action='status') to check state, or workflow(action='init') to re-orient.",
            output.status,
            stderr.trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim())
        .map_err(|e| format!("workflow returned invalid JSON: {e}\nraw output: {stdout}\nThe workflow script may have a bug. Call workflow(action='status') to retry."))
}

/// Call the workflow with a registration event. Returns instructions for the LLM.
pub async fn register_workflow(
    state: &Arc<AppState>,
    workflow_path: &str,
    session_id: &str,
    workflow_params: Option<&serde_json::Value>,
    working_dir: Option<&str>,
) -> Result<WorkflowRegistration, String> {
    let path = resolve_path(workflow_path, working_dir);
    let lock = state.workflow_lock(&path);
    let _guard = lock.lock().await;

    let input = WorkflowInput {
        event: Some("register".into()),
        action: None,
        session_id: session_id.into(),
        params: workflow_params.cloned(),
    };

    let value = execute_workflow(&path, &input, WORKFLOW_TIMEOUT, working_dir, state.config.port).await?;

    serde_json::from_value::<WorkflowRegistration>(value)
        .map_err(|e| format!("workflow registration response missing required fields: {e}"))
}

/// Call the workflow with a runtime action from the LLM. Returns the message to show the LLM.
pub async fn call_workflow(
    state: &Arc<AppState>,
    workflow_path: &str,
    session_id: &str,
    action: &str,
    params: Option<&serde_json::Value>,
    working_dir: Option<&str>,
) -> Result<String, String> {
    let path = resolve_path(workflow_path, working_dir);
    let lock = state.workflow_lock(&path);
    let _guard = lock.lock().await;

    let input = WorkflowInput {
        event: None,
        action: Some(action.into()),
        session_id: session_id.into(),
        params: params.cloned(),
    };

    let value = execute_workflow(&path, &input, WORKFLOW_TIMEOUT, working_dir, state.config.port).await?;

    let resp: WorkflowResponse =
        serde_json::from_value(value).map_err(|e| format!("invalid workflow response: {e}"))?;

    if let Some(err) = resp.error {
        return Err(err);
    }

    let message = resp
        .message
        .ok_or_else(|| "workflow returned no message".to_string())?;

    // Append verification criteria if the workflow provided them
    match resp.verify {
        Some(criteria) => Ok(format!("{message}\n\nVerify before proceeding: {criteria}")),
        None => Ok(message),
    }
}

/// Fire-and-forget lifecycle event notification to the workflow.
pub fn notify_workflow(
    state: &Arc<AppState>,
    workflow_path: &str,
    event: &str,
    session_id: &str,
    working_dir: Option<&str>,
) {
    let state = state.clone();
    let workflow_path = workflow_path.to_string();
    let event = event.to_string();
    let session_id = session_id.to_string();
    let working_dir = working_dir.map(String::from);

    tokio::spawn(async move {
        let path = resolve_path(&workflow_path, working_dir.as_deref());
        let lock = state.workflow_lock(&path);
        let _guard = lock.lock().await;

        let input = WorkflowInput {
            event: Some(event.clone()),
            action: None,
            session_id: session_id.clone(),
            params: None,
        };

        if let Err(e) = execute_workflow(
            &path,
            &input,
            NOTIFY_TIMEOUT,
            working_dir.as_deref(),
            state.config.port,
        )
        .await
        {
            tracing::warn!(
                "workflow lifecycle notification '{event}' for session '{session_id}' failed: {e}"
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_input(session_id: &str) -> WorkflowInput {
        WorkflowInput {
            event: Some("test".into()),
            action: None,
            session_id: session_id.into(),
            params: None,
        }
    }

    fn make_script(content: &str) -> tempfile::TempPath {
        let mut f = tempfile::Builder::new()
            .suffix(".sh")
            .tempfile()
            .unwrap();
        writeln!(f, "#!/usr/bin/env bash").unwrap();
        writeln!(f, "{content}").unwrap();
        f.flush().unwrap();

        let path = f.path().to_path_buf();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Close the write fd so Linux allows exec (avoids ETXTBSY)
        f.into_temp_path()
    }

    #[tokio::test]
    async fn execute_workflow_valid_json() {
        let script = make_script(r#"cat /dev/stdin > /dev/null; echo '{"message":"hello","count":42}'"#);
        let input = make_input("sess-1");
        let result = execute_workflow(&script, &input, Duration::from_secs(5), None, 9999).await;
        let val = result.unwrap();
        assert_eq!(val["message"], "hello");
        assert_eq!(val["count"], 42);
    }

    #[tokio::test]
    async fn execute_workflow_nonzero_exit() {
        let script = make_script("echo 'something broke' >&2; exit 1");
        let input = make_input("sess-2");
        let result = execute_workflow(&script, &input, Duration::from_secs(5), None, 9999).await;
        let err = result.unwrap_err();
        assert!(err.contains("crashed"), "expected crash indicator in error: {err}");
        assert!(err.contains("something broke"), "expected stderr in error: {err}");
    }

    #[tokio::test]
    async fn execute_workflow_invalid_json() {
        let script = make_script("echo 'not json'");
        let input = make_input("sess-3");
        let result = execute_workflow(&script, &input, Duration::from_secs(5), None, 9999).await;
        let err = result.unwrap_err();
        assert!(err.contains("invalid JSON"), "expected JSON error: {err}");
    }

    #[tokio::test]
    async fn execute_workflow_timeout() {
        let script = make_script("sleep 60");
        let input = make_input("sess-4");
        let result = execute_workflow(&script, &input, Duration::from_secs(1), None, 9999).await;
        let err = result.unwrap_err();
        assert!(err.contains("timed out"), "expected timeout error: {err}");
    }

    #[test]
    fn resolve_path_absolute() {
        let result = resolve_path("/usr/bin/workflow", None);
        assert_eq!(result, PathBuf::from("/usr/bin/workflow"));

        let result = resolve_path("/usr/bin/workflow", Some("/other/dir"));
        assert_eq!(result, PathBuf::from("/usr/bin/workflow"));
    }

    #[test]
    fn resolve_path_relative_with_working_dir() {
        let result = resolve_path("scripts/run.sh", Some("/project"));
        assert_eq!(result, PathBuf::from("/project/scripts/run.sh"));
    }

    #[test]
    fn resolve_path_relative_without_working_dir() {
        let result = resolve_path("scripts/run.sh", None);
        assert_eq!(result, PathBuf::from("scripts/run.sh"));
    }
}
