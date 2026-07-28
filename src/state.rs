use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use ractor::{Actor, ActorRef};

use crate::config::OuijaConfig;
use crate::persistence::OuijaSettings;
use crate::project_index::ProjectInfo;
use crate::scheduler::{ScheduledTask, TaskRun};
use crate::transport::Transport;

#[derive(Clone)]
struct OwnedSessionAgent {
    owner: crate::daemon_protocol::ResourceOwner,
    pane: String,
    actor: ActorRef<crate::session_agent::SessionMsg>,
}

/// Sanitize a name into a valid session ID (lowercase alphanumeric + dashes).
pub fn sanitize_session_id(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Resolve a unique session ID for a new registration.
///
/// Walks `base_id`, `base_id-2`, `base_id-3`, ... until either an unused id is
/// found or the existing entry's pane matches `target_pane` (idempotent
/// re-registration of the same pane). Caps at `MAX_NAME_SUFFIX` attempts; on
/// overflow returns the last id tried with a `tracing::warn!` so the caller's
/// `Event::Register` either replaces the holder via apply_register's pane-dedup
/// or fails loudly rather than spinning forever.
///
/// `id_to_pane` is a snapshot of `proto.sessions` keyed by id with the value
/// being the pane currently bound to that id. Callers that already hold a
/// `proto.sessions` read lock can build this in one pass; callers without a
/// lock can pass a lazily-constructed map. Either way, the helper itself is
/// pure — no I/O, no awaits, no locks — so it composes cleanly with both
/// the lock-held (`hooks::session_start_inner`) and lock-free
/// (`AppState::scan_and_autoregister_panes`) call sites.
///
/// `target_pane = None` means the caller has no pane to dedupe against (e.g.
/// API-driven registration without a `pane` field). In that case every
/// existing entry counts as a conflict; we never collapse to the base id just
/// because some other holder also happens to have a None pane.
pub fn resolve_unique_session_id(
    id_to_pane: &HashMap<String, Option<String>>,
    base_id: &str,
    target_pane: Option<&str>,
) -> String {
    let mut id = base_id.to_string();
    let mut suffix = 2u32;
    while let Some(existing_pane) = id_to_pane.get(&id) {
        // Same-pane idempotency: if the existing entry is bound to the
        // same pane the caller is registering, return the current id so
        // apply_register's idempotent path runs instead of inventing a new id.
        if target_pane.is_some() && existing_pane.as_deref() == target_pane {
            return id;
        }
        id = format!("{base_id}-{suffix}");
        if suffix > MAX_NAME_SUFFIX {
            tracing::warn!(
                "resolve_unique_session_id: exhausted suffixes 2..={MAX_NAME_SUFFIX} for base '{base_id}', returning '{id}'"
            );
            return id;
        }
        suffix += 1;
    }
    id
}

/// Expand `~/` to `$HOME/` in a path string.
pub fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        format!("{home}/{rest}")
    } else {
        path.to_string()
    }
}

/// Stable physical identity for project-directory gates and ownership checks.
///
/// Existing paths (including symlinks) resolve through `canonicalize`. For a
/// not-yet-created target, the nearest existing ancestor of the original path
/// is canonicalized and the missing suffix is then normalized. Resolving the
/// existing prefix first preserves filesystem semantics for paths such as
/// `symlink/../missing`, where `..` applies after traversing the symlink.
pub(crate) fn project_dir_identity(path: &str) -> String {
    let expanded = PathBuf::from(expand_tilde(path));
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(expanded)
    };
    if let Ok(canonical) = std::fs::canonicalize(&absolute) {
        return canonical.to_string_lossy().into_owned();
    }

    let normalize = |path: &Path| {
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    normalized.pop();
                }
                other => normalized.push(other.as_os_str()),
            }
        }
        normalized
    };

    let mut ancestor = absolute.clone();
    let mut missing = Vec::new();
    loop {
        if let Ok(mut canonical) = std::fs::canonicalize(&ancestor) {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return normalize(&canonical).to_string_lossy().into_owned();
        }
        let Some(component) = ancestor.components().next_back() else {
            return normalize(&absolute).to_string_lossy().into_owned();
        };
        let component = match component {
            std::path::Component::Normal(name) => name.to_os_string(),
            std::path::Component::CurDir => ".".into(),
            std::path::Component::ParentDir => "..".into(),
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return normalize(&absolute).to_string_lossy().into_owned();
            }
        };
        missing.push(component);
        if !ancestor.pop() {
            return normalize(&absolute).to_string_lossy().into_owned();
        }
    }
}

/// Resolve a pane's cwd to the actual project root.
/// If the path is inside a `.claude/worktrees/<branch>` or `.ouija/worktrees/<branch>` directory,
/// walk up to the repo root so autoregistration derives the project name, not the branch.
///
/// Phase 1: hardcoded to the Claude Code and Ouija worktree layouts. This function is called
/// during auto-registration before a per-session backend is known.
/// Phase 2: delegate to `backend.resolve_project_root(path)` once per-session backends are supported.
pub fn resolve_project_root(path: &str) -> &str {
    // Look for `/.claude/worktrees/` or `/.ouija/worktrees/` in the path
    if let Some(idx) = path.find("/.claude/worktrees/") {
        &path[..idx]
    } else if let Some(idx) = path.find("/.ouija/worktrees/") {
        &path[..idx]
    } else {
        path
    }
}

/// Returns true when a resolved project root is the user's home directory and
/// must not be auto-registered.
///
/// An agent whose cwd is still `$HOME` at SessionStart is a premature hook
/// mis-fire — e.g. an opencode SessionStart firing before the agent has cd'd
/// into its worktree. Registering it derives a generic `basename($HOME)-N`
/// session ("daniel-N") that then owns the live pane and survives task
/// cleanup (cleanup targets the real task-slug name), leaking forever (#1483).
pub fn is_home_project_root(project_root: &str) -> bool {
    match std::env::var("HOME") {
        Ok(home) => root_matches_home(project_root, &home),
        Err(_) => false,
    }
}

/// Pure comparison helper: trailing-slash-insensitive equality of a project
/// root against a (non-empty) home directory.
fn root_matches_home(project_root: &str, home: &str) -> bool {
    let home = home.trim_end_matches('/');
    !home.is_empty() && project_root.trim_end_matches('/') == home
}

/// Named transport map keyed by transport name (e.g. "nostr").
type TransportMap = HashMap<String, Arc<dyn Transport>>;

/// A node with this npub is already connected.
#[derive(Debug)]
pub struct DuplicateNode(pub String);

impl std::fmt::Display for DuplicateNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for DuplicateNode {}

/// Thread-safe shared reference to the daemon's application state.
pub type SharedState = Arc<AppState>;

#[derive(Clone, Debug)]
pub(crate) struct EffectDeliveryFailure {
    reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryOutcome {
    Accepted,
    Rejected(String),
    Ambiguous(String),
}

fn prompt_async_failure_reason(
    decision: crate::nostr_transport::PromptAsyncFallbackDecision,
) -> String {
    format!("prompt_async request failed: {decision:?}")
}

fn http_delivery_attempt_failure(
    decision: crate::nostr_transport::PromptAsyncFallbackDecision,
) -> DeliveryOutcome {
    let reason = prompt_async_failure_reason(decision);
    match decision {
        crate::nostr_transport::PromptAsyncFallbackDecision::DefiniteNonAcceptance => {
            DeliveryOutcome::Rejected(reason)
        }
        crate::nostr_transport::PromptAsyncFallbackDecision::Ambiguous => {
            DeliveryOutcome::Ambiguous(reason)
        }
    }
}

async fn session_owns_pane(state: &AppState, session_id: &str, pane: &str) -> Result<(), String> {
    let registered_pane = {
        let proto = state.protocol.read().await;
        proto
            .sessions
            .get(session_id)
            .and_then(|session| session.pane.clone())
    };

    if registered_pane.as_deref() == Some(pane) {
        Ok(())
    } else {
        Err(format!("pane {pane} is not owned by session {session_id}"))
    }
}

async fn deliver_raw_tmux_for_session(
    state: &AppState,
    request: &InjectDeliveryRequest<'_>,
    inject_config: Option<crate::backend::InjectConfig>,
    tui_pattern: Option<String>,
    assistant_guard: Option<(
        crate::daemon_protocol::ResourceOwner,
        OwnedAssistantDeliveryEvidence,
    )>,
) -> DeliveryOutcome {
    if let Err(reason) = session_owns_pane(state, request.session_id, request.pane).await {
        return DeliveryOutcome::Rejected(reason);
    }
    if let Some((owner, evidence)) = assistant_guard.as_ref()
        && let Err(reason) = evidence.validate(state, owner, request.pane).await
    {
        return DeliveryOutcome::Rejected(reason);
    }
    let assistant_process_evidence = assistant_guard.as_ref().map(|(owner, evidence)| {
        crate::tmux::OwnedAssistantProcessEvidence {
            owner: owner.clone(),
            process_names: evidence.process_names.clone(),
        }
    });

    let result = match inject_config {
        Some(inject_config) => {
            crate::tmux::locked_inject_raw_tmux_with_config_and_evidence(
                state,
                request.pane,
                request.message,
                request.vim_mode,
                inject_config,
                tui_pattern,
                assistant_process_evidence,
            )
            .await
        }
        None => {
            crate::tmux::locked_inject_raw_tmux(
                state,
                request.session_id,
                request.pane,
                request.message,
                request.vim_mode,
            )
            .await
        }
    };

    result
        .map(|()| DeliveryOutcome::Accepted)
        .unwrap_or_else(|error| DeliveryOutcome::Rejected(error.to_string()))
}

async fn deliver_by_current_session_plan(
    state: &AppState,
    request: &InjectDeliveryRequest<'_>,
    assistant_guard: Option<(
        crate::daemon_protocol::ResourceOwner,
        OwnedAssistantDeliveryEvidence,
    )>,
) -> DeliveryOutcome {
    match crate::tmux::session_delivery_plan(state, request.session_id, request.pane).await {
        crate::tmux::SessionDeliveryPlan::Http(delivery) => {
            if let Err(reason) = session_owns_pane(state, request.session_id, request.pane).await {
                return DeliveryOutcome::Rejected(reason);
            }
            if let Some((owner, evidence)) = assistant_guard.as_ref()
                && let Err(reason) = evidence.validate(state, owner, request.pane).await
            {
                return DeliveryOutcome::Rejected(reason);
            }

            crate::tmux::deliver_via_http(
                state,
                &delivery.backend_session_id,
                delivery.project_dir.as_deref(),
                request.message,
                delivery.model.as_deref(),
                delivery.effort.as_deref(),
            )
            .await
            .map(|()| DeliveryOutcome::Accepted)
            .unwrap_or_else(http_delivery_attempt_failure)
        }
        crate::tmux::SessionDeliveryPlan::RawTmux {
            inject_config,
            tui_pattern,
        } => {
            deliver_raw_tmux_for_session(
                state,
                request,
                Some(inject_config),
                tui_pattern,
                assistant_guard,
            )
            .await
        }
        crate::tmux::SessionDeliveryPlan::Unavailable(reason) => DeliveryOutcome::Rejected(reason),
    }
}

pub(crate) struct InjectDeliveryRequest<'a> {
    pub session_id: &'a str,
    pub pane: &'a str,
    pub message: &'a str,
    pub vim_mode: bool,
    pub delivery_method: Option<&'a str>,
    pub recorded_method: Option<&'a str>,
}

#[derive(Clone, Debug)]
pub(crate) struct OwnedAssistantDeliveryEvidence {
    pub backend: Option<String>,
    pub backend_session_id: Option<String>,
    pub process_names: Vec<String>,
}

impl OwnedAssistantDeliveryEvidence {
    async fn validate(
        &self,
        state: &AppState,
        owner: &crate::daemon_protocol::ResourceOwner,
        pane: &str,
    ) -> Result<(), String> {
        let metadata_matches = state
            .protocol
            .read()
            .await
            .sessions
            .get(&owner.session_id)
            .is_some_and(|session| {
                session.owner() == *owner
                    && session.pane.as_deref() == Some(pane)
                    && session.metadata.backend == self.backend
                    && session.metadata.backend_session_id == self.backend_session_id
            });
        if !metadata_matches {
            return Err(format!(
                "scheduled delivery evidence changed for session {}",
                owner.session_id
            ));
        }

        if cfg!(test) {
            let live = state
                .cached_assistant_panes
                .read()
                .await
                .iter()
                .any(|candidate| {
                    candidate.pane_id == pane
                        && candidate.process_name.as_deref().is_some_and(|process| {
                            self.process_names.iter().any(|name| name == process)
                        })
                });
            return live.then_some(()).ok_or_else(|| {
                format!(
                    "scheduled delivery target {} is no longer running its assistant process",
                    owner.session_id
                )
            });
        }

        let pane = pane.to_string();
        let owner = owner.clone();
        let process_names = self.process_names.clone();
        tokio::task::spawn_blocking(move || {
            let observed = crate::tmux::inspect_pane_owner(&pane)
                .map_err(|error| format!("failed to inspect scheduled pane owner: {error}"))?
                .ok_or_else(|| "scheduled pane has no physical owner".to_string())?;
            if !crate::tmux::physical_owner_matches(&observed, &owner) {
                return Err("scheduled pane physical owner changed".to_string());
            }
            let names = process_names.iter().map(String::as_str).collect::<Vec<_>>();
            if !crate::tmux::pane_alive(&pane, &names) {
                return Err("scheduled pane is no longer running its assistant process".to_string());
            }
            Ok(())
        })
        .await
        .map_err(|error| format!("scheduled pane validation task failed: {error}"))?
    }
}

pub(crate) async fn deliver_inject_message_effect(
    state: &Arc<AppState>,
    request: InjectDeliveryRequest<'_>,
) -> DeliveryOutcome {
    deliver_inject_message_effect_with_evidence(state, request, None).await
}

async fn deliver_inject_message_effect_with_evidence(
    state: &Arc<AppState>,
    request: InjectDeliveryRequest<'_>,
    assistant_guard: Option<(
        crate::daemon_protocol::ResourceOwner,
        OwnedAssistantDeliveryEvidence,
    )>,
) -> DeliveryOutcome {
    let method = request.delivery_method.or(request.recorded_method);
    match method {
        Some("http") => deliver_by_current_session_plan(state, &request, assistant_guard).await,
        Some("tmux") => {
            deliver_raw_tmux_for_session(state, &request, None, None, assistant_guard).await
        }
        _ => deliver_by_current_session_plan(state, &request, assistant_guard).await,
    }
}

pub(crate) async fn deliver_owned_inject_message_effect(
    state: &Arc<AppState>,
    owner: &crate::daemon_protocol::ResourceOwner,
    assistant_evidence: Option<OwnedAssistantDeliveryEvidence>,
    request: InjectDeliveryRequest<'_>,
) -> DeliveryOutcome {
    if owner.session_id != request.session_id {
        return DeliveryOutcome::Rejected(format!(
            "scheduled delivery owner changed for session {}",
            request.session_id
        ));
    }
    let pane = request.pane.to_string();
    state
        .with_owned_pane_claim(owner, &pane, || async {
            let assistant_guard = assistant_evidence.map(|evidence| (owner.clone(), evidence));
            deliver_inject_message_effect_with_evidence(state, request, assistant_guard).await
        })
        .await
        .unwrap_or_else(|| {
            DeliveryOutcome::Rejected(format!(
                "scheduled delivery owner changed for session {}",
                owner.session_id
            ))
        })
}

fn human_active_context_limit(limit_secs: u64) -> String {
    let hours = limit_secs / 3600;
    let minutes = (limit_secs % 3600) / 60;
    let seconds = limit_secs % 60;
    let mut parts = Vec::new();
    if hours > 0 {
        parts.push(format!(
            "{hours} {}",
            if hours == 1 { "hour" } else { "hours" }
        ));
    }
    if minutes > 0 {
        parts.push(format!(
            "{minutes} {}",
            if minutes == 1 { "minute" } else { "minutes" }
        ));
    }
    if seconds > 0 || parts.is_empty() {
        parts.push(format!(
            "{seconds} {}",
            if seconds == 1 { "second" } else { "seconds" }
        ));
    }
    parts.join(" ")
}

fn active_context_restart_due_message(
    session_id: &str,
    limit_secs: u64,
    has_stored_prompt: bool,
) -> String {
    let prompt_guidance = if has_stored_prompt {
        "This session has a stored prompt; it will be replayed before the one-shot continuation."
    } else {
        "This session has no stored prompt; make the one-shot continuation complete enough to finish the work on its own."
    };
    format!(
        r#"<ouija-status type="active-context-restart-due">
Active context refresh is due for session "{session_id}" after {limit} of active work.

At this stopped safe boundary, prepare a concise, self-contained continuation. Include the goal, completed work, remaining work, decisions, blockers, and exact next steps. Verify live state (files, tests, and current session/task status) before writing it.

{prompt_guidance}

Run this quoted heredoc to start the fresh session:
ouija restart-session "{session_id}" --fresh --one-shot-file /dev/stdin <<'OUIJA_CONTINUATION'
Write the verified continuation here.
OUIJA_CONTINUATION
</ouija-status>"#,
        limit = human_active_context_limit(limit_secs),
    )
}

async fn notify_active_context_restart_due(
    state: &Arc<AppState>,
    owner: &crate::daemon_protocol::ResourceOwner,
) {
    let notification = {
        let protocol = state.protocol.read().await;
        protocol
            .sessions
            .get(&owner.session_id)
            .and_then(|session| {
                if session.owner() != *owner || !session.metadata.active_context_restart_due {
                    return None;
                }
                let limit_secs = session.metadata.fresh_context_after_active_secs?;
                if limit_secs == 0 {
                    return None;
                }
                Some((
                    session.pane.clone()?,
                    session.metadata.vim_mode,
                    limit_secs,
                    session.metadata.prompt.is_some(),
                ))
            })
    };
    let Some((pane, vim_mode, limit_secs, has_stored_prompt)) = notification else {
        return;
    };

    let message =
        active_context_restart_due_message(&owner.session_id, limit_secs, has_stored_prompt);
    if let Err(error) =
        crate::tmux::locked_inject_owned(state, owner, &pane, &message, vim_mode).await
    {
        tracing::warn!(
            session = %owner.session_id,
            incarnation = %owner.incarnation,
            "active-context restart notification delivery skipped: {error}"
        );
    }
}

fn spawn_owned_active_context_restart_due_delivery(
    state: &Arc<AppState>,
    owner: &crate::daemon_protocol::ResourceOwner,
) {
    let state = Arc::clone(state);
    let owner = owner.clone();
    tokio::spawn(async move {
        // The delivery remains owned by this exact incarnation: the initial
        // snapshot and `locked_inject_owned` both reject a superseded owner.
        notify_active_context_restart_due(&state, &owner).await;
    });
}

/// Central daemon state holding sessions, nodes, and transports.
pub struct AppState {
    pub config: OuijaConfig,
    /// Pure protocol state machine — source of truth for all sessions.
    pub protocol: RwLock<crate::daemon_protocol::DaemonState>,
    pub nodes: RwLock<HashMap<String, NodeInfo>>,
    pub message_log: RwLock<VecDeque<LogEntry>>,
    pub log_file: PathBuf,
    transports: RwLock<TransportMap>,
    pub settings: RwLock<OuijaSettings>,
    pub scheduled_tasks: RwLock<HashMap<String, ScheduledTask>>,
    pub task_runs: RwLock<VecDeque<TaskRun>>,
    /// Per-pane FIFO injection queues (each backed by a background worker).
    pane_queues: std::sync::Mutex<
        HashMap<String, tokio::sync::mpsc::UnboundedSender<crate::tmux::InjectRequest>>,
    >,
    /// Serializes log file writes to prevent interleaved lines.
    log_file_lock: std::sync::Mutex<()>,
    /// Serializes task_runs.jsonl writes.
    task_run_log_lock: std::sync::Mutex<()>,
    /// Connected remote daemon npubs, prevents duplicate connections.
    /// Maps npub -> node name.
    connected_npubs: std::sync::Mutex<HashMap<String, String>>,
    /// Debounce: last time we reciprocated a session list to each node.
    last_reciprocated: std::sync::Mutex<HashMap<String, std::time::Instant>>,
    /// Active session agents keyed by exact lifecycle owner.
    session_agents: RwLock<HashMap<crate::daemon_protocol::ResourceOwner, OwnedSessionAgent>>,
    /// Per-resource async gates serialize external pane/backend claims and cleanup
    /// without holding the protocol lock across tmux, process, or HTTP I/O.
    resource_gates:
        std::sync::Mutex<HashMap<ResourceGateKey, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    /// Indexed projects from projects_dir, keyed by directory basename.
    pub project_index: RwLock<HashMap<String, ProjectInfo>>,
    /// Pending remote command results: command string → oneshot senders.
    pending_commands: std::sync::Mutex<Vec<(String, tokio::sync::oneshot::Sender<String>)>>,
    /// Cached tmux panes running the coding assistant, refreshed by the reaper loop.
    pub(crate) cached_assistant_panes: RwLock<Vec<crate::tmux::TmuxPane>>,
    /// Short-lived suppression after explicit removal. This replaces an
    /// indefinite `@ouija_id` marker as the protection against the scanner
    /// re-registering a pane before kill-session finishes.
    autoregister_suppressed_panes: std::sync::Mutex<HashMap<String, std::time::Instant>>,
    /// Per-fire worktree panes bound to the exact session incarnation that
    /// created them.
    /// Reaper runs `git worktree prune` when these panes die.
    pub perfire_worktree_panes: RwLock<HashMap<String, PerFireWorktreeClaim>>,
    /// Dedup: prevents concurrent sweeps from accumulating hung blocking threads.
    sweep_in_progress: std::sync::atomic::AtomicBool,
    /// Backoff gate after a sweep timeout. When `Some(t)`, sweeps are skipped
    /// until `Instant::now() >= t`. The orphan blocking thread from a timed-out
    /// sweep keeps `sweep_in_progress = true`; this gate prevents subsequent
    /// sweeps from clearing the dedup claim and spawning another orphan on every
    /// heartbeat. After the window expires, the next entry clears both the
    /// backoff and the dedup flag, accepting one more orphan to retain liveness.
    sweep_backoff_until: std::sync::Mutex<Option<std::time::Instant>>,
    pub backends: crate::backend::BackendRegistry,
    pub http_client: reqwest::Client,
    /// Queued prompts for HttpApi sessions awaiting a readiness signal.
    /// TuiInjection sessions pass prompts as CLI args instead.
    /// Maps session_id -> queued readiness prompt.
    pub pending_prompts: std::sync::Mutex<std::collections::HashMap<String, PendingPrompt>>,
    compact_in_progress: std::sync::Mutex<std::collections::HashSet<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ResourceGateKey {
    Pane(String),
    BackendSession(String),
    ProjectDir(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PerFireWorktreeClaim {
    pub owner: crate::daemon_protocol::ResourceOwner,
    pub project_dir: String,
}

pub(crate) struct CompactInProgressGuard<'a> {
    state: &'a AppState,
    key: String,
}

impl Drop for CompactInProgressGuard<'_> {
    fn drop(&mut self) {
        self.state
            .compact_in_progress
            .lock()
            .expect("compact_in_progress mutex poisoned")
            .remove(&self.key);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPrompt {
    pub pane_id: String,
    pub prompt: String,
    pub backend_session_id: Option<String>,
    pub owner: Option<crate::daemon_protocol::ResourceOwner>,
}

impl PendingPrompt {
    pub fn new(pane_id: String, prompt: String, backend_session_id: Option<String>) -> Self {
        Self {
            pane_id,
            prompt,
            backend_session_id,
            owner: None,
        }
    }

    pub fn with_owner(mut self, owner: crate::daemon_protocol::ResourceOwner) -> Self {
        self.owner = Some(owner);
        self
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// Mutable metadata describing a session's configuration and context.
///
/// # Design: Trigger + SessionConfig + Runtime
///
/// SessionMetadata = SessionConfig (prompt, reminder, project_dir, on_fire) + Runtime
/// (iteration, iteration_log, last_iteration_at) + Display (role, bulletin, vim_mode).
/// ScheduledTask (scheduler.rs) = SessionConfig + Trigger (cron, enabled, next_run).
/// The shared SessionConfig fields are stamped here when a task creates or revives
/// a session.
///
/// The SessionConfig fields aren't a named type yet — they're copied field-by-field
/// during the trigger→session handoff. Extracting a named SessionConfig would make
/// this explicit, especially if a third trigger type (file watch) is added.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMetadata {
    #[serde(default)]
    pub vim_mode: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Whether this session is visible to and reachable from remote nodes.
    #[serde(default = "default_true")]
    pub networked: bool,
    /// When the session's role/project_dir was last explicitly set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_metadata_update: Option<DateTime<Utc>>,
    /// Coding assistant conversation/session ID (UUID) for `--resume` on restart.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "claude_session_id"
    )]
    pub backend_session_id: Option<String>,
    /// Which coding assistant backend this session uses (e.g. "claude-code").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Strength of an OpenCode backend-session binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode_binding: Option<crate::daemon_protocol::OpenCodeBinding>,
    /// Monotonic token used to reject stale async restart commits.
    #[serde(default)]
    pub restart_generation: u64,
    /// In-memory legacy-repair reservation mirrored from protocol metadata.
    #[serde(default, skip_serializing, skip_deserializing)]
    pub backend_repair_reservation: Option<crate::daemon_protocol::BackendRepairReservation>,
    /// Per-registration token used to reject stale async commits.
    #[serde(default)]
    pub session_incarnation: crate::daemon_protocol::SessionIncarnation,
    /// Short project description extracted from Cargo.toml, package.json, or README.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_description: Option<String>,
    /// Free-form bulletin: what this session needs, offers, or is working on.
    /// Used by the pairing evaluator to discover collaboration opportunities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bulletin: Option<String>,
    /// Whether this session runs in an isolated git worktree (backend worktree mode).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub worktree: bool,
    /// Which LLM model this session is configured to use.
    ///
    /// For claude-code: passed as `--model <X>` on the CLI.
    /// For opencode: split on first `/` and sent as `{providerID,modelID}` on
    /// each `prompt_async` body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort / variant for the model.
    ///
    /// For claude-code: passed as `--effort <X>` on the CLI.
    /// For codex-cli: passed as `-c model_reasoning_effort="<X>"`.
    /// For opencode: sent as `variant` on each `prompt_async` body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Optional Codex home override for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<String>,
    /// Reminder text re-injected on idle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminder: Option<String>,
    /// Explicit parent session that owns lifecycle decisions for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    /// Explicit behavior to follow when this session is idle or done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_policy: Option<crate::daemon_protocol::IdlePolicy>,
    /// Original prompt from session_start, stored for re-injection on iteration.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "original_prompt"
    )]
    pub prompt: Option<String>,
    /// How many times loop_next has been called.
    #[serde(default, alias = "loop_iteration")]
    pub iteration: u64,
    /// Log messages from each iteration. Capped at 100.
    #[serde(default, skip_serializing_if = "Vec::is_empty", alias = "loop_log")]
    pub iteration_log: Vec<crate::daemon_protocol::IterationLogEntry>,
    /// Unix timestamp of the most recent iteration. Used by stall detection.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "last_loop_next"
    )]
    pub last_iteration_at: Option<i64>,
    /// What happens each time a scheduled task fires for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_fire: Option<crate::scheduler::OnFire>,
    /// Last known on-disk presence of `project_dir` as of the most recent
    /// worktree sweep. `None` = never checked, `Some(true)` = on disk,
    /// `Some(false)` = missing (stale registration, issue #661).
    ///
    /// Mirror of `SessionMeta::worktree_present` — see that field's doc
    /// comment for the semantic boundaries (only meaningful for Local
    /// sessions with `project_dir` set; distinct from metadata staleness).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_present: Option<bool>,
    /// Positive active-work duration after which a fresh context restart is due.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fresh_context_after_active_secs: Option<u64>,
    /// Completed active-work time accumulated for the fresh-context policy.
    #[serde(default)]
    pub active_context_accumulated_secs: u64,
    /// Open active-work segment start time, if the session is currently active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_context_segment_started_at: Option<i64>,
    /// Whether the fresh-context active-time threshold has been reached.
    #[serde(default)]
    pub active_context_restart_due: bool,
}

fn default_true() -> bool {
    true
}

impl Default for SessionMetadata {
    fn default() -> Self {
        Self {
            vim_mode: false,
            project_dir: None,
            role: None,
            networked: true,
            last_metadata_update: None,
            backend_session_id: None,
            backend: None,
            opencode_binding: None,
            restart_generation: 0,
            backend_repair_reservation: None,
            session_incarnation: crate::daemon_protocol::SessionIncarnation::default(),
            project_description: None,
            bulletin: None,
            worktree: false,
            model: None,
            effort: None,
            codex_home: None,
            reminder: None,
            parent_session: None,
            idle_policy: None,
            prompt: None,
            iteration: 0,
            iteration_log: Vec::new(),
            last_iteration_at: None,
            on_fire: None,
            worktree_present: None,
            fresh_context_after_active_secs: None,
            active_context_accumulated_secs: 0,
            active_context_segment_started_at: None,
            active_context_restart_due: false,
        }
    }
}

/// A registered coding assistant session bound to a tmux pane.
#[derive(Clone, Debug, Serialize)]
pub struct Session {
    pub id: String,
    pub pane: Option<String>,
    pub origin: SessionOrigin,
    pub registered_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
    pub metadata: SessionMetadata,
}

