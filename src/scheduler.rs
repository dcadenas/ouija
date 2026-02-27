use std::collections::HashMap;

use chrono::{DateTime, Utc};
use croner::Cron;
use serde::{Deserialize, Serialize};

use crate::state::SharedState;
use crate::tmux;

/// How often the scheduler checks for due tasks.
const SCHEDULER_TICK_SECS: u64 = 15;
/// Max time to wait for claude to start in a revived pane.
const REVIVAL_TIMEOUT_SECS: u64 = 30;
/// Interval between readiness polls during session revival.
const REVIVAL_POLL_SECS: u64 = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: String,
    pub name: String,
    pub cron: String,
    pub target_session: String,
    pub message: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub next_run: Option<DateTime<Utc>>,
    pub last_run: Option<DateTime<Utc>>,
    pub last_status: Option<TaskRunStatus>,
    pub run_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
    #[serde(default)]
    pub once: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunStatus {
    Ok,
    Revived,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRun {
    pub task_id: String,
    pub task_name: String,
    pub timestamp: DateTime<Utc>,
    pub status: TaskRunStatus,
    pub error: Option<String>,
    pub target_session: String,
    pub revived_pane: Option<String>,
}

/// Validate a cron expression and return a human-readable description.
pub fn validate_cron(expr: &str) -> Result<String, String> {
    let cron = expr.parse::<Cron>().map_err(|e| format!("{e}"))?;
    Ok(cron.pattern.to_string())
}

/// Compute the next run time from now for a cron expression.
pub fn compute_next_run(expr: &str) -> Option<DateTime<Utc>> {
    let cron = expr.parse::<Cron>().ok()?;
    cron.find_next_occurrence(&Utc::now(), false).ok()
}

/// Generate an 8-char hex task ID.
pub fn generate_task_id() -> String {
    format!("{:08x}", rand::random::<u32>())
}

/// Format a scheduled task injection message.
pub fn format_scheduled_message(message: &str) -> String {
    format!("[scheduled task]: {message}")
}

/// Run the scheduler loop, checking for due tasks every 15 seconds.
pub async fn run_scheduler(state: SharedState) {
    // Recompute next_run for all tasks on startup
    recompute_all_next_runs(&state).await;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(SCHEDULER_TICK_SECS)).await;
        tick(&state).await;
    }
}

/// Recompute `next_run` for all enabled tasks (e.g. after daemon restart).
async fn recompute_all_next_runs(state: &SharedState) {
    let mut tasks = state.scheduled_tasks.write().await;
    let mut changed = false;
    for task in tasks.values_mut() {
        if task.enabled {
            task.next_run = compute_next_run(&task.cron);
            changed = true;
        }
    }
    if changed {
        state.persist_tasks_from(&tasks);
    }
}

/// Single tick: find due tasks and execute them sequentially.
async fn tick(state: &SharedState) {
    let now = Utc::now();

    // Collect due task IDs under a short read lock
    let due_ids: Vec<String> = {
        let tasks = state.scheduled_tasks.read().await;
        tasks
            .values()
            .filter(|t| t.enabled && t.next_run.is_some_and(|nr| nr <= now))
            .map(|t| t.id.clone())
            .collect()
    };

    for id in due_ids {
        execute_task(state, &id).await;
    }
}

/// Execute a single scheduled task by ID.
pub async fn execute_task(state: &SharedState, task_id: &str) {
    // Read the task snapshot
    let task = {
        let tasks = state.scheduled_tasks.read().await;
        match tasks.get(task_id) {
            Some(t) => t.clone(),
            None => return,
        }
    };

    let formatted = format_scheduled_message(&task.message);
    let run = execute_injection(state, &task, &formatted).await;

    // Update task state
    state
        .update_task(task_id, |t| {
            t.last_run = Some(run.timestamp);
            t.last_status = Some(run.status.clone());
            t.run_count += 1;
            t.next_run = compute_next_run(&t.cron);
        })
        .await;

    state.log_task_run(run).await;

    // Auto-delete one-shot tasks after execution
    if task.once {
        state.remove_task(task_id).await;
    }
}

