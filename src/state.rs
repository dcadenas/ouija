use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
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
    /// Active session agents, keyed by session ID.
    session_agents: RwLock<HashMap<String, ActorRef<crate::session_agent::SessionMsg>>>,
    /// Indexed projects from projects_dir, keyed by directory basename.
    pub project_index: RwLock<HashMap<String, ProjectInfo>>,
    /// Pending remote command results: command string → oneshot senders.
    pending_commands: std::sync::Mutex<Vec<(String, tokio::sync::oneshot::Sender<String>)>>,
    /// Cached tmux panes running the coding assistant, refreshed by the reaper loop.
    pub(crate) cached_assistant_panes: RwLock<Vec<crate::tmux::TmuxPane>>,
    /// Per-fire worktree panes: pane_id → project_dir.
    /// Reaper runs `git worktree prune` when these panes die.
    pub perfire_worktree_panes: RwLock<HashMap<String, String>>,
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPrompt {
    pub pane_id: String,
    pub prompt: String,
    pub backend_session_id: Option<String>,
}

impl PendingPrompt {
    pub fn new(pane_id: String, prompt: String, backend_session_id: Option<String>) -> Self {
        Self {
            pane_id,
            prompt,
            backend_session_id,
        }
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
    /// For opencode: sent as `variant` on each `prompt_async` body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Reminder text re-injected on idle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminder: Option<String>,
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
            project_description: None,
            bulletin: None,
            worktree: false,
            model: None,
            effort: None,
            reminder: None,
            prompt: None,
            iteration: 0,
            iteration_log: Vec::new(),
            last_iteration_at: None,
            on_fire: None,
            worktree_present: None,
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

impl AppState {
    #[cfg(test)]
    pub fn new_for_test() -> Arc<Self> {
        Arc::new(Self {
            config: crate::config::OuijaConfig {
                name: "test".into(),
                npub: "npub1test".into(),
                port: 0,
                data_dir: std::path::PathBuf::from("/tmp/ouija-test-agent"),
                config_dir: std::path::PathBuf::from("/tmp/ouija-test-agent"),
            },
            protocol: RwLock::new(crate::daemon_protocol::DaemonState::new(
                "npub1test".into(),
                "test".into(),
            )),
            nodes: RwLock::new(HashMap::new()),
            message_log: RwLock::new(VecDeque::with_capacity(MAX_LOG)),
            log_file: std::path::PathBuf::from("/tmp/ouija-test-agent/messages.jsonl"),
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
            project_index: RwLock::new(HashMap::new()),
            pending_commands: std::sync::Mutex::new(Vec::new()),
            cached_assistant_panes: RwLock::new(Vec::new()),
            perfire_worktree_panes: RwLock::new(HashMap::new()),
            sweep_in_progress: std::sync::atomic::AtomicBool::new(false),
            sweep_backoff_until: std::sync::Mutex::new(None),
            backends: crate::backend::BackendRegistry::default_registry(),
            http_client: reqwest::Client::new(),
            pending_prompts: std::sync::Mutex::new(std::collections::HashMap::new()),
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
            project_index: RwLock::new(HashMap::new()),
            pending_commands: std::sync::Mutex::new(Vec::new()),
            cached_assistant_panes: RwLock::new(Vec::new()),
            perfire_worktree_panes: RwLock::new(HashMap::new()),
            sweep_in_progress: std::sync::atomic::AtomicBool::new(false),
            sweep_backoff_until: std::sync::Mutex::new(None),
            backends: crate::backend::BackendRegistry::default_registry(),
            http_client: reqwest::Client::new(),
            pending_prompts: std::sync::Mutex::new(std::collections::HashMap::new()),
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
        // Collect process names for each backend
        let mut backend_process_names: Vec<(String, Vec<String>)> = Vec::new();
        for name in self.backends.available() {
            if let Some(b) = self.backends.get(name) {
                let pnames: Vec<String> = b.process_names().iter().map(|s| s.to_string()).collect();
                backend_process_names.push((name.to_string(), pnames));
            }
        }

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

    /// Find session by pane OR backend session ID (opencode UUID).
    pub async fn find_session_by_pane_or_backend_sid(
        &self,
        pane: Option<&str>,
        backend_sid: Option<&str>,
    ) -> Option<String> {
        let proto = self.protocol.read().await;
        proto
            .sessions
            .values()
            .find(|s| {
                pane.is_some_and(|p| s.pane.as_deref() == Some(p))
                    || backend_sid
                        .is_some_and(|b| s.metadata.backend_session_id.as_deref() == Some(b))
            })
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

    async fn _apply_and_execute(
        self: &Arc<Self>,
        event: crate::daemon_protocol::Event,
    ) -> Vec<crate::daemon_protocol::Effect> {
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

        let delivery_failure = self.execute_effects(&effects).await;

        if let Some(failure) = delivery_failure {
            self.clear_pending_reply_for_failed_effect_delivery(&effects)
                .await;
            self.rollback_sender_state_for_failed_effect_delivery(rollback)
                .await;
            return rewrite_send_delivery_failure(effects, &failure.reason);
        }

        if effects.iter().any(|effect| {
            matches!(effect, crate::daemon_protocol::Effect::SendFailed { .. })
        }) {
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
                    http_delivery,
                    ..
                } => {
                    let effect_method = delivery_method.as_deref().or(recorded_method);
                    let effect_http_delivery = http_delivery.as_ref().or(recorded_http_delivery);
                    let result = match effect_method {
                        Some("http") => {
                            match effect_http_delivery {
                                Some(delivery) => crate::tmux::deliver_via_http(
                                    self,
                                    &delivery.backend_session_id,
                                    delivery.project_dir.as_deref(),
                                    message,
                                    delivery.model.as_deref(),
                                    delivery.effort.as_deref(),
                                )
                                .await,
                                None => {
                                    Err(anyhow::anyhow!(
                                        "http delivery skipped: no recorded backend_session_id on send"
                                    ))
                                }
                            }
                        }
                        Some("tmux") => {
                            crate::tmux::locked_inject_raw_tmux(
                                self, session_id, pane, message, *vim_mode,
                            )
                            .await
                        }
                        _ => crate::tmux::locked_inject(self, session_id, pane, message, *vim_mode)
                            .await,
                    };
                    if let Err(error) = result {
                        tracing::warn!(session = %session_id, "message delivery failed: {error}");
                        delivery_failure.get_or_insert_with(|| EffectDeliveryFailure {
                            reason: error.to_string(),
                        });
                    }
                }
                Effect::DeliverHttpMessage {
                    session_id,
                    message,
                    http_delivery,
                    ..
                } => {
                    match Some(http_delivery).or(recorded_http_delivery) {
                        Some(delivery) => {
                            if let Err(error) = crate::tmux::deliver_via_http(
                                self,
                                &delivery.backend_session_id,
                                delivery.project_dir.as_deref(),
                                message,
                                delivery.model.as_deref(),
                                delivery.effort.as_deref(),
                            )
                            .await
                            {
                                tracing::warn!(session = %session_id, "http delivery failed: {error}");
                                delivery_failure.get_or_insert_with(|| EffectDeliveryFailure {
                                    reason: error.to_string(),
                                });
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
                    }
                }
                Effect::SetTmuxVar { pane, name, value } => {
                    let p = pane.clone();
                    let n = name.clone();
                    let v = value.clone();
                    // FIXME: fire-and-forget — this spawn_blocking is NOT
                    // awaited, so a fast caller (e.g. `ouija clear-reminder`
                    // from the newly spawned session) can race against the
                    // `@ouija_session` pane-var write and fall through to the
                    // slower API lookup. The OUIJA_SESSION_ID env var set by
                    // `tmux::pane_env_args` at spawn time is the primary
                    // signal and masks this race in practice; do not rely on
                    // the tmux pane var as the sole session-id source.
                    tokio::task::spawn_blocking(move || crate::tmux_var::set(&p, &n, &v));
                }
                Effect::ClearTmuxVar { pane, name } => {
                    let p = pane.clone();
                    let n = name.clone();
                    tokio::task::spawn_blocking(move || crate::tmux_var::clear(&p, &n));
                }
                Effect::RenameWindow { pane, name } => {
                    let p = pane.clone();
                    let n = name.clone();
                    tokio::task::spawn_blocking(move || crate::tmux::rename_window(&p, &n));
                }
                Effect::EnableAutoRename { pane } => {
                    let p = pane.clone();
                    tokio::task::spawn_blocking(move || crate::tmux::enable_automatic_rename(&p));
                }
                Effect::SpawnAgent { session_id, pane } => {
                    self.spawn_session_agent(session_id, pane).await;
                }
                Effect::StopAgent { session_id } => {
                    if let Some(agent) = self
                        .session_agents
                        .write()
                        .await
                        .remove(session_id.as_str())
                    {
                        agent.stop(None);
                    }
                }
                Effect::RenameAgent { old_id, new_id } => {
                    let mut agents = self.session_agents.write().await;
                    if let Some(agent) = agents.remove(old_id.as_str()) {
                        let _ = agent.cast(crate::session_agent::SessionMsg::Renamed {
                            new_id: new_id.clone(),
                        });
                        agents.insert(new_id.clone(), agent);
                    }
                }
                Effect::ClearPendingReplies { removed_ids } => {
                    self.clear_orphaned_pending_replies(removed_ids).await;
                }
                Effect::Persist => {
                    let proto = self.protocol.read().await;
                    self.persist_protocol_state(&proto);
                }
                Effect::CleanupWorktree { project_dir } => {
                    let dir = project_dir.clone();
                    tokio::task::spawn(async move {
                        Self::cleanup_worktree_dir(&dir).await;
                    });
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
                            None,  // branch
                            None,  // base_branch
                            false, // force_reset — remote /start never resets (hub#528 guard)
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
                        let (result, _prompt_msg_id) = crate::nostr_transport::restart_session(
                            &state,
                            &name,
                            fresh,
                            prompt.as_deref(),
                            from.as_deref(),
                            expects_reply,
                            None,
                            None, // model
                            None, // effort
                            reminder.as_deref(),
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
        let Some((to, msg_id)) = effects.iter().find_map(|effect| match effect {
            crate::daemon_protocol::Effect::SendDelivered { to, msg_id, .. } => {
                Some((to.clone(), *msg_id))
            }
            crate::daemon_protocol::Effect::InjectMessage {
                session_id,
                pending_reply_msg_id,
                ..
            } => pending_reply_msg_id.map(|msg_id| (session_id.clone(), msg_id)),
            crate::daemon_protocol::Effect::DeliverHttpMessage {
                session_id,
                pending_reply_msg_id,
                ..
            } => pending_reply_msg_id.map(|msg_id| (session_id.clone(), msg_id)),
            _ => None,
        }) else {
            return;
        };

        let mut proto = self.protocol.write().await;
        let Some(pending) = proto.pending_replies.get_mut(&to) else {
            return;
        };
        pending.retain(|entry| entry.msg_id != msg_id);
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

    async fn finalize_successful_effect_delivery(&self, rollback: Option<FailedEffectSendRollback>) {
        let Some(rollback) = rollback else {
            return;
        };
        if !rollback.done {
            return;
        }

        let mut proto = self.protocol.write().await;
        if let Some(entry) = rollback.pending_reply_before_send {
            if let Some(pending) = proto.pending_replies.get_mut(&rollback.sender_id) {
                pending.retain(|pending| pending.msg_id != entry.msg_id || pending != &entry);
                if pending.is_empty() {
                    proto.pending_replies.remove(&rollback.sender_id);
                }
            }
        }
        if rollback.sender_reminder.is_some()
            && let Some(session) = proto.sessions.get_mut(&rollback.sender_id)
            && session.metadata.reminder == rollback.sender_reminder.flatten()
        {
            session.metadata.reminder = None;
        }
    }

    /// Persist protocol state sessions to disk.
    pub(crate) fn persist_protocol_state(&self, proto: &crate::daemon_protocol::DaemonState) {
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
                        project_description: m.project_description.clone(),
                        bulletin: m.bulletin.clone(),
                        worktree: m.worktree,
                        model: m.model.clone(),
                        effort: m.effort.clone(),
                        reminder: m.reminder.clone(),
                        prompt: m.prompt.clone(),
                        iteration: m.iteration,
                        iteration_log: m.iteration_log.clone(),
                        last_iteration_at: m.last_iteration_at,
                        on_fire: m.on_fire.clone(),
                        worktree_present: m.worktree_present,
                    },
                };
                (k.clone(), session)
            })
            .collect();
        self.persist_sessions_from(&sessions);
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

    /// Spawn a session agent for a local session.
    pub async fn spawn_session_agent(self: &Arc<Self>, id: &str, pane: &str) {
        // Stop any existing agent first (e.g. from pane dedup re-registration)
        if let Some(old) = self.session_agents.write().await.remove(id) {
            old.stop(None);
        }
        let agent = crate::session_agent::SessionAgent {
            app_state: Arc::clone(self),
        };
        let args = crate::session_agent::SessionAgentArgs {
            session_id: id.to_string(),
            pane: pane.to_string(),
        };
        match Actor::spawn(None, agent, args).await {
            Ok((actor_ref, _handle)) => {
                self.session_agents
                    .write()
                    .await
                    .insert(id.to_string(), actor_ref);
                tracing::info!("spawned session agent for {id}");
            }
            Err(e) => {
                tracing::error!("failed to spawn session agent for {id}: {e}");
            }
        }
    }

    /// Send a message to a session's agent (if it exists).
    pub async fn notify_agent(&self, session_id: &str, msg: crate::session_agent::SessionMsg) {
        let agent = {
            let agents = self.session_agents.read().await;
            agents.get(session_id).cloned()
        };
        if let Some(agent) = agent {
            let _ = agent.cast(msg);
        }
    }

    /// Query a session agent for its pending replies (RPC).
    pub async fn query_agent_pending_replies(
        &self,
        session_id: &str,
    ) -> Vec<crate::daemon_protocol::PendingReplyEntry> {
        let agents = self.session_agents.read().await;
        if let Some(agent) = agents.get(session_id) {
            ractor::call!(agent, crate::session_agent::SessionMsg::GetPendingReplies)
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// Drain the pending compact continuation from a session agent (RPC).
    /// Returns None if the agent has no pending continuation or the session has no agent.
    pub async fn drain_agent_compact_continuation(&self, session_id: &str) -> Option<String> {
        let agents = self.session_agents.read().await;
        if let Some(agent) = agents.get(session_id) {
            ractor::call!(
                agent,
                crate::session_agent::SessionMsg::DrainPendingCompactContinuation
            )
            .unwrap_or(None)
        } else {
            None
        }
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
        let agents = self.session_agents.read().await;
        if let Some(agent) = agents.get(session_id) {
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
    pub async fn collect_excess_idle_sessions(&self) -> Vec<String> {
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
        stale.iter().take(excess).map(|s| s.id.clone()).collect()
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
                    tracing::debug!("worktree sweep in backoff window after recent timeout, skipping");
                    return;
                }
                *backoff = None;
                self.sweep_in_progress
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let sessions_with_dirs: Vec<(String, String)> = {
            let proto = self.protocol.read().await;
            proto
                .sessions
                .values()
                .filter(|s| {
                    matches!(s.origin, crate::daemon_protocol::Origin::Local)
                        && s.metadata.project_dir.is_some()
                })
                .filter_map(|s| Some((s.id.clone(), s.metadata.project_dir.clone()?)))
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
        if self.sweep_in_progress.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::debug!("worktree sweep already in progress, skipping");
            return;
        }
        // Deduplicate project dirs to avoid N² stat calls
        let unique_dirs: Vec<String> = {
            let mut dirs: Vec<String> = sessions_with_dirs
                .iter()
                .map(|(_, d)| d.clone())
                .collect();
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
        ).await {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => {
                tracing::warn!("worktree sweep spawn_blocking failed: {e}");
                self.sweep_in_progress.store(false, std::sync::atomic::Ordering::Relaxed);
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
                    std::time::Instant::now()
                        + std::time::Duration::from_secs(SWEEP_BACKOFF_SECS),
                );
                return;
            }
        };
        // Only update sessions whose dirs were successfully checked
        let updates: Vec<(String, String, bool)> = sessions_with_dirs
            .into_iter()
            .filter_map(|(id, dir)| {
                presence_map.get(&dir).map(|p| (id, dir.clone(), *p))
            })
            .collect();
        if !updates.is_empty() {
            let _ = self
                .apply_and_execute(crate::daemon_protocol::Event::MarkWorktreePresence {
                    updates,
                })
                .await;
        }
        // Always reset the dedup flag, even on early return or timeout
        self.sweep_in_progress.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn persist_sessions_from(&self, sessions: &HashMap<String, Session>) {
        let persisted: Vec<_> = sessions
            .values()
            .filter_map(crate::persistence::PersistedSession::from_session)
            .collect();
        if let Err(e) = crate::persistence::save_sessions(&self.config.data_dir, &persisted) {
            tracing::warn!("failed to persist sessions: {e}");
        }
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
        let names: Vec<String> = self.backends.all_process_names();
        let panes = match tokio::task::spawn_blocking(move || {
            let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
            crate::tmux::find_assistant_panes(&name_refs)
        })
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("spawn_blocking join error: {e}")))
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("tmux scan failed: {e}");
                return;
            }
        };

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

            // Skip if the pane has an @ouija_id tmux variable — it was claimed
            // by session_start or the registration hook and may be mid-restart.
            let pane_id_check = pane.pane_id.clone();
            let has_ouija_id = tokio::task::spawn_blocking(move || {
                std::process::Command::new("tmux")
                    .args(["show-options", "-pv", "-t", &pane_id_check, "@ouija_id"])
                    .output()
                    .map(|o| o.status.success() && !o.stdout.is_empty())
                    .unwrap_or(false)
            })
            .await
            .unwrap_or(false);
            if has_ouija_id {
                continue;
            }

            let Some(ref path) = pane.pane_current_path else {
                continue;
            };

            let project_root = resolve_project_root(path);
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
            self.apply_and_execute(crate::daemon_protocol::Event::Register {
                id: id.clone(),
                pane: Some(pane.pane_id.clone()),
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
            proto.pending_replies
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

    // --- Pure functions ---

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
    async fn execute_effects_uses_recorded_tmux_method_for_send_inject() {
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use axum::Router;
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
    async fn execute_effects_delivers_http_from_recorded_snapshot_without_live_session() {
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use axum::Router;
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

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
    async fn incoming_weak_opencode_inject_uses_apply_time_delivery_method() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut config = test_config();
        config.port = port.checked_sub(320).unwrap();
        let state = AppState::new(config);
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
            session.metadata.opencode_binding = Some(
                crate::daemon_protocol::OpenCodeBinding::StrongManaged,
            );
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

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut config = test_config();
        config.port = port.checked_sub(320).unwrap();
        let state = AppState::new(config);
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
    async fn failed_incoming_delivery_clears_structured_reply_id_not_forged_xml_id() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

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
    async fn apply_and_execute_reports_headless_http_send_failure_when_prompt_async_fails() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

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
            !effects
                .iter()
                .any(|effect| matches!(effect, crate::daemon_protocol::Effect::SendDelivered { .. }))
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut config = test_config();
        config.port = port.checked_sub(320).unwrap();
        let state = AppState::new(config);
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
            crate::daemon_protocol::Effect::LogMessage { delivered: false, .. }
        )));

        let proto = state.protocol.read().await;
        assert!(!proto.pending_replies.contains_key("oc"));
    }

    #[tokio::test]
    async fn apply_and_execute_clears_incoming_pending_reply_after_headless_http_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut config = test_config();
        config.port = port.checked_sub(320).unwrap();
        let state = AppState::new(config);
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
            crate::daemon_protocol::Effect::LogMessage { delivered: false, .. }
        )));

        let proto = state.protocol.read().await;
        assert!(!proto.pending_replies.contains_key("oc"));
    }

    #[tokio::test]
    async fn apply_and_execute_restores_sender_reply_state_after_delivery_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

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
            StatusCode::BAD_GATEWAY
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
            opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
            project_description: Some("test project".into()),
            vim_mode: true,
            worktree: true,
            model: Some("openrouter/sonnet".into()),
            effort: Some("max".into()),
            reminder: Some("remember to...".into()),
            prompt: Some("do the thing".into()),
            iteration: 3,
            iteration_log: vec![],
            last_iteration_at: Some(1_700_000_000),
            on_fire: Some(crate::scheduler::OnFire::NewSession),
            worktree_present: Some(false),
        };
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "s1".into(),
                pane: Some("%1".into()),
                metadata: meta,
            })
            .await;

        // Trigger the real persist path (same code Effect::Persist dispatches to).
        {
            let proto = state.protocol.read().await;
            state.persist_protocol_state(&proto);
        }

        // Read sessions.json back from disk.
        let loaded = crate::persistence::load_sessions(&config.data_dir)
            .expect("load_sessions after persist");
        let s = loaded
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
        assert!(hydrated.on_fire.is_some());
        assert_eq!(hydrated.last_iteration_at, Some(1_700_000_000));
        assert_eq!(hydrated.last_metadata_update, Some(1_700_000_100));
        assert_eq!(hydrated.worktree_present, Some(false));
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
}
