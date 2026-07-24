use std::collections::HashMap;

use chrono::{DateTime, Utc};
use croner::Cron;
use serde::{Deserialize, Serialize};

use crate::state::SharedState;
use crate::tmux;

/// How often the scheduler checks for due tasks.
const SCHEDULER_TICK_SECS: u64 = 15;
/// Max time to wait for the backend to start in a revived pane.
const REVIVAL_TIMEOUT_SECS: u64 = 30;
/// Extra time to wait for the backend's TUI prompt after process appears.
const TUI_READY_TIMEOUT_SECS: u64 = 30;
/// Interval between readiness polls during session revival.
const REVIVAL_POLL_SECS: u64 = 2;

/// What happens each time the task fires.
#[derive(
    Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash, schemars::JsonSchema,
)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum OnFire {
    /// Inject into live session; revive with --continue if dead.
    #[default]
    ContinueSession,
    /// Start fresh when dead/missing; no-op when alive unless a prompt is set.
    NewSession,
    /// Named worktree that persists across fires.
    /// `clear_context: true` starts a new conversation each fire.
    /// `clear_context: false` continues/resumes the previous conversation.
    PersistentWorktree {
        #[serde(default)]
        clear_context: bool,
    },
    /// Anonymous worktree, created fresh and cleaned up after each fire.
    /// Always starts a new conversation (context clearing is implicit).
    DisposableWorktree,
}

impl OnFire {
    /// Whether this mode clears conversation context on each fire.
    pub fn clears_context(&self) -> bool {
        match self {
            Self::ContinueSession => false,
            Self::NewSession => true,
            Self::PersistentWorktree { clear_context } => *clear_context,
            Self::DisposableWorktree => true,
        }
    }

    /// Whether this mode uses a worktree.
    pub fn uses_worktree(&self) -> bool {
        matches!(
            self,
            Self::PersistentWorktree { .. } | Self::DisposableWorktree
        )
    }

    /// Whether the worktree is disposable (cleaned up after fire).
    pub fn is_disposable_worktree(&self) -> bool {
        matches!(self, Self::DisposableWorktree)
    }

    /// Whether this mode kills an alive session's process on each fire.
    /// Only worktree modes with context clearing need to kill alive sessions.
    /// Non-clearing modes keep the alive process and inject any configured prompt.
    pub fn kills_alive(&self) -> bool {
        match self {
            Self::ContinueSession | Self::NewSession => false,
            Self::PersistentWorktree { clear_context } => *clear_context,
            Self::DisposableWorktree => true,
        }
    }
}

/// A cron-driven task that injects messages into sessions.
///
/// # Design: Trigger + SessionConfig + Runtime
///
/// ScheduledTask = SessionConfig (prompt, reminder, project_dir, on_fire) + Trigger
/// (cron, enabled, next_run). SessionMetadata (state.rs) = SessionConfig + Runtime
/// (iteration, iteration_log). The shared SessionConfig fields (prompt, reminder,
/// project_dir, on_fire) are stamped onto SessionMetadata when the task creates or
/// revives a session — that's the handoff.
///
/// A third trigger type (file watch — see GitHub issue #1) would add
/// Trigger::FileWatch alongside Trigger::Cron and the implicit Trigger::SelfDriven
/// (loop_next). If that happens, extracting a named SessionConfig type would make
/// the trigger→session handoff explicit instead of field-by-field copying.
#[derive(Clone, Debug, Serialize)]
pub struct ScheduledTask {
    pub id: String,
    pub name: String,
    pub cron: String,
    /// Optional: inject into this existing session (ContinueSession only).
    /// When absent or when creating a new session, `name` is used instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_session: Option<String>,
    /// Bootstrap: prompt for creating/reviving the target session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Bootstrap: reminder for the target session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminder: Option<String>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub next_run: Option<DateTime<Utc>>,
    pub last_run: Option<DateTime<Utc>>,
    pub last_status: Option<TaskRunStatus>,
    pub run_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
    /// Backend used when this task creates, revives, or respawns its session.
    /// When unset, the scheduler preserves existing session metadata or falls
    /// back to the daemon default backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Model override used with `backend`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Effort/variant override used with `backend`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default)]
    pub once: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "claude_session_id"
    )]
    pub backend_session_id: Option<String>,
    #[serde(default)]
    pub on_fire: OnFire,
}

impl ScheduledTask {
    /// The ouija session name to look up or create.
    /// For ContinueSession, prefer target_session if set; otherwise use the task name.
    /// For all other OnFire variants, always use the task name.
    pub fn session_name(&self) -> &str {
        if matches!(self.on_fire, OnFire::ContinueSession) {
            self.target_session.as_deref().unwrap_or(&self.name)
        } else {
            &self.name
        }
    }
}

impl<'de> serde::Deserialize<'de> for ScheduledTask {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            id: String,
            name: String,
            cron: String,
            #[serde(default)]
            target_session: Option<String>,
            #[serde(default)]
            prompt: Option<String>,
            #[serde(default)]
            reminder: Option<String>,
            enabled: bool,
            created_at: DateTime<Utc>,
            #[serde(default)]
            next_run: Option<DateTime<Utc>>,
            #[serde(default)]
            last_run: Option<DateTime<Utc>>,
            #[serde(default)]
            last_status: Option<TaskRunStatus>,
            #[serde(default)]
            run_count: u64,
            #[serde(default)]
            project_dir: Option<String>,
            #[serde(default)]
            backend: Option<String>,
            #[serde(default)]
            model: Option<String>,
            #[serde(default)]
            effort: Option<String>,
            #[serde(default)]
            once: bool,
            #[serde(default, alias = "claude_session_id")]
            backend_session_id: Option<String>,
            #[serde(default)]
            on_fire: Option<OnFire>,
            #[serde(default)]
            fresh: Option<bool>,
            #[serde(default)]
            worktree: Option<bool>,
            #[serde(default)]
            worktree_mode: Option<String>,
        }

        let raw = Raw::deserialize(deserializer)?;
        let on_fire = raw.on_fire.unwrap_or_else(|| {
            let fresh = raw.fresh.unwrap_or(false);
            let worktree = raw.worktree.unwrap_or(false);
            let worktree_mode = raw.worktree_mode.as_deref();
            match (fresh, worktree, worktree_mode) {
                (_, true, Some("per-fire")) => OnFire::DisposableWorktree,
                (false, true, _) => OnFire::PersistentWorktree {
                    clear_context: false,
                },
                (true, true, _) => OnFire::PersistentWorktree {
                    clear_context: true,
                },
                (true, false, _) => OnFire::NewSession,
                _ => OnFire::ContinueSession,
            }
        });

        Ok(ScheduledTask {
            id: raw.id,
            name: raw.name,
            cron: raw.cron,
            target_session: raw.target_session,
            prompt: raw.prompt,
            reminder: raw.reminder,
            enabled: raw.enabled,
            created_at: raw.created_at,
            next_run: raw.next_run,
            last_run: raw.last_run,
            last_status: raw.last_status,
            run_count: raw.run_count,
            project_dir: raw.project_dir,
            backend: raw.backend,
            model: raw.model,
            effort: raw.effort,
            once: raw.once,
            backend_session_id: raw.backend_session_id,
            on_fire,
        })
    }
}

/// Outcome of a single scheduled task execution.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunStatus {
    Ok,
    Failed,
}

/// Record of a completed task execution with status and context.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRun {
    pub task_id: String,
    pub task_name: String,
    pub timestamp: DateTime<Utc>,
    pub status: TaskRunStatus,
    pub error: Option<String>,
    pub session_name: String,
    pub revived_pane: Option<String>,
}

impl TaskRun {
    /// Create an Ok run for this task.
    fn ok(task: &ScheduledTask, revived_pane: Option<String>) -> Self {
        Self {
            task_id: task.id.clone(),
            task_name: task.name.clone(),
            timestamp: Utc::now(),
            status: TaskRunStatus::Ok,
            error: None,
            session_name: task.session_name().to_string(),
            revived_pane,
        }
    }

    /// Create a Failed run for this task.
    fn failed(task: &ScheduledTask, error: String) -> Self {
        Self {
            task_id: task.id.clone(),
            task_name: task.name.clone(),
            timestamp: Utc::now(),
            status: TaskRunStatus::Failed,
            error: Some(error),
            session_name: task.session_name().to_string(),
            revived_pane: None,
        }
    }
}