/// Where a session originated: local tmux, remote node, or human.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SessionOrigin {
    Local,
    Remote(String),
    /// A human Nostr user. The String is their npub.
    Human(String),
}

/// Metadata for a connected remote daemon node.
#[derive(Clone, Debug, Serialize)]
pub struct NodeInfo {
    pub name: String,
    pub daemon_id: String,
    pub connected_at: DateTime<Utc>,
}

/// A recorded inter-session message for the admin log.
#[derive(Clone, Debug, Serialize)]
pub struct LogEntry {
    pub timestamp: DateTime<Utc>,
    pub from: String,
    pub to: String,
    pub message: String,
    pub delivered: bool,
}

/// Max message log entries retained in memory.
const MAX_LOG: usize = 100;
/// Max task run records retained in memory.
const MAX_TASK_RUNS: usize = 200;
/// Max suffix number when resolving auto-registration name conflicts.
const MAX_NAME_SUFFIX: u32 = 100;
/// Reciprocation debounce interval to prevent session list ping-pong.
const RECIPROCATE_DEBOUNCE_SECS: u64 = 30;
const AUTOREGISTER_REMOVE_GRACE_SECS: u64 = 10;

fn autoregister_accepts_pane_inspection(
    inspection: &crate::tmux::ManagedPaneInspection,
    marker_owner_is_referenced: bool,
) -> bool {
    match inspection {
        crate::tmux::ManagedPaneInspection::Unmanaged => true,
        crate::tmux::ManagedPaneInspection::MarkerOwner(_) => !marker_owner_is_referenced,
        crate::tmux::ManagedPaneInspection::Missing
        | crate::tmux::ManagedPaneInspection::ProcessOwner(_) => false,
    }
}

fn wait_for_tmux_owner_convergence<Inspect, Wait>(
    expected_owner: &crate::daemon_protocol::ResourceOwner,
    attempts: usize,
    mut inspect: Inspect,
    mut wait: Wait,
) -> anyhow::Result<()>
where
    Inspect: FnMut() -> anyhow::Result<crate::tmux::ManagedPaneInspection>,
    Wait: FnMut(),
{
    anyhow::ensure!(
        attempts > 0,
        "tmux owner wait requires at least one attempt"
    );
    let mut last_error = String::new();

    for attempt in 0..attempts {
        match inspect() {
            Ok(inspection)
                if crate::tmux::pane_accepts_owner_marker(&inspection, expected_owner) =>
            {
                return Ok(());
            }
            Ok(crate::tmux::ManagedPaneInspection::Missing) => {
                last_error = "pane is not visible yet".into();
            }
            Ok(
                crate::tmux::ManagedPaneInspection::ProcessOwner(observed)
                | crate::tmux::ManagedPaneInspection::MarkerOwner(observed),
            ) => {
                last_error = format!(
                    "pane still exposes incarnation {}, expected {}",
                    observed.incarnation, expected_owner.incarnation
                );
            }
            Ok(crate::tmux::ManagedPaneInspection::Unmanaged) => {
                unreachable!("unmanaged panes accept their first owner marker")
            }
            Err(error) => last_error = error.to_string(),
        }

        if attempt + 1 < attempts {
            wait();
        }
    }

    anyhow::bail!("{last_error}")
}

impl AppState {
    #[cfg(test)]
    pub fn new_for_test() -> Arc<Self> {
        let data_dir = tempfile::tempdir()
            .expect("create test data directory")
            .keep();
        Arc::new(Self {
            config: crate::config::OuijaConfig {
                name: "test".into(),
                npub: "npub1test".into(),
                port: 0,
                data_dir: data_dir.clone(),
                config_dir: data_dir.clone(),
            },
            protocol: RwLock::new(crate::daemon_protocol::DaemonState::new(
                "npub1test".into(),
                "test".into(),
            )),
            nodes: RwLock::new(HashMap::new()),
            message_log: RwLock::new(VecDeque::with_capacity(MAX_LOG)),
            log_file: data_dir.join("messages.jsonl"),
            transports: RwLock::new(HashMap::new()),
            settings: RwLock::new(Default::default()),
            scheduled_tasks: RwLock::new(HashMap::new()),
            task_runs: RwLock::new(VecDeque::with_capacity(MAX_TASK_RUNS)),
            pane_queues: std::sync::Mutex::new(HashMap::new()),
            log_file_lock: std::sync::Mutex::new(()),
            task_run_log_lock: std::sync::Mutex::new(()),
            connected_npubs: std::sync::Mutex::new(HashMap::new()),
            last_reciprocated: std::sync::Mutex::new(HashMap::new()),
            session_agents: RwLock::new(HashMap::new()),
            resource_gates: std::sync::Mutex::new(HashMap::new()),
            project_index: RwLock::new(HashMap::new()),
            pending_commands: std::sync::Mutex::new(Vec::new()),
            cached_assistant_panes: RwLock::new(Vec::new()),
            autoregister_suppressed_panes: std::sync::Mutex::new(HashMap::new()),
            perfire_worktree_panes: RwLock::new(HashMap::new()),
            sweep_in_progress: std::sync::atomic::AtomicBool::new(false),
            sweep_backoff_until: std::sync::Mutex::new(None),
            backends: crate::backend::BackendRegistry::default_registry(),
            http_client: reqwest::Client::new(),
            pending_prompts: std::sync::Mutex::new(std::collections::HashMap::new()),
            compact_in_progress: std::sync::Mutex::new(std::collections::HashSet::new()),
        })
    }

    pub fn new(config: OuijaConfig) -> SharedState {
        let log_file = config.data_dir.join("messages.jsonl");
        let settings = crate::persistence::load_settings(&config.config_dir).unwrap_or_default();
        let scheduled_tasks = crate::persistence::load_tasks(&config.data_dir).unwrap_or_default();
        let protocol =
            crate::daemon_protocol::DaemonState::new(config.npub.clone(), config.name.clone());
        Arc::new(Self {
            config,
            protocol: RwLock::new(protocol),
            nodes: RwLock::new(HashMap::new()),
            message_log: RwLock::new(VecDeque::with_capacity(MAX_LOG)),
            log_file,
            transports: RwLock::new(HashMap::new()),
            settings: RwLock::new(settings),
            scheduled_tasks: RwLock::new(scheduled_tasks),
            task_runs: RwLock::new(VecDeque::with_capacity(MAX_TASK_RUNS)),
            pane_queues: std::sync::Mutex::new(HashMap::new()),
            log_file_lock: std::sync::Mutex::new(()),
            task_run_log_lock: std::sync::Mutex::new(()),
            connected_npubs: std::sync::Mutex::new(HashMap::new()),
            last_reciprocated: std::sync::Mutex::new(HashMap::new()),
            session_agents: RwLock::new(HashMap::new()),
            resource_gates: std::sync::Mutex::new(HashMap::new()),
            project_index: RwLock::new(HashMap::new()),
            pending_commands: std::sync::Mutex::new(Vec::new()),
            cached_assistant_panes: RwLock::new(Vec::new()),
            autoregister_suppressed_panes: std::sync::Mutex::new(HashMap::new()),
            perfire_worktree_panes: RwLock::new(HashMap::new()),
            sweep_in_progress: std::sync::atomic::AtomicBool::new(false),
            sweep_backoff_until: std::sync::Mutex::new(None),
            backends: crate::backend::BackendRegistry::default_registry(),
            http_client: reqwest::Client::new(),
            pending_prompts: std::sync::Mutex::new(std::collections::HashMap::new()),
            compact_in_progress: std::sync::Mutex::new(std::collections::HashSet::new()),
        })
    }

    fn resource_gate(&self, key: ResourceGateKey) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self
            .resource_gates
            .lock()
            .expect("resource gate map mutex poisoned");
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(&key).and_then(std::sync::Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        gates.insert(key, Arc::downgrade(&gate));
        gate
    }

    fn pane_resource_gate(&self, pane: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.resource_gate(ResourceGateKey::Pane(pane.to_string()))
    }

    fn backend_resource_gate(&self, backend_session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.resource_gate(ResourceGateKey::BackendSession(
            backend_session_id.to_string(),
        ))
    }

    #[cfg(test)]
    fn project_dir_resource_gate(&self, project_dir: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.resource_gate(ResourceGateKey::ProjectDir(project_dir_identity(
            project_dir,
        )))
    }

    async fn lock_project_dir_resource(
        &self,
        project_dir: &str,
    ) -> (tokio::sync::OwnedMutexGuard<()>, String) {
        loop {
            let identity = project_dir_identity(project_dir);
            let resource = self
                .resource_gate(ResourceGateKey::ProjectDir(identity.clone()))
                .lock_owned()
                .await;
            if project_dir_identity(project_dir) == identity {
                return (resource, identity);
            }
        }
    }

    async fn lock_project_dir_candidates(
        &self,
        project_dirs: &[String],
    ) -> (Vec<tokio::sync::OwnedMutexGuard<()>>, Vec<String>) {
        loop {
            let mut identities = project_dirs
                .iter()
                .map(|dir| project_dir_identity(dir))
                .collect::<Vec<_>>();
            identities.sort();
            identities.dedup();
            let mut guards = Vec::with_capacity(identities.len());
            for identity in &identities {
                guards.push(
                    self.resource_gate(ResourceGateKey::ProjectDir(identity.clone()))
                        .lock_owned()
                        .await,
                );
            }
            let mut stable = project_dirs
                .iter()
                .map(|dir| project_dir_identity(dir))
                .collect::<Vec<_>>();
            stable.sort();
            stable.dedup();
            if stable == identities {
                return (guards, identities);
            }
        }
    }

    async fn event_resource_keys(
        &self,
        event: &crate::daemon_protocol::Event,
    ) -> Vec<ResourceGateKey> {
        fn add_entry(
            keys: &mut Vec<ResourceGateKey>,
            project_dirs: &mut Vec<String>,
            entry: &crate::daemon_protocol::SessionEntry,
        ) {
            if let Some(pane) = &entry.pane {
                keys.push(ResourceGateKey::Pane(pane.clone()));
            }
            if let Some(backend_session_id) = &entry.metadata.backend_session_id {
                keys.push(ResourceGateKey::BackendSession(backend_session_id.clone()));
            }
            if let Some(project_dir) = &entry.metadata.project_dir {
                project_dirs.push(project_dir.clone());
            }
        }
        fn add_current(
            keys: &mut Vec<ResourceGateKey>,
            project_dirs: &mut Vec<String>,
            protocol: &crate::daemon_protocol::DaemonState,
            id: &str,
        ) {
            if let Some(entry) = protocol.sessions.get(id) {
                add_entry(keys, project_dirs, entry);
            }
        }

        let protocol = self.protocol.read().await;
        let mut keys = Vec::new();
        let mut project_dirs = Vec::new();
        match event {
            crate::daemon_protocol::Event::Register { id, pane, metadata } => {
                add_current(&mut keys, &mut project_dirs, &protocol, id);
                if let Some(pane) = pane {
                    keys.push(ResourceGateKey::Pane(pane.clone()));
                }
                if let Some(backend_session_id) = &metadata.backend_session_id {
                    keys.push(ResourceGateKey::BackendSession(backend_session_id.clone()));
                }
                if let Some(project_dir) = &metadata.project_dir {
                    project_dirs.push(project_dir.clone());
                }
            }
            crate::daemon_protocol::Event::RegisterIfPaneUnbound {
                id, pane, metadata, ..
            } => {
                add_current(&mut keys, &mut project_dirs, &protocol, id);
                keys.push(ResourceGateKey::Pane(pane.clone()));
                if let Some(backend_session_id) = &metadata.backend_session_id {
                    keys.push(ResourceGateKey::BackendSession(backend_session_id.clone()));
                }
                if let Some(project_dir) = &metadata.project_dir {
                    project_dirs.push(project_dir.clone());
                }
            }
            crate::daemon_protocol::Event::StageFreshLaunch { id, .. }
            | crate::daemon_protocol::Event::Rename { old_id: id, .. }
            | crate::daemon_protocol::Event::Remove { id, .. }
            | crate::daemon_protocol::Event::UpdateMetadata { id, .. } => {
                add_current(&mut keys, &mut project_dirs, &protocol, id);
            }
            crate::daemon_protocol::Event::FreshContextRestartSucceeded { owner } => {
                add_current(&mut keys, &mut project_dirs, &protocol, &owner.session_id);
            }
            // Active-context accounting is a pure, exact-owner protocol
            // mutation. It must not wait behind owned delivery I/O holding the
            // pane/backend/project gates; the protocol lock serializes the
            // update and `DaemonState::apply` rejects superseded owners.
            crate::daemon_protocol::Event::ActiveContextActive { .. }
            | crate::daemon_protocol::Event::ActiveContextStopped { .. } => {}
            crate::daemon_protocol::Event::RefreshLaunchMetadata {
                id, pane, metadata, ..
            } => {
                add_current(&mut keys, &mut project_dirs, &protocol, id);
                if let Some(pane) = pane {
                    keys.push(ResourceGateKey::Pane(pane.clone()));
                }
                if let Some(backend_session_id) = &metadata.backend_session_id {
                    keys.push(ResourceGateKey::BackendSession(backend_session_id.clone()));
                }
                if let Some(project_dir) = &metadata.project_dir {
                    project_dirs.push(project_dir.clone());
                }
            }
            crate::daemon_protocol::Event::RemoveOwned {
                owner,
                expected_pane,
                ..
            } => {
                add_current(&mut keys, &mut project_dirs, &protocol, &owner.session_id);
                if let Some(pane) = expected_pane {
                    keys.push(ResourceGateKey::Pane(pane.clone()));
                }
            }
            crate::daemon_protocol::Event::CompleteOwnedStop {
                owner,
                expected_pane,
                ..
            } => {
                add_current(&mut keys, &mut project_dirs, &protocol, &owner.session_id);
                keys.push(ResourceGateKey::Pane(expected_pane.clone()));
            }
            crate::daemon_protocol::Event::RollbackProvisionalRegistration {
                id,
                pane,
                previous,
                ..
            } => {
                add_current(&mut keys, &mut project_dirs, &protocol, id);
                keys.push(ResourceGateKey::Pane(pane.clone()));
                if let Some(previous) = previous {
                    add_entry(&mut keys, &mut project_dirs, previous);
                }
            }
            crate::daemon_protocol::Event::RollbackFreshLaunch {
                id,
                pane,
                previous,
                provisional_pane,
                ..
            } => {
                add_current(&mut keys, &mut project_dirs, &protocol, id);
                if let Some(pane) = pane {
                    keys.push(ResourceGateKey::Pane(pane.clone()));
                }
                if let Some(pane) = provisional_pane {
                    keys.push(ResourceGateKey::Pane(pane.clone()));
                }
                if let Some(previous) = previous {
                    add_entry(&mut keys, &mut project_dirs, previous);
                }
            }
            crate::daemon_protocol::Event::RemoveIfStale { owner, .. } => {
                add_current(&mut keys, &mut project_dirs, &protocol, &owner.session_id);
            }
            crate::daemon_protocol::Event::AdoptBackend {
                id,
                backend_session_id,
                ..
            }
            | crate::daemon_protocol::Event::RebindBackend {
                id,
                backend_session_id,
                ..
            } => {
                add_current(&mut keys, &mut project_dirs, &protocol, id);
                if !backend_session_id.is_empty() {
                    keys.push(ResourceGateKey::BackendSession(backend_session_id.clone()));
                }
            }
            crate::daemon_protocol::Event::ReapDead { dead_sessions } => {
                for (owner, pane) in dead_sessions {
                    add_current(&mut keys, &mut project_dirs, &protocol, &owner.session_id);
                    keys.push(ResourceGateKey::Pane(pane.clone()));
                }
            }
            crate::daemon_protocol::Event::PruneStale { sessions } => {
                for (owner, project_dir) in sessions {
                    add_current(&mut keys, &mut project_dirs, &protocol, &owner.session_id);
                    project_dirs.push(project_dir.clone());
                }
            }
            crate::daemon_protocol::Event::IncomingWire { .. }
            | crate::daemon_protocol::Event::Send { .. } => {}
            crate::daemon_protocol::Event::MarkWorktreePresence { updates } => {
                for (owner, project_dir, _) in updates {
                    add_current(&mut keys, &mut project_dirs, &protocol, &owner.session_id);
                    project_dirs.push(project_dir.clone());
                }
            }
        }
        drop(protocol);
        keys.extend(
            project_dirs
                .into_iter()
                .map(|dir| ResourceGateKey::ProjectDir(project_dir_identity(&dir))),
        );
        keys.sort();
        keys.dedup();
        keys
    }

    async fn lock_event_resources(
        &self,
        event: &crate::daemon_protocol::Event,
    ) -> Vec<tokio::sync::OwnedMutexGuard<()>> {
        loop {
            let keys = self.event_resource_keys(event).await;
            let mut guards = Vec::with_capacity(keys.len());
            for key in &keys {
                guards.push(self.resource_gate(key.clone()).lock_owned().await);
            }
            if self.event_resource_keys(event).await == keys {
                return guards;
            }
        }
    }