/// Try to inject into the target session, reviving if needed.
async fn execute_injection(
    state: &SharedState,
    task: &ScheduledTask,
    formatted: &str,
) -> TaskRun {
    let timestamp = Utc::now();

    // Look up target session
    let session = {
        let sessions = state.sessions.read().await;
        sessions.get(&task.target_session).cloned()
    };

    let Some(session) = session else {
        return TaskRun {
            task_id: task.id.clone(),
            task_name: task.name.clone(),
            timestamp,
            status: TaskRunStatus::Failed,
            error: Some(format!("session '{}' not found", task.target_session)),
            target_session: task.target_session.clone(),
            revived_pane: None,
        };
    };

    // Only handle local sessions
    if !matches!(session.origin, crate::state::SessionOrigin::Local) {
        return TaskRun {
            task_id: task.id.clone(),
            task_name: task.name.clone(),
            timestamp,
            status: TaskRunStatus::Failed,
            error: Some("cannot target remote sessions".into()),
            target_session: task.target_session.clone(),
            revived_pane: None,
        };
    }

    let Some(pane) = &session.pane else {
        return TaskRun {
            task_id: task.id.clone(),
            task_name: task.name.clone(),
            timestamp,
            status: TaskRunStatus::Failed,
            error: Some("session has no tmux pane".into()),
            target_session: task.target_session.clone(),
            revived_pane: None,
        };
    };

    // Check if pane is alive
    let pane_id = pane.clone();
    let alive =
        tokio::task::spawn_blocking(move || tmux::pane_alive(&pane_id))
            .await
            .unwrap_or(false);

    if alive {
        // Direct injection
        return inject_into_pane(state, task, pane, session.metadata.vim_mode, formatted).await;
    }

    // Pane is dead — attempt revival
    let project_dir = task
        .project_dir
        .as_deref()
        .or(session.metadata.project_dir.as_deref());

    match revive_and_inject(state, task, project_dir, formatted).await {
        Ok(new_pane) => TaskRun {
            task_id: task.id.clone(),
            task_name: task.name.clone(),
            timestamp,
            status: TaskRunStatus::Revived,
            error: None,
            target_session: task.target_session.clone(),
            revived_pane: Some(new_pane),
        },
        Err(e) => TaskRun {
            task_id: task.id.clone(),
            task_name: task.name.clone(),
            timestamp,
            status: TaskRunStatus::Failed,
            error: Some(e.to_string()),
            target_session: task.target_session.clone(),
            revived_pane: None,
        },
    }
}

/// Inject a message into a live pane.
async fn inject_into_pane(
    state: &SharedState,
    task: &ScheduledTask,
    pane: &str,
    vim_mode: bool,
    formatted: &str,
) -> TaskRun {
    let pane = pane.to_string();
    let vim = vim_mode;
    let msg = formatted.to_string();
    let lock = state.pane_lock(&pane);
    let _guard = lock.lock().await;

    let result =
        tokio::task::spawn_blocking(move || tmux::inject(&pane, &msg, vim)).await;

    let timestamp = Utc::now();
    match result {
        Ok(Ok(())) => TaskRun {
            task_id: task.id.clone(),
            task_name: task.name.clone(),
            timestamp,
            status: TaskRunStatus::Ok,
            error: None,
            target_session: task.target_session.clone(),
            revived_pane: None,
        },
        Ok(Err(e)) => TaskRun {
            task_id: task.id.clone(),
            task_name: task.name.clone(),
            timestamp,
            status: TaskRunStatus::Failed,
            error: Some(e.to_string()),
            target_session: task.target_session.clone(),
            revived_pane: None,
        },
        Err(e) => TaskRun {
            task_id: task.id.clone(),
            task_name: task.name.clone(),
            timestamp,
            status: TaskRunStatus::Failed,
            error: Some(e.to_string()),
            target_session: task.target_session.clone(),
            revived_pane: None,
        },
    }
}