/// Validate a cron expression and return a human-readable description.
///
/// # Errors
///
/// Returns the parse error as a `String` if `expr` is not valid cron syntax.
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

    let run = execute_injection(state, &task).await;

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
async fn execute_injection(state: &SharedState, task: &ScheduledTask) -> TaskRun {
    let session_name = task.session_name();

    // Look up session
    let session = {
        let proto = state.protocol.read().await;
        proto.sessions.get(session_name).cloned()
    };

    // Session not found — create from scratch if task has enough info.
    // No prior metadata to honour; backend default applies.
    let Some(session) = session else {
        if task.project_dir.is_some() || task.prompt.is_some() {
            tracing::info!("session '{session_name}' not found, creating from task project_dir",);
            return revive_from_task(
                state,
                task,
                None,
                None,
                task.model.clone(),
                task.effort.clone(),
                None,
                task.backend.clone(),
            )
            .await;
        }
        return TaskRun::failed(
            task,
            format!("session '{session_name}' not found and task has no project_dir"),
        );
    };

    // Only handle local sessions
    if !matches!(session.origin, crate::daemon_protocol::Origin::Local) {
        return TaskRun::failed(task, "cannot target remote sessions".into());
    }

    let Some(pane) = &session.pane else {
        // Session exists but has no pane — revive it, carrying model/effort
        // from the snapshot we already have in scope.
        let task_backend = task.backend.clone();
        return revive_from_task(
            state,
            task,
            Some(session.owner()),
            None,
            task.model.clone().or_else(|| {
                task_backend
                    .is_none()
                    .then(|| session.metadata.model.clone())
                    .flatten()
            }),
            task.effort.clone().or_else(|| {
                task_backend
                    .is_none()
                    .then(|| session.metadata.effort.clone())
                    .flatten()
            }),
            session.metadata.codex_home.clone(),
            task_backend.or_else(|| session.metadata.backend.clone()),
        )
        .await;
    };

    // Check if pane is alive
    let alive = task_pane_alive(state, pane).await;
    let snapshot_owner = session.owner();

    // Capture model/effort from the session snapshot we already have, so a
    // subsequent Unregister race cannot silently downgrade the respawn to
    // backend defaults. This is the same snapshot used for `session.pane`
    // and `session.metadata.project_dir` below — all three fields come from
    // the same atomic read above at line 368-371.
    let snapshot_model = session.metadata.model.clone();
    let snapshot_effort = session.metadata.effort.clone();
    let snapshot_codex_home = session.metadata.codex_home.clone();
    let snapshot_backend = session.metadata.backend.clone();
    let task_backend = task.backend.clone();
    let launch = TaskLaunchSelection {
        model: task.model.clone().or_else(|| {
            task_backend
                .is_none()
                .then(|| snapshot_model.clone())
                .flatten()
        }),
        effort: task.effort.clone().or_else(|| {
            task_backend
                .is_none()
                .then(|| snapshot_effort.clone())
                .flatten()
        }),
        codex_home: snapshot_codex_home,
        backend_name: task_backend.or(snapshot_backend),
    };

    if alive {
        if !scheduled_snapshot_is_current(state, &snapshot_owner, Some(pane)).await {
            return TaskRun::failed(
                task,
                format!(
                    "scheduled liveness result for '{session_name}' was superseded by a replacement"
                ),
            );
        }
        if task.on_fire.kills_alive() {
            let dir = task
                .project_dir
                .as_deref()
                .or(session.metadata.project_dir.as_deref())
                .unwrap_or("/tmp");
            return respawn_and_inject(state, task, &snapshot_owner, pane, dir, launch).await;
        }
        if let Err(error) = inject_alive_session_prompt(
            state,
            task,
            &snapshot_owner,
            session_name,
            pane,
            session.metadata.vim_mode,
        )
        .await
        {
            return TaskRun::failed(task, error);
        }
        return TaskRun::ok(task, None);
    }

    // Pane is dead — attempt revival, falling back to session's project_dir
    let project_dir = task
        .project_dir
        .as_deref()
        .or(session.metadata.project_dir.as_deref());
    revive_from_task(
        state,
        task,
        Some(snapshot_owner),
        project_dir,
        launch.model,
        launch.effort,
        launch.codex_home,
        launch.backend_name,
    )
    .await
}

async fn scheduled_snapshot_is_current(
    state: &SharedState,
    owner: &crate::daemon_protocol::ResourceOwner,
    pane: Option<&str>,
) -> bool {
    state
        .protocol
        .read()
        .await
        .sessions
        .get(&owner.session_id)
        .is_some_and(|session| session.owner() == *owner && session.pane.as_deref() == pane)
}

async fn scheduled_launch_is_current(
    state: &SharedState,
    owner: &crate::daemon_protocol::ResourceOwner,
    pane: &str,
) -> bool {
    let protocol = state.protocol.read().await;
    let session_matches = protocol
        .sessions
        .get(&owner.session_id)
        .is_some_and(|session| session.owner() == *owner);
    let pane_matches = protocol
        .sessions
        .get(&owner.session_id)
        .is_some_and(|session| session.pane.as_deref() == Some(pane))
        || protocol
            .lifecycle_leases
            .get(&owner.session_id)
            .is_some_and(|lease| {
                lease.inert_pane.as_deref() == Some(pane)
                    && lease.inert_pane_owner.as_ref() == Some(owner)
            });
    session_matches && pane_matches
}

async fn task_pane_alive(state: &SharedState, pane: &str) -> bool {
    if cfg!(test) {
        return state
            .list_assistant_panes()
            .await
            .iter()
            .any(|p| p.pane_id == pane);
    }

    let pane_id = pane.to_string();
    let names: Vec<String> = state.backends.all_process_names();
    tokio::task::spawn_blocking(move || {
        let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        tmux::pane_alive(&pane_id, &name_refs)
    })
    .await
    .unwrap_or(false)
}

fn task_prompt_text(task: &ScheduledTask) -> Option<String> {
    task.prompt.as_ref().map(|prompt| match &task.reminder {
        Some(reminder) => format!("{prompt}\n\n{reminder}"),
        None => prompt.clone(),
    })
}

async fn inject_alive_session_prompt(
    state: &SharedState,
    task: &ScheduledTask,
    owner: &crate::daemon_protocol::ResourceOwner,
    session_name: &str,
    pane: &str,
    vim_mode: bool,
) -> Result<(), String> {
    let Some(message) = task_prompt_text(task) else {
        return Ok(());
    };

    match crate::state::deliver_owned_inject_message_effect(
        state,
        owner,
        crate::state::InjectDeliveryRequest {
            session_id: session_name,
            pane,
            message: &message,
            vim_mode,
            delivery_method: None,
            recorded_method: None,
        },
    )
    .await
    {
        crate::state::DeliveryOutcome::Accepted => Ok(()),
        crate::state::DeliveryOutcome::Rejected(reason) => Err(reason),
        crate::state::DeliveryOutcome::Ambiguous(reason) => {
            Err(format!("prompt delivery ambiguous: {reason}"))
        }
    }
}

async fn remove_scheduled_pane_for_owners(
    state: &SharedState,
    pane: &str,
    owners: Vec<crate::daemon_protocol::ResourceOwner>,
) -> anyhow::Result<()> {
    let pane = pane.to_string();
    let pane_for_guard = pane.clone();
    let owners_for_guard = owners.clone();
    state
        .with_allowed_pane_cleanup(&owners_for_guard, &pane_for_guard, move || async move {
            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let live_owner = crate::tmux::inspect_pane_owner(&pane)?;
                if !live_owner.as_ref().is_some_and(|owner| {
                    owners
                        .iter()
                        .any(|expected| crate::tmux::physical_owner_matches(owner, expected))
                }) {
                    return Ok(());
                }
                let status = std::process::Command::new("tmux")
                    .args(["kill-pane", "-t", &pane])
                    .status()?;
                let remaining_owner = crate::tmux::inspect_pane_owner(&pane)?;
                if !status.success()
                    && remaining_owner.as_ref().is_some_and(|owner| {
                        owners
                            .iter()
                            .any(|expected| crate::tmux::physical_owner_matches(owner, expected))
                    })
                {
                    anyhow::bail!("failed to remove exact scheduled pane {pane}");
                }
                Ok(())
            })
            .await?
        })
        .await
        .unwrap_or(Ok(()))
}

async fn rollback_reserved_scheduled_pane(
    state: &SharedState,
    owner: &crate::daemon_protocol::ResourceOwner,
    pane: &str,
    credential: Option<&str>,
) -> anyhow::Result<()> {
    remove_scheduled_pane_for_owners(state, pane, vec![owner.clone()]).await?;
    let outcome = state
        .rollback_reserved_start(owner, pane, credential)
        .await?;
    if outcome == crate::daemon_protocol::LifecycleMutationOutcome::Applied {
        state.abort_lifecycle(owner).await?;
    }
    Ok(())
}

async fn rollback_scheduled_revival_authority(
    state: &SharedState,
    reserved_owner: Option<&crate::daemon_protocol::ResourceOwner>,
    restart_claim: Option<&ScheduledExistingLaunchClaim>,
    pane: &str,
    credential: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(claim) = restart_claim {
        remove_scheduled_pane_for_owners(state, pane, vec![claim.target_owner.clone()]).await?;
        let outcome = state
            .rollback_restart_launch(&claim.lease_owner, &claim.target_owner, None)
            .await?;
        if !matches!(
            outcome,
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
                | crate::daemon_protocol::LifecycleMutationOutcome::NotFound
                | crate::daemon_protocol::LifecycleMutationOutcome::Superseded
        ) {
            anyhow::bail!("scheduled restart rollback was rejected ({outcome:?})");
        }
        return Ok(());
    }
    let Some(owner) = reserved_owner else {
        anyhow::bail!("scheduled revival rollback has no lifecycle authority");
    };
    rollback_reserved_scheduled_pane(state, owner, pane, credential).await
}

async fn rollback_scheduled_respawn_claim_with<F, Fut>(
    state: &SharedState,
    claim: &ScheduledExistingLaunchClaim,
    pane: &str,
    cleanup: F,
) -> anyhow::Result<()>
where
    F: FnOnce(crate::daemon_protocol::ResourceOwner, String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    cleanup(claim.target_owner.clone(), pane.to_string()).await?;
    let outcome = state
        .rollback_restart_launch(&claim.lease_owner, &claim.target_owner, None)
        .await?;
    if !matches!(
        outcome,
        crate::daemon_protocol::LifecycleMutationOutcome::Applied
            | crate::daemon_protocol::LifecycleMutationOutcome::NotFound
            | crate::daemon_protocol::LifecycleMutationOutcome::Superseded
    ) {
        anyhow::bail!("scheduled same-pane rollback was rejected ({outcome:?})");
    }
    Ok(())
}

async fn rollback_scheduled_respawn_claim(
    state: &SharedState,
    claim: &ScheduledExistingLaunchClaim,
    pane: &str,
) -> anyhow::Result<()> {
    rollback_scheduled_respawn_claim_with(state, claim, pane, |owner, pane| async move {
        remove_scheduled_pane_for_owners(state, &pane, vec![owner]).await
    })
    .await
}

#[derive(Debug, Clone)]
struct TaskLaunchSelection {
    model: Option<String>,
    effort: Option<String>,
    codex_home: Option<String>,
    backend_name: Option<String>,
}

#[derive(Debug, Clone)]
struct ScheduledExistingLaunchClaim {
    lease_owner: crate::daemon_protocol::ResourceOwner,
    target_owner: crate::daemon_protocol::ResourceOwner,
}

async fn claim_scheduled_existing_launch(
    state: &SharedState,
    expected_owner: &crate::daemon_protocol::ResourceOwner,
    backend_name: &str,
    replace_backend_identity: bool,
    session_start_credential: Option<String>,
) -> Result<ScheduledExistingLaunchClaim, String> {
    claim_scheduled_existing_owner(state, expected_owner).await?;
    stage_claimed_scheduled_existing_launch(
        state,
        expected_owner,
        backend_name,
        replace_backend_identity,
        session_start_credential,
    )
    .await
}

async fn claim_scheduled_existing_owner(
    state: &SharedState,
    expected_owner: &crate::daemon_protocol::ResourceOwner,
) -> Result<(), String> {
    match state.claim_existing_start(expected_owner).await {
        Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => Ok(()),
        Ok(outcome) => Err(format!(
            "scheduled launch for '{}' was superseded before external work ({outcome:?})",
            expected_owner.session_id
        )),
        Err(error) => Err(format!(
            "scheduled launch for '{}' could not persist lifecycle authority: {error}",
            expected_owner.session_id
        )),
    }
}