    pub(crate) async fn with_owned_pane_claim<F, Fut, T>(
        &self,
        owner: &crate::daemon_protocol::ResourceOwner,
        pane: &str,
        action: F,
    ) -> Option<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let gate = self.pane_resource_gate(pane);
        let _resource = gate.lock().await;
        let current = {
            let protocol = self.protocol.read().await;
            protocol
                .sessions
                .get(&owner.session_id)
                .is_some_and(|session| {
                    session.owner() == *owner && session.pane.as_deref() == Some(pane)
                })
                || protocol
                    .lifecycle_leases
                    .get(&owner.session_id)
                    .is_some_and(|lease| {
                        lease.inert_pane.as_deref() == Some(pane)
                            && lease.inert_pane_owner.as_ref() == Some(owner)
                    })
        };
        if current { Some(action().await) } else { None }
    }

    pub(crate) async fn with_owned_pane_cleanup<F, Fut, T>(
        &self,
        owner: &crate::daemon_protocol::ResourceOwner,
        pane: &str,
        action: F,
    ) -> Option<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        self.with_allowed_pane_cleanup(std::slice::from_ref(owner), pane, action)
            .await
    }

    pub(crate) async fn with_allowed_pane_cleanup<F, Fut, T>(
        &self,
        owners: &[crate::daemon_protocol::ResourceOwner],
        pane: &str,
        action: F,
    ) -> Option<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let gate = self.pane_resource_gate(pane);
        let _resource = gate.lock().await;
        let allowed = {
            let protocol = self.protocol.read().await;
            let pane_conflict = protocol.sessions.values().any(|session| {
                session.pane.as_deref() == Some(pane) && !owners.contains(&session.owner())
            });
            !pane_conflict
        };
        if allowed { Some(action().await) } else { None }
    }

    pub(crate) async fn with_owned_backend_cleanup<F, Fut, T>(
        &self,
        owner: &crate::daemon_protocol::ResourceOwner,
        backend_session_id: &str,
        action: F,
    ) -> Option<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let gate = self.backend_resource_gate(backend_session_id);
        let _resource = gate.lock().await;
        let allowed = {
            let protocol = self.protocol.read().await;
            let backend_conflict = protocol.sessions.values().any(|session| {
                session.metadata.backend_session_id.as_deref() == Some(backend_session_id)
                    && session.owner() != *owner
            });
            !backend_conflict
        };
        if allowed { Some(action().await) } else { None }
    }

    pub(crate) async fn with_owned_backend_claim<F, Fut, T>(
        &self,
        owner: &crate::daemon_protocol::ResourceOwner,
        backend_session_id: &str,
        action: F,
    ) -> Option<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let gate = self.backend_resource_gate(backend_session_id);
        let _resource = gate.lock().await;
        let current = self
            .protocol
            .read()
            .await
            .sessions
            .get(&owner.session_id)
            .is_some_and(|session| {
                session.owner() == *owner
                    && session.metadata.backend_session_id.as_deref() == Some(backend_session_id)
            });
        if current { Some(action().await) } else { None }
    }

    /// Durably claim a project directory for an exact lifecycle lease and keep
    /// its resource gate held across the caller's filesystem operation.
    pub(crate) async fn with_reserved_project_dir_claim<F, Fut, T>(
        self: &Arc<Self>,
        lease_owner: &crate::daemon_protocol::ResourceOwner,
        project_dir_owner: crate::daemon_protocol::ResourceOwner,
        project_dir: &str,
        cleanup_if_missing: bool,
        action: F,
    ) -> anyhow::Result<Option<T>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let (_resource, identity) = self.lock_project_dir_resource(project_dir).await;
        let cleanup_on_abandon = cleanup_if_missing && !std::path::Path::new(project_dir).exists();
        {
            let mut proto = self.protocol.write().await;
            let before = proto.clone();
            let outcome = proto.record_project_dir_claim(
                lease_owner,
                project_dir_owner,
                identity,
                cleanup_on_abandon,
            );
            if outcome != crate::daemon_protocol::LifecycleMutationOutcome::Applied {
                return Ok(None);
            }
            if let Err(error) = self.persist_protocol_state(&proto) {
                *proto = before;
                return Err(error);
            }
        }
        Ok(Some(action().await))
    }

    /// Choose among possible directory layouts only after all candidate gates
    /// are held, then persist and act on that same resolved target.
    pub(crate) async fn with_reserved_project_dir_choice<S, F, Fut, T>(
        self: &Arc<Self>,
        lease_owner: &crate::daemon_protocol::ResourceOwner,
        project_dir_owner: crate::daemon_protocol::ResourceOwner,
        candidates: &[String],
        select: S,
        action: F,
    ) -> anyhow::Result<Option<T>>
    where
        S: FnOnce() -> String,
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let (_resources, identities) = self.lock_project_dir_candidates(candidates).await;
        let project_dir = select();
        let identity = project_dir_identity(&project_dir);
        if !identities.contains(&identity) {
            anyhow::bail!("resolved project directory was not among locked candidates");
        }
        let cleanup_on_abandon = !std::path::Path::new(&project_dir).exists();
        {
            let mut proto = self.protocol.write().await;
            let before = proto.clone();
            let outcome = proto.record_project_dir_claim(
                lease_owner,
                project_dir_owner,
                identity,
                cleanup_on_abandon,
            );
            if outcome != crate::daemon_protocol::LifecycleMutationOutcome::Applied {
                return Ok(None);
            }
            if let Err(error) = self.persist_protocol_state(&proto) {
                *proto = before;
                return Err(error);
            }
        }
        Ok(Some(action(project_dir).await))
    }

    /// Run directory cleanup only when no active session or different
    /// lifecycle owner currently claims the same path.
    pub(crate) async fn with_owned_worktree_cleanup<F, Fut, T>(
        &self,
        owner: &crate::daemon_protocol::ResourceOwner,
        project_dir: &str,
        action: F,
    ) -> Option<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let (_resource, identity) = self.lock_project_dir_resource(project_dir).await;
        let (active_dirs, reserved_claims) = {
            let protocol = self.protocol.read().await;
            let active_dirs = protocol
                .sessions
                .values()
                .filter_map(|session| session.metadata.project_dir.as_deref())
                .map(String::from)
                .collect::<Vec<_>>();
            let reserved_claims = protocol
                .lifecycle_leases
                .values()
                .filter_map(|lease| {
                    Some((lease.project_dir.clone()?, lease.project_dir_owner.clone()))
                })
                .collect::<Vec<_>>();
            (active_dirs, reserved_claims)
        };
        let active_claim = active_dirs
            .iter()
            .any(|dir| project_dir_identity(dir) == identity);
        let reserved_by_replacement = reserved_claims.iter().any(|(dir, claim_owner)| {
            project_dir_identity(dir) == identity && claim_owner.as_ref() != Some(owner)
        });
        let allowed = !active_claim && !reserved_by_replacement;
        if allowed { Some(action().await) } else { None }
    }

    pub(crate) async fn track_perfire_worktree(
        &self,
        owner: &crate::daemon_protocol::ResourceOwner,
        pane: &str,
        project_dir: &str,
    ) -> bool {
        let (_resource, identity) = self.lock_project_dir_resource(project_dir).await;
        let current = self
            .protocol
            .read()
            .await
            .sessions
            .get(&owner.session_id)
            .map(|session| {
                (
                    session.owner(),
                    session.pane.clone(),
                    session.metadata.project_dir.clone(),
                )
            });
        let current = current.is_some_and(|(current_owner, current_pane, current_dir)| {
            current_owner == *owner
                && current_pane.as_deref() == Some(pane)
                && current_dir
                    .as_deref()
                    .is_some_and(|dir| project_dir_identity(dir) == identity)
        });
        if !current {
            return false;
        }
        self.perfire_worktree_panes.write().await.insert(
            pane.to_string(),
            PerFireWorktreeClaim {
                owner: owner.clone(),
                project_dir: project_dir.to_string(),
            },
        );
        true
    }

    pub(crate) async fn prune_dead_perfire_worktree(
        &self,
        pane: &str,
        claim: &PerFireWorktreeClaim,
    ) -> bool {
        let (_resource, identity) = self.lock_project_dir_resource(&claim.project_dir).await;
        {
            let mut tracking = self.perfire_worktree_panes.write().await;
            if tracking.get(pane) != Some(claim) {
                return false;
            }
            tracking.remove(pane);
        }
        let (active_claims, reserved_claims) = {
            let protocol = self.protocol.read().await;
            let active_claims = protocol
                .sessions
                .values()
                .filter_map(|session| {
                    Some((session.metadata.project_dir.clone()?, session.owner()))
                })
                .collect::<Vec<_>>();
            let reserved_claims = protocol
                .lifecycle_leases
                .values()
                .filter_map(|lease| {
                    Some((lease.project_dir.clone()?, lease.project_dir_owner.clone()))
                })
                .collect::<Vec<_>>();
            (active_claims, reserved_claims)
        };
        let replacement_claim = active_claims
            .iter()
            .any(|(dir, owner)| project_dir_identity(dir) == identity && *owner != claim.owner)
            || reserved_claims.iter().any(|(dir, owner)| {
                project_dir_identity(dir) == identity && owner.as_ref() != Some(&claim.owner)
            });
        if replacement_claim {
            return false;
        }
        let project_dir = claim.project_dir.clone();
        let _ = tokio::task::spawn_blocking(move || {
            std::process::Command::new("git")
                .args(["-C", &project_dir, "worktree", "prune"])
                .status()
        })
        .await;
        true
    }

    pub(crate) fn try_acquire_compact_in_progress(
        &self,
        key: &str,
    ) -> Option<CompactInProgressGuard<'_>> {
        let mut compact_in_progress = self
            .compact_in_progress
            .lock()
            .expect("compact_in_progress mutex poisoned");
        if !compact_in_progress.insert(key.to_string()) {
            return None;
        }
        Some(CompactInProgressGuard {
            state: self,
            key: key.to_string(),
        })
    }

    /// Resolve the backend for a given session by looking up its metadata.
    pub async fn backend_for_session(
        &self,
        session_id: &str,
    ) -> std::sync::Arc<dyn crate::backend::CodingAssistant> {
        let backend_name = self
            .protocol
            .read()
            .await
            .sessions
            .get(session_id)
            .and_then(|s| s.metadata.backend.as_deref())
            .map(String::from);
        match backend_name {
            Some(name) => self
                .backends
                .get(&name)
                .unwrap_or_else(|| self.backends.default()),
            None => self.backends.default(),
        }
    }

    /// Detect which backend is running in a tmux pane by walking the process tree.
    ///
    /// Returns the backend name (e.g. `"opencode"`, `"claude-code"`) if a known
    /// backend process is found, or `None` if detection fails.
    pub async fn detect_backend_in_pane(&self, pane: &str) -> Option<String> {
        // Candidate process names for every registered backend. Deliberately not
        // filtered by `available()`: that runs each backend's `is_available()` CLI
        // probe (e.g. a slow/hanging npx `codex --version`) on the caller — which
        // both blocks this tokio worker and would drop a live codex pane whenever
        // the probe is slow. Detection is pure process-tree matching and needs no
        // availability check.
        let backend_process_names: Vec<(String, Vec<String>)> =
            self.backends.all_backend_process_names();

        let pane = pane.to_string();
        tokio::task::spawn_blocking(move || {
            use std::process::Command;

            let output = Command::new("tmux")
                .args(["display-message", "-t", &pane, "-p", "#{pane_pid}"])
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            let pane_pid: u32 = String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse()
                .ok()?;

            let output = Command::new("ps")
                .args(["-eo", "pid,ppid,comm"])
                .output()
                .ok()?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut children: std::collections::HashMap<u32, Vec<u32>> =
                std::collections::HashMap::new();
            let mut names: std::collections::HashMap<u32, String> =
                std::collections::HashMap::new();

            for line in stdout.lines().skip(1) {
                let mut parts = line.split_whitespace();
                let (Some(pid_s), Some(ppid_s), Some(comm)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                let (Ok(pid), Ok(ppid)) = (pid_s.parse::<u32>(), ppid_s.parse::<u32>()) else {
                    continue;
                };
                children.entry(ppid).or_default().push(pid);
                names.insert(pid, comm.to_string());
            }

            // BFS from pane_pid, check each process against known backend names.
            // Match both exact name and dot-prefixed name (e.g. ".opencode"
            // which appears when run via npm/node wrapper).
            let mut stack = vec![pane_pid];
            while let Some(pid) = stack.pop() {
                if let Some(comm) = names.get(&pid) {
                    for (backend_name, pnames) in &backend_process_names {
                        for pn in pnames {
                            if comm == pn || comm.strip_prefix('.') == Some(pn.as_str()) {
                                return Some(backend_name.clone());
                            }
                        }
                    }
                }
                if let Some(kids) = children.get(&pid) {
                    stack.extend(kids);
                }
            }
            None
        })
        .await
        .ok()
        .flatten()
    }

    /// Find the session ID registered on a given pane (full `%NNN` format).
    pub async fn find_session_by_pane(&self, pane: &str) -> Option<String> {
        let proto = self.protocol.read().await;
        proto
            .sessions
            .values()
            .find(|s| s.pane.as_deref() == Some(pane))
            .map(|s| s.id.clone())
    }

    /// Apply a protocol event and execute all resulting effects.
    ///
    /// The pure state transition happens under the protocol lock.
    /// Effects are executed after the lock is released.
    pub fn apply_and_execute(
        self: &Arc<Self>,
        event: crate::daemon_protocol::Event,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Vec<crate::daemon_protocol::Effect>> + Send + '_>,
    > {
        Box::pin(self._apply_and_execute(event))
    }

    pub(crate) async fn apply_owned_event_if<F>(
        self: &Arc<Self>,
        owner: &crate::daemon_protocol::ResourceOwner,
        event: crate::daemon_protocol::Event,
        predicate: F,
    ) -> Vec<crate::daemon_protocol::Effect>
    where
        F: FnOnce(&crate::daemon_protocol::SessionEntry) -> bool,
    {
        let resource_guards = self.lock_event_resources(&event).await;
        let effects = {
            let mut protocol = self.protocol.write().await;
            if protocol
                .sessions
                .get(&owner.session_id)
                .is_none_or(|session| session.owner() != *owner || !predicate(session))
            {
                return vec![];
            }
            protocol.apply(event)
        };
        drop(resource_guards);
        self.execute_effects(&effects).await;
        effects
    }

    pub async fn bind_backend_identity(
        self: &Arc<Self>,
        target_session_id: &str,
        identity: &crate::backend::BackendSessionIdentity,
        launch_credential: Option<&str>,
        expected_incarnation: Option<crate::daemon_protocol::SessionIncarnation>,
    ) -> crate::daemon_protocol::BackendIdentityBindResult {
        let resource_event = crate::daemon_protocol::Event::AdoptBackend {
            id: target_session_id.to_string(),
            backend: identity.backend.clone(),
            backend_session_id: identity.session_id.clone(),
            expected_backend_session_id: None,
            expected_session_start_credential: launch_credential.map(String::from),
        };
        let resource_guards = self.lock_event_resources(&resource_event).await;
        let result = {
            let mut protocol = self.protocol.write().await;
            if expected_incarnation.is_some_and(|expected| {
                protocol
                    .sessions
                    .get(target_session_id)
                    .is_none_or(|session| session.metadata.session_incarnation != expected)
            }) {
                return crate::daemon_protocol::BackendIdentityBindResult {
                    outcome: crate::daemon_protocol::BackendIdentityBindOutcome::TargetNotFound,
                    effects: vec![],
                };
            }
            protocol.bind_backend_identity(target_session_id, identity, launch_credential)
        };
        drop(resource_guards);
        if !result.effects.is_empty() {
            self.execute_effects(&result.effects).await;
        }
        result
    }

    #[cfg(test)]
    pub(crate) async fn with_backend_binding_transition<F, T>(
        &self,
        target_session_id: &str,
        target_backend_session_id: Option<&str>,
        transition: F,
    ) -> T
    where
        F: FnOnce(&mut crate::daemon_protocol::DaemonState) -> T,
    {
        let resource_event = crate::daemon_protocol::Event::AdoptBackend {
            id: target_session_id.to_string(),
            backend: String::new(),
            backend_session_id: target_backend_session_id.unwrap_or_default().to_string(),
            expected_backend_session_id: None,
            expected_session_start_credential: None,
        };
        let resource_guards = self.lock_event_resources(&resource_event).await;
        let result = {
            let mut protocol = self.protocol.write().await;
            transition(&mut protocol)
        };
        drop(resource_guards);
        result
    }

    /// Atomically stage a fresh managed launch and publish its effects.
    ///
    /// Unlike the generic event API, this exposes whether the authority
    /// transition was accepted so a caller can avoid doing external launch
    /// work after a concurrent restart or repair has taken ownership.
    #[cfg(test)]
    pub fn stage_fresh_launch(
        self: &Arc<Self>,
        id: &str,
        backend: String,
        session_start_credential: Option<String>,
        expected_repair_reservation: Option<crate::daemon_protocol::BackendRepairReservation>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = crate::daemon_protocol::StageFreshLaunchOutcome>
                + Send
                + '_,
        >,
    > {
        Box::pin(self._stage_fresh_launch(
            id.to_owned(),
            backend,
            session_start_credential,
            expected_repair_reservation,
        ))
    }

    #[cfg(test)]
    async fn _stage_fresh_launch(
        self: &Arc<Self>,
        id: String,
        backend: String,
        session_start_credential: Option<String>,
        expected_repair_reservation: Option<crate::daemon_protocol::BackendRepairReservation>,
    ) -> crate::daemon_protocol::StageFreshLaunchOutcome {
        let resource_event = crate::daemon_protocol::Event::StageFreshLaunch {
            id: id.clone(),
            backend: backend.clone(),
            session_start_credential: session_start_credential.clone(),
            expected_repair_reservation: expected_repair_reservation.clone(),
        };
        let resource_guards = self.lock_event_resources(&resource_event).await;
        let (outcome, effects) = {
            let mut state = self.protocol.write().await;
            let before = state.clone();
            let result = state.stage_fresh_launch(
                &id,
                backend,
                session_start_credential,
                expected_repair_reservation,
            );
            if matches!(
                result.outcome,
                crate::daemon_protocol::StageFreshLaunchOutcome::Staged { .. }
            ) && let Err(error) = self.persist_protocol_state(&state)
            {
                *state = before;
                tracing::warn!(
                    session_id = %id,
                    "failed to persist fresh launch lifecycle authority: {error}"
                );
                return crate::daemon_protocol::StageFreshLaunchOutcome::PersistenceFailed;
            }
            let effects: Vec<_> = result
                .effects
                .into_iter()
                .filter(|effect| !matches!(effect, crate::daemon_protocol::Effect::Persist))
                .collect();
            (result.outcome, effects)
        };
        drop(resource_guards);
        self.execute_effects(&effects).await;
        outcome
    }

    pub fn stage_restart_launch(
        self: &Arc<Self>,
        lease_owner: &crate::daemon_protocol::ResourceOwner,
        backend: String,
        replace_backend_identity: bool,
        session_start_credential: Option<String>,
        expected_repair_reservation: Option<crate::daemon_protocol::BackendRepairReservation>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = crate::daemon_protocol::StageFreshLaunchOutcome>
                + Send
                + '_,
        >,
    > {
        Box::pin(self._stage_restart_launch(
            lease_owner.clone(),
            backend,
            replace_backend_identity,
            session_start_credential,
            expected_repair_reservation,
        ))
    }

    async fn _stage_restart_launch(
        self: &Arc<Self>,
        lease_owner: crate::daemon_protocol::ResourceOwner,
        backend: String,
        replace_backend_identity: bool,
        session_start_credential: Option<String>,
        expected_repair_reservation: Option<crate::daemon_protocol::BackendRepairReservation>,
    ) -> crate::daemon_protocol::StageFreshLaunchOutcome {
        let resource_event = crate::daemon_protocol::Event::StageFreshLaunch {
            id: lease_owner.session_id.clone(),
            backend: backend.clone(),
            session_start_credential: session_start_credential.clone(),
            expected_repair_reservation: expected_repair_reservation.clone(),
        };
        let resource_guards = self.lock_event_resources(&resource_event).await;
        let (outcome, effects) = {
            let mut state = self.protocol.write().await;
            let before = state.clone();
            let result = state.stage_restart_launch(
                &lease_owner,
                backend,
                replace_backend_identity,
                session_start_credential,
                expected_repair_reservation,
            );
            if matches!(
                result.outcome,
                crate::daemon_protocol::StageFreshLaunchOutcome::Staged { .. }
            ) && let Err(error) = self.persist_protocol_state(&state)
            {
                *state = before;
                tracing::warn!(
                    session_id = %lease_owner.session_id,
                    "failed to persist restart target authority: {error}"
                );
                return crate::daemon_protocol::StageFreshLaunchOutcome::PersistenceFailed;
            }
            let effects = result
                .effects
                .into_iter()
                .filter(|effect| !matches!(effect, crate::daemon_protocol::Effect::Persist))
                .collect::<Vec<_>>();
            (result.outcome, effects)
        };
        drop(resource_guards);
        self.execute_effects(&effects).await;
        outcome
    }

    pub fn complete_restart_launch(
        self: &Arc<Self>,
        lease_owner: &crate::daemon_protocol::ResourceOwner,
        target_owner: &crate::daemon_protocol::ResourceOwner,
        pane: Option<String>,
        metadata: crate::daemon_protocol::SessionMeta,
        physical_respawned: bool,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = anyhow::Result<crate::daemon_protocol::LifecycleMutationOutcome>,
                > + Send
                + '_,
        >,
    > {
        Box::pin(self._complete_restart_launch(
            lease_owner.clone(),
            target_owner.clone(),
            pane,
            metadata,
            physical_respawned,
        ))
    }

    async fn _complete_restart_launch(
        self: &Arc<Self>,
        lease_owner: crate::daemon_protocol::ResourceOwner,
        target_owner: crate::daemon_protocol::ResourceOwner,
        pane: Option<String>,
        metadata: crate::daemon_protocol::SessionMeta,
        physical_respawned: bool,
    ) -> anyhow::Result<crate::daemon_protocol::LifecycleMutationOutcome> {
        let resource_event = crate::daemon_protocol::Event::RefreshLaunchMetadata {
            id: target_owner.session_id.clone(),
            expected_incarnation: target_owner.incarnation,
            pane: pane.clone(),
            metadata: metadata.clone(),
        };
        let resource_guards = self.lock_event_resources(&resource_event).await;
        let (outcome, effects) = {
            let mut state = self.protocol.write().await;
            let before = state.clone();
            let result = state.complete_restart_launch(
                &lease_owner,
                &target_owner,
                pane,
                metadata,
                physical_respawned,
            );
            if result.outcome == crate::daemon_protocol::LifecycleMutationOutcome::Applied
                && let Err(error) = self.persist_protocol_state(&state)
            {
                *state = before;
                return Err(error);
            }
            let effects = result
                .effects
                .into_iter()
                .filter(|effect| !matches!(effect, crate::daemon_protocol::Effect::Persist))
                .collect::<Vec<_>>();
            (result.outcome, effects)
        };
        drop(resource_guards);
        self.execute_effects(&effects).await;
        Ok(outcome)
    }

    pub async fn record_restart_backend_claim(
        &self,
        lease_owner: &crate::daemon_protocol::ResourceOwner,
        target_owner: &crate::daemon_protocol::ResourceOwner,
        backend: String,
        backend_session_id: String,
    ) -> anyhow::Result<crate::daemon_protocol::LifecycleMutationOutcome> {
        let gate = self.backend_resource_gate(&backend_session_id);
        let _resource = gate.lock().await;
        let mut state = self.protocol.write().await;
        let before = state.clone();
        let outcome = state.record_restart_backend_claim(
            lease_owner,
            target_owner,
            backend,
            backend_session_id,
        );
        if outcome == crate::daemon_protocol::LifecycleMutationOutcome::Applied
            && let Err(error) = self.persist_protocol_state(&state)
        {
            *state = before;
            return Err(error);
        }
        Ok(outcome)
    }

    pub async fn clear_restart_backend_claim(
        &self,
        lease_owner: &crate::daemon_protocol::ResourceOwner,
        target_owner: &crate::daemon_protocol::ResourceOwner,
        backend_session_id: &str,
    ) -> anyhow::Result<crate::daemon_protocol::LifecycleMutationOutcome> {
        let gate = self.backend_resource_gate(backend_session_id);
        let _resource = gate.lock().await;
        let mut state = self.protocol.write().await;
        let before = state.clone();
        let outcome =
            state.clear_restart_backend_claim(lease_owner, target_owner, backend_session_id);
        if outcome == crate::daemon_protocol::LifecycleMutationOutcome::Applied
            && let Err(error) = self.persist_protocol_state(&state)
        {
            *state = before;
            return Err(error);
        }
        Ok(outcome)
    }

    pub fn rollback_restart_launch(
        self: &Arc<Self>,
        lease_owner: &crate::daemon_protocol::ResourceOwner,
        target_owner: &crate::daemon_protocol::ResourceOwner,
        provisional_pane: Option<&str>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = anyhow::Result<crate::daemon_protocol::LifecycleMutationOutcome>,
                > + Send
                + '_,
        >,
    > {
        Box::pin(self._rollback_restart_launch(
            lease_owner.clone(),
            target_owner.clone(),
            provisional_pane.map(str::to_owned),
        ))
    }

    async fn _rollback_restart_launch(
        self: &Arc<Self>,
        lease_owner: crate::daemon_protocol::ResourceOwner,
        target_owner: crate::daemon_protocol::ResourceOwner,
        provisional_pane: Option<String>,
    ) -> anyhow::Result<crate::daemon_protocol::LifecycleMutationOutcome> {
        let resource_event = {
            let state = self.protocol.read().await;
            let current = state.sessions.get(&target_owner.session_id);
            let previous = state
                .lifecycle_leases
                .get(&lease_owner.session_id)
                .and_then(|lease| lease.restart_previous.as_deref())
                .cloned();
            crate::daemon_protocol::Event::RollbackFreshLaunch {
                id: target_owner.session_id.clone(),
                pane: current.and_then(|session| session.pane.clone()),
                credential: current
                    .and_then(|session| session.metadata.session_start_credential.clone()),
                staged_incarnation: target_owner.incarnation,
                previous,
                provisional_pane: provisional_pane.clone(),
            }
        };
        let resource_guards = self.lock_event_resources(&resource_event).await;
        let mut state = self.protocol.write().await;
        let before = state.clone();
        let result =
            state.rollback_restart_launch(&lease_owner, &target_owner, provisional_pane.as_deref());
        if result.outcome == crate::daemon_protocol::LifecycleMutationOutcome::Applied
            && let Err(error) = self.persist_protocol_state(&state)
        {
            *state = before;
            return Err(error);
        }
        let effects = result
            .effects
            .into_iter()
            .filter(|effect| !matches!(effect, crate::daemon_protocol::Effect::Persist))
            .collect::<Vec<_>>();
        drop(state);
        drop(resource_guards);
        self.execute_effects(&effects).await;
        Ok(result.outcome)
    }

    /// Reserve a start and durably publish its allocator/lease state before
    /// returning authority to a caller that may perform external work.
    #[allow(dead_code)] // Introduced in Chunk 1; launch callers adopt it in Chunk 2.
    pub async fn reserve_start(
        self: &Arc<Self>,
        session_id: &str,
    ) -> anyhow::Result<crate::daemon_protocol::StartDisposition> {
        let mut proto = self.protocol.write().await;
        let before = proto.clone();
        let disposition = proto
            .reserve_start(session_id)
            .map_err(anyhow::Error::from)?;
        if matches!(
            disposition,
            crate::daemon_protocol::StartDisposition::Reserved(_)
        ) && let Err(error) = self.persist_protocol_state(&proto)
        {
            *proto = before;
            return Err(error);
        }
        Ok(disposition)
    }

    /// Durably associate an inert pre-launch pane with its exact lease so a
    /// daemon restart can remove only that abandoned resource.
    pub async fn record_inert_start_pane(
        self: &Arc<Self>,
        lease_owner: &crate::daemon_protocol::ResourceOwner,
        pane_owner: crate::daemon_protocol::ResourceOwner,
        pane: String,
    ) -> anyhow::Result<crate::daemon_protocol::LifecycleMutationOutcome> {
        let pane_gate = self.pane_resource_gate(&pane);
        let _resource = pane_gate.lock().await;
        let mut proto = self.protocol.write().await;
        let before = proto.clone();
        let outcome = proto.record_inert_start_pane(lease_owner, pane_owner, pane);
        if outcome == crate::daemon_protocol::LifecycleMutationOutcome::Applied
            && let Err(error) = self.persist_protocol_state(&proto)
        {
            *proto = before;
            return Err(error);
        }
        Ok(outcome)
    }

    /// Durably claim the exact incumbent for `/start`'s existing-session
    /// restart behavior before the background task can perform external work.
    pub async fn claim_existing_start(
        self: &Arc<Self>,
        owner: &crate::daemon_protocol::ResourceOwner,
    ) -> anyhow::Result<crate::daemon_protocol::LifecycleMutationOutcome> {
        let mut proto = self.protocol.write().await;
        let before = proto.clone();
        let outcome = proto.claim_existing_start(owner);
        if outcome == crate::daemon_protocol::LifecycleMutationOutcome::Applied
            && let Err(error) = self.persist_protocol_state(&proto)
        {
            *proto = before;
            return Err(error);
        }
        Ok(outcome)
    }

    /// Durably retain an exact session ID and pane while its backend exits.
    pub async fn claim_existing_stop(
        self: &Arc<Self>,
        owner: &crate::daemon_protocol::ResourceOwner,
        pane: &str,
        cleanup_project_dir_on_abandon: bool,
    ) -> anyhow::Result<crate::daemon_protocol::LifecycleMutationOutcome> {
        let mut proto = self.protocol.write().await;
        let before = proto.clone();
        let outcome = proto.claim_existing_stop(owner, pane, cleanup_project_dir_on_abandon);
        if outcome == crate::daemon_protocol::LifecycleMutationOutcome::Applied
            && let Err(error) = self.persist_protocol_state(&proto)
        {
            *proto = before;
            return Err(error);
        }
        Ok(outcome)
    }

    /// Revalidate the exact live row and durable Stopping lease together.
    pub async fn owns_stopping_session(
        &self,
        owner: &crate::daemon_protocol::ResourceOwner,
        pane: &str,
    ) -> bool {
        self.protocol
            .read()
            .await
            .owns_stopping_session(owner, pane)
    }

    /// Publish a reserved start's active owner while retaining its pre-launch
    /// lease through the external backend-command boundary.
    pub async fn commit_reserved_start(
        self: &Arc<Self>,
        owner: &crate::daemon_protocol::ResourceOwner,
        pane: Option<String>,
        metadata: crate::daemon_protocol::SessionMeta,
    ) -> anyhow::Result<crate::daemon_protocol::LifecycleMutationOutcome> {
        let resource_event = crate::daemon_protocol::Event::Register {
            id: owner.session_id.clone(),
            pane: pane.clone(),
            metadata: metadata.clone(),
        };
        let resource_guards = self.lock_event_resources(&resource_event).await;
        let (outcome, effects) = {
            let mut proto = self.protocol.write().await;
            let before = proto.clone();
            let result = proto.commit_reserved_start(owner, pane, metadata);
            if result.outcome == crate::daemon_protocol::LifecycleMutationOutcome::Applied
                && let Err(error) = self.persist_protocol_state(&proto)
            {
                *proto = before;
                return Err(error);
            }
            let effects: Vec<_> = result
                .effects
                .into_iter()
                .filter(|effect| !matches!(effect, crate::daemon_protocol::Effect::Persist))
                .collect();
            (result.outcome, effects)
        };
        drop(resource_guards);
        self.execute_launch_registration_effects(&effects).await;
        Ok(outcome)
    }

    /// Execute the bounded effect set emitted by an exact-owner start commit.
    ///
    /// This deliberately does not call the general effect executor: that
    /// executor can receive another remote StartSession, which would make the
    /// start future recursively contain itself. `commit_reserved_start`
    /// emits only ordinary registration effects.
    async fn execute_launch_registration_effects(
        self: &Arc<Self>,
        effects: &[crate::daemon_protocol::Effect],
    ) {
        use crate::daemon_protocol::Effect;

        for effect in effects {
            match effect {
                Effect::Broadcast(message) => {
                    crate::transport::broadcast(self, message).await;
                }
                Effect::BroadcastSessionList => {
                    crate::transport::broadcast_local_sessions(self).await;
                }
                Effect::SetTmuxVar {
                    owner,
                    pane,
                    name,
                    value,
                } => {
                    self.set_owned_tmux_var(owner, pane, name, value).await;
                }
                Effect::WaitForTmuxOwner { owner, pane } => {
                    self.wait_for_owned_tmux_owner(owner, pane).await;
                }
                Effect::ClearTmuxVar { owner, pane, name } => {
                    self.clear_owned_tmux_var(owner, pane, name).await;
                }
                Effect::HoldAutoregister { pane } => {
                    self.autoregister_suppressed_panes
                        .lock()
                        .expect("autoregister suppression mutex poisoned")
                        .insert(
                            pane.clone(),
                            std::time::Instant::now()
                                + std::time::Duration::from_secs(AUTOREGISTER_REMOVE_GRACE_SECS),
                        );
                }
                Effect::EnableAutoRename { owner, pane } => {
                    self.enable_owned_auto_rename(owner, pane).await;
                }
                Effect::SpawnAgent { owner, pane } => {
                    self.spawn_session_agent(owner, pane).await;
                }
                Effect::StopAgent { owner, pane } => {
                    self.stop_session_agent(owner, pane).await;
                }
                Effect::ClearPendingReplies { removed_ids } => {
                    self.clear_orphaned_pending_replies(removed_ids).await;
                }
                Effect::ClearOwnedPendingReplies { removed_owners } => {
                    let mut protocol = self.protocol.write().await;
                    let removed_ids = removed_owners
                        .iter()
                        .filter(|owner| !protocol.sessions.contains_key(&owner.session_id))
                        .map(|owner| owner.session_id.clone())
                        .collect::<Vec<_>>();
                    protocol.clear_orphaned_replies(&removed_ids);
                }
                Effect::ProvisionalRollbackOk { owner, pane } => {
                    self.kill_owned_pane(owner, pane).await;
                }
                Effect::Log { level, message } => match level {
                    crate::daemon_protocol::LogLevel::Debug => tracing::debug!("{message}"),
                    crate::daemon_protocol::LogLevel::Info => tracing::info!("{message}"),
                    crate::daemon_protocol::LogLevel::Warn => tracing::warn!("{message}"),
                },
                Effect::RegisterOk { .. } | Effect::RemoveOk { .. } => {}
                unexpected => {
                    tracing::warn!(
                        ?unexpected,
                        "unexpected effect emitted by reserved start commit"
                    );
                }
            }
        }
    }

    /// Durably apply final launch metadata only while `owner` still owns the
    /// public session ID.
    pub async fn finalize_reserved_start(
        self: &Arc<Self>,
        owner: &crate::daemon_protocol::ResourceOwner,
        pane: Option<String>,
        metadata: crate::daemon_protocol::SessionMeta,
    ) -> anyhow::Result<crate::daemon_protocol::LifecycleMutationOutcome> {
        let event = crate::daemon_protocol::Event::RefreshLaunchMetadata {
            id: owner.session_id.clone(),
            expected_incarnation: owner.incarnation,
            pane,
            metadata,
        };
        let resource_guards = self.lock_event_resources(&event).await;
        let effects = {
            let mut proto = self.protocol.write().await;
            let Some(current) = proto.sessions.get(&owner.session_id) else {
                return Ok(crate::daemon_protocol::LifecycleMutationOutcome::NotFound);
            };
            if !matches!(current.origin, crate::daemon_protocol::Origin::Local) {
                return Ok(crate::daemon_protocol::LifecycleMutationOutcome::Rejected);
            }
            if current.metadata.session_incarnation != owner.incarnation {
                return Ok(crate::daemon_protocol::LifecycleMutationOutcome::Superseded);
            }

            let before = proto.clone();
            let effects = proto.apply(event);
            if let Err(error) = self.persist_protocol_state(&proto) {
                *proto = before;
                return Err(error);
            }
            effects
                .into_iter()
                .filter(|effect| !matches!(effect, crate::daemon_protocol::Effect::Persist))
                .collect::<Vec<_>>()
        };
        drop(resource_guards);
        self.execute_launch_registration_effects(&effects).await;
        Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied)
    }

    /// Durably remove a failed start only while `owner` still owns the exact
    /// pane and launch credential.
    pub async fn rollback_reserved_start(
        self: &Arc<Self>,
        owner: &crate::daemon_protocol::ResourceOwner,
        pane: &str,
        credential: Option<&str>,
    ) -> anyhow::Result<crate::daemon_protocol::LifecycleMutationOutcome> {
        self.rollback_launch(owner, Some(pane), credential, None, Some(pane))
            .await
    }

    /// Durably restore a failed launch's previous entry under an exact target
    /// owner. The caller must first remove any provisional pane by exact owner;
    /// this transition only clears the matching recovery record.
    pub async fn rollback_launch(
        self: &Arc<Self>,
        owner: &crate::daemon_protocol::ResourceOwner,
        pane: Option<&str>,
        credential: Option<&str>,
        previous: Option<crate::daemon_protocol::SessionEntry>,
        provisional_pane: Option<&str>,
    ) -> anyhow::Result<crate::daemon_protocol::LifecycleMutationOutcome> {
        let event = crate::daemon_protocol::Event::RollbackFreshLaunch {
            id: owner.session_id.clone(),
            pane: pane.map(String::from),
            credential: credential.map(String::from),
            staged_incarnation: owner.incarnation,
            previous,
            provisional_pane: provisional_pane.map(String::from),
        };
        let resource_guards = self.lock_event_resources(&event).await;
        let effects = {
            let mut proto = self.protocol.write().await;
            let Some(current) = proto.sessions.get(&owner.session_id) else {
                return Ok(crate::daemon_protocol::LifecycleMutationOutcome::NotFound);
            };
            if !matches!(current.origin, crate::daemon_protocol::Origin::Local) {
                return Ok(crate::daemon_protocol::LifecycleMutationOutcome::Rejected);
            }
            if current.metadata.session_incarnation != owner.incarnation
                || current.pane.as_deref() != pane
                || current.metadata.session_start_credential.as_deref() != credential
            {
                return Ok(crate::daemon_protocol::LifecycleMutationOutcome::Superseded);
            }

            let before = proto.clone();
            let effects = proto.apply(event);
            if let Some(lease) = proto.lifecycle_leases.get_mut(&owner.session_id)
                && lease.inert_pane.as_deref() == provisional_pane
                && lease.inert_pane_owner.as_ref() == Some(owner)
            {
                lease.inert_pane = None;
                lease.inert_pane_owner = None;
            }
            if let Err(error) = self.persist_protocol_state(&proto) {
                *proto = before;
                return Err(error);
            }
            effects
                .into_iter()
                .filter(|effect| {
                    !matches!(
                        effect,
                        crate::daemon_protocol::Effect::Persist
                            | crate::daemon_protocol::Effect::ProvisionalRollbackOk { .. }
                    )
                })
                .collect::<Vec<_>>()
        };
        drop(resource_guards);
        self.execute_launch_registration_effects(&effects).await;
        Ok(crate::daemon_protocol::LifecycleMutationOutcome::Applied)
    }

    /// Release an exact lifecycle lease only after the removal is durable.
    #[allow(dead_code)] // Introduced in Chunk 1; launch callers adopt it in Chunk 2.
    pub async fn abort_lifecycle(
        self: &Arc<Self>,
        owner: &crate::daemon_protocol::ResourceOwner,
    ) -> anyhow::Result<crate::daemon_protocol::LifecycleMutationOutcome> {
        let mut proto = self.protocol.write().await;
        let before = proto.clone();
        let outcome = proto.abort_lifecycle(owner);
        if outcome == crate::daemon_protocol::LifecycleMutationOutcome::Applied
            && let Err(error) = self.persist_protocol_state(&proto)
        {
            *proto = before;
            return Err(error);
        }
        Ok(outcome)
    }

    async fn _apply_and_execute(
        self: &Arc<Self>,
        event: crate::daemon_protocol::Event,
    ) -> Vec<crate::daemon_protocol::Effect> {
        let resource_guards = self.lock_event_resources(&event).await;
        let (effects, rollback) = {
            let mut state = self.protocol.write().await;
            let mut rollback = FailedEffectSendRollback::capture_for_event(&state, &event);
            let effects = state.apply(event);
            if let Some(rollback) = &mut rollback {
                rollback.capture_after_send(&state);
                rollback.reserve_sender_state_after_send(&mut state);
            }
            (effects, rollback)
        };
        drop(resource_guards);

        let delivery_failure = self.execute_effects(&effects).await;

        if let Some(failure) = delivery_failure {
            self.clear_pending_reply_for_failed_effect_delivery(&effects)
                .await;
            self.rollback_sender_state_for_failed_effect_delivery(rollback)
                .await;
            return rewrite_send_delivery_failure(effects, &failure.reason);
        }

        if effects
            .iter()
            .any(|effect| matches!(effect, crate::daemon_protocol::Effect::SendFailed { .. }))
        {
            self.rollback_sender_state_for_failed_effect_delivery(rollback)
                .await;
            return effects;
        }

        self.finalize_successful_effect_delivery(rollback).await;

        effects
    }

    pub(crate) async fn execute_effects(
        self: &Arc<Self>,
        effects: &[crate::daemon_protocol::Effect],
    ) -> Option<EffectDeliveryFailure> {
        use crate::daemon_protocol::{Effect, LogLevel};

        let recorded_method = effects.iter().find_map(|effect| match effect {
            Effect::SendDelivered { method, .. } => Some(method.as_str()),
            _ => None,
        });
        let recorded_http_delivery = effects.iter().find_map(|effect| match effect {
            Effect::SendDelivered { http_delivery, .. } => http_delivery.as_ref(),
            _ => None,
        });
        let mut delivery_failure = None;

        for effect in effects {
            match effect {
                Effect::Broadcast(msg) => {
                    if delivery_failure.is_some() {
                        if let crate::protocol::WireMessage::SessionSendAck {
                            from,
                            to,
                            delivered: true,
                            daemon_id,
                        } = msg
                        {
                            let failed_ack = crate::protocol::WireMessage::SessionSendAck {
                                from: from.clone(),
                                to: to.clone(),
                                delivered: false,
                                daemon_id: daemon_id.clone(),
                            };
                            crate::transport::broadcast(self, &failed_ack).await;
                            continue;
                        }
                    }
                    crate::transport::broadcast(self, msg).await;
                }
                Effect::BroadcastSessionList => {
                    crate::transport::broadcast_local_sessions(self).await;
                }
                Effect::InjectMessage {
                    session_id,
                    pane,
                    message,
                    vim_mode,
                    delivery_method,
                    ..
                } => {
                    let outcome = deliver_inject_message_effect(
                        self,
                        InjectDeliveryRequest {
                            session_id,
                            pane,
                            message,
                            vim_mode: *vim_mode,
                            delivery_method: delivery_method.as_deref(),
                            recorded_method,
                        },
                    )
                    .await;
                    match outcome {
                        DeliveryOutcome::Accepted => {}
                        DeliveryOutcome::Rejected(reason) => {
                            tracing::warn!(session = %session_id, "message delivery failed: {reason}");
                            delivery_failure.get_or_insert(EffectDeliveryFailure { reason });
                        }
                        DeliveryOutcome::Ambiguous(reason) => {
                            tracing::warn!(session = %session_id, "message delivery outcome ambiguous; preserving delivered state: {reason}");
                        }
                    }
                }
                Effect::DeliverHttpMessage {
                    session_id,
                    message,
                    http_delivery,
                    ..
                } => match Some(http_delivery).or(recorded_http_delivery) {
                    Some(delivery) => {
                        if let Err(decision) = crate::tmux::deliver_via_http(
                            self,
                            &delivery.backend_session_id,
                            delivery.project_dir.as_deref(),
                            message,
                            delivery.model.as_deref(),
                            delivery.effort.as_deref(),
                        )
                        .await
                        {
                            match http_delivery_attempt_failure(decision) {
                                DeliveryOutcome::Accepted => {}
                                DeliveryOutcome::Rejected(reason) => {
                                    tracing::warn!(session = %session_id, "http delivery failed: {reason}");
                                    delivery_failure
                                        .get_or_insert(EffectDeliveryFailure { reason });
                                }
                                DeliveryOutcome::Ambiguous(reason) => {
                                    tracing::warn!(session = %session_id, "http delivery outcome ambiguous; preserving delivered state: {reason}");
                                }
                            }
                        }
                    }
                    None => {
                        let error = anyhow::anyhow!(
                            "http delivery skipped: no recorded backend_session_id on send"
                        );
                        tracing::warn!(session = %session_id, "{error}");
                        delivery_failure.get_or_insert_with(|| EffectDeliveryFailure {
                            reason: error.to_string(),
                        });
                    }
                },
                Effect::SetTmuxVar {
                    owner,
                    pane,
                    name,
                    value,
                } => {
                    self.set_owned_tmux_var(owner, pane, name, value).await;
                }
                Effect::WaitForTmuxOwner { owner, pane } => {
                    self.wait_for_owned_tmux_owner(owner, pane).await;
                }
                Effect::ClearTmuxVar { owner, pane, name } => {
                    self.clear_owned_tmux_var(owner, pane, name).await;
                }
                Effect::HoldAutoregister { pane } => {
                    self.autoregister_suppressed_panes
                        .lock()
                        .expect("autoregister suppression mutex poisoned")
                        .insert(
                            pane.clone(),
                            std::time::Instant::now()
                                + std::time::Duration::from_secs(AUTOREGISTER_REMOVE_GRACE_SECS),
                        );
                }
                Effect::ProvisionalRollbackOk { owner, pane } => {
                    self.kill_owned_pane(owner, pane).await;
                }
                Effect::RenameWindow { pane, name } => {
                    let p = pane.clone();
                    let n = name.clone();
                    tokio::task::spawn_blocking(move || crate::tmux::rename_window(&p, &n));
                }
                Effect::EnableAutoRename { owner, pane } => {
                    self.enable_owned_auto_rename(owner, pane).await;
                }
                Effect::SpawnAgent { owner, pane } => {
                    self.spawn_session_agent(owner, pane).await;
                }
                Effect::StopAgent { owner, pane } => {
                    self.stop_session_agent(owner, pane).await;
                }
                Effect::ActiveContextRestartDue { owner } => {
                    spawn_owned_active_context_restart_due_delivery(self, owner);
                }
                Effect::RenameAgent {
                    old_owner,
                    new_owner,
                } => {
                    self.rename_session_agent(old_owner, new_owner).await;
                }
                Effect::ClearPendingReplies { removed_ids } => {
                    self.clear_orphaned_pending_replies(removed_ids).await;
                }
                Effect::ClearOwnedPendingReplies { removed_owners } => {
                    let mut protocol = self.protocol.write().await;
                    let removed_ids = removed_owners
                        .iter()
                        .filter(|owner| !protocol.sessions.contains_key(&owner.session_id))
                        .map(|owner| owner.session_id.clone())
                        .collect::<Vec<_>>();
                    protocol.clear_orphaned_replies(&removed_ids);
                }
                Effect::Persist => {
                    let proto = self.protocol.read().await;
                    if let Err(error) = self.persist_protocol_state(&proto) {
                        tracing::warn!("failed to persist protocol state: {error}");
                    }
                }
                Effect::CleanupWorktree { owner, project_dir } => {
                    self.cleanup_worktree_dir_if_unused(owner, project_dir)
                        .await;
                }
                Effect::SendToHuman { npub, message } => {
                    let _ = crate::nostr_transport::send_plain_dm(self, npub, message).await;
                }
                Effect::ExecuteCommand { command, daemon_id } => {
                    tracing::info!("received command from {daemon_id}: {command}");
                    // Spawn as detached task to break async recursion chain
                    // (command → start_session → revive_or_start_pane → apply_and_execute)
                    let state = Arc::clone(self);
                    let cmd = command.clone();
                    tokio::spawn(async move {
                        let result =
                            crate::nostr_transport::handle_human_command(&state, &cmd).await;
                        let reply = crate::protocol::WireMessage::CommandResult {
                            command: cmd,
                            result,
                            daemon_id: state.config.npub.clone(),
                        };
                        crate::transport::broadcast(&state, &reply).await;
                    });
                }
                Effect::ExecuteSessionStart {
                    name,
                    worktree,
                    project_dir,
                    prompt,
                    reminder,
                    from,
                    expects_reply,
                    daemon_id: sender_id,
                } => {
                    tracing::info!("received session_start from {sender_id}: {name}");
                    let state = Arc::clone(self);
                    let name = name.clone();
                    let worktree = *worktree;
                    let project_dir = project_dir.clone();
                    let prompt = prompt.clone();
                    let reminder = reminder.clone();
                    let from = from.clone();
                    let expects_reply = *expects_reply;
                    tokio::spawn(async move {
                        let (result, _prompt_msg_id) = crate::nostr_transport::start_session(
                            &state,
                            &name,
                            worktree,
                            project_dir.as_deref(),
                            prompt.as_deref(),
                            from.as_deref(),
                            expects_reply,
                            None,
                            None, // model
                            None, // effort
                            reminder.as_deref(),
                            None,  // parent_session
                            None,  // idle_policy
                            None,  // branch
                            None,  // base_branch
                            false, // force_reset — remote /start never resets (hub#528 guard)
                            None,  // reserve inside the transport start boundary
                        )
                        .await;
                        let reply = crate::protocol::WireMessage::CommandResult {
                            command: format!("/start {name}"),
                            result,
                            daemon_id: state.config.npub.clone(),
                        };
                        crate::transport::broadcast(&state, &reply).await;
                    });
                }
                Effect::ExecuteSessionRestart {
                    name,
                    fresh,
                    prompt,
                    reminder,
                    from,
                    expects_reply,
                    daemon_id: sender_id,
                } => {
                    tracing::info!("received session_restart from {sender_id}: {name}");
                    let state = Arc::clone(self);
                    let name = name.clone();
                    let fresh = fresh.unwrap_or(false);
                    let prompt = prompt.clone();
                    let reminder = reminder.clone();
                    let from = from.clone();
                    let expects_reply = *expects_reply;
                    tokio::spawn(async move {
                        let (result, _prompt_msg_id, _) = crate::nostr_transport::restart_session(
                            &state,
                            &name,
                            fresh,
                            None,
                            prompt.as_deref(),
                            from.as_deref(),
                            expects_reply,
                            None,
                            None, // model
                            None, // effort
                            reminder.as_deref(),
                            crate::nostr_transport::ParentSessionOverride::PreservePrevious,
                            None, // idle_policy
                        )
                        .await;
                        let reply = crate::protocol::WireMessage::CommandResult {
                            command: format!("/restart {name}"),
                            result,
                            daemon_id: state.config.npub.clone(),
                        };
                        crate::transport::broadcast(&state, &reply).await;
                    });
                }
                Effect::DeliverCommandResult {
                    daemon_id,
                    command,
                    result,
                } => {
                    tracing::info!("command result from {daemon_id}: {command} -> {result}");
                    self.deliver_command_result(daemon_id, command, result)
                        .await;
                }
                Effect::RecordNode {
                    daemon_id,
                    daemon_name,
                } => {
                    self.nodes.write().await.insert(
                        daemon_id.clone(),
                        NodeInfo {
                            name: daemon_name.clone(),
                            daemon_id: daemon_id.clone(),
                            connected_at: Utc::now(),
                        },
                    );
                }
                Effect::Reciprocate { daemon_id } => {
                    if self.should_reciprocate(daemon_id) {
                        tracing::info!("reciprocating session list to {daemon_id}");
                        crate::transport::broadcast_local_sessions(self).await;
                    }
                }
                Effect::LogMessage {
                    from,
                    to,
                    message,
                    delivered,
                    transport,
                } => {
                    let delivered = if delivery_failure.is_some() {
                        false
                    } else {
                        *delivered
                    };
                    self.log_message(
                        from.clone(),
                        to.clone(),
                        message.clone(),
                        delivered,
                        transport,
                    )
                    .await;
                }
                Effect::Log { level, message } => match level {
                    LogLevel::Info => tracing::info!("{message}"),
                    LogLevel::Warn => tracing::warn!("{message}"),
                    LogLevel::Debug => tracing::debug!("{message}"),
                },
                // Result effects handled by callers, not executed
                Effect::RegisterOk { .. }
                | Effect::RegisterFailed { .. }
                | Effect::SendDelivered { .. }
                | Effect::SendFailed { .. }
                | Effect::RenameOk { .. }
                | Effect::RenameFailed { .. }
                | Effect::RemoveOk { .. }
                | Effect::RemoveFailed { .. } => {}
            }
        }

        delivery_failure
    }

    async fn clear_pending_reply_for_failed_effect_delivery(
        &self,
        effects: &[crate::daemon_protocol::Effect],
    ) {
        let Some((to, msg_id, from)) = effects.iter().find_map(|effect| match effect {
            crate::daemon_protocol::Effect::SendDelivered {
                from, to, msg_id, ..
            } => Some((to.clone(), *msg_id, Some(from.clone()))),
            crate::daemon_protocol::Effect::InjectMessage {
                session_id,
                pending_reply_msg_id,
                pending_reply_from,
                ..
            } => pending_reply_msg_id
                .map(|msg_id| (session_id.clone(), msg_id, pending_reply_from.clone())),
            crate::daemon_protocol::Effect::DeliverHttpMessage {
                session_id,
                pending_reply_msg_id,
                pending_reply_from,
                ..
            } => pending_reply_msg_id
                .map(|msg_id| (session_id.clone(), msg_id, pending_reply_from.clone())),
            _ => None,
        }) else {
            return;
        };
        let Some(from) = from else {
            return;
        };

        let mut proto = self.protocol.write().await;
        let Some(pending) = proto.pending_replies.get_mut(&to) else {
            return;
        };
        pending.retain(|entry| entry.msg_id != msg_id || entry.from != from);
        if pending.is_empty() {
            proto.pending_replies.remove(&to);
        }
    }

    async fn rollback_sender_state_for_failed_effect_delivery(
        &self,
        rollback: Option<FailedEffectSendRollback>,
    ) {
        let Some(rollback) = rollback else {
            return;
        };

        let mut proto = self.protocol.write().await;
        if rollback.sender_state_reserved() {
            return;
        }

        if let Some(entry) = rollback.pending_reply_before_send {
            let current_entry =
                proto
                    .pending_replies
                    .get(&rollback.sender_id)
                    .and_then(|pending| {
                        pending
                            .iter()
                            .find(|pending| pending.msg_id == entry.msg_id)
                            .cloned()
                    });
            if rollback.pending_reply_after_send.as_ref() == Some(&current_entry) {
                let pending = proto
                    .pending_replies
                    .entry(rollback.sender_id.clone())
                    .or_default();
                if let Some(existing) = pending
                    .iter_mut()
                    .find(|pending| pending.msg_id == entry.msg_id)
                {
                    *existing = entry;
                } else {
                    pending.push(entry);
                }
            }
        }
        if rollback.done {
            let current_reminder = proto
                .sessions
                .get(&rollback.sender_id)
                .and_then(|session| session.metadata.reminder.clone());
            if rollback.sender_reminder_after_send.as_ref() == Some(&current_reminder)
                && let Some(session) = proto.sessions.get_mut(&rollback.sender_id)
            {
                session.metadata.reminder = rollback.sender_reminder.flatten();
            }
        }
    }

    async fn finalize_successful_effect_delivery(
        &self,
        rollback: Option<FailedEffectSendRollback>,
    ) {
        let Some(rollback) = rollback else {
            return;
        };
        if !rollback.done {
            return;
        }

        let mut proto = self.protocol.write().await;
        if let Some(entry) = rollback.pending_reply_before_send {
            if let Some(pending) = proto.pending_replies.get_mut(&rollback.sender_id) {
                pending.retain(|pending| pending.msg_id != entry.msg_id);
                if pending.is_empty() {
                    proto.pending_replies.remove(&rollback.sender_id);
                }
            }
        }
        if rollback.sender_reminder.is_some()
            && let Some(session) = proto.sessions.get_mut(&rollback.sender_id)
        {
            session.metadata.reminder = None;
        }
    }

    /// Persist protocol state sessions to disk.
    pub(crate) fn persist_protocol_state(
        &self,
        proto: &crate::daemon_protocol::DaemonState,
    ) -> anyhow::Result<()> {
        // Convert DaemonState sessions to the persisted Session format.
        //
        // IMPORTANT: every field on SessionMetadata must be explicitly copied
        // from SessionMeta here. A `..Default::default()` tail silently drops
        // any field not enumerated, so Effect::Persist writes nulls for those
        // fields, and a daemon restart loses them — which was the root cause
        // of the round-4 regression that zeroed model, effort, backend,
        // backend_session_id, project_description, last_metadata_update,
        // on_fire, and last_iteration_at on every persist.
        //
        // If you add a new field to SessionMetadata, add it here too. The
        // persist_protocol_state_round_trips_all_metadata_fields test in
        // state::tests exercises the full round-trip so a drop will surface
        // as a test failure, not a silent behaviour change.
        let sessions: HashMap<String, Session> = proto
            .sessions
            .iter()
            .map(|(k, entry)| {
                let m = &entry.metadata;
                let session = Session {
                    id: entry.id.clone(),
                    pane: entry.pane.clone(),
                    origin: match &entry.origin {
                        crate::daemon_protocol::Origin::Local => SessionOrigin::Local,
                        crate::daemon_protocol::Origin::Remote(d) => {
                            SessionOrigin::Remote(d.clone())
                        }
                        crate::daemon_protocol::Origin::Human(n) => SessionOrigin::Human(n.clone()),
                    },
                    registered_at: Utc::now(),
                    last_activity_at: Utc::now(),
                    metadata: SessionMetadata {
                        vim_mode: m.vim_mode,
                        project_dir: m.project_dir.clone(),
                        role: m.role.clone(),
                        networked: m.networked,
                        last_metadata_update: m
                            .last_metadata_update
                            .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0)),
                        backend_session_id: m.backend_session_id.clone(),
                        backend: m.backend.clone(),
                        opencode_binding: m.opencode_binding.clone(),
                        restart_generation: m.restart_generation,
                        backend_repair_reservation: m.backend_repair_reservation.clone(),
                        session_incarnation: m.session_incarnation,
                        project_description: m.project_description.clone(),
                        bulletin: m.bulletin.clone(),
                        worktree: m.worktree,
                        model: m.model.clone(),
                        effort: m.effort.clone(),
                        codex_home: m.codex_home.clone(),
                        reminder: m.reminder.clone(),
                        parent_session: m.parent_session.clone(),
                        idle_policy: m.idle_policy.clone(),
                        prompt: m.prompt.clone(),
                        iteration: m.iteration,
                        iteration_log: m.iteration_log.clone(),
                        last_iteration_at: m.last_iteration_at,
                        on_fire: m.on_fire.clone(),
                        worktree_present: m.worktree_present,
                        fresh_context_after_active_secs: m.fresh_context_after_active_secs,
                        active_context_accumulated_secs: m.active_context_accumulated_secs,
                        active_context_segment_started_at: m.active_context_segment_started_at,
                        active_context_restart_due: m.active_context_restart_due,
                    },
                };
                (k.clone(), session)
            })
            .collect();
        self.persist_sessions_from(
            &sessions,
            proto.incarnation_high_water,
            proto.lifecycle_leases.clone(),
        )
    }

    /// Clean up a git worktree directory if it has no uncommitted changes.
    /// Supports ouija-managed worktrees (both `~/.ouija/worktrees/` and legacy
    /// `<repo>/.ouija/worktrees/`) and Claude Code (`.claude/worktrees/`) paths.
    pub(crate) async fn cleanup_worktree_dir(dir: &str) {
        let dir_owned = dir.to_string();
        // Resolve the main repo via git. This handles every layout: the
        // legacy `<repo>/.ouija/worktrees/<name>`, the newer
        // `~/.ouija/worktrees/<repo>/<name>`, and Claude Code's
        // `<repo>/.claude/worktrees/<branch>`. String-matching the prefix
        // before `.ouija/worktrees/` incorrectly resolves the home-based
        // layout to `~` (not a repo), so always ask git.
        let dir_clone = dir.to_string();
        let repo = match tokio::task::spawn_blocking(move || {
            std::process::Command::new("git")
                .args(["-C", &dir_clone, "rev-parse", "--show-toplevel"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .await
        {
            Ok(Some(r)) if !r.is_empty() => r,
            _ => {
                tracing::info!("worktree {dir_owned} not inside a git repo, skipping cleanup");
                return;
            }
        };
        let dir_clone = dir_owned.clone();
        let has_changes = tokio::task::spawn_blocking(move || {
            std::process::Command::new("git")
                .args(["-C", &dir_clone, "status", "--porcelain"])
                .output()
                .map(|o| !o.stdout.is_empty())
                .unwrap_or(true)
        })
        .await
        .unwrap_or(true);
        if has_changes {
            tracing::info!("worktree {dir_owned} has uncommitted changes, keeping it");
            return;
        }
        tracing::info!("cleaning up worktree: {dir_owned}");
        let _ = tokio::task::spawn_blocking(move || {
            let _ = std::process::Command::new("git")
                .args(["-C", &repo, "worktree", "remove", &dir_owned, "--force"])
                .status();
        })
        .await;
    }

    /// Remove a worktree under its project-directory resource gate. Protocol
    /// state is sampled briefly; git/filesystem I/O never holds that lock.
    pub(crate) async fn cleanup_worktree_dir_if_unused(
        &self,
        owner: &crate::daemon_protocol::ResourceOwner,
        dir: &str,
    ) -> bool {
        let cleaned = self
            .with_owned_worktree_cleanup(owner, dir, || async move {
                Self::cleanup_worktree_dir(dir).await;
            })
            .await;
        if cleaned.is_none() {
            tracing::info!("skipping worktree cleanup for {dir}: other sessions still using it");
            return false;
        }
        true
    }

    /// Register a connected node by npub.
    ///
    /// Returns the existing node name if this npub is already connected.
    pub fn try_add_node(&self, npub: &str, name: &str) -> Result<(), DuplicateNode> {
        let mut connected = self
            .connected_npubs
            .lock()
            .expect("connected_npubs poisoned");
        if let Some(existing) = connected.get(npub) {
            return Err(DuplicateNode(existing.clone()));
        }
        connected.insert(npub.to_string(), name.to_string());
        Ok(())
    }

    /// Disconnect a remote node.
    ///
    /// Removes the node from the connected set, deauthorizes the peer in all
    /// transports (so future messages are rejected), removes all its remote
    /// sessions, and removes it from persisted connections.
    /// Returns the number of sessions removed.
    pub async fn disconnect_node(&self, daemon_id: &str) -> usize {
        // Remove from connected_npubs
        self.connected_npubs
            .lock()
            .expect("connected_npubs poisoned")
            .remove(daemon_id);

        // Deauthorize peer in all transports so messages are rejected
        for t in self.transports().await.values() {
            t.deauthorize_peer(daemon_id).await;
        }

        // Remove from nodes map
        self.nodes.write().await.remove(daemon_id);

        // Remove all remote sessions from this daemon
        let mut proto = self.protocol.write().await;
        let to_remove: Vec<String> = proto.sessions
            .iter()
            .filter(|(_, s)| matches!(&s.origin, crate::daemon_protocol::Origin::Remote(d) if d == daemon_id))
            .map(|(key, _)| key.clone())
            .collect();
        let count = to_remove.len();
        for key in &to_remove {
            proto.sessions.remove(key);
        }
        drop(proto);

        // Remove from persisted connections
        if let Ok(mut conns) = crate::persistence::load_connections(&self.config.data_dir) {
            conns.retain(|c| c.daemon_npub.as_deref() != Some(daemon_id));
            let data = serde_json::to_string(&conns).unwrap_or_default();
            let _ = std::fs::write(
                self.config.data_dir.join("connections.json"),
                data.as_bytes(),
            );
        }

        count
    }

    /// Enqueue an injection request for a pane, spawning its worker if needed.
    pub fn enqueue_inject(&self, req: crate::tmux::InjectRequest) {
        let pane_key = req.pane.clone();
        let mut queues = self.pane_queues.lock().expect("pane_queues poisoned");

        // Try existing channel; recover the request if the worker died.
        let req = if let Some(tx) = queues.get(&pane_key) {
            match tx.send(req) {
                Ok(()) => return,
                Err(e) => {
                    queues.remove(&pane_key);
                    e.0
                }
            }
        } else {
            req
        };

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(req).expect("fresh channel cannot be closed");
        tokio::spawn(crate::tmux::pane_inject_loop(rx));
        queues.insert(pane_key, tx);
    }

    /// Return a snapshot of all active transports.
    pub async fn transports(&self) -> TransportMap {
        self.transports.read().await.clone()
    }

    /// Look up a transport by name (e.g. "nostr").
    pub async fn transport_by_name(&self, name: &str) -> Option<Arc<dyn Transport>> {
        self.transports.read().await.get(name).cloned()
    }

    /// Register a transport, keyed by its `transport_name()`.
    pub async fn add_transport(&self, t: Arc<dyn Transport>) {
        self.transports
            .write()
            .await
            .insert(t.transport_name().to_string(), t);
    }

    /// Spawn an agent only while the exact owner and pane are current.
    pub async fn spawn_session_agent(
        self: &Arc<Self>,
        owner: &crate::daemon_protocol::ResourceOwner,
        pane: &str,
    ) {
        let protocol = self.protocol.read().await;
        if protocol
            .sessions
            .get(&owner.session_id)
            .is_none_or(|session| {
                session.owner() != *owner || session.pane.as_deref() != Some(pane)
            })
        {
            return;
        }

        let mut agents = self.session_agents.write().await;
        if let Some(old) = agents.remove(owner) {
            old.actor.stop(None);
        }
        let agent = crate::session_agent::SessionAgent {
            app_state: Arc::clone(self),
        };
        let args = crate::session_agent::SessionAgentArgs {
            owner: owner.clone(),
            pane: pane.to_string(),
        };
        match Actor::spawn(None, agent, args).await {
            Ok((actor_ref, _handle)) => {
                agents.insert(
                    owner.clone(),
                    OwnedSessionAgent {
                        owner: owner.clone(),
                        pane: pane.to_string(),
                        actor: actor_ref,
                    },
                );
                tracing::info!(
                    session = %owner.session_id,
                    incarnation = %owner.incarnation,
                    "spawned session agent"
                );
            }
            Err(e) => {
                tracing::error!(
                    session = %owner.session_id,
                    incarnation = %owner.incarnation,
                    "failed to spawn session agent: {e}"
                );
            }
        }
    }

    async fn stop_session_agent(&self, owner: &crate::daemon_protocol::ResourceOwner, pane: &str) {
        let mut agents = self.session_agents.write().await;
        if agents.get(owner).is_some_and(|agent| agent.pane == pane)
            && let Some(agent) = agents.remove(owner)
        {
            agent.actor.stop(None);
        }
    }

    async fn rename_session_agent(
        &self,
        old_owner: &crate::daemon_protocol::ResourceOwner,
        new_owner: &crate::daemon_protocol::ResourceOwner,
    ) {
        let protocol = self.protocol.read().await;
        if protocol
            .sessions
            .get(&new_owner.session_id)
            .is_none_or(|session| session.owner() != *new_owner)
        {
            return;
        }
        let mut agents = self.session_agents.write().await;
        let Some(mut agent) = agents.get(old_owner).cloned() else {
            return;
        };
        if protocol.sessions[&new_owner.session_id].pane.as_deref() != Some(agent.pane.as_str()) {
            return;
        }
        agents.remove(old_owner);
        let _ = agent.actor.cast(crate::session_agent::SessionMsg::Renamed {
            old_owner: old_owner.clone(),
            new_owner: new_owner.clone(),
        });
        agent.owner = new_owner.clone();
        if let Some(displaced) = agents.insert(new_owner.clone(), agent) {
            displaced.actor.stop(None);
        }
    }

    async fn current_session_agent(
        &self,
        session_id: &str,
    ) -> Option<ActorRef<crate::session_agent::SessionMsg>> {
        let protocol = self.protocol.read().await;
        let owner = protocol.sessions.get(session_id)?.owner();
        self.session_agents
            .read()
            .await
            .get(&owner)
            .map(|agent| agent.actor.clone())
    }

    async fn current_owned_session_agent(
        &self,
        owner: &crate::daemon_protocol::ResourceOwner,
    ) -> Option<ActorRef<crate::session_agent::SessionMsg>> {
        let protocol = self.protocol.read().await;
        if protocol
            .sessions
            .get(&owner.session_id)
            .is_none_or(|session| session.owner() != *owner)
        {
            return None;
        }
        self.session_agents
            .read()
            .await
            .get(owner)
            .map(|agent| agent.actor.clone())
    }

    /// Send a message to a session's agent (if it exists).
    pub async fn notify_agent(&self, session_id: &str, msg: crate::session_agent::SessionMsg) {
        let agent = self.current_session_agent(session_id).await;
        if let Some(agent) = agent {
            let _ = agent.cast(msg);
        }
    }

    /// Send a message only to the agent for the exact lifecycle owner.
    ///
    /// Returns false when the session was replaced after the caller resolved
    /// its owner or when that owner has no live agent.
    pub async fn notify_agent_owned(
        &self,
        owner: &crate::daemon_protocol::ResourceOwner,
        msg: crate::session_agent::SessionMsg,
    ) -> bool {
        self.current_owned_session_agent(owner)
            .await
            .is_some_and(|agent| agent.cast(msg).is_ok())
    }

    /// Query a session agent for its pending replies (RPC).
    pub async fn query_agent_pending_replies(
        &self,
        session_id: &str,
    ) -> Vec<crate::daemon_protocol::PendingReplyEntry> {
        if let Some(agent) = self.current_session_agent(session_id).await {
            ractor::call!(agent, crate::session_agent::SessionMsg::GetPendingReplies)
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// Drain the pending compact continuation from a session agent (RPC).
    /// Returns None if the agent has no pending continuation or the session has no agent.
    pub async fn drain_agent_compact_continuation(&self, session_id: &str) -> Option<String> {
        if let Some(agent) = self.current_session_agent(session_id).await {
            ractor::call!(
                agent,
                crate::session_agent::SessionMsg::DrainPendingCompactContinuation
            )
            .unwrap_or(None)
        } else {
            None
        }
    }

    /// Drain a compact continuation only from the exact lifecycle owner's agent.
    pub async fn drain_agent_compact_continuation_owned(
        &self,
        owner: &crate::daemon_protocol::ResourceOwner,
    ) -> Option<String> {
        if let Some(agent) = self.current_owned_session_agent(owner).await {
            ractor::call!(
                agent,
                crate::session_agent::SessionMsg::DrainPendingCompactContinuation
            )
            .unwrap_or(None)
        } else {
            None
        }
    }

    async fn set_owned_tmux_var(
        &self,
        owner: &crate::daemon_protocol::ResourceOwner,
        pane: &str,
        name: &str,
        value: &str,
    ) {
        if cfg!(test) {
            return;
        }
        let owner = owner.clone();
        let pane = pane.to_string();
        let name = name.to_string();
        let value = value.to_string();
        let pane_for_guard = pane.clone();
        let owner_for_guard = owner.clone();
        let _ = self
            .with_owned_pane_claim(&owner_for_guard, &pane_for_guard, move || async move {
                let pane_for_inspection = pane.clone();
                let inspection = match tokio::task::spawn_blocking(move || {
                    crate::tmux::inspect_managed_pane(&pane_for_inspection)
                })
                .await
                {
                    Ok(Ok(inspection)) => inspection,
                    Ok(Err(error)) => {
                        tracing::warn!(
                            %pane,
                            %name,
                            %error,
                            "failed to inspect owner before tmux variable write"
                        );
                        return;
                    }
                    Err(error) => {
                        tracing::warn!(
                            %pane,
                            %name,
                            %error,
                            "pane owner inspection task failed before tmux variable write"
                        );
                        return;
                    }
                };

                let observed_owner_is_referenced = match &inspection {
                    crate::tmux::ManagedPaneInspection::MarkerOwner(observed)
                        if !crate::tmux::physical_owner_matches(observed, &owner) =>
                    {
                        self.protocol
                            .read()
                            .await
                            .references_resource_owner(observed)
                    }
                    _ => false,
                };
                if !crate::tmux::pane_marker_write_is_authorized(
                    &inspection,
                    &owner,
                    observed_owner_is_referenced,
                ) {
                    tracing::warn!(
                        %pane,
                        %name,
                        ?inspection,
                        expected_owner = ?owner,
                        "refused tmux variable write for conflicting pane owner"
                    );
                    return;
                }

                let reclaimable_marker = match inspection {
                    crate::tmux::ManagedPaneInspection::MarkerOwner(observed)
                        if !crate::tmux::physical_owner_matches(&observed, &owner) =>
                    {
                        Some(observed)
                    }
                    _ => None,
                };
                let _ = tokio::task::spawn_blocking(move || {
                    let current = match crate::tmux::inspect_managed_pane(&pane) {
                        Ok(current) => current,
                        Err(error) => {
                            tracing::warn!(
                                %pane,
                                %name,
                                %error,
                                "failed to re-inspect owner before tmux variable write"
                            );
                            return;
                        }
                    };
                    let still_authorized = crate::tmux::pane_accepts_owner_marker(&current, &owner)
                        || matches!(
                            (&current, reclaimable_marker.as_ref()),
                            (
                                crate::tmux::ManagedPaneInspection::MarkerOwner(current),
                                Some(reclaimable),
                            ) if current == reclaimable
                        );
                    if !still_authorized {
                        tracing::warn!(
                            %pane,
                            %name,
                            ?current,
                            expected_owner = ?owner,
                            "pane owner changed before tmux variable write"
                        );
                        return;
                    }
                    if let Err(error) = crate::tmux_var::set(&pane, &name, &value) {
                        tracing::warn!(
                            %pane,
                            %name,
                            %error,
                            "failed to set owned tmux variable"
                        );
                    }
                })
                .await;
            })
            .await;
    }

    async fn wait_for_owned_tmux_owner(
        &self,
        owner: &crate::daemon_protocol::ResourceOwner,
        pane: &str,
    ) {
        if cfg!(test) {
            return;
        }
        let owner = owner.clone();
        let pane = pane.to_string();
        let pane_for_guard = pane.clone();
        let owner_for_guard = owner.clone();
        let _ = self
            .with_owned_pane_claim(&owner_for_guard, &pane_for_guard, move || async move {
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(error) = wait_for_tmux_owner_convergence(
                        &owner,
                        20,
                        || crate::tmux::inspect_managed_pane(&pane),
                        || std::thread::sleep(std::time::Duration::from_millis(25)),
                    ) {
                        tracing::warn!(
                            %pane,
                            %error,
                            "respawned pane owner did not converge"
                        );
                    }
                })
                .await;
            })
            .await;
    }

    async fn clear_owned_tmux_var(
        &self,
        owner: &crate::daemon_protocol::ResourceOwner,
        pane: &str,
        name: &str,
    ) {
        let owner = owner.clone();
        let pane = pane.to_string();
        let name = name.to_string();
        let pane_for_guard = pane.clone();
        let owner_for_guard = owner.clone();
        let _ = self
            .with_owned_pane_cleanup(&owner_for_guard, &pane_for_guard, move || async move {
                let _ = tokio::task::spawn_blocking(move || {
                    if crate::tmux::inspect_pane_owner(&pane)
                        .ok()
                        .flatten()
                        .is_some_and(|observed| {
                            crate::tmux::physical_owner_matches(&observed, &owner)
                        })
                    {
                        crate::tmux_var::clear(&pane, &name);
                    }
                })
                .await;
            })
            .await;
    }

    async fn enable_owned_auto_rename(
        &self,
        owner: &crate::daemon_protocol::ResourceOwner,
        pane: &str,
    ) {
        let owner = owner.clone();
        let pane = pane.to_string();
        let pane_for_guard = pane.clone();
        let owner_for_guard = owner.clone();
        let _ = self
            .with_owned_pane_cleanup(&owner_for_guard, &pane_for_guard, move || async move {
                let _ = tokio::task::spawn_blocking(move || {
                    if crate::tmux::inspect_pane_owner(&pane)
                        .ok()
                        .flatten()
                        .is_some_and(|observed| {
                            crate::tmux::physical_owner_matches(&observed, &owner)
                        })
                    {
                        crate::tmux::enable_automatic_rename(&pane);
                    }
                })
                .await;
            })
            .await;
    }

    pub(crate) async fn kill_owned_pane(
        &self,
        owner: &crate::daemon_protocol::ResourceOwner,
        pane: &str,
    ) {
        if cfg!(test) {
            return;
        }
        let owner = owner.clone();
        let pane = pane.to_string();
        let pane_for_guard = pane.clone();
        let owner_for_guard = owner.clone();
        let _ = self
            .with_owned_pane_cleanup(&owner_for_guard, &pane_for_guard, move || async move {
                let _ = tokio::task::spawn_blocking(move || {
                    if crate::tmux::inspect_pane_owner(&pane)
                        .ok()
                        .flatten()
                        .is_some_and(|observed| {
                            crate::tmux::physical_owner_matches(&observed, &owner)
                        })
                    {
                        let _ = std::process::Command::new("tmux")
                            .args(["kill-pane", "-t", &pane])
                            .status();
                    }
                })
                .await;
            })
            .await;
    }

    /// Atomically set a pending compact continuation only if the slot is empty (RPC).
    /// Returns true if acquired, false if a continuation is already pending or the
    /// session has no agent. Used to reject concurrent compact requests that would
    /// otherwise silently overwrite each other's continuation.
    pub async fn try_set_pending_compact_continuation(
        &self,
        session_id: &str,
        text: String,
    ) -> bool {
        if let Some(agent) = self.current_session_agent(session_id).await {
            ractor::call!(
                agent,
                crate::session_agent::SessionMsg::TrySetPendingCompactContinuation,
                text
            )
            .unwrap_or(false)
        } else {
            false
        }
    }

    /// Clear pending replies targeting removed sessions from protocol state.
    pub(crate) async fn clear_orphaned_pending_replies(&self, removed_ids: &[String]) {
        let mut proto = self.protocol.write().await;
        proto.clear_orphaned_replies(removed_ids);
    }

    /// If local session count exceeds `max_local_sessions`, return idle/stale
    /// sessions that can be closed to bring the count back to the limit.
    /// Only sessions with stale metadata are eligible — active sessions are never killed.
    pub async fn collect_excess_idle_sessions(
        &self,
    ) -> Vec<(crate::daemon_protocol::ResourceOwner, String)> {
        let max = self.settings.read().await.max_local_sessions as usize;
        if max == 0 {
            return vec![];
        }
        let proto = self.protocol.read().await;
        let local: Vec<_> = proto
            .sessions
            .values()
            .filter(|s| matches!(s.origin, crate::daemon_protocol::Origin::Local))
            .collect();
        if local.len() <= max {
            return vec![];
        }
        let excess = local.len() - max;
        // Only consider stale sessions for eviction
        let mut stale: Vec<_> = local
            .into_iter()
            .filter(|s| s.metadata.is_stale())
            .collect();
        // Sort by last activity (oldest first)
        stale.sort_by_key(|s| s.metadata.last_metadata_update.unwrap_or(s.registered_at));
        stale
            .iter()
            .take(excess)
            .filter_map(|s| Some((s.owner(), s.pane.clone()?)))
            .collect()
    }

    /// Sweep worktree presence for local sessions with project_dir.
    ///
    /// Snapshot (id, project_dir) pairs, deduplicate dirs, check existence
    /// via spawn_blocking, then dispatch MarkWorktreePresence event.
    pub async fn sweep_worktree_presence(self: &Arc<Self>) {
        // Backoff gate: if a prior sweep timed out and the cooldown is still
        // active, skip without acquiring the dedup flag (the orphan blocking
        // thread that triggered the timeout still holds it). Once the window
        // has elapsed, force-clear both the backoff and the dedup flag — the
        // orphan thread is presumed permanently hung; the next sweep accepts
        // the risk of accumulating one more orphan to keep the feature alive.
        {
            let mut backoff = self.sweep_backoff_until.lock().unwrap();
            if let Some(until) = *backoff {
                if std::time::Instant::now() < until {
                    tracing::debug!(
                        "worktree sweep in backoff window after recent timeout, skipping"
                    );
                    return;
                }
                *backoff = None;
                self.sweep_in_progress
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let sessions_with_dirs: Vec<(crate::daemon_protocol::ResourceOwner, String)> = {
            let proto = self.protocol.read().await;
            proto
                .sessions
                .values()
                .filter(|s| {
                    matches!(s.origin, crate::daemon_protocol::Origin::Local)
                        && s.metadata.project_dir.is_some()
                })
                .filter_map(|s| Some((s.owner(), s.metadata.project_dir.clone()?)))
                .collect()
        };
        if sessions_with_dirs.is_empty() {
            // Do NOT clear sweep_in_progress here: this caller never claimed the
            // flag (the swap(true) acquire below comes after this check), so it
            // has no business releasing it. Clearing would clobber a concurrent
            // sweep's claim and let a subsequent sweep run in parallel.
            return;
        }
        // Dedup: skip if a prior sweep is still running
        if self
            .sweep_in_progress
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::debug!("worktree sweep already in progress, skipping");
            return;
        }
        // Deduplicate project dirs to avoid N² stat calls
        let unique_dirs: Vec<String> = {
            let mut dirs: Vec<String> = sessions_with_dirs.iter().map(|(_, d)| d.clone()).collect();
            dirs.sort();
            dirs.dedup();
            dirs
        };
        // Check which dirs exist on disk
        // Only mark presence on clean ENOENT success/failure; other errors skip the session
        // Wrap in timeout to prevent hung NFS/FUSE mounts from blocking the reaper
        const SWEEP_TIMEOUT_SECS: u64 = 30;
        // Backoff after a timeout: orphan blocking threads keep running on the
        // hung FS until the mount unhangs (spawn_blocking is not cancellable).
        // The backoff caps orphan accumulation rate at 1 per window instead of
        // 1 per heartbeat (~5s).
        const SWEEP_BACKOFF_SECS: u64 = 300;
        let unique_dirs = unique_dirs.clone();
        let presence_map: std::collections::HashMap<String, bool> = match tokio::time::timeout(
            std::time::Duration::from_secs(SWEEP_TIMEOUT_SECS),
            tokio::task::spawn_blocking(move || {
                let mut map = std::collections::HashMap::new();
                for dir in unique_dirs {
                    let presence = match std::fs::metadata(&dir) {
                        Ok(m) if m.is_dir() => Some(true),
                        Ok(_) => {
                            tracing::debug!("worktree path exists but is not a directory: {}", dir);
                            None // exists but not a directory - skip this session
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
                        Err(e) => {
                            tracing::debug!("worktree stat failed for {}: {}", dir, e);
                            None // skip this session
                        }
                    };
                    if let Some(p) = presence {
                        map.insert(dir, p);
                    }
                }
                map
            }),
        )
        .await
        {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => {
                tracing::warn!("worktree sweep spawn_blocking failed: {e}");
                self.sweep_in_progress
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            Err(_) => {
                tracing::warn!(
                    "worktree sweep timed out after {SWEEP_TIMEOUT_SECS}s - possible hung mount; \
                     backing off for {SWEEP_BACKOFF_SECS}s"
                );
                // Do NOT clear sweep_in_progress: the orphan blocking thread is
                // still running on the hung FS and conceptually still owns the
                // flag. Combined with the backoff_until gate at entry, this caps
                // orphan-thread accumulation at 1 per backoff window instead of
                // 1 per reaper heartbeat.
                *self.sweep_backoff_until.lock().unwrap() = Some(
                    std::time::Instant::now() + std::time::Duration::from_secs(SWEEP_BACKOFF_SECS),
                );
                return;
            }
        };
        // Only update sessions whose dirs were successfully checked
        let updates: Vec<(crate::daemon_protocol::ResourceOwner, String, bool)> =
            sessions_with_dirs
                .into_iter()
                .filter_map(|(owner, dir)| presence_map.get(&dir).map(|p| (owner, dir.clone(), *p)))
                .collect();
        if !updates.is_empty() {
            let _ = self
                .apply_and_execute(crate::daemon_protocol::Event::MarkWorktreePresence { updates })
                .await;
        }
        // Always reset the dedup flag, even on early return or timeout
        self.sweep_in_progress
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    fn persist_sessions_from(
        &self,
        sessions: &HashMap<String, Session>,
        incarnation_high_water: crate::daemon_protocol::SessionIncarnation,
        lifecycle_leases: std::collections::BTreeMap<
            String,
            crate::daemon_protocol::LifecycleLease,
        >,
    ) -> anyhow::Result<()> {
        let persisted: Vec<_> = sessions
            .values()
            .filter_map(crate::persistence::PersistedSession::from_session)
            .collect();
        let snapshot = crate::persistence::PersistedLifecycleState::new(
            persisted,
            incarnation_high_water,
            lifecycle_leases,
        );
        crate::persistence::save_sessions(&self.config.data_dir, &snapshot)
    }

    pub async fn cached_assistant_panes(&self) -> Vec<crate::tmux::TmuxPane> {
        self.cached_assistant_panes.read().await.clone()
    }

    /// Return a current snapshot of tmux panes running a known assistant.
    ///
    /// Production path: runs a fresh `find_assistant_panes` so the caller sees
    /// panes that appeared since the last periodic scan. This is what the
    /// auto-provision branch in `backend_session_ready` needs — the very
    /// first readiness callback for a brand-new pane fires in the
    /// milliseconds after opencode startup, well before the periodic
    /// scanner's next tick.
    ///
    /// Test path: short-circuits to `cached_assistant_panes`, which
    /// `new_for_test()` initialises empty but tests can seed with
    /// `*state.cached_assistant_panes.write().await = vec![...]`. This keeps
    /// unit tests off the real tmux server, matching the `cfg!(test)` pattern
    /// documented in CLAUDE.md for tmux-side primitives.
    pub async fn list_assistant_panes(&self) -> Vec<crate::tmux::TmuxPane> {
        if cfg!(test) {
            return self.cached_assistant_panes().await;
        }
        let names: Vec<String> = self.backends.all_process_names();
        tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
            crate::tmux::find_assistant_panes(&refs).unwrap_or_default()
        })
        .await
        .unwrap_or_default()
    }

    /// Scan tmux for assistant panes, update cache, and auto-register unregistered ones.
    pub async fn scan_and_autoregister_panes(self: &Arc<Self>) {
        let panes = self.list_assistant_panes().await;

        // Update cache
        *self.cached_assistant_panes.write().await = panes.clone();

        let auto_register = self.settings.read().await.auto_register;
        if !auto_register {
            return;
        }

        // Build lookup tables from current sessions (single lock acquisition).
        // These are updated within the loop so subsequent panes see prior registrations.
        let (mut registered_panes, mut id_to_pane) = {
            let proto = self.protocol.read().await;
            let registered: std::collections::HashSet<String> = proto
                .sessions
                .values()
                .filter(|s| matches!(s.origin, crate::daemon_protocol::Origin::Local))
                .filter_map(|s| s.pane.clone())
                .collect();
            let id_to_pane: std::collections::HashMap<String, Option<String>> = proto
                .sessions
                .iter()
                .map(|(id, s)| (id.clone(), s.pane.clone()))
                .collect();
            (registered, id_to_pane)
        };

        for pane in &panes {
            if registered_panes.contains(&pane.pane_id) {
                continue;
            }

            let now = std::time::Instant::now();
            let is_explicitly_removing = {
                let mut suppressed = self
                    .autoregister_suppressed_panes
                    .lock()
                    .expect("autoregister suppression mutex poisoned");
                suppressed.retain(|_, until| *until > now);
                suppressed.contains_key(&pane.pane_id)
            };
            if is_explicitly_removing {
                continue;
            }

            let inspection = if cfg!(test) {
                Ok(crate::tmux::ManagedPaneInspection::Unmanaged)
            } else {
                let pane_id = pane.pane_id.clone();
                tokio::task::spawn_blocking(move || crate::tmux::inspect_managed_pane(&pane_id))
                    .await
                    .unwrap_or_else(|error| Err(error.into()))
            };
            let expected_orphaned_marker_owner = match &inspection {
                Ok(crate::tmux::ManagedPaneInspection::MarkerOwner(owner)) => Some(owner.clone()),
                _ => None,
            };
            let marker_owner_is_referenced =
                if let Some(owner) = expected_orphaned_marker_owner.as_ref() {
                    self.protocol.read().await.references_resource_owner(owner)
                } else {
                    false
                };
            match inspection {
                Ok(inspection)
                    if autoregister_accepts_pane_inspection(
                        &inspection,
                        marker_owner_is_referenced,
                    ) => {}
                Ok(inspection) => {
                    tracing::warn!(
                        pane = %pane.pane_id,
                        ?inspection,
                        "skipping auto-registration for pane with non-reclaimable owner evidence"
                    );
                    continue;
                }
                Err(error) => {
                    tracing::warn!(
                        pane = %pane.pane_id,
                        %error,
                        "skipping auto-registration after pane owner inspection failed"
                    );
                    continue;
                }
            }

            // An @ouija_id marker is not durable ownership evidence. A live
            // daemon session is already covered by registered_panes above;
            // otherwise this is a legacy orphan and must be allowed to claim
            // a normal ID again.

            let Some(ref path) = pane.pane_current_path else {
                continue;
            };

            let project_root = resolve_project_root(path);

            // Same defense as the session-start hook: never auto-register a pane
            // whose resolved root is $HOME. Without this, a home-cwd pane the
            // hook already refused could still be grabbed generically here as
            // "daniel-N" and leak past task cleanup (#1483).
            if is_home_project_root(project_root) {
                continue;
            }

            let basename = std::path::Path::new(project_root)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");

            let base_id = sanitize_session_id(basename);

            if base_id.is_empty() {
                continue;
            }

            // Resolve name conflicts using pre-computed map (no lock re-acquisition).
            // Shared with hooks::session_start_inner via resolve_unique_session_id.
            let id = resolve_unique_session_id(&id_to_pane, &base_id, Some(pane.pane_id.as_str()));

            let proto_meta = crate::daemon_protocol::SessionMeta {
                project_dir: Some(project_root.to_string()),
                role: Some(format!("working on {basename}")),
                ..Default::default()
            };

            tracing::info!("auto-registering pane {} as '{id}'", pane.pane_id);
            self.apply_and_execute(crate::daemon_protocol::Event::RegisterIfPaneUnbound {
                id: id.clone(),
                pane: pane.pane_id.clone(),
                expected_backend_session_id: None,
                expected_orphaned_marker_owner,
                metadata: proto_meta,
            })
            .await;

            // Update maps so the next pane in this loop sees this registration.
            // Without this, two panes in the same directory both claim the base
            // name and the second overwrites the first.
            id_to_pane.insert(id.clone(), Some(pane.pane_id.clone()));
            registered_panes.insert(pane.pane_id.clone());
        }
    }

    /// Whether we should reciprocate a session list to this node.
    ///
    /// Debounced at 30s to prevent infinite ping-pong over Nostr.
    pub fn should_reciprocate(&self, daemon_id: &str) -> bool {
        let mut map = self
            .last_reciprocated
            .lock()
            .expect("last_reciprocated poisoned");
        let now = std::time::Instant::now();
        if let Some(last) = map.get(daemon_id) {
            if now.duration_since(*last) < std::time::Duration::from_secs(RECIPROCATE_DEBOUNCE_SECS)
            {
                return false;
            }
        }
        map.insert(daemon_id.to_string(), now);
        true
    }

    /// Register a oneshot sender for a pending remote command result.
    #[allow(dead_code)]
    pub fn register_pending_command(
        &self,
        command: String,
    ) -> tokio::sync::oneshot::Receiver<String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending_commands
            .lock()
            .expect("pending_commands poisoned")
            .push((command, tx));
        rx
    }

    /// Deliver a command result to the first matching pending sender.
    pub async fn deliver_command_result(&self, _daemon_id: &str, command: &str, result: &str) {
        let tx = {
            let mut pending = self
                .pending_commands
                .lock()
                .expect("pending_commands poisoned");
            pending
                .iter()
                .position(|(cmd, _)| cmd == command)
                .map(|idx| pending.remove(idx).1)
        };
        if let Some(tx) = tx {
            let _ = tx.send(result.to_string());
        }
    }

    pub async fn local_session_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let proto = self.protocol.read().await;
        let mut entries: Vec<(&str, bool, Option<&str>, Option<&str>)> = proto
            .sessions
            .values()
            .filter(|s| matches!(s.origin, crate::daemon_protocol::Origin::Local))
            .map(|s| {
                (
                    s.id.as_str(),
                    s.metadata.networked,
                    s.metadata.role.as_deref(),
                    s.metadata.bulletin.as_deref(),
                )
            })
            .collect();
        entries.sort_by_key(|(id, _, _, _)| *id);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        entries.hash(&mut hasher);
        hasher.finish()
    }

    /// Each new pane is registered with a name derived from its working
    /// directory basename (e.g. `/Users/me/code/api` becomes `api`).
    /// Returns `(session_id, pane_id)` pairs for newly registered sessions.
    pub async fn add_task(&self, task: ScheduledTask) {
        let mut tasks = self.scheduled_tasks.write().await;
        tasks.insert(task.id.clone(), task);
        self.persist_tasks_from(&tasks);
    }

    pub async fn remove_task(&self, id: &str) -> Option<ScheduledTask> {
        let mut tasks = self.scheduled_tasks.write().await;
        let removed = tasks.remove(id);
        if removed.is_some() {
            self.persist_tasks_from(&tasks);
        }
        removed
    }

    pub async fn update_task(&self, id: &str, f: impl FnOnce(&mut ScheduledTask)) {
        let mut tasks = self.scheduled_tasks.write().await;
        if let Some(task) = tasks.get_mut(id) {
            f(task);
            self.persist_tasks_from(&tasks);
        }
    }

    pub async fn log_task_run(&self, run: TaskRun) {
        {
            let _guard = self
                .task_run_log_lock
                .lock()
                .expect("task_run_log_lock poisoned");
            if let Err(e) = crate::persistence::append_task_run(&self.config.data_dir, &run) {
                tracing::warn!("failed to append task run: {e}");
            }
        }
        let mut runs = self.task_runs.write().await;
        if runs.len() >= MAX_TASK_RUNS {
            runs.pop_front();
        }
        runs.push_back(run);
    }

    pub fn persist_tasks_from(&self, tasks: &HashMap<String, ScheduledTask>) {
        if let Err(e) = crate::persistence::save_tasks(&self.config.data_dir, tasks) {
            tracing::warn!("failed to persist tasks: {e}");
        }
    }

    pub async fn log_message(
        &self,
        from: String,
        to: String,
        message: String,
        delivered: bool,
        method: &str,
    ) {
        let ts = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let line = serde_json::json!({
            "ts": ts,
            "from": from,
            "to": to,
            "method": method,
            "delivered": delivered,
        });
        {
            let _guard = self.log_file_lock.lock().expect("log_file_lock poisoned");
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.log_file)
            {
                use std::io::Write;
                let _ = writeln!(f, "{}", line);
            }
        }

        let entry = LogEntry {
            timestamp: Utc::now(),
            from,
            to,
            message,
            delivered,
        };
        let mut log = self.message_log.write().await;
        if log.len() >= MAX_LOG {
            log.pop_front();
        }
        log.push_back(entry);
    }

    /// Port where opencode serve is expected to run.
    /// Convention: daemon_port + 320.
    pub fn opencode_serve_port(&self) -> u16 {
        self.config.port + 320
    }
}

fn rewrite_send_delivery_failure(
    effects: Vec<crate::daemon_protocol::Effect>,
    reason: &str,
) -> Vec<crate::daemon_protocol::Effect> {
    effects
        .into_iter()
        .map(|effect| match effect {
            crate::daemon_protocol::Effect::SendDelivered { from, to, .. } => {
                crate::daemon_protocol::Effect::SendFailed {
                    from,
                    to,
                    reason: reason.to_string(),
                    renamed_to: None,
                }
            }
            crate::daemon_protocol::Effect::LogMessage {
                from,
                to,
                message,
                delivered: true,
                transport,
            } => crate::daemon_protocol::Effect::LogMessage {
                from,
                to,
                message,
                delivered: false,
                transport,
            },
            other => other,
        })
        .collect()
}

struct FailedEffectSendRollback {
    sender_id: String,
    pending_reply_before_send: Option<crate::daemon_protocol::PendingReplyEntry>,
    pending_reply_after_send: Option<Option<crate::daemon_protocol::PendingReplyEntry>>,
    sender_reminder: Option<Option<String>>,
    sender_reminder_after_send: Option<Option<String>>,
    sender_state_reserved: bool,
    done: bool,
}

impl FailedEffectSendRollback {
    fn capture_for_event(
        proto: &crate::daemon_protocol::DaemonState,
        event: &crate::daemon_protocol::Event,
    ) -> Option<Self> {
        let crate::daemon_protocol::Event::Send {
            from,
            responds_to,
            done,
            ..
        } = event
        else {
            return None;
        };

        let pending_reply_before_send = responds_to.and_then(|msg_id| {
            proto
                .pending_replies
                .get(from)
                .and_then(|pending| pending.iter().find(|entry| entry.msg_id == msg_id).cloned())
        });
        Some(Self {
            sender_id: from.clone(),
            pending_reply_before_send,
            pending_reply_after_send: None,
            sender_reminder: done.then(|| {
                proto
                    .sessions
                    .get(from)
                    .and_then(|session| session.metadata.reminder.clone())
            }),
            sender_reminder_after_send: None,
            sender_state_reserved: false,
            done: *done,
        })
    }

    fn capture_after_send(&mut self, proto: &crate::daemon_protocol::DaemonState) {
        if let Some(before) = &self.pending_reply_before_send {
            self.pending_reply_after_send = Some(
                proto
                    .pending_replies
                    .get(&self.sender_id)
                    .and_then(|pending| {
                        pending
                            .iter()
                            .find(|entry| entry.msg_id == before.msg_id)
                            .cloned()
                    }),
            );
        }
        if self.done {
            self.sender_reminder_after_send = Some(
                proto
                    .sessions
                    .get(&self.sender_id)
                    .and_then(|session| session.metadata.reminder.clone()),
            );
        }
    }

    fn reserve_sender_state_after_send(&mut self, proto: &mut crate::daemon_protocol::DaemonState) {
        if !self.done {
            return;
        }

        if let Some(entry) = self.pending_reply_before_send.clone()
            && self.pending_reply_after_send == Some(None)
        {
            proto
                .pending_replies
                .entry(self.sender_id.clone())
                .or_default()
                .push(entry);
            self.sender_state_reserved = true;
        }

        if self.sender_reminder.is_some()
            && self.sender_reminder_after_send == Some(None)
            && let Some(session) = proto.sessions.get_mut(&self.sender_id)
        {
            session.metadata.reminder = self.sender_reminder.clone().flatten();
            self.sender_state_reserved = true;
        }
    }

    fn sender_state_reserved(&self) -> bool {
        self.sender_state_reserved
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::daemon_protocol::Origin;

    #[test]
    fn tmux_owner_wait_converges_after_respawn() {
        let incumbent = crate::daemon_protocol::ResourceOwner {
            session_id: "worker".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(41),
        };
        let replacement = crate::daemon_protocol::ResourceOwner {
            session_id: "worker".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(42),
        };
        let mut observations = std::collections::VecDeque::from([
            crate::tmux::ManagedPaneInspection::ProcessOwner(incumbent.clone()),
            crate::tmux::ManagedPaneInspection::ProcessOwner(incumbent),
            crate::tmux::ManagedPaneInspection::ProcessOwner(replacement.clone()),
        ]);
        let mut inspection_count = 0;
        let mut wait_count = 0;

        wait_for_tmux_owner_convergence(
            &replacement,
            20,
            || {
                inspection_count += 1;
                Ok(observations
                    .pop_front()
                    .expect("test must provide an inspection for each attempt"))
            },
            || wait_count += 1,
        )
        .expect("replacement owner should become writable");

        assert_eq!(inspection_count, 3);
        assert_eq!(wait_count, 2);
    }

    #[test]
    fn tmux_owner_wait_rejects_persistent_other_owner() {
        let incumbent = crate::daemon_protocol::ResourceOwner {
            session_id: "worker".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(41),
        };
        let replacement = crate::daemon_protocol::ResourceOwner {
            session_id: "worker".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(42),
        };
        let mut inspection_count = 0;
        let mut wait_count = 0;

        let error = wait_for_tmux_owner_convergence(
            &replacement,
            3,
            || {
                inspection_count += 1;
                Ok(crate::tmux::ManagedPaneInspection::ProcessOwner(
                    incumbent.clone(),
                ))
            },
            || wait_count += 1,
        )
        .expect_err("a persistent conflicting owner must fail closed");

        assert!(error.to_string().contains("expected 42"), "{error}");
        assert_eq!(inspection_count, 3);
        assert_eq!(wait_count, 2);
    }

    #[test]
    fn autoregister_skips_complete_process_owners_but_allows_marker_orphans() {
        let owner = crate::daemon_protocol::ResourceOwner {
            session_id: "old".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(42),
        };

        assert!(!autoregister_accepts_pane_inspection(
            &crate::tmux::ManagedPaneInspection::ProcessOwner(owner.clone()),
            false,
        ));
        assert!(autoregister_accepts_pane_inspection(
            &crate::tmux::ManagedPaneInspection::MarkerOwner(owner.clone()),
            false,
        ));
        assert!(!autoregister_accepts_pane_inspection(
            &crate::tmux::ManagedPaneInspection::MarkerOwner(owner),
            true,
        ));
        assert!(autoregister_accepts_pane_inspection(
            &crate::tmux::ManagedPaneInspection::Unmanaged,
            false,
        ));
        assert!(!autoregister_accepts_pane_inspection(
            &crate::tmux::ManagedPaneInspection::Missing,
            false,
        ));
    }

    #[tokio::test]
    async fn queued_old_pane_cleanup_skips_same_pane_replacement() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%99".into()),
                metadata: Default::default(),
            })
            .await;
        let old_owner = state.protocol.read().await.sessions["worker"].owner();
        let gate = state.pane_resource_gate("%99");
        let held = gate.lock().await;
        let touched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let queued = std::sync::Arc::new(tokio::sync::Notify::new());
        let cleanup_state = state.clone();
        let cleanup_owner = old_owner.clone();
        let cleanup_touched = touched.clone();
        let cleanup_queued = queued.clone();
        let cleanup = tokio::spawn(async move {
            cleanup_queued.notify_one();
            cleanup_state
                .with_owned_pane_cleanup(&cleanup_owner, "%99", || async move {
                    cleanup_touched.store(true, std::sync::atomic::Ordering::SeqCst);
                })
                .await
        });
        queued.notified().await;

        {
            let mut protocol = state.protocol.write().await;
            protocol.apply(crate::daemon_protocol::Event::Remove {
                id: "worker".into(),
                keep_worktree: true,
            });
            protocol.apply(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%99".into()),
                metadata: Default::default(),
            });
            assert_ne!(protocol.sessions["worker"].owner(), old_owner);
        }
        drop(held);

        assert_eq!(cleanup.await.expect("cleanup task failed"), None);
        assert!(
            !touched.load(std::sync::atomic::Ordering::SeqCst),
            "stale cleanup must not touch a pane now claimed by a replacement"
        );
    }

    #[tokio::test]
    async fn queued_old_backend_cleanup_skips_replacement_binding() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_shared".into()),
                    ..Default::default()
                },
            })
            .await;
        let old_owner = state.protocol.read().await.sessions["worker"].owner();
        let gate = state.backend_resource_gate("ses_shared");
        let held = gate.lock().await;
        let touched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let queued = std::sync::Arc::new(tokio::sync::Notify::new());
        let cleanup_state = state.clone();
        let cleanup_owner = old_owner.clone();
        let cleanup_touched = touched.clone();
        let cleanup_queued = queued.clone();
        let cleanup = tokio::spawn(async move {
            cleanup_queued.notify_one();
            cleanup_state
                .with_owned_backend_cleanup(&cleanup_owner, "ses_shared", || async move {
                    cleanup_touched.store(true, std::sync::atomic::Ordering::SeqCst);
                })
                .await
        });
        queued.notified().await;

        {
            let mut protocol = state.protocol.write().await;
            protocol.apply(crate::daemon_protocol::Event::Remove {
                id: "worker".into(),
                keep_worktree: true,
            });
            protocol.apply(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_shared".into()),
                    ..Default::default()
                },
            });
            assert_ne!(protocol.sessions["worker"].owner(), old_owner);
        }
        drop(held);

        assert_eq!(cleanup.await.expect("cleanup task failed"), None);
        assert!(
            !touched.load(std::sync::atomic::Ordering::SeqCst),
            "stale cleanup must not delete a backend session rebound to a replacement"
        );
    }

    #[tokio::test]
    async fn active_pane_cleanup_serializes_replacement_transition() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%99".into()),
                metadata: Default::default(),
            })
            .await;
        let old_owner = state.protocol.read().await.sessions["worker"].owner();
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let cleanup_state = state.clone();
        let cleanup_owner = old_owner.clone();
        let cleanup_entered = entered.clone();
        let cleanup = tokio::spawn(async move {
            cleanup_state
                .with_owned_pane_cleanup(&cleanup_owner, "%99", || async move {
                    cleanup_entered.notify_one();
                    let _ = release_rx.await;
                })
                .await
        });
        entered.notified().await;

        let attempted = std::sync::Arc::new(tokio::sync::Notify::new());
        let replacement_state = state.clone();
        let replacement_attempted = attempted.clone();
        let replacement = tokio::spawn(async move {
            replacement_attempted.notify_one();
            replacement_state
                .apply_and_execute(crate::daemon_protocol::Event::Remove {
                    id: "worker".into(),
                    keep_worktree: true,
                })
                .await;
            replacement_state
                .apply_and_execute(crate::daemon_protocol::Event::Register {
                    id: "worker".into(),
                    pane: Some("%99".into()),
                    metadata: Default::default(),
                })
                .await;
            replacement_state.protocol.read().await.sessions["worker"].owner()
        });
        attempted.notified().await;
        assert_eq!(
            state.protocol.read().await.sessions["worker"].owner(),
            old_owner,
            "replacement must not publish ownership while old cleanup holds the pane gate"
        );

        release_tx.send(()).expect("release cleanup");
        assert_eq!(cleanup.await.expect("cleanup task failed"), Some(()));
        assert_ne!(
            replacement.await.expect("replacement task failed"),
            old_owner
        );
    }

    #[tokio::test]
    async fn active_backend_cleanup_serializes_identity_bind() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    session_start_credential: Some("proof".into()),
                    ..Default::default()
                },
            })
            .await;
        let owner = state.protocol.read().await.sessions["worker"].owner();
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let cleanup_state = state.clone();
        let cleanup_owner = owner.clone();
        let cleanup_entered = entered.clone();
        let cleanup = tokio::spawn(async move {
            cleanup_state
                .with_owned_backend_cleanup(&cleanup_owner, "ses_shared", || async move {
                    cleanup_entered.notify_one();
                    let _ = release_rx.await;
                })
                .await
        });
        entered.notified().await;

        let attempted = std::sync::Arc::new(tokio::sync::Notify::new());
        let bind_state = state.clone();
        let bind_attempted = attempted.clone();
        let bind = tokio::spawn(async move {
            bind_attempted.notify_one();
            bind_state
                .bind_backend_identity(
                    "worker",
                    &crate::backend::BackendSessionIdentity {
                        backend: "opencode".into(),
                        session_id: "ses_shared".into(),
                    },
                    Some("proof"),
                    Some(owner.incarnation),
                )
                .await
        });
        attempted.notified().await;
        assert!(
            state.protocol.read().await.sessions["worker"]
                .metadata
                .backend_session_id
                .is_none(),
            "backend bind must not publish while cleanup holds the backend gate"
        );

        release_tx.send(()).expect("release backend cleanup");
        assert_eq!(cleanup.await.expect("cleanup task failed"), Some(()));
        assert!(matches!(
            bind.await.expect("bind task failed").outcome,
            crate::daemon_protocol::BackendIdentityBindOutcome::Bound { .. }
        ));
    }

    #[tokio::test]
    async fn renamed_session_retains_physical_pane_cleanup_authority() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "before".into(),
                pane: Some("%99".into()),
                metadata: Default::default(),
            })
            .await;
        let immutable_process_owner = state.protocol.read().await.sessions["before"].owner();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Rename {
                old_id: "before".into(),
                new_id: "after".into(),
            })
            .await;
        let renamed_owner = state.protocol.read().await.sessions["after"].owner();
        let touched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cleanup_touched = touched.clone();

        let result = state
            .with_owned_pane_cleanup(&renamed_owner, "%99", || async move {
                cleanup_touched.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .await;

        assert_eq!(result, Some(()));
        assert!(touched.load(std::sync::atomic::Ordering::SeqCst));
        assert!(crate::tmux::physical_owner_matches(
            &immutable_process_owner,
            &renamed_owner
        ));
    }

    #[tokio::test]
    async fn queued_old_worktree_cleanup_skips_same_directory_replacement() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/tmp/shared-worktree".into()),
                    ..Default::default()
                },
            })
            .await;
        let old_owner = state.protocol.read().await.sessions["worker"].owner();
        let gate = state.project_dir_resource_gate("/tmp/shared-worktree");
        let held = gate.lock().await;
        let touched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let queued = std::sync::Arc::new(tokio::sync::Notify::new());
        let cleanup_state = state.clone();
        let cleanup_owner = old_owner.clone();
        let cleanup_touched = touched.clone();
        let cleanup_queued = queued.clone();
        let cleanup = tokio::spawn(async move {
            cleanup_queued.notify_one();
            cleanup_state
                .with_owned_worktree_cleanup(
                    &cleanup_owner,
                    "/tmp/shared-worktree",
                    || async move {
                        cleanup_touched.store(true, std::sync::atomic::Ordering::SeqCst);
                    },
                )
                .await
        });
        queued.notified().await;

        {
            let mut protocol = state.protocol.write().await;
            protocol.apply(crate::daemon_protocol::Event::Remove {
                id: "worker".into(),
                keep_worktree: true,
            });
            protocol.apply(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%100".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/tmp/./shared-worktree".into()),
                    ..Default::default()
                },
            });
            assert_ne!(protocol.sessions["worker"].owner(), old_owner);
        }
        drop(held);

        assert_eq!(cleanup.await.expect("cleanup task failed"), None);
        assert!(
            !touched.load(std::sync::atomic::Ordering::SeqCst),
            "stale cleanup must not delete a directory claimed by a replacement"
        );
    }

    #[tokio::test]
    async fn replacement_project_dir_claim_waits_for_inflight_cleanup() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "old".into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/tmp/serialized-worktree".into()),
                    ..Default::default()
                },
            })
            .await;
        let old_owner = state.protocol.read().await.sessions["old"].owner();
        state
            .apply_and_execute(crate::daemon_protocol::Event::RemoveOwned {
                owner: old_owner.clone(),
                expected_pane: Some("%99".into()),
                keep_worktree: true,
            })
            .await;

        let cleanup_started = std::sync::Arc::new(tokio::sync::Notify::new());
        let release_cleanup = std::sync::Arc::new(tokio::sync::Notify::new());
        let cleanup_state = state.clone();
        let started = cleanup_started.clone();
        let release = release_cleanup.clone();
        let cleanup = tokio::spawn(async move {
            cleanup_state
                .with_owned_worktree_cleanup(
                    &old_owner,
                    "/tmp/serialized-worktree",
                    || async move {
                        started.notify_one();
                        release.notified().await;
                    },
                )
                .await
        });
        cleanup_started.notified().await;

        let replacement_state = state.clone();
        let replacement = tokio::spawn(async move {
            replacement_state
                .apply_and_execute(crate::daemon_protocol::Event::Register {
                    id: "replacement".into(),
                    pane: Some("%100".into()),
                    metadata: crate::daemon_protocol::SessionMeta {
                        project_dir: Some("/tmp/serialized-worktree".into()),
                        ..Default::default()
                    },
                })
                .await
        });
        tokio::task::yield_now().await;
        assert!(
            !replacement.is_finished(),
            "replacement claim must wait while cleanup owns the directory gate"
        );

        release_cleanup.notify_one();
        assert_eq!(cleanup.await.expect("cleanup task failed"), Some(()));
        replacement.await.expect("replacement task failed");
        assert_eq!(
            state.protocol.read().await.sessions["replacement"]
                .metadata
                .project_dir
                .as_deref(),
            Some("/tmp/serialized-worktree")
        );
    }

    #[tokio::test]
    async fn worktree_cleanup_skips_a_reserved_sharer_without_a_session_row() {
        let state = AppState::new_for_test();
        let stale_owner = crate::daemon_protocol::ResourceOwner {
            session_id: "old".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(1),
        };
        let replacement_owner = match state.reserve_start("replacement").await.unwrap() {
            crate::daemon_protocol::StartDisposition::Reserved(owner) => owner,
            disposition => panic!("expected reservation, got {disposition:?}"),
        };
        let claim_result = state
            .with_reserved_project_dir_claim(
                &replacement_owner,
                replacement_owner.clone(),
                "/tmp/reserved-worktree",
                false,
                || async {},
            )
            .await
            .unwrap();
        assert_eq!(claim_result, Some(()));

        let touched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cleanup_touched = touched.clone();
        let result = state
            .with_owned_worktree_cleanup(&stale_owner, "/tmp/reserved-worktree", || async move {
                cleanup_touched.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .await;

        assert_eq!(result, None);
        assert!(!touched.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn paneless_directory_claim_is_durable_before_external_work() {
        let state = AppState::new_for_test();
        let owner = match state.reserve_start("paneless").await.unwrap() {
            crate::daemon_protocol::StartDisposition::Reserved(owner) => owner,
            disposition => panic!("expected reservation, got {disposition:?}"),
        };
        let external_work_observed_claim =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = external_work_observed_claim.clone();
        let state_for_action = state.clone();
        let owner_for_action = owner.clone();

        let result = state
            .with_reserved_project_dir_claim(
                &owner,
                owner.clone(),
                "/tmp/.ouija/worktrees/project/paneless",
                true,
                || async move {
                    let persisted =
                        crate::persistence::load_sessions(&state_for_action.config.data_dir)
                            .unwrap();
                    let lease = &persisted.lifecycle_leases[&owner_for_action.session_id];
                    observed.store(
                        lease.project_dir.as_deref()
                            == Some("/tmp/.ouija/worktrees/project/paneless")
                            && lease.project_dir_owner.as_ref() == Some(&owner_for_action)
                            && lease.project_dir_cleanup_on_abandon
                            && lease.inert_pane.is_none(),
                        std::sync::atomic::Ordering::SeqCst,
                    );
                },
            )
            .await
            .unwrap();

        assert_eq!(result, Some(()));
        assert!(
            external_work_observed_claim.load(std::sync::atomic::Ordering::SeqCst),
            "the paneless crash envelope must be persisted before filesystem work"
        );
    }

    // --- Pure functions ---

    #[test]
    #[cfg(unix)]
    fn project_dir_identity_unifies_symlinked_parent_for_missing_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let physical = root.path().join("physical");
        let alias = root.path().join("alias");
        std::fs::create_dir(&physical).unwrap();
        symlink(&physical, &alias).unwrap();

        assert_eq!(
            project_dir_identity(physical.join("future").to_str().unwrap()),
            project_dir_identity(alias.join("./future").to_str().unwrap())
        );
    }

    #[test]
    #[cfg(unix)]
    fn project_dir_identity_resolves_parent_after_symlink_traversal() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let physical = root.path().join("physical");
        let inner = physical.join("inner");
        let alias = root.path().join("alias");
        std::fs::create_dir_all(&inner).unwrap();
        symlink(&inner, &alias).unwrap();

        assert_eq!(
            project_dir_identity(physical.join("target").to_str().unwrap()),
            project_dir_identity(alias.join("../target").to_str().unwrap())
        );
    }

    #[test]
    fn resolve_project_root_normal_path() {
        assert_eq!(
            resolve_project_root("/Users/dan/code/myproject"),
            "/Users/dan/code/myproject"
        );
    }

    #[test]
    fn resolve_project_root_worktree_path() {
        assert_eq!(
            resolve_project_root("/Users/dan/code/chess-reader/.claude/worktrees/feature-branch"),
            "/Users/dan/code/chess-reader"
        );
    }

    #[test]
    fn resolve_project_root_linux_worktree() {
        assert_eq!(
            resolve_project_root("/home/daniel/code/ouija/.claude/worktrees/auto-register"),
            "/home/daniel/code/ouija"
        );
    }

    #[test]
    fn resolve_project_root_ouija_worktree() {
        assert_eq!(
            resolve_project_root("/home/daniel/code/ouija/.ouija/worktrees/feature-x"),
            "/home/daniel/code/ouija"
        );
    }

    // --- is_home_project_root / root_matches_home (#1483) ---

    #[test]
    fn root_matches_home_exact() {
        assert!(root_matches_home("/home/daniel", "/home/daniel"));
    }

    #[test]
    fn root_matches_home_trailing_slash_insensitive() {
        assert!(root_matches_home("/home/daniel/", "/home/daniel"));
        assert!(root_matches_home("/home/daniel", "/home/daniel/"));
    }

    #[test]
    fn root_matches_home_rejects_project_under_home() {
        assert!(!root_matches_home(
            "/home/daniel/code/ouija",
            "/home/daniel"
        ));
    }

    #[test]
    fn root_matches_home_empty_home_never_matches() {
        // A blank $HOME must not turn every empty/relative root into "home".
        assert!(!root_matches_home("", ""));
        assert!(!root_matches_home("/", ""));
    }

    // --- resolve_unique_session_id ---

    #[test]
    fn resolve_unique_session_id_no_conflicts_returns_base() {
        let map: HashMap<String, Option<String>> = HashMap::new();
        assert_eq!(
            resolve_unique_session_id(&map, "ouija", Some("%17")),
            "ouija"
        );
    }

    #[test]
    fn resolve_unique_session_id_same_pane_returns_base_idempotent() {
        // Re-resolving a name that already maps to the same pane must NOT
        // bump to a new suffix. The protocol handles idempotent re-register
        // (same id, same pane) without side effects; if the helper invented
        // a new id here we'd lose that idempotency and silently rename
        // sessions on every hook fire.
        let mut map = HashMap::new();
        map.insert("ouija".into(), Some("%17".into()));
        assert_eq!(
            resolve_unique_session_id(&map, "ouija", Some("%17")),
            "ouija"
        );
    }

    #[test]
    fn resolve_unique_session_id_distinct_pane_bumps_suffix() {
        // Same base_id, different pane: must allocate -2.
        let mut map = HashMap::new();
        map.insert("ouija".into(), Some("%17".into()));
        assert_eq!(
            resolve_unique_session_id(&map, "ouija", Some("%18")),
            "ouija-2"
        );
    }

    #[test]
    fn resolve_unique_session_id_walks_through_taken_suffixes() {
        // ouija and ouija-2 are taken (different panes); helper must skip to ouija-3.
        let mut map = HashMap::new();
        map.insert("ouija".into(), Some("%17".into()));
        map.insert("ouija-2".into(), Some("%18".into()));
        assert_eq!(
            resolve_unique_session_id(&map, "ouija", Some("%19")),
            "ouija-3"
        );
    }

    #[test]
    fn resolve_unique_session_id_no_target_pane_treats_existing_as_conflict() {
        // When target_pane is None (caller has no pane to dedupe against), every
        // existing entry counts as a conflict — never collapse to base just
        // because some other id_to_pane entry happens to also be None.
        let mut map = HashMap::new();
        map.insert("ouija".into(), None);
        assert_eq!(resolve_unique_session_id(&map, "ouija", None), "ouija-2");
    }

    #[test]
    fn resolve_unique_session_id_overflow_returns_last_attempted_id() {
        // Saturate the namespace from base..=base-MAX_NAME_SUFFIX with foreign
        // panes. The helper must not loop forever and must not panic; it
        // returns the last id it tried so the caller's apply_register can
        // reject it (Register dedup will replace whatever currently owns
        // that id rather than silently corrupt state).
        let mut map = HashMap::new();
        map.insert("ouija".into(), Some("%1".into()));
        for n in 2..=MAX_NAME_SUFFIX {
            map.insert(format!("ouija-{n}"), Some(format!("%{n}")));
        }
        let resolved = resolve_unique_session_id(&map, "ouija", Some("%9999"));
        // The overflow stop happens after format!("{base}-{suffix}") with
        // suffix == MAX_NAME_SUFFIX + 1. We don't pin the exact id; what
        // matters is that the call returns and is finite.
        assert!(
            resolved.starts_with("ouija"),
            "expected resolved id to start with the base, got: {resolved}"
        );
    }

    // --- AppState async tests ---

    pub(crate) fn test_config() -> OuijaConfig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.keep();
        OuijaConfig {
            name: "test".into(),
            data_dir: path.clone(),
            config_dir: path,
            port: 0,
            npub: "npub1test".into(),
        }
    }

    #[tokio::test]
    async fn durable_start_reservation_rolls_back_when_snapshot_write_fails() {
        let config = test_config();
        std::fs::create_dir(config.data_dir.join("sessions.tmp")).unwrap();
        let state = AppState::new(config);

        let result = state.reserve_start("worker").await;

        assert!(result.is_err());
        let proto = state.protocol.read().await;
        assert!(proto.lifecycle_leases.is_empty());
        assert_eq!(
            proto.incarnation_high_water,
            crate::daemon_protocol::SessionIncarnation::default()
        );
    }

    #[tokio::test]
    async fn durable_start_commit_persists_the_exact_owner_and_prelaunch_lease() {
        let config = test_config();
        let state = AppState::new(config.clone());
        let owner = match state.reserve_start("worker").await.unwrap() {
            crate::daemon_protocol::StartDisposition::Reserved(owner) => owner,
            other => panic!("expected reservation, got {other:?}"),
        };
        assert_eq!(
            state
                .record_inert_start_pane(&owner, owner.clone(), "%1".into())
                .await
                .unwrap(),
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );

        let outcome = state
            .commit_reserved_start(
                &owner,
                Some("%1".into()),
                crate::daemon_protocol::SessionMeta::default(),
            )
            .await
            .unwrap();

        assert_eq!(
            outcome,
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );
        let persisted = crate::persistence::load_sessions(&config.data_dir).unwrap();
        assert_eq!(persisted.lifecycle_leases["worker"].owner, owner);
        assert_eq!(
            persisted.lifecycle_leases["worker"].inert_pane.as_deref(),
            Some("%1")
        );
        assert_eq!(
            persisted.sessions[0].metadata.session_incarnation,
            owner.incarnation
        );
    }

    #[tokio::test]
    async fn inert_pane_owner_can_launch_before_session_commit() {
        let state = AppState::new_for_test();
        let owner = match state.reserve_start("worker").await.unwrap() {
            crate::daemon_protocol::StartDisposition::Reserved(owner) => owner,
            other => panic!("expected reservation, got {other:?}"),
        };
        state
            .record_inert_start_pane(&owner, owner.clone(), "%staged".into())
            .await
            .unwrap();

        let allowed = state
            .with_owned_pane_claim(&owner, "%staged", || async { "launched" })
            .await;

        assert_eq!(allowed, Some("launched"));
    }

    #[tokio::test]
    async fn superseded_session_owner_cannot_claim_reused_pane() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%reused".into()),
                metadata: Default::default(),
            })
            .await;
        let superseded = state.protocol.read().await.sessions["worker"].owner();

        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%reused".into()),
                metadata: Default::default(),
            })
            .await;

        let touched = state
            .with_owned_pane_claim(&superseded, "%reused", || async { true })
            .await;
        assert_eq!(touched, None);
    }

    #[tokio::test]
    async fn durable_start_commit_rolls_back_when_snapshot_write_fails() {
        let config = test_config();
        let state = AppState::new(config.clone());
        let owner = match state.reserve_start("worker").await.unwrap() {
            crate::daemon_protocol::StartDisposition::Reserved(owner) => owner,
            other => panic!("expected reservation, got {other:?}"),
        };
        std::fs::create_dir(config.data_dir.join("sessions.tmp")).unwrap();

        let result = state
            .commit_reserved_start(
                &owner,
                Some("%1".into()),
                crate::daemon_protocol::SessionMeta::default(),
            )
            .await;

        assert!(result.is_err());
        let proto = state.protocol.read().await;
        assert_eq!(proto.lifecycle_leases["worker"].owner, owner);
        assert!(!proto.sessions.contains_key("worker"));
    }

    #[tokio::test]
    async fn durable_stop_claim_persists_cleanup_authority_before_external_work() {
        let config = test_config();
        let state = AppState::new(config.clone());
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%1".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_worker".into()),
                    project_dir: Some("/tmp/.ouija/worktrees/project/worker".into()),
                    ..Default::default()
                },
            })
            .await;
        let owner = state.protocol.read().await.sessions["worker"].owner();

        let outcome = state.claim_existing_stop(&owner, "%1", true).await.unwrap();

        assert_eq!(
            outcome,
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );
        let persisted = crate::persistence::load_sessions(&config.data_dir).unwrap();
        let lease = &persisted.lifecycle_leases["worker"];
        assert_eq!(lease.owner, owner);
        assert_eq!(
            lease.phase,
            crate::daemon_protocol::LifecyclePhase::Stopping
        );
        assert!(lease.project_dir_cleanup_on_abandon);
        assert_eq!(lease.inert_pane.as_deref(), Some("%1"));
        assert_eq!(lease.backend.as_deref(), Some("opencode"));
        assert_eq!(lease.backend_session_id.as_deref(), Some("ses_worker"));
        assert_eq!(lease.backend_session_owner.as_ref(), Some(&owner));
    }

    #[tokio::test]
    async fn durable_start_failure_rollback_restores_state_when_snapshot_write_fails() {
        let config = test_config();
        let state = AppState::new(config.clone());
        let owner = match state.reserve_start("worker").await.unwrap() {
            crate::daemon_protocol::StartDisposition::Reserved(owner) => owner,
            other => panic!("expected reservation, got {other:?}"),
        };
        state
            .commit_reserved_start(
                &owner,
                Some("%1".into()),
                crate::daemon_protocol::SessionMeta::default(),
            )
            .await
            .unwrap();
        std::fs::create_dir(config.data_dir.join("sessions.tmp")).unwrap();

        let result = state.rollback_reserved_start(&owner, "%1", None).await;

        assert!(result.is_err());
        let proto = state.protocol.read().await;
        assert_eq!(
            proto.sessions["worker"].metadata.session_incarnation,
            owner.incarnation
        );
        assert_eq!(proto.sessions["worker"].pane.as_deref(), Some("%1"));
    }

    #[tokio::test]
    async fn durable_start_abort_persists_lease_release() {
        let config = test_config();
        let state = AppState::new(config.clone());
        let owner = match state.reserve_start("worker").await.unwrap() {
            crate::daemon_protocol::StartDisposition::Reserved(owner) => owner,
            other => panic!("expected reservation, got {other:?}"),
        };

        let outcome = state.abort_lifecycle(&owner).await.unwrap();

        assert_eq!(
            outcome,
            crate::daemon_protocol::LifecycleMutationOutcome::Applied
        );
        let persisted = crate::persistence::load_sessions(&config.data_dir).unwrap();
        assert!(persisted.lifecycle_leases.is_empty());
        assert_eq!(persisted.incarnation_high_water, owner.incarnation);
    }

    #[tokio::test]
    async fn durable_start_abort_rolls_back_when_snapshot_write_fails() {
        let config = test_config();
        let state = AppState::new(config.clone());
        let owner = match state.reserve_start("worker").await.unwrap() {
            crate::daemon_protocol::StartDisposition::Reserved(owner) => owner,
            other => panic!("expected reservation, got {other:?}"),
        };
        std::fs::create_dir(config.data_dir.join("sessions.tmp")).unwrap();

        let result = state.abort_lifecycle(&owner).await;

        assert!(result.is_err());
        let proto = state.protocol.read().await;
        assert_eq!(proto.lifecycle_leases["worker"].owner, owner);
    }

    #[tokio::test]
    async fn fresh_launch_stage_does_not_escape_when_snapshot_write_fails() {
        let config = test_config();
        let state = AppState::new(config.clone());
        proto_register(&state, "worker", Some("%1")).await;
        let before = state.protocol.read().await.clone();
        std::fs::create_dir(config.data_dir.join("sessions.tmp")).unwrap();

        let outcome = state
            .stage_fresh_launch("worker", "codex-cli".into(), Some("proof".into()), None)
            .await;

        assert_eq!(
            outcome,
            crate::daemon_protocol::StageFreshLaunchOutcome::PersistenceFailed
        );
        assert_eq!(*state.protocol.read().await, before);
    }

    /// Config whose opencode serve port (`config.port + 320` = 320) is
    /// deterministically dead: unprivileged test processes cannot bind ports
    /// below 1024, so prompt_async delivery always fails with connection
    /// refused. The previous bind-ephemeral-then-drop approach raced sibling
    /// tests that bind `127.0.0.1:0` with a live `/session/{id}/prompt_async`
    /// route — the freed port could be re-issued to them, flipping an
    /// expected delivery failure into an accepted delivery (flaky).
    fn dead_opencode_serve_config() -> OuijaConfig {
        test_config()
    }

    /// Helper: register a session via the protocol path.
    async fn proto_register(state: &Arc<AppState>, id: &str, pane: Option<&str>) {
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: id.into(),
                pane: pane.map(Into::into),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
    }

    #[tokio::test]
    async fn delayed_remove_effects_do_not_stop_a_replacement_agent() {
        let state = AppState::new_for_test();
        proto_register(&state, "worker", Some("%1")).await;
        let old_owner = state.protocol.read().await.sessions["worker"].owner();

        let stale_effects = {
            let mut proto = state.protocol.write().await;
            proto.apply(crate::daemon_protocol::Event::RemoveOwned {
                owner: old_owner.clone(),
                expected_pane: Some("%1".into()),
                keep_worktree: true,
            })
        };
        proto_register(&state, "worker", Some("%1")).await;
        let replacement_owner = state.protocol.read().await.sessions["worker"].owner();
        assert_ne!(replacement_owner, old_owner);
        assert!(
            state
                .session_agents
                .read()
                .await
                .contains_key(&replacement_owner)
        );

        state.execute_effects(&stale_effects).await;

        assert_eq!(
            state.protocol.read().await.sessions["worker"].owner(),
            replacement_owner
        );
        assert!(
            state
                .session_agents
                .read()
                .await
                .contains_key(&replacement_owner),
            "old removal effects must not stop the replacement's agent"
        );
        assert_eq!(
            state.session_agents.read().await[&replacement_owner].owner,
            replacement_owner,
            "the exact-owner entry must remain bound to the replacement incarnation"
        );
    }

    #[tokio::test]
    async fn delayed_rename_preserves_both_renamed_and_reused_id_agents() {
        let state = AppState::new_for_test();
        proto_register(&state, "worker", Some("%1")).await;
        let old_owner = state.protocol.read().await.sessions["worker"].owner();

        let rename_effects = {
            let mut protocol = state.protocol.write().await;
            protocol.apply(crate::daemon_protocol::Event::Rename {
                old_id: "worker".into(),
                new_id: "renamed".into(),
            })
        };
        let renamed_owner = state.protocol.read().await.sessions["renamed"].owner();
        proto_register(&state, "worker", Some("%2")).await;
        let replacement_owner = state.protocol.read().await.sessions["worker"].owner();

        state.execute_effects(&rename_effects).await;

        let agents = state.session_agents.read().await;
        assert!(agents.contains_key(&renamed_owner));
        assert!(agents.contains_key(&replacement_owner));
        assert!(!agents.contains_key(&old_owner));
    }

    #[tokio::test]
    async fn worktree_cleanup_fails_closed_when_a_replacement_uses_the_directory() {
        let state = AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "replacement".into(),
                pane: Some("%2".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/tmp/.ouija/worktrees/project/replacement".into()),
                    ..Default::default()
                },
            })
            .await;
        let replacement_owner = state.protocol.read().await.sessions["replacement"].owner();

        assert!(
            !state
                .cleanup_worktree_dir_if_unused(
                    &replacement_owner,
                    "/tmp/.ouija/worktrees/project/replacement",
                )
                .await
        );
    }

    #[tokio::test]
    async fn execute_effects_uses_recorded_tmux_method_for_send_inject() {
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::net::TcpListener;

        async fn prompt_async(AxumState(calls): AxumState<StdArc<AtomicUsize>>) -> StatusCode {
            calls.fetch_add(1, Ordering::SeqCst);
            StatusCode::NO_CONTENT
        }

        let calls = StdArc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session/{session_id}/prompt_async", post(prompt_async))
            .with_state(calls.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut config = test_config();
        config.port = port - 320;
        let state = AppState::new(config);
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_live".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::StrongManaged,
                        ),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }

        let effects = vec![
            crate::daemon_protocol::Effect::InjectMessage {
                session_id: "oc".into(),
                pane: "%1".into(),
                message: "hello".into(),
                vim_mode: false,
                delivery_method: None,
                http_delivery: None,
                pending_reply_msg_id: None,
                pending_reply_from: None,
            },
            crate::daemon_protocol::Effect::SendDelivered {
                from: "sender".into(),
                to: "oc".into(),
                method: "tmux".into(),
                msg_id: 7,
                http_delivery: None,
            },
        ];

        state.execute_effects(&effects).await;

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn normal_tmux_inject_rejects_pane_not_owned_by_session() {
        let state = AppState::new_for_test();
        proto_register(&state, "target", Some("%1")).await;

        let outcome = deliver_inject_message_effect(
            &state,
            InjectDeliveryRequest {
                session_id: "target",
                pane: "%2",
                message: "hello",
                vim_mode: false,
                delivery_method: Some("tmux"),
                recorded_method: None,
            },
        )
        .await;

        assert!(
            matches!(outcome, DeliveryOutcome::Rejected(ref reason) if reason.contains("pane %2 is not owned by session target")),
            "expected stale pane rejection, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn methodless_inject_rejects_pane_not_owned_by_session() {
        let state = AppState::new_for_test();
        proto_register(&state, "target", Some("%1")).await;

        let outcome = deliver_inject_message_effect(
            &state,
            InjectDeliveryRequest {
                session_id: "target",
                pane: "%2",
                message: "hello",
                vim_mode: false,
                delivery_method: None,
                recorded_method: None,
            },
        )
        .await;

        assert!(
            matches!(outcome, DeliveryOutcome::Rejected(ref reason) if reason.contains("pane %2 is not owned by session target")),
            "expected stale pane rejection, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn owned_inject_rejects_same_pane_replacement() {
        let state = AppState::new_for_test();
        proto_register(&state, "target", Some("%same")).await;
        let stale_owner = state.protocol.read().await.sessions["target"].owner();
        {
            let mut proto = state.protocol.write().await;
            proto.apply(crate::daemon_protocol::Event::Remove {
                id: "target".into(),
                keep_worktree: true,
            });
            proto.apply(crate::daemon_protocol::Event::Register {
                id: "target".into(),
                pane: Some("%same".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            });
        }

        let outcome = deliver_owned_inject_message_effect(
            &state,
            &stale_owner,
            None,
            InjectDeliveryRequest {
                session_id: "target",
                pane: "%same",
                message: "must not reach replacement",
                vim_mode: false,
                delivery_method: Some("tmux"),
                recorded_method: None,
            },
        )
        .await;

        assert!(
            matches!(outcome, DeliveryOutcome::Rejected(ref reason) if reason.contains("owner changed")),
            "expected exact-owner rejection, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn methodless_inject_stale_pane_marks_delivery_failed() {
        let state = AppState::new_for_test();
        proto_register(&state, "sender", Some("%9")).await;
        proto_register(&state, "target", Some("%1")).await;

        let effects = vec![
            crate::daemon_protocol::Effect::InjectMessage {
                session_id: "target".into(),
                pane: "%1".into(),
                message: "hello".into(),
                vim_mode: false,
                delivery_method: None,
                http_delivery: None,
                pending_reply_msg_id: None,
                pending_reply_from: None,
            },
            crate::daemon_protocol::Effect::LogMessage {
                from: "sender".into(),
                to: "target".into(),
                message: "hello".into(),
                delivered: true,
                transport: "nostr".into(),
            },
        ];
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.get_mut("target").unwrap().pane = Some("%2".into());
        }

        let failure = state.execute_effects(&effects).await;

        assert!(
            failure.as_ref().is_some_and(|failure| failure
                .reason
                .contains("pane %1 is not owned by session target")),
            "expected stale pane failure, got {failure:?}"
        );
        let log = state.message_log.read().await;
        assert_eq!(log.len(), 1);
        assert!(!log[0].delivered);
    }

    #[tokio::test]
    async fn execute_effects_delivers_http_from_recorded_snapshot_without_live_session() {
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::net::TcpListener;

        async fn prompt_async(AxumState(calls): AxumState<StdArc<AtomicUsize>>) -> StatusCode {
            calls.fetch_add(1, Ordering::SeqCst);
            StatusCode::NO_CONTENT
        }

        let calls = StdArc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session/{session_id}/prompt_async", post(prompt_async))
            .with_state(calls.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut config = test_config();
        config.port = port - 320;
        let state = AppState::new(config);
        let effects = vec![
            crate::daemon_protocol::Effect::DeliverHttpMessage {
                session_id: "oc".into(),
                message: "hello".into(),
                http_delivery: crate::daemon_protocol::HttpDeliverySnapshot {
                    backend_session_id: "ses_live".into(),
                    project_dir: None,
                    model: None,
                    effort: None,
                },
                pending_reply_msg_id: None,
                pending_reply_from: None,
            },
            crate::daemon_protocol::Effect::SendDelivered {
                from: "sender".into(),
                to: "oc".into(),
                method: "http".into(),
                msg_id: 8,
                http_delivery: Some(crate::daemon_protocol::HttpDeliverySnapshot {
                    backend_session_id: "ses_recorded".into(),
                    project_dir: None,
                    model: None,
                    effort: None,
                }),
            },
        ];

        state.execute_effects(&effects).await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn execute_effects_reports_strong_opencode_inject_failure_without_recorded_method() {
        let state = AppState::new(dead_opencode_serve_config());
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_live".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::StrongManaged,
                        ),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }
        let effects = vec![crate::daemon_protocol::Effect::InjectMessage {
            session_id: "oc".into(),
            pane: "%1".into(),
            message: "hello".into(),
            vim_mode: false,
            delivery_method: Some("http".into()),
            http_delivery: Some(crate::daemon_protocol::HttpDeliverySnapshot {
                backend_session_id: "ses_live".into(),
                project_dir: None,
                model: None,
                effort: None,
            }),
            pending_reply_msg_id: None,
            pending_reply_from: None,
        }];

        let failure = state.execute_effects(&effects).await;

        assert!(
            failure
                .as_ref()
                .is_some_and(|failure| failure.reason.contains("prompt_async request failed")),
            "expected observable HTTP delivery failure, got {failure:?}"
        );
    }

    #[tokio::test]
    async fn execute_effects_revalidates_http_inject_against_current_opencode_binding() {
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::net::TcpListener;

        async fn prompt_async(AxumState(calls): AxumState<StdArc<AtomicUsize>>) -> StatusCode {
            calls.fetch_add(1, Ordering::SeqCst);
            StatusCode::NO_CONTENT
        }

        let calls = StdArc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session/{session_id}/prompt_async", post(prompt_async))
            .with_state(calls.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut config = test_config();
        config.port = port - 320;
        let state = AppState::new(config);
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_live".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::WeakAdopted,
                        ),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }

        let effects = vec![crate::daemon_protocol::Effect::InjectMessage {
            session_id: "oc".into(),
            pane: "%1".into(),
            message: "hello".into(),
            vim_mode: false,
            delivery_method: Some("http".into()),
            http_delivery: Some(crate::daemon_protocol::HttpDeliverySnapshot {
                backend_session_id: "ses_live".into(),
                project_dir: None,
                model: None,
                effort: None,
            }),
            pending_reply_msg_id: None,
            pending_reply_from: None,
        }];

        let failure = state.execute_effects(&effects).await;

        assert!(failure.is_none());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "stale/forged HTTP inject metadata must not bypass the shared OpenCode delivery gate"
        );
        server.abort();
    }

    #[tokio::test]
    async fn execute_effects_rejects_strong_opencode_http_inject_after_session_moves_panes() {
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::net::TcpListener;

        async fn prompt_async(AxumState(calls): AxumState<StdArc<AtomicUsize>>) -> StatusCode {
            calls.fetch_add(1, Ordering::SeqCst);
            StatusCode::NO_CONTENT
        }

        let calls = StdArc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session/{session_id}/prompt_async", post(prompt_async))
            .with_state(calls.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut config = test_config();
        config.port = port - 320;
        let state = AppState::new(config);
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_live".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::StrongManaged,
                        ),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }

        let effects = vec![crate::daemon_protocol::Effect::InjectMessage {
            session_id: "oc".into(),
            pane: "%1".into(),
            message: "hello".into(),
            vim_mode: false,
            delivery_method: Some("http".into()),
            http_delivery: Some(crate::daemon_protocol::HttpDeliverySnapshot {
                backend_session_id: "ses_live".into(),
                project_dir: None,
                model: None,
                effort: None,
            }),
            pending_reply_msg_id: None,
            pending_reply_from: None,
        }];
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.get_mut("oc").unwrap().pane = Some("%2".into());
        }

        let failure = state.execute_effects(&effects).await;

        assert!(
            failure.as_ref().is_some_and(|failure| failure
                .reason
                .contains("pane %1 is not owned by session oc")),
            "expected stale pane rejection, got {failure:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "stale HTTP inject must not call prompt_async"
        );
        server.abort();
    }

    #[tokio::test]
    async fn incoming_weak_opencode_inject_uses_apply_time_delivery_method() {
        let state = AppState::new(dead_opencode_serve_config());
        let effects = {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: Some("%17".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_old".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::WeakAdopted,
                        ),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
            proto.apply(crate::daemon_protocol::Event::IncomingWire {
                msg: crate::protocol::WireMessage::SessionSend {
                    from: "remote".into(),
                    to: "oc".into(),
                    message: "hello".into(),
                    expects_reply: false,
                    msg_id: 42,
                    responds_to: None,
                    done: false,
                },
                sender_npub: Some("npub1remote".into()),
            })
        };
        {
            let mut proto = state.protocol.write().await;
            let session = proto.sessions.get_mut("oc").unwrap();
            session.metadata.backend_session_id = Some("ses_new".into());
            session.metadata.opencode_binding =
                Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged);
        }

        let failure = state.execute_effects(&effects).await;

        assert!(failure.is_none());
    }

    #[tokio::test]
    async fn execute_effects_broadcasts_failure_ack_after_inject_failure() {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingTransport {
            broadcasts: StdArc<AtomicUsize>,
            failure_acks: StdArc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl crate::transport::Transport for CountingTransport {
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }

            async fn broadcast(&self, msg: &crate::protocol::WireMessage) -> bool {
                self.broadcasts.fetch_add(1, Ordering::SeqCst);
                if matches!(
                    msg,
                    crate::protocol::WireMessage::SessionSendAck {
                        delivered: false,
                        ..
                    }
                ) {
                    self.failure_acks.fetch_add(1, Ordering::SeqCst);
                }
                true
            }

            async fn connect(
                &self,
                _ticket: &str,
                _state: Arc<AppState>,
                _wait: bool,
            ) -> anyhow::Result<()> {
                Ok(())
            }

            async fn ticket_string(&self) -> Option<String> {
                None
            }

            async fn regenerate(
                &self,
                _config_dir: &std::path::Path,
                _data_dir: &std::path::Path,
            ) -> anyhow::Result<String> {
                Ok("ticket".into())
            }

            fn endpoint_id(&self) -> Option<String> {
                None
            }

            fn is_ready(&self) -> bool {
                true
            }

            fn transport_name(&self) -> &'static str {
                "counting"
            }
        }

        let state = AppState::new(dead_opencode_serve_config());
        let broadcasts = StdArc::new(AtomicUsize::new(0));
        let failure_acks = StdArc::new(AtomicUsize::new(0));
        state
            .add_transport(StdArc::new(CountingTransport {
                broadcasts: broadcasts.clone(),
                failure_acks: failure_acks.clone(),
            }))
            .await;
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_live".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::StrongManaged,
                        ),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }
        let effects = vec![
            crate::daemon_protocol::Effect::InjectMessage {
                session_id: "oc".into(),
                pane: "%1".into(),
                message: "hello".into(),
                vim_mode: false,
                delivery_method: Some("http".into()),
                http_delivery: Some(crate::daemon_protocol::HttpDeliverySnapshot {
                    backend_session_id: "ses_live".into(),
                    project_dir: None,
                    model: None,
                    effort: None,
                }),
                pending_reply_msg_id: None,
                pending_reply_from: None,
            },
            crate::daemon_protocol::Effect::Broadcast(
                crate::protocol::WireMessage::SessionSendAck {
                    from: "remote".into(),
                    to: "oc".into(),
                    delivered: true,
                    daemon_id: "remote-daemon".into(),
                },
            ),
        ];

        let failure = state.execute_effects(&effects).await;

        assert!(failure.is_some());
        assert_eq!(broadcasts.load(Ordering::SeqCst), 1);
        assert_eq!(failure_acks.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn execute_effects_does_not_rewrite_ack_after_ambiguous_http_inject_failure() {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingTransport {
            success_acks: StdArc<AtomicUsize>,
            failure_acks: StdArc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl crate::transport::Transport for CountingTransport {
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }

            async fn broadcast(&self, msg: &crate::protocol::WireMessage) -> bool {
                match msg {
                    crate::protocol::WireMessage::SessionSendAck {
                        delivered: true, ..
                    } => {
                        self.success_acks.fetch_add(1, Ordering::SeqCst);
                    }
                    crate::protocol::WireMessage::SessionSendAck {
                        delivered: false, ..
                    } => {
                        self.failure_acks.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }
                true
            }

            async fn connect(
                &self,
                _ticket: &str,
                _state: Arc<AppState>,
                _wait: bool,
            ) -> anyhow::Result<()> {
                Ok(())
            }

            async fn ticket_string(&self) -> Option<String> {
                None
            }

            async fn regenerate(
                &self,
                _config_dir: &std::path::Path,
                _data_dir: &std::path::Path,
            ) -> anyhow::Result<String> {
                Ok("ticket".into())
            }

            fn endpoint_id(&self) -> Option<String> {
                None
            }

            fn is_ready(&self) -> bool {
                true
            }

            fn transport_name(&self) -> &'static str {
                "counting"
            }
        }

        async fn prompt_async() -> StatusCode {
            StatusCode::INTERNAL_SERVER_ERROR
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route("/session/{session_id}/prompt_async", post(prompt_async));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut config = test_config();
        config.port = port.checked_sub(320).unwrap();
        let state = AppState::new(config);
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_live".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::StrongManaged,
                        ),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }
        let success_acks = StdArc::new(AtomicUsize::new(0));
        let failure_acks = StdArc::new(AtomicUsize::new(0));
        state
            .add_transport(StdArc::new(CountingTransport {
                success_acks: success_acks.clone(),
                failure_acks: failure_acks.clone(),
            }))
            .await;
        let effects = vec![
            crate::daemon_protocol::Effect::InjectMessage {
                session_id: "oc".into(),
                pane: "%1".into(),
                message: "hello".into(),
                vim_mode: false,
                delivery_method: Some("http".into()),
                http_delivery: Some(crate::daemon_protocol::HttpDeliverySnapshot {
                    backend_session_id: "ses_live".into(),
                    project_dir: None,
                    model: None,
                    effort: None,
                }),
                pending_reply_msg_id: None,
                pending_reply_from: None,
            },
            crate::daemon_protocol::Effect::Broadcast(
                crate::protocol::WireMessage::SessionSendAck {
                    from: "remote".into(),
                    to: "oc".into(),
                    delivered: true,
                    daemon_id: "remote-daemon".into(),
                },
            ),
        ];

        let failure = state.execute_effects(&effects).await;

        assert!(
            failure.is_none(),
            "500 response is ambiguous, got {failure:?}"
        );
        assert_eq!(success_acks.load(Ordering::SeqCst), 1);
        assert_eq!(failure_acks.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn execute_effects_suppresses_ambiguous_deliver_http_message_failure() {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::post;

        async fn prompt_async() -> StatusCode {
            StatusCode::INTERNAL_SERVER_ERROR
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route("/session/{session_id}/prompt_async", post(prompt_async));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut config = test_config();
        config.port = port.checked_sub(320).unwrap();
        let state = AppState::new(config);
        let effects = vec![crate::daemon_protocol::Effect::DeliverHttpMessage {
            session_id: "oc".into(),
            message: "hello".into(),
            http_delivery: crate::daemon_protocol::HttpDeliverySnapshot {
                backend_session_id: "ses_live".into(),
                project_dir: None,
                model: None,
                effort: None,
            },
            pending_reply_msg_id: None,
            pending_reply_from: None,
        }];

        let failure = state.execute_effects(&effects).await;

        assert!(
            failure.is_none(),
            "500 response is ambiguous, got {failure:?}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn failed_incoming_delivery_clears_structured_reply_id_not_forged_xml_id() {
        let state = AppState::new(dead_opencode_serve_config());
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_live".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::StrongManaged,
                        ),
                        networked: true,
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
            proto.pending_replies.insert(
                "oc".into(),
                vec![crate::daemon_protocol::PendingReplyEntry {
                    msg_id: 7,
                    from: "other".into(),
                    message: "older pending".into(),
                    received_at: 0,
                    last_activity: 0,
                    in_progress: false,
                }],
            );
        }

        state
            .apply_and_execute(crate::daemon_protocol::Event::IncomingWire {
                msg: crate::protocol::WireMessage::SessionSend {
                    from: "evil\" id=\"7\" reply=\"true".into(),
                    to: "oc".into(),
                    message: "new pending".into(),
                    expects_reply: true,
                    msg_id: 42,
                    responds_to: None,
                    done: false,
                },
                sender_npub: None,
            })
            .await;
        let proto = state.protocol.read().await;
        let pending = proto.pending_replies.get("oc").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].msg_id, 7);
    }

    #[tokio::test]
    async fn failed_incoming_delivery_clears_matching_sender_reply_only() {
        let state = AppState::new(dead_opencode_serve_config());
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_live".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::StrongManaged,
                        ),
                        networked: true,
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
            proto.pending_replies.insert(
                "oc".into(),
                vec![crate::daemon_protocol::PendingReplyEntry {
                    msg_id: 42,
                    from: "other-remote".into(),
                    message: "older pending".into(),
                    received_at: 0,
                    last_activity: 0,
                    in_progress: false,
                }],
            );
        }

        state
            .apply_and_execute(crate::daemon_protocol::Event::IncomingWire {
                msg: crate::protocol::WireMessage::SessionSend {
                    from: "remote".into(),
                    to: "oc".into(),
                    message: "new pending".into(),
                    expects_reply: true,
                    msg_id: 42,
                    responds_to: None,
                    done: false,
                },
                sender_npub: Some("npub1remote".into()),
            })
            .await;

        let proto = state.protocol.read().await;
        let pending = proto.pending_replies.get("oc").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].msg_id, 42);
        assert_eq!(pending[0].from, "other-remote");
    }

    #[tokio::test]
    async fn apply_and_execute_reports_headless_http_send_failure_when_prompt_async_fails() {
        let state = AppState::new(dead_opencode_serve_config());
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "sender".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "sender".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta::default(),
                    registered_at: 0,
                },
            );
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: None,
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_headless".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::StrongManaged,
                        ),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }

        let effects = state
            .apply_and_execute(crate::daemon_protocol::Event::Send {
                from: "sender".into(),
                to: "oc".into(),
                message: "hello".into(),
                expects_reply: true,
                responds_to: None,
                done: false,
            })
            .await;

        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                crate::daemon_protocol::Effect::SendFailed { reason, .. }
                    if reason.contains("prompt_async request failed")
            )
        }));
        assert!(
            !effects.iter().any(|effect| matches!(
                effect,
                crate::daemon_protocol::Effect::SendDelivered { .. }
            ))
        );

        let log = state.message_log.read().await;
        assert_eq!(log.len(), 1);
        assert!(!log[0].delivered);
        drop(log);

        let proto = state.protocol.read().await;
        assert!(!proto.pending_replies.contains_key("oc"));
    }

    #[tokio::test]
    async fn apply_and_execute_clears_incoming_pending_reply_after_inject_failure() {
        let state = AppState::new(dead_opencode_serve_config());
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: Some("%17".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_incoming".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::StrongManaged,
                        ),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }

        let effects = state
            .apply_and_execute(crate::daemon_protocol::Event::IncomingWire {
                msg: crate::protocol::WireMessage::SessionSend {
                    from: "remote".into(),
                    to: "oc".into(),
                    message: "hello".into(),
                    expects_reply: true,
                    msg_id: 42,
                    responds_to: None,
                    done: false,
                },
                sender_npub: Some("npub1remote".into()),
            })
            .await;

        assert!(effects.iter().any(|effect| matches!(
            effect,
            crate::daemon_protocol::Effect::LogMessage {
                delivered: false,
                ..
            }
        )));

        let proto = state.protocol.read().await;
        assert!(!proto.pending_replies.contains_key("oc"));
    }

    #[tokio::test]
    async fn apply_and_execute_clears_incoming_pending_reply_after_headless_http_failure() {
        let state = AppState::new(dead_opencode_serve_config());
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: None,
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_headless".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::StrongManaged,
                        ),
                        networked: true,
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }

        let effects = state
            .apply_and_execute(crate::daemon_protocol::Event::IncomingWire {
                msg: crate::protocol::WireMessage::SessionSend {
                    from: "remote".into(),
                    to: "oc".into(),
                    message: "hello".into(),
                    expects_reply: true,
                    msg_id: 42,
                    responds_to: None,
                    done: false,
                },
                sender_npub: Some("npub1remote".into()),
            })
            .await;

        assert!(effects.iter().any(|effect| matches!(
            effect,
            crate::daemon_protocol::Effect::LogMessage {
                delivered: false,
                ..
            }
        )));

        let proto = state.protocol.read().await;
        assert!(!proto.pending_replies.contains_key("oc"));
    }

    #[tokio::test]
    async fn apply_and_execute_restores_sender_reply_state_after_delivery_failure() {
        let state = AppState::new(dead_opencode_serve_config());
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "sender".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "sender".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        reminder: Some("keep working".into()),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: None,
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_headless".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::StrongManaged,
                        ),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
            proto.pending_replies.insert(
                "sender".into(),
                vec![crate::daemon_protocol::PendingReplyEntry {
                    msg_id: 7,
                    from: "requester".into(),
                    message: "please respond".into(),
                    received_at: 100,
                    last_activity: 100,
                    in_progress: false,
                }],
            );
        }

        let effects = state
            .apply_and_execute(crate::daemon_protocol::Event::Send {
                from: "sender".into(),
                to: "oc".into(),
                message: "done, but unreachable".into(),
                expects_reply: false,
                responds_to: Some(7),
                done: true,
            })
            .await;

        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                crate::daemon_protocol::Effect::SendFailed { reason, .. }
                    if reason.contains("prompt_async request failed")
            )
        }));

        let proto = state.protocol.read().await;
        let pending = proto.pending_replies.get("sender").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].msg_id, 7);
        assert_eq!(
            proto.sessions["sender"].metadata.reminder.as_deref(),
            Some("keep working")
        );
    }

    #[tokio::test]
    async fn apply_and_execute_restores_sender_state_after_send_failed_before_delivery() {
        let state = AppState::new_for_test();
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "sender".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "sender".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        reminder: Some("keep working".into()),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
            proto.pending_replies.insert(
                "sender".into(),
                vec![crate::daemon_protocol::PendingReplyEntry {
                    msg_id: 7,
                    from: "requester".into(),
                    message: "please respond".into(),
                    received_at: 100,
                    last_activity: 100,
                    in_progress: false,
                }],
            );
        }

        let effects = state
            .apply_and_execute(crate::daemon_protocol::Event::Send {
                from: "sender".into(),
                to: "missing".into(),
                message: "done, but missing".into(),
                expects_reply: false,
                responds_to: Some(7),
                done: true,
            })
            .await;

        assert!(effects.iter().any(|effect| matches!(
            effect,
            crate::daemon_protocol::Effect::SendFailed { to, .. } if to == "missing"
        )));

        let proto = state.protocol.read().await;
        let pending = proto.pending_replies.get("sender").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].msg_id, 7);
        assert_eq!(
            proto.sessions["sender"].metadata.reminder.as_deref(),
            Some("keep working")
        );
    }

    #[tokio::test]
    async fn apply_and_execute_does_not_restore_concurrently_cleared_sender_reply_state() {
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use tokio::sync::Notify;

        #[derive(Clone)]
        struct Gate {
            started: StdArc<Notify>,
            release: StdArc<Notify>,
        }

        async fn prompt_async(AxumState(gate): AxumState<Gate>) -> StatusCode {
            gate.started.notify_one();
            gate.release.notified().await;
            StatusCode::NOT_FOUND
        }

        let gate = Gate {
            started: StdArc::new(Notify::new()),
            release: StdArc::new(Notify::new()),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session/{session_id}/prompt_async", post(prompt_async))
            .with_state(gate.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut config = test_config();
        config.port = port.checked_sub(320).unwrap();
        let state = AppState::new(config);
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "sender".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "sender".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        reminder: Some("keep working".into()),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
            proto.sessions.insert(
                "oc".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "oc".into(),
                    pane: None,
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_headless".into()),
                        opencode_binding: Some(
                            crate::daemon_protocol::OpenCodeBinding::StrongManaged,
                        ),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
            proto.pending_replies.insert(
                "sender".into(),
                vec![crate::daemon_protocol::PendingReplyEntry {
                    msg_id: 7,
                    from: "requester".into(),
                    message: "please respond".into(),
                    received_at: 100,
                    last_activity: 100,
                    in_progress: false,
                }],
            );
        }

        let delivery = tokio::spawn({
            let state = state.clone();
            async move {
                state
                    .apply_and_execute(crate::daemon_protocol::Event::Send {
                        from: "sender".into(),
                        to: "oc".into(),
                        message: "done, but unreachable".into(),
                        expects_reply: false,
                        responds_to: Some(7),
                        done: true,
                    })
                    .await
            }
        });
        gate.started.notified().await;
        {
            let mut proto = state.protocol.write().await;
            proto.pending_replies.remove("sender");
            proto.sessions.get_mut("sender").unwrap().metadata.reminder = None;
        }

        gate.release.notify_one();
        let effects = delivery.await.unwrap();

        assert!(effects.iter().any(|effect| {
            matches!(effect, crate::daemon_protocol::Effect::SendFailed { reason, .. } if reason.contains("prompt_async"))
        }));
        let proto = state.protocol.read().await;
        assert!(!proto.pending_replies.contains_key("sender"));
        assert_eq!(proto.sessions["sender"].metadata.reminder, None);
        server.abort();
    }

    #[tokio::test]
    async fn successful_delivery_clears_sender_state_by_msg_id_after_mutations() {
        let state = AppState::new_for_test();
        let original_entry = crate::daemon_protocol::PendingReplyEntry {
            msg_id: 7,
            from: "requester".into(),
            message: "please respond".into(),
            received_at: 100,
            last_activity: 100,
            in_progress: false,
        };
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "sender".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "sender".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        reminder: Some("keep working (activity tick)".into()),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
            proto.pending_replies.insert(
                "sender".into(),
                vec![crate::daemon_protocol::PendingReplyEntry {
                    last_activity: 200,
                    in_progress: true,
                    ..original_entry.clone()
                }],
            );
        }

        state
            .finalize_successful_effect_delivery(Some(FailedEffectSendRollback {
                sender_id: "sender".into(),
                pending_reply_before_send: Some(original_entry),
                pending_reply_after_send: None,
                sender_reminder: Some(Some("keep working".into())),
                sender_reminder_after_send: None,
                sender_state_reserved: false,
                done: true,
            }))
            .await;

        let proto = state.protocol.read().await;
        assert!(!proto.pending_replies.contains_key("sender"));
        assert_eq!(proto.sessions["sender"].metadata.reminder, None);
    }

    #[tokio::test]
    async fn register_session_basic() {
        let state = AppState::new(test_config());
        proto_register(&state, "s1", Some("%1")).await;

        let proto = state.protocol.read().await;
        let sessions = &proto.sessions;
        assert_eq!(sessions.len(), 1);
        assert!(sessions.contains_key("s1"));
    }

    #[tokio::test]
    async fn register_session_dedup_by_pane() {
        let state = AppState::new(test_config());
        proto_register(&state, "old", Some("%1")).await;
        proto_register(&state, "new", Some("%1")).await;

        let proto = state.protocol.read().await;
        let sessions = &proto.sessions;
        assert_eq!(sessions.len(), 1);
        assert!(sessions.contains_key("new"));
        assert!(!sessions.contains_key("old"));
    }

    #[tokio::test]
    async fn register_session_same_id_different_pane_updates() {
        let state = AppState::new(test_config());
        proto_register(&state, "s1", Some("%1")).await;
        let effects = state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "s1".into(),
                pane: Some("%2".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;

        // Re-registering same ID with new pane succeeds (e.g. restart)
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, crate::daemon_protocol::Effect::RegisterOk { .. }))
        );

        let proto = state.protocol.read().await;
        let sessions = &proto.sessions;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions.get("s1").unwrap().pane.as_deref(), Some("%2"));
    }

    #[tokio::test]
    async fn persist_protocol_state_round_trips_all_metadata_fields() {
        // Regression (review round 4): persist_protocol_state built
        // SessionMetadata by hand with ..Default::default() tail, silently
        // dropping model, effort, backend, backend_session_id,
        // project_description, last_metadata_update, on_fire,
        // last_iteration_at. Every Effect::Persist wrote null for those
        // fields, so a daemon restart would load them back as None and
        // silently downgrade sessions (claude: drop --model on restart;
        // scheduler: drop flags on revive; opencode deliver_via_http: drop
        // model/variant on every message). Exercise the full
        // persist → load → deserialise round-trip.
        let config = test_config();
        let state = AppState::new(config.clone());

        // Register a session with every metadata field set so we can detect
        // any field that persist_protocol_state drops.
        let meta = crate::daemon_protocol::SessionMeta {
            project_dir: Some("/tmp/proj".into()),
            role: Some("worker".into()),
            networked: false,
            bulletin: Some("available".into()),
            last_metadata_update: Some(1_700_000_100),
            backend_session_id: Some("oc_abc123".into()),
            backend: Some("opencode".into()),
            session_start_credential: None,
            backend_repair_reservation: None,
            opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
            restart_generation: 7,
            session_incarnation: crate::daemon_protocol::SessionIncarnation(11),
            project_description: Some("test project".into()),
            vim_mode: true,
            worktree: true,
            model: Some("openrouter/sonnet".into()),
            effort: Some("max".into()),
            codex_home: None,
            reminder: Some("remember to...".into()),
            parent_session: Some("parent".into()),
            idle_policy: Some(crate::daemon_protocol::IdlePolicy::AskParentWhenDone),
            prompt: Some("do the thing".into()),
            iteration: 3,
            iteration_log: vec![],
            last_iteration_at: Some(1_700_000_000),
            on_fire: Some(crate::scheduler::OnFire::NewSession),
            worktree_present: Some(false),
            fresh_context_after_active_secs: Some(3_600),
            active_context_accumulated_secs: 1_234,
            active_context_segment_started_at: Some(1_700_000_050),
            active_context_restart_due: true,
        };
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "s1".into(),
                pane: Some("%1".into()),
                metadata: meta,
            })
            .await;
        let pending_owner = {
            let mut proto = state.protocol.write().await;
            match proto.reserve_start("pending").unwrap() {
                crate::daemon_protocol::StartDisposition::Reserved(owner) => owner,
                other => panic!("expected pending reservation, got {other:?}"),
            }
        };

        // Trigger the real persist path (same code Effect::Persist dispatches to).
        {
            let proto = state.protocol.read().await;
            state.persist_protocol_state(&proto).unwrap();
        }

        // Read sessions.json back from disk.
        let loaded = crate::persistence::load_sessions(&config.data_dir)
            .expect("load_sessions after persist");
        assert_eq!(
            loaded.incarnation_high_water, pending_owner.incarnation,
            "allocator high-water mark dropped by persist"
        );
        assert_eq!(
            loaded.lifecycle_leases["pending"].owner, pending_owner,
            "in-flight lifecycle lease dropped by persist"
        );
        let s = loaded
            .sessions
            .iter()
            .find(|p| p.id == "s1")
            .expect("session s1 not persisted");

        // Every field that was set on the SessionMeta must round-trip.
        assert_eq!(
            s.metadata.model.as_deref(),
            Some("openrouter/sonnet"),
            "model dropped by persist"
        );
        assert_eq!(
            s.metadata.effort.as_deref(),
            Some("max"),
            "effort dropped by persist"
        );
        assert_eq!(
            s.metadata.backend.as_deref(),
            Some("opencode"),
            "backend dropped by persist"
        );
        assert_eq!(
            s.metadata.backend_session_id.as_deref(),
            Some("oc_abc123"),
            "backend_session_id dropped by persist"
        );
        assert_eq!(
            s.metadata.opencode_binding,
            Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
            "opencode_binding dropped by persist"
        );
        assert_eq!(
            s.metadata.restart_generation, 7,
            "restart_generation dropped by persist"
        );
        assert_eq!(
            s.metadata.project_description.as_deref(),
            Some("test project"),
            "project_description dropped by persist"
        );
        assert!(
            s.metadata.last_metadata_update.is_some(),
            "last_metadata_update dropped by persist"
        );
        assert_eq!(
            s.metadata.last_iteration_at,
            Some(1_700_000_000),
            "last_iteration_at dropped by persist"
        );
        assert!(s.metadata.on_fire.is_some(), "on_fire dropped by persist");
        assert_eq!(
            s.metadata.role.as_deref(),
            Some("worker"),
            "role dropped by persist"
        );
        assert_eq!(
            s.metadata.bulletin.as_deref(),
            Some("available"),
            "bulletin dropped by persist"
        );
        assert_eq!(
            s.metadata.reminder.as_deref(),
            Some("remember to..."),
            "reminder preserved"
        );
        assert_eq!(
            s.metadata.prompt.as_deref(),
            Some("do the thing"),
            "prompt preserved"
        );
        assert!(s.metadata.vim_mode, "vim_mode preserved");
        assert!(s.metadata.worktree, "worktree preserved");
        assert!(!s.metadata.networked, "networked=false preserved");
        assert_eq!(s.metadata.iteration, 3, "iteration preserved");
        assert_eq!(
            s.metadata.worktree_present,
            Some(false),
            "worktree_present dropped by persist (issue #661)"
        );

        // Full restart simulation: feed the persisted SessionMetadata back
        // through metadata_to_session_meta (the function apply_persisted
        // uses on startup) and assert the re-hydrated SessionMeta matches
        // what we registered. This closes the round-trip for the paths the
        // reviewer called out:
        //   (a) restart_session prev_metadata fallback — reads from
        //       proto.sessions, which is populated by metadata_to_session_meta.
        //   (b) scheduler respawn/revive — reads from the same place.
        //   (c) locked_inject HttpApi — reads from the same place.
        let hydrated = crate::daemon_protocol::metadata_to_session_meta_for_test(&s.metadata);
        assert_eq!(hydrated.model.as_deref(), Some("openrouter/sonnet"));
        assert_eq!(hydrated.effort.as_deref(), Some("max"));
        assert_eq!(hydrated.backend.as_deref(), Some("opencode"));
        assert_eq!(hydrated.backend_session_id.as_deref(), Some("oc_abc123"));
        assert_eq!(
            hydrated.opencode_binding,
            Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged)
        );
        assert_eq!(hydrated.restart_generation, 7);
        assert!(hydrated.on_fire.is_some());
        assert_eq!(hydrated.last_iteration_at, Some(1_700_000_000));
        assert_eq!(hydrated.last_metadata_update, Some(1_700_000_100));
        assert_eq!(hydrated.worktree_present, Some(false));
        assert_eq!(hydrated.fresh_context_after_active_secs, Some(3_600));
        assert_eq!(hydrated.active_context_accumulated_secs, 1_234);
        assert_eq!(
            hydrated.active_context_segment_started_at,
            Some(1_700_000_050)
        );
        assert!(hydrated.active_context_restart_due);
    }

    #[tokio::test]
    async fn persist_protocol_state_round_trips_active_context_accounting() {
        // Break caught: hand-written protocol-to-persistence conversion can
        // omit new fields, resetting an active session's refresh policy after
        // daemon recovery.
        let config = test_config();
        let state = AppState::new(config.clone());
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "worker".into(),
                pane: Some("%1".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    fresh_context_after_active_secs: Some(3_600),
                    active_context_accumulated_secs: 1_234,
                    active_context_segment_started_at: Some(1_700_000_000),
                    active_context_restart_due: true,
                    ..Default::default()
                },
            })
            .await;

        {
            let proto = state.protocol.read().await;
            state.persist_protocol_state(&proto).unwrap();
        }

        let loaded = crate::persistence::load_sessions(&config.data_dir).unwrap();
        let persisted = loaded
            .sessions
            .iter()
            .find(|session| session.id == "worker")
            .expect("worker must persist");
        let hydrated =
            crate::daemon_protocol::metadata_to_session_meta_for_test(&persisted.metadata);
        assert_eq!(hydrated.fresh_context_after_active_secs, Some(3_600));
        assert_eq!(hydrated.active_context_accumulated_secs, 1_234);
        assert_eq!(
            hydrated.active_context_segment_started_at,
            Some(1_700_000_000)
        );
        assert!(hydrated.active_context_restart_due);
    }

    #[tokio::test]
    async fn register_session_same_id_same_pane_updates() {
        let state = AppState::new(test_config());
        proto_register(&state, "s1", Some("%1")).await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "s1".into(),
                pane: Some("%1".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    vim_mode: true,
                    ..Default::default()
                },
            })
            .await;

        let proto = state.protocol.read().await;
        let sessions = &proto.sessions;
        assert!(sessions.get("s1").unwrap().metadata.vim_mode);
    }

    #[tokio::test]
    async fn rename_session_basic() {
        let state = AppState::new(test_config());
        proto_register(&state, "old", Some("%1")).await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Rename {
                old_id: "old".into(),
                new_id: "new".into(),
            })
            .await;

        let proto = state.protocol.read().await;
        let sessions = &proto.sessions;
        assert!(!sessions.contains_key("old"));
        assert!(sessions.contains_key("new"));
    }

    #[tokio::test]
    async fn rename_session_rejects_slash() {
        let state = AppState::new(test_config());
        proto_register(&state, "s1", Some("%1")).await;
        let effects = state
            .apply_and_execute(crate::daemon_protocol::Event::Rename {
                old_id: "s1".into(),
                new_id: "has/slash".into(),
            })
            .await;
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, crate::daemon_protocol::Effect::RenameFailed { .. }))
        );
        assert!(state.protocol.read().await.sessions.contains_key("s1"));
    }

    #[tokio::test]
    async fn rename_nonexistent_returns_none() {
        let state = AppState::new(test_config());
        let effects = state
            .apply_and_execute(crate::daemon_protocol::Event::Rename {
                old_id: "nope".into(),
                new_id: "new".into(),
            })
            .await;
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, crate::daemon_protocol::Effect::RenameFailed { .. }))
        );
    }

    #[tokio::test]
    async fn remove_session_basic() {
        let state = AppState::new(test_config());
        proto_register(&state, "s1", Some("%1")).await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Remove {
                id: "s1".into(),
                keep_worktree: false,
            })
            .await;
        assert!(state.protocol.read().await.sessions.is_empty());
    }

    #[tokio::test]
    async fn remove_nonexistent_is_noop() {
        let state = AppState::new(test_config());
        let effects = state
            .apply_and_execute(crate::daemon_protocol::Event::Remove {
                id: "nope".into(),
                keep_worktree: false,
            })
            .await;
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, crate::daemon_protocol::Effect::RemoveFailed { .. }))
        );
    }

    #[tokio::test]
    async fn remove_remote_session_fails() {
        let state = AppState::new(test_config());
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "remote/s1".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "remote/s1".into(),
                    origin: crate::daemon_protocol::Origin::Remote("remote".into()),
                    ..Default::default()
                },
            );
        }
        let effects = state
            .apply_and_execute(crate::daemon_protocol::Event::Remove {
                id: "remote/s1".into(),
                keep_worktree: false,
            })
            .await;
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, crate::daemon_protocol::Effect::RemoveFailed { .. }))
        );
        assert_eq!(state.protocol.read().await.sessions.len(), 1);
    }

    /// Helper to build a SessionEntry for tests.
    fn test_entry(
        id: &str,
        pane: Option<&str>,
        origin: crate::daemon_protocol::Origin,
        metadata: crate::daemon_protocol::SessionMeta,
    ) -> crate::daemon_protocol::SessionEntry {
        crate::daemon_protocol::SessionEntry {
            id: id.into(),
            pane: pane.map(Into::into),
            origin,
            metadata,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn log_message_caps_at_max() {
        let state = AppState::new(test_config());
        for i in 0..150 {
            state
                .log_message("from".into(), "to".into(), format!("msg {i}"), true, "test")
                .await;
        }
        let log = state.message_log.read().await;
        assert_eq!(log.len(), MAX_LOG);
    }

    #[tokio::test]
    async fn local_session_hash_changes_on_networked_toggle() {
        let state = AppState::new(test_config());
        proto_register(&state, "s1", Some("%1")).await;

        let hash_networked = state.local_session_hash().await;

        // Toggle s1 to non-networked
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.get_mut("s1").unwrap().metadata.networked = false;
        }
        let hash_not_networked = state.local_session_hash().await;

        assert_ne!(hash_networked, hash_not_networked);
    }

    #[tokio::test]
    async fn disconnect_node_removes_sessions() {
        let state = AppState::new(test_config());
        // Add a remote session
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "remote/s1".into(),
                test_entry(
                    "remote/s1",
                    None,
                    crate::daemon_protocol::Origin::Remote("npub1remote".into()),
                    crate::daemon_protocol::SessionMeta::default(),
                ),
            );
            proto.sessions.insert(
                "remote/s2".into(),
                test_entry(
                    "remote/s2",
                    None,
                    crate::daemon_protocol::Origin::Remote("npub1remote".into()),
                    crate::daemon_protocol::SessionMeta::default(),
                ),
            );
        }
        // Add node info
        state.nodes.write().await.insert(
            "npub1remote".into(),
            NodeInfo {
                name: "remote".into(),
                daemon_id: "npub1remote".into(),
                connected_at: Utc::now(),
            },
        );
        state.try_add_node("npub1remote", "remote").unwrap();

        let removed = state.disconnect_node("npub1remote").await;
        assert_eq!(removed, 2);
        assert!(state.protocol.read().await.sessions.is_empty());
        assert!(state.nodes.read().await.is_empty());
    }

    #[test]
    fn session_metadata_networked_defaults_true() {
        let meta = SessionMetadata::default();
        assert!(meta.networked);
    }

    #[test]
    fn session_metadata_networked_serde_default() {
        // Old JSON without "networked" field should default to true
        let json = r#"{"vim_mode": false}"#;
        let meta: SessionMetadata = serde_json::from_str(json).unwrap();
        assert!(meta.networked);
    }

    // --- SessionOrigin serde ---

    #[test]
    fn session_origin_human_round_trip() {
        let origin = SessionOrigin::Human("npub1abc".into());
        let json = serde_json::to_string(&origin).unwrap();
        let parsed: SessionOrigin = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, SessionOrigin::Human(npub) if npub == "npub1abc"));
    }

    #[test]
    fn session_origin_human_deserializes() {
        let json = r#"{"Human":"npub1xyz"}"#;
        let origin: SessionOrigin = serde_json::from_str(json).unwrap();
        assert!(matches!(origin, SessionOrigin::Human(npub) if npub == "npub1xyz"));
    }

    #[tokio::test]
    async fn update_session_metadata_sets_role() {
        let state = AppState::new(test_config());
        proto_register(&state, "s1", Some("%1")).await;

        state
            .apply_and_execute(crate::daemon_protocol::Event::UpdateMetadata {
                id: "s1".into(),
                role: Some("debugging auth".into()),
                bulletin: None,
                project_dir: None,
                networked: None,
            })
            .await;

        let proto = state.protocol.read().await;
        assert_eq!(
            proto.sessions["s1"].metadata.role.as_deref(),
            Some("debugging auth")
        );
    }

    #[tokio::test]
    async fn local_session_hash_changes_on_role_update() {
        let state = AppState::new(test_config());
        proto_register(&state, "s1", Some("%1")).await;

        let hash_before = state.local_session_hash().await;

        state
            .apply_and_execute(crate::daemon_protocol::Event::UpdateMetadata {
                id: "s1".into(),
                role: Some("new role".into()),
                bulletin: None,
                project_dir: None,
                networked: None,
            })
            .await;

        let hash_after = state.local_session_hash().await;
        assert_ne!(hash_before, hash_after);
    }

    #[tokio::test]
    async fn update_metadata_sets_bulletin() {
        let state = AppState::new(test_config());
        proto_register(&state, "s1", Some("%1")).await;

        state
            .apply_and_execute(crate::daemon_protocol::Event::UpdateMetadata {
                id: "s1".into(),
                role: None,
                bulletin: Some("offering review".into()),
                project_dir: None,
                networked: None,
            })
            .await;

        let proto = state.protocol.read().await;
        assert_eq!(
            proto.sessions["s1"].metadata.bulletin.as_deref(),
            Some("offering review")
        );
    }

    // --- collect_excess_idle_sessions ---

    #[tokio::test]
    async fn excess_idle_disabled_when_zero() {
        let state = AppState::new(test_config());
        // max_local_sessions defaults to 0 (disabled)
        proto_register(&state, "s1", Some("%1")).await;
        assert!(state.collect_excess_idle_sessions().await.is_empty());
    }

    #[tokio::test]
    async fn excess_idle_no_eviction_at_limit() {
        let state = AppState::new(test_config());
        state.settings.write().await.max_local_sessions = 2;
        proto_register(&state, "s1", Some("%1")).await;
        proto_register(&state, "s2", Some("%2")).await;
        assert!(state.collect_excess_idle_sessions().await.is_empty());
    }

    #[tokio::test]
    async fn excess_idle_evicts_when_over_limit() {
        use crate::daemon_protocol::{Origin, SessionMeta};
        let state = AppState::new(test_config());
        state.settings.write().await.max_local_sessions = 2;

        // Insert 3 local sessions
        {
            let mut proto = state.protocol.write().await;
            for name in &["a", "b", "c"] {
                proto.sessions.insert(
                    name.to_string(),
                    test_entry(name, Some("%1"), Origin::Local, SessionMeta::default()),
                );
            }
        }

        let evicted = state.collect_excess_idle_sessions().await;
        assert_eq!(evicted.len(), 1);
    }

    #[tokio::test]
    async fn excess_idle_ignores_remote_and_human() {
        use crate::daemon_protocol::{Origin, SessionMeta};
        let state = AppState::new(test_config());
        state.settings.write().await.max_local_sessions = 1;

        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "local".into(),
                test_entry("local", Some("%1"), Origin::Local, SessionMeta::default()),
            );
            proto.sessions.insert(
                "remote/r1".into(),
                test_entry(
                    "remote/r1",
                    None,
                    Origin::Remote("npub1x".into()),
                    SessionMeta::default(),
                ),
            );
            proto.sessions.insert(
                "human".into(),
                test_entry(
                    "human",
                    None,
                    Origin::Human("npub1h".into()),
                    SessionMeta::default(),
                ),
            );
        }

        assert!(state.collect_excess_idle_sessions().await.is_empty());
    }

    #[tokio::test]
    async fn sweep_worktree_presence_sets_true_for_existing_dir() {
        let state = AppState::new_for_test();
        let tempdir = tempfile::tempdir().unwrap();
        let project_dir = tempdir.path().to_str().unwrap().to_string();
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "local/s1".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "local/s1".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        project_dir: Some(project_dir.clone()),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }
        state.sweep_worktree_presence().await;
        {
            let proto = state.protocol.read().await;
            let session = proto.sessions.get("local/s1").unwrap();
            assert_eq!(
                session.metadata.worktree_present,
                Some(true),
                "existing dir should show as present"
            );
        }
    }

    #[tokio::test]
    async fn sweep_worktree_presence_sets_false_for_missing_dir() {
        let state = AppState::new_for_test();
        let missing_dir = "/tmp/ouija-test-nonexistent-dir-12345".to_string();
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "local/s1".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "local/s1".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        project_dir: Some(missing_dir.clone()),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }
        state.sweep_worktree_presence().await;
        {
            let proto = state.protocol.read().await;
            let session = proto.sessions.get("local/s1").unwrap();
            assert_eq!(
                session.metadata.worktree_present,
                Some(false),
                "missing dir should show as absent"
            );
        }
    }

    #[tokio::test]
    async fn sweep_worktree_presence_skips_non_local() {
        let state = AppState::new_for_test();
        let tempdir = tempfile::tempdir().unwrap();
        let project_dir = tempdir.path().to_str().unwrap().to_string();
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "remote/s1".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "remote/s1".into(),
                    pane: None,
                    origin: Origin::Remote("npub1x".into()),
                    metadata: crate::daemon_protocol::SessionMeta {
                        project_dir: Some(project_dir.clone()),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
            proto.sessions.insert(
                "local/s1".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "local/s1".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        project_dir: Some(project_dir),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }
        state.sweep_worktree_presence().await;
        {
            let proto = state.protocol.read().await;
            // Local session should be updated
            let local = proto.sessions.get("local/s1").unwrap();
            assert_eq!(local.metadata.worktree_present, Some(true));
            // Remote session should be skipped (None)
            let remote = proto.sessions.get("remote/s1").unwrap();
            assert_eq!(remote.metadata.worktree_present, None);
        }
    }

    #[tokio::test]
    async fn sweep_worktree_presence_respects_backoff_after_timeout() {
        // Regression: when sweep_backoff_until is set and the window has not
        // expired, sweep_worktree_presence must skip without doing any work
        // and without touching sweep_in_progress (which is still held by the
        // orphan blocking thread that triggered the timeout).
        let state = AppState::new_for_test();
        // Simulate a prior timeout: dedup flag stays held by the orphan,
        // and backoff_until is set to the future.
        state
            .sweep_in_progress
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *state.sweep_backoff_until.lock().unwrap() =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
        let tempdir = tempfile::tempdir().unwrap();
        let project_dir = tempdir.path().to_str().unwrap().to_string();
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "local/s1".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "local/s1".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        project_dir: Some(project_dir),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }
        state.sweep_worktree_presence().await;
        {
            let proto = state.protocol.read().await;
            let session = proto.sessions.get("local/s1").unwrap();
            assert_eq!(
                session.metadata.worktree_present, None,
                "sweep should be skipped during backoff window"
            );
        }
        assert!(
            state
                .sweep_in_progress
                .load(std::sync::atomic::Ordering::Relaxed),
            "sweep_in_progress flag must remain set during backoff (orphan thread still holds it)"
        );
    }

    #[tokio::test]
    async fn sweep_worktree_presence_clears_expired_backoff_and_runs() {
        // Regression: once the backoff window has elapsed, the next sweep entry
        // clears sweep_backoff_until AND force-clears sweep_in_progress (the
        // orphan thread is presumed permanently hung; we accept the cost of
        // potentially accumulating one more orphan to keep sweeps alive).
        let state = AppState::new_for_test();
        state
            .sweep_in_progress
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *state.sweep_backoff_until.lock().unwrap() =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
        let tempdir = tempfile::tempdir().unwrap();
        let project_dir = tempdir.path().to_str().unwrap().to_string();
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "local/s1".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "local/s1".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        project_dir: Some(project_dir),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }
        state.sweep_worktree_presence().await;
        {
            let proto = state.protocol.read().await;
            let session = proto.sessions.get("local/s1").unwrap();
            assert_eq!(
                session.metadata.worktree_present,
                Some(true),
                "sweep should run after backoff window expired"
            );
        }
        assert!(
            state.sweep_backoff_until.lock().unwrap().is_none(),
            "backoff_until must be cleared once the window expires"
        );
    }

    #[tokio::test]
    async fn sweep_worktree_presence_empty_snapshot_does_not_clear_dedup_flag() {
        // Regression: when sessions_with_dirs is empty, the early return must NOT
        // call sweep_in_progress.store(false). The flag is owned by whichever caller
        // successfully ran swap(true); a caller that bypassed swap (because the
        // session snapshot was empty during transient churn) has no claim on it.
        // Clearing here would clobber a concurrent sweep's flag and let a subsequent
        // sweep run in parallel, defeating the dedup invariant.
        let state = AppState::new_for_test();
        // Simulate a concurrent sweep mid-flight: another caller has acquired the flag.
        state
            .sweep_in_progress
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // No sessions registered, so sessions_with_dirs is empty.
        state.sweep_worktree_presence().await;
        // The flag must still be true: this caller never owned it.
        assert!(
            state
                .sweep_in_progress
                .load(std::sync::atomic::Ordering::Relaxed),
            "empty-snapshot early return must not clear sweep_in_progress flag it never owned"
        );
    }

    #[tokio::test]
    async fn sweep_worktree_presence_follows_symlinks() {
        let state = AppState::new_for_test();
        let real_dir = tempfile::tempdir().unwrap();
        let real_path = real_dir.path();
        let symlink_path = real_path.join("symlink_to_dir");
        std::os::unix::fs::symlink(real_path, &symlink_path).unwrap();
        let project_dir = symlink_path.to_str().unwrap().to_string();
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "local/s1".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "local/s1".into(),
                    pane: Some("%1".into()),
                    origin: Origin::Local,
                    metadata: crate::daemon_protocol::SessionMeta {
                        project_dir: Some(project_dir),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }
        state.sweep_worktree_presence().await;
        {
            let proto = state.protocol.read().await;
            let session = proto.sessions.get("local/s1").unwrap();
            assert_eq!(
                session.metadata.worktree_present,
                Some(true),
                "symlink to existing dir should show as present"
            );
        }
    }

    #[tokio::test]
    async fn scan_recovers_orphaned_pane_claim_with_normal_base_id() {
        let state = AppState::new_for_test();
        *state.cached_assistant_panes.write().await = vec![crate::tmux::TmuxPane {
            pane_id: "%orphan".into(),
            session_name: "ouija".into(),
            pane_current_path: Some("/tmp/ouija".into()),
            process_name: Some("codex".into()),
        }];

        // This models a surviving shell pane that still carries a legacy
        // @ouija_id=ouija-2 claim after the daemon lost its session record.
        // The scanner must allocate the free base ID again so `ouija whoami`
        // resolves through the newly registered pane.
        state.scan_and_autoregister_panes().await;

        let proto = state.protocol.read().await;
        let recovered = proto
            .sessions
            .get("ouija")
            .expect("base ID should be reusable");
        assert_eq!(recovered.pane.as_deref(), Some("%orphan"));
    }

    #[tokio::test]
    async fn scan_respects_explicit_kill_suppression_window() {
        let state = AppState::new_for_test();
        *state.cached_assistant_panes.write().await = vec![crate::tmux::TmuxPane {
            pane_id: "%removing".into(),
            session_name: "ouija".into(),
            pane_current_path: Some("/tmp/ouija".into()),
            process_name: Some("codex".into()),
        }];
        state.autoregister_suppressed_panes.lock().unwrap().insert(
            "%removing".into(),
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        );

        state.scan_and_autoregister_panes().await;

        assert!(
            state.protocol.read().await.sessions.is_empty(),
            "the scanner must not resurrect a pane while explicit kill-session is in progress"
        );
    }
}