/// Revive a dead session: create new tmux window, launch claude, re-register, inject.
async fn revive_and_inject(
    state: &SharedState,
    task: &ScheduledTask,
    project_dir: Option<&str>,
    formatted: &str,
) -> anyhow::Result<String> {
    let dir = project_dir
        .map(String::from)
        .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()));

    // Create new tmux window
    let new_pane = tokio::task::spawn_blocking({
        let dir = dir.clone();
        move || -> anyhow::Result<String> {
            let output = std::process::Command::new("tmux")
                .args(["new-window", "-d", "-P", "-F", "#{pane_id}"])
                .output()?;
            if !output.status.success() {
                anyhow::bail!(
                    "tmux new-window failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            let pane_id = String::from_utf8_lossy(&output.stdout).trim().to_string();

            // Launch claude --continue in the project dir
            let cmd = format!("cd {} && claude --continue", shell_escape(&dir));
            std::process::Command::new("tmux")
                .args(["send-keys", "-t", &pane_id, &cmd, "Enter"])
                .status()?;

            Ok(pane_id)
        }
    })
    .await??;

    // Poll for readiness (claude process appears in pane)
    let poll_pane = new_pane.clone();
    let ready = tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(REVIVAL_TIMEOUT_SECS);
        while std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_secs(REVIVAL_POLL_SECS));
            if let Ok(output) = std::process::Command::new("tmux")
                .args([
                    "display-message",
                    "-t",
                    &poll_pane,
                    "-p",
                    "#{pane_current_command}",
                ])
                .output()
            {
                let cmd = String::from_utf8_lossy(&output.stdout);
                if cmd.trim() == "claude" {
                    return true;
                }
            }
        }
        false
    })
    .await
    .unwrap_or(false);

    if !ready {
        anyhow::bail!("claude did not start within {REVIVAL_TIMEOUT_SECS}s in pane {new_pane}");
    }

    // Re-register session with new pane (same ID, so dedup check won't fire)
    let metadata = crate::state::SessionMetadata {
        project_dir: project_dir.map(String::from),
        ..Default::default()
    };
    if let Err(e) = state
        .register_session(task.target_session.clone(), Some(new_pane.clone()), metadata)
        .await
    {
        anyhow::bail!("failed to re-register revived session: {e}");
    }

    // Inject the scheduled message
    let pane_for_inject = new_pane.clone();
    let msg = formatted.to_string();
    let lock = state.pane_lock(&pane_for_inject);
    let _guard = lock.lock().await;
    tokio::task::spawn_blocking(move || tmux::inject(&pane_for_inject, &msg, false))
        .await??;

    Ok(new_pane)
}

/// Escape a string for safe use in shell commands.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Create a new `ScheduledTask` with computed `next_run`.
pub fn new_task(
    name: String,
    cron: String,
    target_session: String,
    message: String,
    project_dir: Option<String>,
    once: bool,
) -> ScheduledTask {
    let next_run = compute_next_run(&cron);
    ScheduledTask {
        id: generate_task_id(),
        name,
        cron,
        target_session,
        message,
        enabled: true,
        created_at: Utc::now(),
        next_run,
        last_run: None,
        last_status: None,
        run_count: 0,
        project_dir,
        once,
    }
}

/// Build a HashMap from a Vec of tasks, keyed by ID.
pub fn tasks_to_map(tasks: Vec<ScheduledTask>) -> HashMap<String, ScheduledTask> {
    tasks.into_iter().map(|t| (t.id.clone(), t)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_cron_valid() {
        let result = validate_cron("*/5 * * * *");
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    #[test]
    fn validate_cron_invalid() {
        let result = validate_cron("not a cron");
        assert!(result.is_err());
    }

    #[test]
    fn compute_next_run_returns_future() {
        let next = compute_next_run("*/1 * * * *");
        assert!(next.is_some());
        assert!(next.unwrap() > Utc::now());
    }

    #[test]
    fn compute_next_run_invalid_returns_none() {
        assert!(compute_next_run("bad").is_none());
    }

    #[test]
    fn task_id_is_8_hex_chars() {
        let id = generate_task_id();
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn task_serialization_round_trip() {
        let task = ScheduledTask {
            id: "a1b2c3d4".into(),
            name: "test task".into(),
            cron: "*/5 * * * *".into(),
            target_session: "web".into(),
            message: "hello".into(),
            enabled: true,
            created_at: Utc::now(),
            next_run: Some(Utc::now()),
            last_run: None,
            last_status: None,
            run_count: 0,
            project_dir: Some("/tmp".into()),
            once: false,
        };
        let json = serde_json::to_string(&task).unwrap();
        let decoded: ScheduledTask = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, task.id);
        assert_eq!(decoded.name, task.name);
        assert_eq!(decoded.project_dir, task.project_dir);
    }

    #[test]
    fn format_scheduled_message_basic() {
        assert_eq!(
            format_scheduled_message("check logs"),
            "[scheduled task]: check logs"
        );
    }

    #[test]
    fn shell_escape_basic() {
        assert_eq!(shell_escape("/home/user"), "'/home/user'");
    }

    #[test]
    fn shell_escape_with_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn new_task_has_next_run() {
        let task = new_task(
            "t".into(),
            "*/1 * * * *".into(),
            "web".into(),
            "hi".into(),
            None,
            false,
        );
        assert!(task.next_run.is_some());
        assert!(task.enabled);
        assert_eq!(task.run_count, 0);
    }
}