async fn stage_claimed_scheduled_existing_launch(
    state: &SharedState,
    expected_owner: &crate::daemon_protocol::ResourceOwner,
    backend_name: &str,
    replace_backend_identity: bool,
    session_start_credential: Option<String>,
) -> Result<ScheduledExistingLaunchClaim, String> {
    let staged = state
        .stage_restart_launch(
            expected_owner,
            backend_name.to_string(),
            replace_backend_identity,
            session_start_credential,
            None,
        )
        .await;
    match staged {
        crate::daemon_protocol::StageFreshLaunchOutcome::Staged { incarnation } => {
            Ok(ScheduledExistingLaunchClaim {
                lease_owner: expected_owner.clone(),
                target_owner: crate::daemon_protocol::ResourceOwner {
                    session_id: expected_owner.session_id.clone(),
                    incarnation,
                },
            })
        }
        crate::daemon_protocol::StageFreshLaunchOutcome::Rejected => {
            let _ = state.abort_lifecycle(expected_owner).await;
            Err(format!(
                "scheduled launch for '{}' was superseded before external work",
                expected_owner.session_id
            ))
        }
        crate::daemon_protocol::StageFreshLaunchOutcome::PersistenceFailed => {
            let _ = state.abort_lifecycle(expected_owner).await;
            Err(format!(
                "scheduled launch for '{}' could not persist target lifecycle authority",
                expected_owner.session_id
            ))
        }
    }
}

/// Stage a scheduled Codex clear-context launch before its pane is respawned.
/// The returned incarnation is the only generation allowed to receive the
/// launch credential's eventual SessionStart binding.
#[cfg(test)]
async fn stage_scheduled_codex_launch(
    state: &SharedState,
    session_id: &str,
    backend_name: &str,
    session_start_credential: Option<String>,
) -> Result<Option<crate::daemon_protocol::SessionIncarnation>, String> {
    let Some(session_start_credential) = session_start_credential else {
        return Ok(None);
    };

    match state
        .stage_fresh_launch(
            session_id,
            backend_name.to_string(),
            Some(session_start_credential),
            None,
        )
        .await
    {
        crate::daemon_protocol::StageFreshLaunchOutcome::Staged { incarnation } => {
            Ok(Some(incarnation))
        }
        crate::daemon_protocol::StageFreshLaunchOutcome::Rejected => Err(format!(
            "scheduled launch for '{session_id}' was superseded before pane respawn"
        )),
        crate::daemon_protocol::StageFreshLaunchOutcome::PersistenceFailed => Err(format!(
            "scheduled launch for '{session_id}' could not persist lifecycle authority"
        )),
    }
}

/// Respawn the backend in an existing pane (for clears_context on a live session).
///
/// `model` and `effort` are passed from the caller's atomic snapshot of the
/// session (taken under the same lock acquisition as `dir` / `pane`) so a
/// concurrent Unregister between the caller's read and this function cannot
/// silently downgrade the respawn to backend defaults.
async fn respawn_and_inject(
    state: &SharedState,
    task: &ScheduledTask,
    expected_owner: &crate::daemon_protocol::ResourceOwner,
    pane: &str,
    dir: &str,
    launch: TaskLaunchSelection,
) -> TaskRun {
    let pane_id = pane.to_string();
    let dir = dir.to_string();
    let uses_worktree = task.on_fire.uses_worktree();
    let is_disposable = task.on_fire.is_disposable_worktree();
    let task_name = task.name.clone();

    let backend = if let Some(name) = launch.backend_name.as_deref() {
        match state.backends.get_required(name) {
            Ok(backend) => backend,
            Err(message) => return TaskRun::failed(task, message),
        }
    } else {
        state.backend_for_session(task.session_name()).await
    };
    let backend_name = backend.name().to_string();
    let session_start_credential =
        (backend_name == "codex-cli").then(crate::daemon_protocol::new_session_start_credential);
    let prior_session = {
        let proto = state.protocol.read().await;
        proto
            .sessions
            .get(task.session_name())
            .filter(|session| session.owner() == *expected_owner)
            .cloned()
    };
    let Some(_prior_session) = prior_session else {
        return TaskRun::failed(
            task,
            "scheduled pane owner changed before lifecycle claim".into(),
        );
    };
    let claim = match claim_scheduled_existing_launch(
        state,
        expected_owner,
        &backend_name,
        true,
        session_start_credential.clone(),
    )
    .await
    {
        Ok(claim) => claim,
        Err(error) => return TaskRun::failed(task, error),
    };
    let staged_incarnation = claim.target_owner.incarnation;
    match state
        .record_inert_start_pane(
            &claim.lease_owner,
            claim.target_owner.clone(),
            pane_id.clone(),
        )
        .await
    {
        Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {}
        Ok(outcome) => {
            let _ = state
                .rollback_restart_launch(&claim.lease_owner, &claim.target_owner, None)
                .await;
            return TaskRun::failed(
                task,
                format!("scheduled pane claim was superseded before respawn ({outcome:?})"),
            );
        }
        Err(error) => {
            let _ = state
                .rollback_restart_launch(&claim.lease_owner, &claim.target_owner, None)
                .await;
            return TaskRun::failed(
                task,
                format!("scheduled pane claim could not be persisted: {error}"),
            );
        }
    }
    let settings = state.settings.read().await;
    let claude_permission_mode = settings.claude_permission_mode.clone();
    let launch_model =
        crate::backend::resolve_launch_model_config(&backend_name, launch.model.clone(), &settings);
    drop(settings);
    let launch_codex_home = launch_model.codex_home.clone().or(launch.codex_home);
    crate::backend::codex::install_configured_home(launch_codex_home.as_deref());
    let claude_cmd = backend.build_start_command(&crate::backend::StartOpts {
        project_dir: dir.to_string(),
        worktree: if uses_worktree {
            if is_disposable {
                Some(crate::backend::WorktreeMode::Disposable)
            } else {
                Some(crate::backend::WorktreeMode::Named(task_name.clone()))
            }
        } else {
            None
        },
        model: launch_model.model,
        effort: launch.effort,
        permission_mode: claude_permission_mode,
        codex_home: launch_codex_home.clone(),
    });
    let claude_cmd = match session_start_credential.as_deref() {
        Some(credential) => match crate::backend::codex::with_session_start_hook(
            claude_cmd,
            launch_codex_home.as_deref(),
            task.session_name(),
            credential,
            staged_incarnation,
        ) {
            Ok(command) => command,
            Err(error) => {
                let cleanup = rollback_scheduled_respawn_claim(state, &claim, &pane_id).await;
                return TaskRun::failed(
                    task,
                    format!(
                        "could not stage Codex launch credential: {error}{}",
                        cleanup
                            .err()
                            .map(|cleanup_error| {
                                format!("; exact target cleanup failed: {cleanup_error}")
                            })
                            .unwrap_or_default()
                    ),
                );
            }
        },
        None => claude_cmd,
    };

    // Pass prompt as CLI arg (same as start_session) so Claude loads
    // CLAUDE.md and rules before processing the prompt.
    let full_cmd = if let Some(full_text) = task_prompt_text(task) {
        let prompt_path = format!("/tmp/ouija-prompt-{}.txt", task_name.replace('/', "-"));
        let _ = std::fs::write(&prompt_path, &full_text);
        let escaped_pf = shell_escape(&prompt_path);
        format!("{claude_cmd} \"$(cat {escaped_pf})\" ; rm -f {escaped_pf}")
    } else {
        claude_cmd
    };

    let session_name = task.session_name().to_string();
    let pane_incarnation = Some(staged_incarnation);
    let respawn_owner = claim.target_owner.clone();
    let respawn_session_name = session_name.clone();
    let respawn_credential = session_start_credential.clone();
    let respawn_command = full_cmd.clone();
    let respawn_pane_id = pane_id.clone();
    let respawn_operation = move || {
        tokio::task::spawn_blocking({
            let pane_id = respawn_pane_id;
            let pane_credential = respawn_credential;
            move || -> anyhow::Result<()> {
                // See `pane_env_args` for why OUIJA_SESSION_ID must ride along.
                let env_args = crate::tmux::pane_env_args(
                    &respawn_session_name,
                    pane_credential.as_deref(),
                    pane_incarnation,
                );
                let mut args: Vec<&str> = vec!["respawn-pane", "-k"];
                args.extend(env_args.iter().map(String::as_str));
                args.extend_from_slice(&["-t", &pane_id, &respawn_command]);
                crate::tmux::configure_managed_pane(&pane_id);
                let output = std::process::Command::new("tmux").args(&args).output()?;
                if !output.status.success() {
                    anyhow::bail!(
                        "respawn-pane failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                Ok(())
            }
        })
    };
    let respawn_result = state
        .with_owned_pane_claim(&respawn_owner, pane, respawn_operation)
        .await
        .unwrap_or_else(|| {
            Ok(Err(anyhow::anyhow!(
                "scheduled pane ownership changed before respawn"
            )))
        });

    match respawn_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let cleanup = rollback_scheduled_respawn_claim(state, &claim, &pane_id).await;
            return TaskRun::failed(
                task,
                format!(
                    "{e}{}",
                    cleanup
                        .err()
                        .map(|error| format!("; exact target cleanup failed: {error}"))
                        .unwrap_or_default()
                ),
            );
        }
        Err(e) => {
            let cleanup = rollback_scheduled_respawn_claim(state, &claim, &pane_id).await;
            return TaskRun::failed(
                task,
                format!(
                    "{e}{}",
                    cleanup
                        .err()
                        .map(|error| format!("; exact target cleanup failed: {error}"))
                        .unwrap_or_default()
                ),
            );
        }
    }

    // Wait for the backend process to start, then inject
    let poll_pane = pane_id.clone();
    let process_names: Vec<String> = backend
        .process_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let ready = tokio::task::spawn_blocking(move || {
        let name_refs: Vec<&str> = process_names.iter().map(|s| s.as_str()).collect();
        wait_for_process(&poll_pane, &name_refs, REVIVAL_TIMEOUT_SECS)
    })
    .await
    .unwrap_or(false);

    if !ready {
        tracing::warn!("backend did not start in time after respawn in pane {pane_id}");
    }

    let final_metadata = {
        let proto = state.protocol.read().await;
        proto
            .sessions
            .get(task.session_name())
            .filter(|session| session.owner() == claim.target_owner)
            .map(|session| {
                let mut metadata = session.metadata.clone();
                if metadata.prompt.is_none() {
                    metadata.prompt = task.prompt.clone();
                }
                if metadata.reminder.is_none() {
                    metadata.reminder = task.reminder.clone();
                }
                if metadata.on_fire.is_none() {
                    metadata.on_fire = Some(task.on_fire.clone());
                }
                if session_start_credential.is_none() {
                    metadata.backend_session_id = None;
                }
                metadata
            })
    };
    let Some(final_metadata) = final_metadata else {
        return TaskRun::failed(
            task,
            "scheduled readiness result was superseded by a replacement".into(),
        );
    };
    match state
        .complete_restart_launch(
            &claim.lease_owner,
            &claim.target_owner,
            Some(pane_id),
            final_metadata,
            true,
        )
        .await
    {
        Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => TaskRun::ok(task, None),
        Ok(outcome) => TaskRun::failed(
            task,
            format!("scheduled restart completion was superseded ({outcome:?})"),
        ),
        Err(error) => TaskRun::failed(
            task,
            format!("scheduled restart completion could not be persisted: {error}"),
        ),
    }
}

/// Create or revive a session and inject a message.
///
/// `project_dir_override` falls back to `task.project_dir` if `None`.
/// Backend metadata is passed through from the caller's session snapshot;
/// for the 'session not found' path the caller passes `None` (there is no
/// prior metadata to honour).
#[expect(
    clippy::too_many_arguments,
    reason = "the scheduler threads one atomic session snapshot without a second mutable config type"
)]
async fn revive_from_task(
    state: &SharedState,
    task: &ScheduledTask,
    expected_owner: Option<crate::daemon_protocol::ResourceOwner>,
    project_dir_override: Option<&str>,
    model: Option<String>,
    effort: Option<String>,
    codex_home: Option<String>,
    backend_name: Option<String>,
) -> TaskRun {
    let project_dir = project_dir_override.or(task.project_dir.as_deref());
    match revive_and_inject(
        state,
        task,
        expected_owner,
        project_dir,
        model,
        effort,
        codex_home,
        backend_name,
    )
    .await
    {
        Ok(new_pane) => TaskRun::ok(task, Some(new_pane)),
        Err(e) => TaskRun::failed(task, e.to_string()),
    }
}

/// Revive a dead session: create new tmux window, launch the backend, re-register, inject.
///
/// Backend metadata is threaded through from the caller's session snapshot
/// — `execute_injection` captures them under the same atomic read that
/// sourced `project_dir`, so a concurrent Unregister between the caller's
/// read and this function cannot silently downgrade the revive to backend
/// defaults. When the caller is the 'session not found' branch, both are
/// `None` (no prior metadata to honour).
#[expect(
    clippy::too_many_arguments,
    reason = "the scheduler threads one atomic session snapshot without a second mutable config type"
)]
async fn revive_and_inject(
    state: &SharedState,
    task: &ScheduledTask,
    expected_owner: Option<crate::daemon_protocol::ResourceOwner>,
    project_dir: Option<&str>,
    model: Option<String>,
    effort: Option<String>,
    codex_home: Option<String>,
    backend_name: Option<String>,
) -> anyhow::Result<String> {
    let dir = project_dir
        .map(String::from)
        .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()));

    let clears_context = task.on_fire.clears_context();
    let uses_worktree = task.on_fire.uses_worktree();
    let is_disposable = task.on_fire.is_disposable_worktree();

    // Build the launch command before entering the blocking closure.
    let worktree = if uses_worktree {
        if is_disposable {
            Some(crate::backend::WorktreeMode::Disposable)
        } else {
            Some(crate::backend::WorktreeMode::Named(task.name.clone()))
        }
    } else {
        None
    };
    let backend = if let Some(name) = backend_name.as_deref() {
        state
            .backends
            .get_required(name)
            .map_err(anyhow::Error::msg)?
    } else {
        state.backend_for_session(task.session_name()).await
    };
    let is_tui = matches!(
        backend.delivery_mode(),
        crate::backend::DeliveryMode::TuiInjection
    );
    let backend_name = backend.name().to_string();
    let _prior_session = if let Some(expected_owner) = expected_owner.as_ref() {
        let prior = state
            .protocol
            .read()
            .await
            .sessions
            .get(task.session_name())
            .filter(|session| session.owner() == *expected_owner)
            .cloned();
        let Some(prior) = prior else {
            anyhow::bail!(
                "scheduled revival for '{}' was superseded before lifecycle claim",
                task.session_name()
            );
        };
        claim_scheduled_existing_owner(state, expected_owner)
            .await
            .map_err(anyhow::Error::msg)?;
        Some(prior)
    } else {
        None
    };
    let reserved_owner = if expected_owner.is_none() {
        match state.reserve_start(task.session_name()).await? {
            crate::daemon_protocol::StartDisposition::Reserved(owner) => Some(owner),
            crate::daemon_protocol::StartDisposition::Existing(owner) => {
                anyhow::bail!(
                    "scheduled revival for '{}' was superseded by existing owner {owner:?}",
                    task.session_name()
                );
            }
            crate::daemon_protocol::StartDisposition::InProgress(owner) => {
                anyhow::bail!(
                    "scheduled revival for '{}' was superseded by in-progress owner {owner:?}",
                    task.session_name()
                );
            }
        }
    } else {
        None
    };
    let settings = state.settings.read().await;
    let claude_permission_mode = settings.claude_permission_mode.clone();
    let launch_model =
        crate::backend::resolve_launch_model_config(&backend_name, model.clone(), &settings);
    drop(settings);
    let launch_codex_home = launch_model.codex_home.clone().or(codex_home);
    crate::backend::codex::install_configured_home(launch_codex_home.as_deref());
    let detected_backend_session_id = if task.backend_session_id.is_none() {
        backend.detect_session_id(&dir)
    } else {
        None
    };
    let resume_backend_session_id = task
        .backend_session_id
        .clone()
        .or_else(|| detected_backend_session_id.clone());
    let replace_backend_identity = clears_context || resume_backend_session_id.is_none();
    let session_start_credential = (backend_name == "codex-cli" && replace_backend_identity)
        .then(crate::daemon_protocol::new_session_start_credential);
    let restart_claim = if let Some(expected_owner) = expected_owner.as_ref() {
        match stage_claimed_scheduled_existing_launch(
            state,
            expected_owner,
            &backend_name,
            replace_backend_identity,
            session_start_credential.clone(),
        )
        .await
        {
            Ok(claim) => Some(claim),
            Err(error) => return Err(anyhow::Error::msg(error)),
        }
    } else {
        None
    };
    let launch_cmd = if clears_context {
        backend.build_start_command(&crate::backend::StartOpts {
            project_dir: dir.clone(),
            worktree,
            model: launch_model.model.clone(),
            effort: effort.clone(),
            permission_mode: claude_permission_mode.clone(),
            codex_home: launch_codex_home.clone(),
        })
    } else {
        backend
            .build_resume_command(&crate::backend::ResumeOpts {
                project_dir: dir.clone(),
                session_id: resume_backend_session_id.clone(),
                worktree,
                model: launch_model.model.clone(),
                effort: effort.clone(),
                permission_mode: claude_permission_mode.clone(),
                codex_home: launch_codex_home.clone(),
            })
            .unwrap_or_else(|| {
                backend.build_start_command(&crate::backend::StartOpts {
                    project_dir: dir.clone(),
                    worktree: None,
                    model: launch_model.model.clone(),
                    effort: effort.clone(),
                    permission_mode: claude_permission_mode.clone(),
                    codex_home: launch_codex_home.clone(),
                })
            })
    };

    crate::backend::claude_code::pre_trust_workspace(&dir);
    crate::backend::pre_trust_mise(&dir);

    let proto_meta = revived_session_metadata(
        task,
        project_dir,
        detected_backend_session_id.clone(),
        RevivedSessionSnapshot {
            model: model.clone(),
            effort: effort.clone(),
            codex_home: launch_codex_home.clone(),
            backend_name: backend.name(),
            is_tui,
            clears_context,
            session_start_credential: session_start_credential.clone(),
        },
    );
    let scheduled_prompt_backend_session_id = proto_meta.backend_session_id.clone();
    let initial_pane_owner = reserved_owner.clone().or_else(|| {
        restart_claim
            .as_ref()
            .map(|claim| claim.target_owner.clone())
    });
    let initial_pane_incarnation = initial_pane_owner.as_ref().map(|owner| owner.incarnation);

    // Create named tmux session/window for the revived session.
    // If a tmux session with the target name exists, add a window to it;
    // otherwise create a new tmux session. Both get the ouija session name.
    let create_pane = || {
        tokio::task::spawn_blocking({
            let dir = dir.clone();
            let window_name = task.session_name().to_string();
            let tmux_session = crate::tmux::tmux_session_name(&dir);
            let pane_credential = session_start_credential.clone();
            let pane_incarnation = initial_pane_incarnation;
            move || -> anyhow::Result<String> {
                let tmux_session_exists = std::process::Command::new("tmux")
                    .args(["has-session", "-t", &tmux_session])
                    .output()
                    .is_ok_and(|o| o.status.success());

                let target = format!("{tmux_session}:");
                // `pane_env_args` exports OUIJA_SESSION_ID (so the ouija CLI
                // can resolve the caller's identity) and suppresses shell
                // history (HISTFILE/fish_history).
                let env_args = crate::tmux::pane_env_args(
                    &window_name,
                    pane_credential.as_deref(),
                    pane_incarnation,
                );
                let output = if tmux_session_exists {
                    let mut args: Vec<&str> = vec!["new-window", "-d"];
                    args.extend(env_args.iter().map(String::as_str));
                    args.extend_from_slice(&[
                        "-t",
                        &target,
                        "-n",
                        &window_name,
                        "-P",
                        "-F",
                        "#{pane_id}",
                    ]);
                    std::process::Command::new("tmux").args(&args).output()?
                } else {
                    let mut args: Vec<&str> = vec!["new-session", "-d"];
                    args.extend(env_args.iter().map(String::as_str));
                    args.extend_from_slice(&[
                        "-s",
                        &tmux_session,
                        "-n",
                        &window_name,
                        "-P",
                        "-F",
                        "#{pane_id}",
                    ]);
                    std::process::Command::new("tmux").args(&args).output()?
                };
                if !output.status.success() {
                    anyhow::bail!(
                        "tmux session/window creation failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                let pane_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
                Ok(pane_id)
            }
        })
    };
    let (directory_lease_owner, directory_owner) = if let Some(owner) = reserved_owner.as_ref() {
        (owner, owner.clone())
    } else if let Some(claim) = restart_claim.as_ref() {
        (&claim.lease_owner, claim.target_owner.clone())
    } else {
        anyhow::bail!("scheduled revival missing lifecycle authority");
    };
    let new_pane_result = match state
        .with_reserved_project_dir_claim(
            directory_lease_owner,
            directory_owner,
            &dir,
            false,
            create_pane,
        )
        .await?
    {
        Some(result) => result,
        None => {
            if let Some(claim) = restart_claim.as_ref() {
                let _ = state
                    .rollback_restart_launch(&claim.lease_owner, &claim.target_owner, None)
                    .await;
            } else if let Some(owner) = reserved_owner.as_ref() {
                let _ = state.abort_lifecycle(owner).await;
            }
            anyhow::bail!(
                "scheduled revival directory claim was superseded for '{}'",
                task.session_name()
            );
        }
    };
    let new_pane = match new_pane_result {
        Ok(Ok(pane)) => pane,
        Ok(Err(error)) => {
            if let Some(claim) = restart_claim.as_ref() {
                let _ = state
                    .rollback_restart_launch(&claim.lease_owner, &claim.target_owner, None)
                    .await;
            } else if let Some(owner) = reserved_owner.as_ref() {
                let _ = state.abort_lifecycle(owner).await;
            }
            return Err(error);
        }
        Err(error) => {
            if let Some(claim) = restart_claim.as_ref() {
                let _ = state
                    .rollback_restart_launch(&claim.lease_owner, &claim.target_owner, None)
                    .await;
            } else if let Some(owner) = reserved_owner.as_ref() {
                let _ = state.abort_lifecycle(owner).await;
            }
            return Err(anyhow::anyhow!(
                "scheduled pane creation task failed: {error}"
            ));
        }
    };

    // The pane exists but has not started the backend yet. Register its
    // launch credential first so Codex's SessionStart hook can find and
    // authenticate the exact pane before it reports its initial thread ID.
    if let Some(owner) = reserved_owner.as_ref() {
        match state
            .record_inert_start_pane(owner, owner.clone(), new_pane.clone())
            .await
        {
            Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {}
            Ok(outcome) => {
                let cleanup =
                    remove_scheduled_pane_for_owners(state, &new_pane, vec![owner.clone()]).await;
                if cleanup.is_ok() {
                    let _ = state.abort_lifecycle(owner).await;
                }
                anyhow::bail!(
                    "scheduled revival pane reservation was superseded ({outcome:?}){}",
                    cleanup
                        .err()
                        .map(|error| format!("; exact pane cleanup failed: {error}"))
                        .unwrap_or_default()
                );
            }
            Err(error) => {
                let cleanup =
                    remove_scheduled_pane_for_owners(state, &new_pane, vec![owner.clone()]).await;
                if cleanup.is_ok() {
                    let _ = state.abort_lifecycle(owner).await;
                }
                anyhow::bail!(
                    "scheduled revival could not persist pane authority: {error}{}",
                    cleanup
                        .err()
                        .map(|cleanup_error| {
                            format!("; exact pane cleanup failed: {cleanup_error}")
                        })
                        .unwrap_or_default()
                );
            }
        }
        match state
            .commit_reserved_start(owner, Some(new_pane.clone()), proto_meta.clone())
            .await
        {
            Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {}
            Ok(outcome) => {
                let cleanup =
                    remove_scheduled_pane_for_owners(state, &new_pane, vec![owner.clone()]).await;
                if cleanup.is_ok() {
                    let _ = state.abort_lifecycle(owner).await;
                }
                anyhow::bail!(
                    "scheduled revival registration was superseded ({outcome:?}){}",
                    cleanup
                        .err()
                        .map(|error| format!("; exact pane cleanup failed: {error}"))
                        .unwrap_or_default()
                );
            }
            Err(error) => {
                let cleanup =
                    remove_scheduled_pane_for_owners(state, &new_pane, vec![owner.clone()]).await;
                if cleanup.is_ok() {
                    let _ = state.abort_lifecycle(owner).await;
                }
                anyhow::bail!(
                    "scheduled revival could not persist registration: {error}{}",
                    cleanup
                        .err()
                        .map(|cleanup_error| {
                            format!("; exact pane cleanup failed: {cleanup_error}")
                        })
                        .unwrap_or_default()
                );
            }
        }
    } else if let Some(claim) = restart_claim.as_ref() {
        match state
            .record_inert_start_pane(
                &claim.lease_owner,
                claim.target_owner.clone(),
                new_pane.clone(),
            )
            .await
        {
            Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {}
            Ok(outcome) => {
                let cleanup = remove_scheduled_pane_for_owners(
                    state,
                    &new_pane,
                    vec![claim.target_owner.clone()],
                )
                .await;
                if cleanup.is_ok() {
                    let _ = state
                        .rollback_restart_launch(&claim.lease_owner, &claim.target_owner, None)
                        .await;
                }
                anyhow::bail!(
                    "scheduled revival pane reservation was superseded ({outcome:?}){}",
                    cleanup
                        .err()
                        .map(|error| format!("; exact pane cleanup failed: {error}"))
                        .unwrap_or_default()
                );
            }
            Err(error) => {
                let cleanup = remove_scheduled_pane_for_owners(
                    state,
                    &new_pane,
                    vec![claim.target_owner.clone()],
                )
                .await;
                if cleanup.is_ok() {
                    let _ = state
                        .rollback_restart_launch(&claim.lease_owner, &claim.target_owner, None)
                        .await;
                }
                anyhow::bail!(
                    "scheduled revival could not persist pane authority: {error}{}",
                    cleanup
                        .err()
                        .map(|cleanup_error| {
                            format!("; exact pane cleanup failed: {cleanup_error}")
                        })
                        .unwrap_or_default()
                );
            }
        }
    }

    // Registration allocates the authoritative incarnation for a newly
    // revived ID. Respawn the still-inert shell with that exact environment
    // before the backend command can run.
    let registered_owner = initial_pane_owner
        .clone()
        .expect("scheduled lifecycle authority has an exact target owner");
    let registered_authority = {
        let proto = state.protocol.read().await;
        let session_matches = proto
            .sessions
            .get(task.session_name())
            .is_some_and(|session| session.owner() == registered_owner);
        let pane_matches = if let Some(claim) = restart_claim.as_ref() {
            proto
                .lifecycle_leases
                .get(task.session_name())
                .is_some_and(|lease| {
                    lease.owner == claim.lease_owner
                        && lease.restart_target_owner.as_ref() == Some(&registered_owner)
                        && lease.inert_pane.as_deref() == Some(new_pane.as_str())
                        && lease.inert_pane_owner.as_ref() == Some(&registered_owner)
                })
        } else {
            proto
                .sessions
                .get(task.session_name())
                .is_some_and(|session| session.pane.as_deref() == Some(new_pane.as_str()))
        };
        session_matches && pane_matches
    };
    if !registered_authority {
        if let Some(initial_owner) = initial_pane_owner.as_ref() {
            remove_scheduled_pane_for_owners(state, &new_pane, vec![initial_owner.clone()]).await?;
        }
        if let Some(claim) = restart_claim.as_ref() {
            let _ = state
                .rollback_restart_launch(&claim.lease_owner, &claim.target_owner, None)
                .await;
        } else if let Some(owner) = reserved_owner.as_ref() {
            let _ = state.abort_lifecycle(owner).await;
        }
        anyhow::bail!(
            "scheduled revival registration was superseded before launch for '{}'",
            task.session_name()
        );
    }
    let registered_incarnation = registered_owner.incarnation;
    let launch_cmd = match session_start_credential.as_deref() {
        Some(credential) => crate::backend::codex::with_session_start_hook(
            launch_cmd,
            launch_codex_home.as_deref(),
            task.session_name(),
            credential,
            registered_incarnation,
        )?,
        None => launch_cmd,
    };
    // Pass prompt as CLI arg so Claude loads CLAUDE.md before processing it.
    // TuiInjection always uses CLI arg; HttpApi only when starting fresh.
    let full_launch_cmd = if clears_context || is_tui {
        if let Some(full_text) = task_prompt_text(task) {
            let prompt_path = format!("/tmp/ouija-prompt-{}.txt", task.name.replace('/', "-"));
            let _ = std::fs::write(&prompt_path, &full_text);
            let escaped_pf = shell_escape(&prompt_path);
            format!("{launch_cmd} \"$(cat {escaped_pf})\" ; rm -f {escaped_pf}")
        } else {
            launch_cmd.clone()
        }
    } else {
        launch_cmd.clone()
    };
    let pane_for_environment = new_pane.clone();
    let session_for_environment = task.session_name().to_string();
    let credential_for_environment = session_start_credential.clone();
    let registered_owner = crate::daemon_protocol::ResourceOwner {
        session_id: task.session_name().to_string(),
        incarnation: registered_incarnation,
    };
    let environment_operation = move || {
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let env_args = crate::tmux::pane_env_args(
                &session_for_environment,
                credential_for_environment.as_deref(),
                Some(registered_incarnation),
            );
            let mut args: Vec<&str> = vec!["respawn-pane", "-k"];
            args.extend(env_args.iter().map(String::as_str));
            args.extend_from_slice(&["-t", &pane_for_environment]);
            let output = std::process::Command::new("tmux").args(&args).output()?;
            if !output.status.success() {
                anyhow::bail!(
                    "scheduled pane environment respawn failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Ok(())
        })
    };
    let environment_result = state
        .with_owned_pane_claim(&registered_owner, &new_pane, environment_operation)
        .await
        .unwrap_or_else(|| {
            Ok(Err(anyhow::anyhow!(
                "scheduled pane ownership changed before environment respawn"
            )))
        });
    match environment_result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            rollback_scheduled_revival_authority(
                state,
                reserved_owner.as_ref(),
                restart_claim.as_ref(),
                &new_pane,
                session_start_credential.as_deref(),
            )
            .await?;
            return Err(error);
        }
        Err(error) => {
            rollback_scheduled_revival_authority(
                state,
                reserved_owner.as_ref(),
                restart_claim.as_ref(),
                &new_pane,
                session_start_credential.as_deref(),
            )
            .await?;
            return Err(anyhow::anyhow!(
                "scheduled pane environment task failed: {error}"
            ));
        }
    }

    let pane_for_launch = new_pane.clone();
    let launch_operation = move || {
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            crate::tmux::configure_managed_pane(&pane_for_launch);
            // Leading space prevents the command from being recorded in shell
            // history (zsh HIST_IGNORE_SPACE / bash HISTCONTROL=ignorespace).
            let launch_then_exit = crate::tmux::close_shell_after(&full_launch_cmd);
            let hidden_cmd = format!(" {launch_then_exit}");
            let status = std::process::Command::new("tmux")
                .args(["send-keys", "-t", &pane_for_launch, &hidden_cmd, "Enter"])
                .status()?;
            if !status.success() {
                anyhow::bail!("tmux send-keys failed for pane {pane_for_launch}");
            }
            Ok(())
        })
    };
    let launch_result = state
        .with_owned_pane_claim(&registered_owner, &new_pane, launch_operation)
        .await
        .unwrap_or_else(|| {
            Ok(Err(anyhow::anyhow!(
                "scheduled pane ownership changed before backend launch"
            )))
        });
    match launch_result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            rollback_scheduled_revival_authority(
                state,
                reserved_owner.as_ref(),
                restart_claim.as_ref(),
                &new_pane,
                session_start_credential.as_deref(),
            )
            .await?;
            return Err(error);
        }
        Err(error) => {
            rollback_scheduled_revival_authority(
                state,
                reserved_owner.as_ref(),
                restart_claim.as_ref(),
                &new_pane,
                session_start_credential.as_deref(),
            )
            .await?;
            return Err(anyhow::anyhow!("scheduled launch task failed: {error}"));
        }
    }

    // Phase 1: Wait for the backend process to appear in the pane
    let poll_pane = new_pane.clone();
    let process_names: Vec<String> = backend
        .process_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let backend_name = backend.name().to_string();
    let tui_pattern = backend.tui_ready_pattern().map(String::from);
    let process_ready = tokio::task::spawn_blocking(move || {
        let name_refs: Vec<&str> = process_names.iter().map(|s| s.as_str()).collect();
        wait_for_process(&poll_pane, &name_refs, REVIVAL_TIMEOUT_SECS)
    })
    .await
    .unwrap_or(false);

    if !process_ready {
        rollback_scheduled_revival_authority(
            state,
            reserved_owner.as_ref(),
            restart_claim.as_ref(),
            &new_pane,
            session_start_credential.as_deref(),
        )
        .await?;
        anyhow::bail!(
            "{backend_name} did not start within {REVIVAL_TIMEOUT_SECS}s in pane {new_pane}"
        );
    }

    // Phase 2: Wait for the backend's TUI to be ready (prompt indicator appears)
    if let Some(pattern) = tui_pattern {
        let poll_pane = new_pane.clone();
        let tui_ready = tokio::task::spawn_blocking(move || {
            wait_for_tui_ready(&poll_pane, Some(&pattern), TUI_READY_TIMEOUT_SECS)
        })
        .await
        .unwrap_or(false);

        if !tui_ready {
            tracing::warn!(
                "{backend_name} TUI prompt not detected within {TUI_READY_TIMEOUT_SECS}s in pane {new_pane}, proceeding anyway"
            );
        }
    }

    if !scheduled_launch_is_current(state, &registered_owner, &new_pane).await {
        anyhow::bail!(
            "scheduled readiness result for '{}' was superseded",
            task.session_name()
        );
    }

    if let Some(claim) = restart_claim.as_ref() {
        let final_metadata = {
            let proto = state.protocol.read().await;
            proto
                .sessions
                .get(task.session_name())
                .filter(|session| session.owner() == claim.target_owner)
                .map(|session| {
                    let mut metadata = session.metadata.clone();
                    metadata.project_dir = proto_meta.project_dir.clone();
                    metadata.prompt = proto_meta.prompt.clone();
                    metadata.reminder = proto_meta.reminder.clone();
                    metadata.model = proto_meta.model.clone();
                    metadata.effort = proto_meta.effort.clone();
                    metadata.codex_home = proto_meta.codex_home.clone();
                    metadata.on_fire = proto_meta.on_fire.clone();
                    metadata.backend = proto_meta.backend.clone();
                    if !replace_backend_identity {
                        metadata.backend_session_id = proto_meta.backend_session_id.clone();
                    }
                    metadata.opencode_binding = metadata
                        .opencode_binding
                        .or(proto_meta.opencode_binding.clone());
                    metadata
                })
        };
        let Some(final_metadata) = final_metadata else {
            anyhow::bail!(
                "scheduled launch completion for '{}' was superseded",
                task.session_name()
            );
        };
        match state
            .complete_restart_launch(
                &claim.lease_owner,
                &claim.target_owner,
                Some(new_pane.clone()),
                final_metadata,
                true,
            )
            .await
        {
            Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied) => {}
            Ok(outcome) => {
                anyhow::bail!("scheduled launch completion was superseded ({outcome:?})");
            }
            Err(error) => {
                anyhow::bail!(
                    "scheduled launch completion could not be persisted; recovery authority retained: {error}"
                );
            }
        }
    } else if let Some(owner) = reserved_owner.as_ref()
        && let Err(error) = state.abort_lifecycle(owner).await
    {
        rollback_reserved_scheduled_pane(
            state,
            owner,
            &new_pane,
            session_start_credential.as_deref(),
        )
        .await?;
        anyhow::bail!("scheduled revival failed to persist launch completion: {error}");
    }

    // Track disposable worktree panes for reaper cleanup
    if task.on_fire.is_disposable_worktree() {
        if let Some(dir) = project_dir {
            state
                .track_perfire_worktree(&registered_owner, &new_pane, dir)
                .await;
        }
    }

    // HttpApi: deliver prompt via schedule_prompt_injection (readiness signal).
    // TuiInjection prompt was already passed as CLI arg above.
    if !is_tui {
        if let Some(ref prompt) = task.prompt {
            let full_text = match &task.reminder {
                Some(r) => format!("{prompt}\n\n{r}"),
                None => prompt.clone(),
            };
            crate::nostr_transport::schedule_prompt_injection_owned(
                state,
                task.session_name(),
                new_pane.clone(),
                full_text,
                scheduled_prompt_backend_session_id,
                registered_owner.clone(),
            );
        }
    }

    Ok(new_pane)
}

/// Undo a definite post-registration launch failure only while this invocation
/// still owns the staged pane and credential. A SessionStart hook that already
/// consumed the credential has committed a newer binding and must win.
#[cfg(test)]
async fn rollback_provisional_revival(
    state: &SharedState,
    session_id: &str,
    pane_id: &str,
    credential: Option<&str>,
    prior_session: Option<&crate::daemon_protocol::SessionEntry>,
) {
    state
        .apply_and_execute(
            crate::daemon_protocol::Event::RollbackProvisionalRegistration {
                id: session_id.to_string(),
                pane: pane_id.to_string(),
                credential: credential.map(str::to_string),
                previous: prior_session.cloned(),
            },
        )
        .await;
}

/// Invert a StageFreshLaunch only while its exact incarnation remains pending.
/// A SessionStart hook consumes the credential, so a concurrently bound thread
/// cannot satisfy this guard and always wins over the failed launch.
#[cfg(test)]
async fn rollback_staged_fresh_launch(
    state: &SharedState,
    session_id: &str,
    pane_id: &str,
    credential: &str,
    staged_incarnation: crate::daemon_protocol::SessionIncarnation,
    previous: &crate::daemon_protocol::SessionEntry,
) {
    state
        .apply_and_execute(crate::daemon_protocol::Event::RollbackFreshLaunch {
            id: session_id.to_string(),
            pane: Some(pane_id.to_string()),
            credential: Some(credential.to_string()),
            staged_incarnation,
            previous: Some(previous.clone()),
            provisional_pane: None,
        })
        .await;
}

struct RevivedSessionSnapshot<'a> {
    model: Option<String>,
    effort: Option<String>,
    codex_home: Option<String>,
    backend_name: &'a str,
    is_tui: bool,
    clears_context: bool,
    session_start_credential: Option<String>,
}

fn revived_session_metadata(
    task: &ScheduledTask,
    project_dir: Option<&str>,
    detected_backend_session_id: Option<String>,
    snapshot: RevivedSessionSnapshot<'_>,
) -> crate::daemon_protocol::SessionMeta {
    let backend_session_id = scheduled_prompt_backend_session_id(
        task.backend_session_id.as_deref(),
        detected_backend_session_id,
        snapshot.backend_name,
        snapshot.is_tui,
        snapshot.clears_context,
    );
    let opencode_binding = revived_opencode_binding(snapshot.backend_name);

    crate::daemon_protocol::SessionMeta {
        project_dir: project_dir.map(String::from),
        prompt: task.prompt.clone(),
        reminder: task.reminder.clone(),
        model: snapshot.model,
        effort: snapshot.effort,
        codex_home: snapshot.codex_home,
        on_fire: Some(task.on_fire.clone()),
        backend_session_id,
        backend: Some(snapshot.backend_name.to_string()),
        session_start_credential: snapshot.session_start_credential,
        opencode_binding,
        ..Default::default()
    }
}

fn revived_opencode_binding(backend_name: &str) -> Option<crate::daemon_protocol::OpenCodeBinding> {
    if backend_name != "opencode" {
        return None;
    }
    Some(crate::daemon_protocol::OpenCodeBinding::WeakAdopted)
}

fn scheduled_prompt_backend_session_id(
    task_backend_session_id: Option<&str>,
    detected_backend_session_id: Option<String>,
    backend_name: &str,
    is_tui: bool,
    clears_context: bool,
) -> Option<String> {
    if clears_context || (is_tui && backend_name != "codex-cli") {
        None
    } else {
        task_backend_session_id
            .map(str::to_string)
            .or(detected_backend_session_id)
    }
}

/// Poll a pane until one of `names` appears anywhere in its process tree
/// (blocking).
///
/// Uses `pane_alive` (a process-tree walk) rather than matching only
/// `pane_current_command`: assistants launched through a shell or npx/node
/// wrapper (e.g. `codex`, whose foreground command reads as `node`) run the
/// real long-lived process as a descendant, which the current-command check
/// would miss.
fn wait_for_process(pane: &str, names: &[&str], timeout_secs: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_secs(REVIVAL_POLL_SECS));
        if crate::tmux::pane_alive(pane, names) {
            return true;
        }
    }
    false
}

/// Poll a pane until the TUI prompt pattern appears (blocking).
/// If `pattern` is `None`, returns `true` immediately (no TUI readiness check).
fn wait_for_tui_ready(pane: &str, pattern: Option<&str>, timeout_secs: u64) -> bool {
    let Some(pattern) = pattern else {
        return true;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_secs(REVIVAL_POLL_SECS));
        if let Ok(output) = std::process::Command::new("tmux")
            .args(["capture-pane", "-t", pane, "-p", "-S", "-20"])
            .output()
        {
            if String::from_utf8_lossy(&output.stdout).contains(pattern) {
                return true;
            }
        }
    }
    false
}

/// Escape a string for safe use in shell commands.
pub(crate) fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Create a new enabled `ScheduledTask` with computed `next_run`.
///
/// The task is assigned a random hex ID and starts with zero runs.
#[expect(
    clippy::too_many_arguments,
    reason = "flat parameters clearer than a builder for internal API"
)]
pub fn new_task(
    name: String,
    cron: String,
    target_session: Option<String>,
    prompt: Option<String>,
    reminder: Option<String>,
    once: bool,
    backend_session_id: Option<String>,
    on_fire: OnFire,
) -> ScheduledTask {
    let next_run = compute_next_run(&cron);
    ScheduledTask {
        id: generate_task_id(),
        name,
        cron,
        target_session,
        prompt,
        reminder,
        enabled: true,
        created_at: Utc::now(),
        next_run,
        last_run: None,
        last_status: None,
        run_count: 0,
        project_dir: None,
        backend: None,
        model: None,
        effort: None,
        once,
        backend_session_id,
        on_fire,
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
            target_session: Some("web".into()),
            prompt: None,
            reminder: None,
            enabled: true,
            created_at: Utc::now(),
            next_run: Some(Utc::now()),
            last_run: None,
            last_status: None,
            run_count: 0,
            project_dir: Some("/tmp".into()),
            backend: Some("codex-cli".into()),
            model: Some("gpt-5.5".into()),
            effort: None,
            once: false,
            backend_session_id: None,
            on_fire: OnFire::ContinueSession,
        };
        let json = serde_json::to_string(&task).unwrap();
        let decoded: ScheduledTask = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, task.id);
        assert_eq!(decoded.name, task.name);
        assert_eq!(decoded.project_dir, task.project_dir);
        assert_eq!(decoded.backend.as_deref(), Some("codex-cli"));
        assert_eq!(decoded.model.as_deref(), Some("gpt-5.5"));
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
            Some("web".into()),
            None,
            None,
            false,
            None,
            OnFire::ContinueSession,
        );
        assert!(task.next_run.is_some());
        assert!(task.enabled);
        assert_eq!(task.run_count, 0);
    }

    #[test]
    fn task_worktree_serialization() {
        let task = ScheduledTask {
            id: "wt123456".into(),
            name: "wt-task".into(),
            cron: "0 9 * * *".into(),
            target_session: Some("web".into()),
            prompt: None,
            reminder: None,
            enabled: true,
            created_at: Utc::now(),
            next_run: None,
            last_run: None,
            last_status: None,
            run_count: 0,
            project_dir: Some("/tmp/project".into()),
            backend: None,
            model: None,
            effort: None,
            once: false,
            backend_session_id: None,
            on_fire: OnFire::DisposableWorktree,
        };
        let json = serde_json::to_string(&task).unwrap();
        assert!(json.contains("\"mode\":\"disposable_worktree\""));
        let decoded: ScheduledTask = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.on_fire, OnFire::DisposableWorktree);
    }

    #[test]
    fn task_worktree_defaults_on_missing_fields() {
        let json = r#"{"id":"x","name":"n","cron":"* * * * *","target_session":"s","enabled":true,"created_at":"2026-01-01T00:00:00Z","run_count":0,"once":false}"#;
        let task: ScheduledTask = serde_json::from_str(json).unwrap();
        assert_eq!(task.on_fire, OnFire::ContinueSession);
    }

    #[test]
    fn on_fire_default_is_continue_session() {
        assert_eq!(OnFire::default(), OnFire::ContinueSession);
    }

    #[test]
    fn on_fire_serialization_round_trip() {
        let variants = vec![
            OnFire::ContinueSession,
            OnFire::NewSession,
            OnFire::PersistentWorktree {
                clear_context: false,
            },
            OnFire::PersistentWorktree {
                clear_context: true,
            },
            OnFire::DisposableWorktree,
        ];
        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let decoded: OnFire = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, variant, "round-trip failed for {json}");
        }
    }

    #[test]
    fn on_fire_clear_context_defaults_false() {
        let json = r#"{"mode":"persistent_worktree"}"#;
        let on_fire: OnFire = serde_json::from_str(json).unwrap();
        assert_eq!(
            on_fire,
            OnFire::PersistentWorktree {
                clear_context: false
            }
        );
        assert!(!on_fire.clears_context());
    }

    #[test]
    fn legacy_task_json_migrates_to_on_fire() {
        let json = r#"{"id":"x","name":"n","cron":"* * * * *","target_session":"s","enabled":true,"created_at":"2026-01-01T00:00:00Z","run_count":0,"fresh":true,"worktree":true,"worktree_mode":"per-fire"}"#;
        let task: ScheduledTask = serde_json::from_str(json).unwrap();
        assert_eq!(task.on_fire, OnFire::DisposableWorktree);
    }

    #[test]
    fn legacy_task_fresh_only_migrates() {
        let json = r#"{"id":"x","name":"n","cron":"* * * * *","target_session":"s","enabled":true,"created_at":"2026-01-01T00:00:00Z","run_count":0,"fresh":true}"#;
        let task: ScheduledTask = serde_json::from_str(json).unwrap();
        assert_eq!(task.on_fire, OnFire::NewSession);
    }

    #[test]
    fn legacy_task_no_flags_migrates() {
        let json = r#"{"id":"x","name":"n","cron":"* * * * *","target_session":"s","enabled":true,"created_at":"2026-01-01T00:00:00Z","run_count":0,"fresh":false}"#;
        let task: ScheduledTask = serde_json::from_str(json).unwrap();
        assert_eq!(task.on_fire, OnFire::ContinueSession);
    }

    #[test]
    fn on_fire_kills_alive() {
        assert!(!OnFire::ContinueSession.kills_alive());
        assert!(!OnFire::NewSession.kills_alive());
        assert!(
            !OnFire::PersistentWorktree {
                clear_context: false
            }
            .kills_alive()
        );
        assert!(
            OnFire::PersistentWorktree {
                clear_context: true
            }
            .kills_alive()
        );
        assert!(OnFire::DisposableWorktree.kills_alive());
    }

    #[test]
    fn new_task_with_prompt_and_reminder() {
        let task = new_task(
            "test-task".into(),
            "0 0 * * *".into(),
            None,
            Some("do the work".into()),
            Some("call loop_next".into()),
            false,
            None,
            OnFire::NewSession,
        );
        assert_eq!(task.prompt.as_deref(), Some("do the work"));
        assert_eq!(task.reminder.as_deref(), Some("call loop_next"));
    }

    #[test]
    fn scheduled_http_prompt_uses_resume_backend_session_id() {
        assert_eq!(
            scheduled_prompt_backend_session_id(
                Some("ses_task"),
                Some("ses_detected".to_string()),
                "opencode",
                false,
                false,
            )
            .as_deref(),
            Some("ses_task")
        );
        assert_eq!(
            scheduled_prompt_backend_session_id(
                None,
                Some("ses_detected".to_string()),
                "opencode",
                false,
                false,
            )
            .as_deref(),
            Some("ses_detected")
        );
        assert_eq!(
            scheduled_prompt_backend_session_id(Some("ses_task"), None, "claude-code", true, false),
            None
        );
        assert_eq!(
            scheduled_prompt_backend_session_id(Some("ses_task"), None, "opencode", false, true),
            None
        );
    }

    #[test]
    fn revived_http_session_metadata_records_queued_backend_session_id() {
        let task = new_task(
            "task".into(),
            "0 0 * * *".into(),
            None,
            Some("prompt".into()),
            None,
            false,
            Some("ses_task".into()),
            OnFire::ContinueSession,
        );

        let metadata = revived_session_metadata(
            &task,
            Some("/tmp/project"),
            Some("ses_detected".to_string()),
            RevivedSessionSnapshot {
                model: Some("anthropic/claude-sonnet-4".into()),
                effort: Some("high".into()),
                codex_home: None,
                backend_name: "opencode",
                is_tui: false,
                clears_context: false,
                session_start_credential: None,
            },
        );

        assert_eq!(metadata.backend_session_id.as_deref(), Some("ses_task"));
        assert_eq!(metadata.backend.as_deref(), Some("opencode"));
        assert_eq!(metadata.model.as_deref(), Some("anthropic/claude-sonnet-4"));
        assert_eq!(metadata.effort.as_deref(), Some("high"));
        assert_eq!(
            metadata.opencode_binding,
            Some(crate::daemon_protocol::OpenCodeBinding::WeakAdopted)
        );
    }

    #[test]
    fn revived_codex_task_metadata_records_backend_without_model_override() {
        let mut task = new_task(
            "daily-report".into(),
            "0 10 * * *".into(),
            None,
            Some("prompt".into()),
            None,
            false,
            Some("old-codex-thread".into()),
            OnFire::NewSession,
        );
        task.backend = Some("codex-cli".into());

        let metadata = revived_session_metadata(
            &task,
            Some("/tmp/project"),
            None,
            RevivedSessionSnapshot {
                model: None,
                effort: None,
                codex_home: None,
                backend_name: "codex-cli",
                is_tui: true,
                clears_context: true,
                session_start_credential: Some("launch-secret".into()),
            },
        );

        assert_eq!(metadata.backend.as_deref(), Some("codex-cli"));
        assert_eq!(
            metadata.backend_session_id, None,
            "a fresh Codex revival must clear the old thread before launch"
        );
        assert_eq!(metadata.model, None);
        assert_eq!(metadata.codex_home, None);
        assert_eq!(
            metadata.session_start_credential.as_deref(),
            Some("launch-secret")
        );
    }

    #[test]
    fn revived_codex_resume_metadata_keeps_selected_thread_id() {
        let mut task = new_task(
            "daily-report".into(),
            "0 10 * * *".into(),
            None,
            Some("prompt".into()),
            None,
            false,
            Some("thread-resumed".into()),
            OnFire::ContinueSession,
        );
        task.backend = Some("codex-cli".into());

        let metadata = revived_session_metadata(
            &task,
            Some("/tmp/project"),
            Some("thread-detected".into()),
            RevivedSessionSnapshot {
                model: None,
                effort: None,
                codex_home: None,
                backend_name: "codex-cli",
                is_tui: true,
                clears_context: false,
                session_start_credential: None,
            },
        );

        assert_eq!(
            metadata.backend_session_id.as_deref(),
            Some("thread-resumed")
        );
        assert_eq!(metadata.session_start_credential, None);
    }

    #[tokio::test]
    async fn scheduled_clear_context_codex_stage_replaces_old_pair_and_accepts_session_start() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "scheduled".into(),
                pane: Some("%existing".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    backend_session_id: Some("thread-old".into()),
                    ..Default::default()
                },
            })
            .await;
        let old_incarnation = state.protocol.read().await.sessions["scheduled"]
            .metadata
            .session_incarnation;

        let staged_incarnation = stage_scheduled_codex_launch(
            &state,
            "scheduled",
            "codex-cli",
            Some("scheduled-proof".into()),
        )
        .await
        .unwrap()
        .expect("Codex clear-context launch stages an incarnation");

        assert_ne!(staged_incarnation, old_incarnation);
        {
            let proto = state.protocol.read().await;
            let metadata = &proto.sessions["scheduled"].metadata;
            assert!(metadata.backend_session_id.is_none());
            assert_eq!(
                metadata.session_start_credential.as_deref(),
                Some("scheduled-proof")
            );
        }

        state
            .apply_and_execute(crate::daemon_protocol::Event::AdoptBackend {
                id: "scheduled".into(),
                backend: "codex-cli".into(),
                backend_session_id: "thread-new".into(),
                expected_backend_session_id: None,
                expected_session_start_credential: Some("scheduled-proof".into()),
            })
            .await;

        let proto = state.protocol.read().await;
        let metadata = &proto.sessions["scheduled"].metadata;
        assert_eq!(metadata.session_incarnation, staged_incarnation);
        assert_eq!(metadata.backend_session_id.as_deref(), Some("thread-new"));
        assert!(metadata.session_start_credential.is_none());
    }

    #[tokio::test]
    async fn scheduled_liveness_result_rejects_same_pane_replacement() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "scheduled".into(),
                pane: Some("%same".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        let stale_owner = state.protocol.read().await.sessions["scheduled"].owner();

        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "scheduled".into(),
                pane: Some("%same".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;

        assert!(
            !scheduled_snapshot_is_current(&state, &stale_owner, Some("%same")).await,
            "a liveness observation for an old incarnation must not authorize injection or revival"
        );
    }

    #[tokio::test]
    async fn scheduled_revival_cannot_claim_replacement_from_stale_snapshot() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "scheduled".into(),
                pane: Some("%old".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_old".into()),
                    ..Default::default()
                },
            })
            .await;
        let stale_owner = state.protocol.read().await.sessions["scheduled"].owner();

        {
            let mut proto = state.protocol.write().await;
            proto.apply(crate::daemon_protocol::Event::Remove {
                id: "scheduled".into(),
                keep_worktree: true,
            });
            proto.apply(crate::daemon_protocol::Event::Register {
                id: "scheduled".into(),
                pane: Some("%winner".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_winner".into()),
                    ..Default::default()
                },
            });
        }
        let winner = state.protocol.read().await.sessions["scheduled"].clone();

        let claimed =
            claim_scheduled_existing_launch(&state, &stale_owner, "opencode", false, None).await;

        assert!(claimed.is_err());
        let proto = state.protocol.read().await;
        assert_eq!(proto.sessions["scheduled"], winner);
        assert!(
            !proto.lifecycle_leases.contains_key("scheduled"),
            "a stale scheduler snapshot must not retain lifecycle authority"
        );
    }

    #[tokio::test]
    async fn failed_same_pane_respawn_cleans_target_before_restoring_incumbent() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "scheduled".into(),
                pane: Some("%same".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    backend_session_id: Some("thread-old".into()),
                    ..Default::default()
                },
            })
            .await;
        let incumbent = state.protocol.read().await.sessions["scheduled"].clone();
        let claim = claim_scheduled_existing_launch(
            &state,
            &incumbent.owner(),
            "codex-cli",
            true,
            Some("proof".into()),
        )
        .await
        .unwrap();
        state
            .record_inert_start_pane(
                &claim.lease_owner,
                claim.target_owner.clone(),
                "%same".into(),
            )
            .await
            .unwrap();
        let cleaned = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cleaned_in_action = cleaned.clone();
        let expected_target = claim.target_owner.clone();

        rollback_scheduled_respawn_claim_with(&state, &claim, "%same", move |owner, pane| {
            assert_eq!(owner, expected_target);
            assert_eq!(pane, "%same");
            cleaned_in_action.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { Ok(()) }
        })
        .await
        .unwrap();

        assert_eq!(cleaned.load(std::sync::atomic::Ordering::SeqCst), 1);
        let proto = state.protocol.read().await;
        assert_eq!(proto.sessions["scheduled"], incumbent);
        assert!(!proto.lifecycle_leases.contains_key("scheduled"));
    }

    #[tokio::test]
    async fn rollback_provisional_revival_removes_unlaunched_new_pane() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "scheduled".into(),
                pane: Some("%staged".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    session_start_credential: Some("credential".into()),
                    ..Default::default()
                },
            })
            .await;

        rollback_provisional_revival(&state, "scheduled", "%staged", Some("credential"), None)
            .await;

        assert!(
            !state
                .protocol
                .read()
                .await
                .sessions
                .contains_key("scheduled")
        );
    }

    #[tokio::test]
    async fn rollback_provisional_revival_keeps_successful_session_start_adoption() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "scheduled".into(),
                pane: Some("%staged".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    session_start_credential: Some("credential".into()),
                    ..Default::default()
                },
            })
            .await;
        state
            .protocol
            .write()
            .await
            .apply(crate::daemon_protocol::Event::AdoptBackend {
                id: "scheduled".into(),
                backend: "codex-cli".into(),
                backend_session_id: "thread-winner".into(),
                expected_backend_session_id: None,
                expected_session_start_credential: Some("credential".into()),
            });

        rollback_provisional_revival(&state, "scheduled", "%staged", Some("credential"), None)
            .await;

        let session = state.protocol.read().await.sessions["scheduled"].clone();
        assert_eq!(session.pane.as_deref(), Some("%staged"));
        assert_eq!(
            session.metadata.backend_session_id.as_deref(),
            Some("thread-winner")
        );
    }

    #[tokio::test]
    async fn staged_fresh_launch_rollback_restores_existing_pair_after_credential_hook_failure() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "scheduled".into(),
                pane: Some("%existing".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    backend_session_id: Some("thread-old".into()),
                    ..Default::default()
                },
            })
            .await;
        let previous = state.protocol.read().await.sessions["scheduled"].clone();
        let staged_incarnation = stage_scheduled_codex_launch(
            &state,
            "scheduled",
            "codex-cli",
            Some("credential".into()),
        )
        .await;
        let staged_incarnation = staged_incarnation.unwrap().unwrap();

        rollback_staged_fresh_launch(
            &state,
            "scheduled",
            "%existing",
            "credential",
            staged_incarnation,
            &previous,
        )
        .await;

        let restored = state.protocol.read().await.sessions["scheduled"].clone();
        assert_eq!(restored.pane.as_deref(), Some("%existing"));
        assert_eq!(
            restored.metadata.backend_session_id.as_deref(),
            Some("thread-old")
        );
        assert_eq!(
            restored.metadata.session_incarnation,
            previous.metadata.session_incarnation
        );
    }

    #[tokio::test]
    async fn staged_fresh_launch_rollback_preserves_concurrently_consumed_session_start() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "scheduled".into(),
                pane: Some("%existing".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    backend_session_id: Some("thread-old".into()),
                    ..Default::default()
                },
            })
            .await;
        let previous = state.protocol.read().await.sessions["scheduled"].clone();
        state
            .apply_and_execute(crate::daemon_protocol::Event::StageFreshLaunch {
                id: "scheduled".into(),
                backend: "codex-cli".into(),
                session_start_credential: Some("credential".into()),
                expected_repair_reservation: None,
            })
            .await;
        let staged_incarnation = state.protocol.read().await.sessions["scheduled"]
            .metadata
            .session_incarnation;

        state
            .apply_and_execute(crate::daemon_protocol::Event::AdoptBackend {
                id: "scheduled".into(),
                backend: "codex-cli".into(),
                backend_session_id: "thread-winner".into(),
                expected_backend_session_id: None,
                expected_session_start_credential: Some("credential".into()),
            })
            .await;

        rollback_staged_fresh_launch(
            &state,
            "scheduled",
            "%existing",
            "credential",
            staged_incarnation,
            &previous,
        )
        .await;

        let retained = state.protocol.read().await.sessions["scheduled"].clone();
        assert_eq!(retained.pane.as_deref(), Some("%existing"));
        assert_eq!(
            retained.metadata.backend_session_id.as_deref(),
            Some("thread-winner")
        );
        assert_eq!(retained.metadata.session_start_credential, None);
        assert_eq!(retained.metadata.session_incarnation, staged_incarnation);
    }

    #[tokio::test]
    async fn staged_fresh_launch_rollback_restores_existing_pair_after_tmux_respawn_failure() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "scheduled".into(),
                pane: Some("%existing".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("codex-cli".into()),
                    backend_session_id: Some("thread-old".into()),
                    ..Default::default()
                },
            })
            .await;
        let previous = state.protocol.read().await.sessions["scheduled"].clone();
        let staged_incarnation = stage_scheduled_codex_launch(
            &state,
            "scheduled",
            "codex-cli",
            Some("credential".into()),
        )
        .await
        .unwrap()
        .unwrap();

        rollback_staged_fresh_launch(
            &state,
            "scheduled",
            "%existing",
            "credential",
            staged_incarnation,
            &previous,
        )
        .await;

        let restored = state.protocol.read().await.sessions["scheduled"].clone();
        assert_eq!(restored, previous);
    }
}
