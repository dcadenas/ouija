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
use crate::tmux_var;
use crate::transport::Transport;

/// Grace period before a local session's tmux pane is checked for liveness.
const REAPER_GRACE_SECS: i64 = 15;

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
/// If the path is inside a `.claude/worktrees/<branch>` directory, walk up to
/// the repo root so autoregistration derives the project name, not the branch.
pub fn resolve_project_root(path: &str) -> &str {
    // Look for `/.claude/worktrees/` in the path
    if let Some(idx) = path.find("/.claude/worktrees/") {
        &path[..idx]
    } else {
        path
    }
}

/// A pending reply that a session owes to a sender.
#[derive(Clone, Debug)]
pub struct PendingReply {
    pub from: String,
    pub message: String,
    pub received_at: DateTime<Utc>,
    /// Whether the session has already been reminded about this pending reply.
    pub reminded: bool,
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

/// Result of [`AppState::register_session`].
#[derive(Debug)]
pub enum RegisterResult {
    Ok {
        session: Box<Session>,
        /// The old session ID if this registration replaced an existing one on the same pane.
        replaced: Option<String>,
    },
    /// Another local session already owns this ID on a different pane.
    Conflict { existing_pane: String },
}

pub type SharedState = Arc<AppState>;

pub struct AppState {
    pub config: OuijaConfig,
    pub sessions: RwLock<HashMap<String, Session>>,
    pub nodes: RwLock<HashMap<String, NodeInfo>>,
    pub message_log: RwLock<VecDeque<LogEntry>>,
    pub log_file: PathBuf,
    transports: RwLock<TransportMap>,
    pub settings: RwLock<OuijaSettings>,
    pub scheduled_tasks: RwLock<HashMap<String, ScheduledTask>>,
    pub task_runs: RwLock<VecDeque<TaskRun>>,
    /// Per-pane FIFO injection queues (each backed by a background worker).
    pane_queues: std::sync::Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<crate::tmux::InjectRequest>>>,
    /// Serializes log file writes to prevent interleaved lines.
    log_file_lock: std::sync::Mutex<()>,
    /// Serializes task_runs.jsonl writes.
    task_run_log_lock: std::sync::Mutex<()>,
    /// Connected remote daemon npubs, prevents duplicate connections.
    /// Maps npub -> node name.
    connected_npubs: std::sync::Mutex<HashMap<String, String>>,
    /// Tracks old session names after rename/re-register (old_id → new_id).
    session_aliases: RwLock<HashMap<String, String>>,
    /// Pane IDs suppressed from auto-register (cleared on daemon restart).
    /// Debounce: last time we reciprocated a session list to each node.
    last_reciprocated: std::sync::Mutex<HashMap<String, std::time::Instant>>,
    /// Active session agents, keyed by session ID.
    session_agents: RwLock<HashMap<String, ActorRef<crate::session_agent::SessionMsg>>>,
    /// Indexed projects from projects_dir, keyed by directory basename.
    pub project_index: RwLock<HashMap<String, ProjectInfo>>,
    /// Pending remote command results: command string → oneshot senders.
    pending_commands: std::sync::Mutex<Vec<(String, tokio::sync::oneshot::Sender<String>)>>,
    /// Cached tmux panes running Claude, refreshed by the reaper loop.
    cached_claude_panes: RwLock<Vec<crate::tmux::TmuxPane>>,
    /// Per-fire worktree panes: pane_id → project_dir.
    /// Reaper runs `git worktree prune` when these panes die.
    pub perfire_worktree_panes: RwLock<HashMap<String, String>>,
}

/// Metadata becomes stale after 30 minutes without an update.
const METADATA_STALE_SECS: i64 = 1800;

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
    /// Claude Code conversation/session ID (UUID) for `--resume` on restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_session_id: Option<String>,
    /// Short project description extracted from Cargo.toml, package.json, or README.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_description: Option<String>,
    /// Free-form bulletin: what this session needs, offers, or is working on.
    /// Used by the pairing evaluator to discover collaboration opportunities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bulletin: Option<String>,
    /// Whether this session runs in an isolated git worktree (claude --worktree).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub worktree: bool,
}

fn default_true() -> bool {
    true
}

impl SessionMetadata {
    /// Returns `true` if metadata has never been explicitly set or is older
    /// than [`METADATA_STALE_SECS`].
    pub fn is_stale(&self) -> bool {
        match self.last_metadata_update {
            None => true,
            Some(ts) => Utc::now().signed_duration_since(ts).num_seconds() > METADATA_STALE_SECS,
        }
    }
}

impl Default for SessionMetadata {
    fn default() -> Self {
        Self {
            vim_mode: false,
            project_dir: None,
            role: None,
            networked: true,
            last_metadata_update: None,
            claude_session_id: None,
            project_description: None,
            bulletin: None,
            worktree: false,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Session {
    pub id: String,
    pub pane: Option<String>,
    pub origin: SessionOrigin,
    pub registered_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
    pub metadata: SessionMetadata,
    /// Block interactive prompts (AskUserQuestion, EnterPlanMode).
    /// Set on tmux injection, cleared when the user types directly.
    #[serde(skip)]
    pub block_interactive: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SessionOrigin {
    Local,
    Remote(String),
    /// A human Nostr user. The String is their npub.
    Human(String),
}

impl SessionOrigin {
    /// Short label for JSON APIs: `"local"`, `"remote"`, `"human"`.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote(_) => "remote",
            Self::Human(_) => "human",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct NodeInfo {
    pub name: String,
    pub daemon_id: String,
    pub connected_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LogEntry {
    pub timestamp: DateTime<Utc>,
    pub from: String,
    pub to: String,
    pub message: String,
    pub delivered: bool,
}

pub fn remote_session_key(daemon_name: &str, raw_id: &str) -> String {
    format!("{daemon_name}/{raw_id}")
}

pub fn strip_remote_prefix(prefixed_id: &str) -> &str {
    prefixed_id
        .split_once('/')
        .map(|(_, raw)| raw)
        .unwrap_or(prefixed_id)
}

const MAX_LOG: usize = 100;
const MAX_TASK_RUNS: usize = 200;

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
            sessions: RwLock::new(HashMap::new()),
            nodes: RwLock::new(HashMap::new()),
            message_log: RwLock::new(VecDeque::with_capacity(100)),
            log_file: std::path::PathBuf::from("/tmp/ouija-test-agent/messages.jsonl"),
            transports: RwLock::new(HashMap::new()),
            settings: RwLock::new(Default::default()),
            scheduled_tasks: RwLock::new(HashMap::new()),
            task_runs: RwLock::new(VecDeque::with_capacity(100)),
            pane_queues: std::sync::Mutex::new(HashMap::new()),
            log_file_lock: std::sync::Mutex::new(()),
            task_run_log_lock: std::sync::Mutex::new(()),
            connected_npubs: std::sync::Mutex::new(HashMap::new()),
            session_aliases: RwLock::new(HashMap::new()),
            last_reciprocated: std::sync::Mutex::new(HashMap::new()),
            session_agents: RwLock::new(HashMap::new()),
            project_index: RwLock::new(HashMap::new()),
            pending_commands: std::sync::Mutex::new(Vec::new()),
            cached_claude_panes: RwLock::new(Vec::new()),
            perfire_worktree_panes: RwLock::new(HashMap::new()),
        })
    }

    pub fn new(config: OuijaConfig) -> SharedState {
        let log_file = config.data_dir.join("messages.jsonl");
        let settings = crate::persistence::load_settings(&config.config_dir).unwrap_or_default();
        let scheduled_tasks = crate::persistence::load_tasks(&config.data_dir).unwrap_or_default();
        Arc::new(Self {
            config,
            sessions: RwLock::new(HashMap::new()),
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
            session_aliases: RwLock::new(HashMap::new()),
            last_reciprocated: std::sync::Mutex::new(HashMap::new()),
            session_agents: RwLock::new(HashMap::new()),
            project_index: RwLock::new(HashMap::new()),
            pending_commands: std::sync::Mutex::new(Vec::new()),
            cached_claude_panes: RwLock::new(Vec::new()),
            perfire_worktree_panes: RwLock::new(HashMap::new()),
        })
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
        let mut sessions = self.sessions.write().await;
        let to_remove: Vec<String> = sessions
            .iter()
            .filter(|(_, s)| matches!(&s.origin, SessionOrigin::Remote(d) if d == daemon_id))
            .map(|(key, _)| key.clone())
            .collect();
        let count = to_remove.len();
        for key in &to_remove {
            sessions.remove(key);
        }
        drop(sessions);

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

    /// Check if a session should be visible to remote nodes.
    pub fn is_session_networked(&self, session: &Session) -> bool {
        session.metadata.networked
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
        let agents = self.session_agents.read().await;
        if let Some(agent) = agents.get(session_id) {
            let _ = agent.cast(msg);
        }
    }

    /// Register (or re-register) a session.
    ///
    /// If the pane is already registered under a different name, the old
    /// session is removed and its ID is returned in `replaced`.
    pub async fn register_session(
        &self,
        id: String,
        pane: Option<String>,
        metadata: SessionMetadata,
    ) -> RegisterResult {
        let mut sessions = self.sessions.write().await;

        // Reject if same ID is already registered on a different pane
        if let Some(ref new_pane) = pane {
            if let Some(existing) = sessions.get(&id) {
                if matches!(existing.origin, SessionOrigin::Local) {
                    if let Some(ref old_pane) = existing.pane {
                        if old_pane != new_pane {
                            tracing::warn!(
                                "session '{id}' already registered on pane {old_pane}, rejecting registration from {new_pane}"
                            );
                            return RegisterResult::Conflict {
                                existing_pane: old_pane.clone(),
                            };
                        }
                    }
                }
            }
        }

        // Dedup: if this pane is already registered under a different ID, remove the old entry
        let replaced = if let Some(ref pane_id) = pane {
            let old_key = sessions
                .iter()
                .find(|(key, s)| {
                    *key != &id
                        && matches!(s.origin, SessionOrigin::Local)
                        && s.pane.as_deref() == Some(pane_id)
                })
                .map(|(key, _)| key.clone());
            if let Some(ref old_key) = old_key {
                tracing::info!("pane {pane_id} re-registered: removing old session '{old_key}'");
                sessions.remove(old_key);
            }
            old_key
        } else {
            None
        };

        let mut metadata = metadata;
        metadata.last_metadata_update = Some(Utc::now());
        let now = Utc::now();
        let session = Session {
            id: id.clone(),
            pane,
            origin: SessionOrigin::Local,
            registered_at: now,
            last_activity_at: now,
            metadata,
            block_interactive: false,
        };
        sessions.insert(id.clone(), session.clone());
        self.persist_sessions_from(&sessions);

        // Set tmux user variable so window names can show the session ID.
        // If the pane is the only pane in its window, also rename the window.
        if let Some(ref pane_id) = session.pane {
            let pane = pane_id.clone();
            let name = id.clone();
            tokio::task::spawn_blocking(move || {
                tmux_var::set(&pane, &name);
                if crate::tmux::is_sole_pane(&pane) {
                    crate::tmux::rename_window(&pane, &name);
                }
            });
        }

        // Record alias so sends to the old name get a helpful hint
        if let Some(ref old_key) = replaced {
            self.add_alias(old_key, &id).await;
        }

        RegisterResult::Ok {
            session: Box::new(session),
            replaced,
        }
    }

    pub async fn rename_session(&self, old_id: &str, new_id: &str) -> Option<Session> {
        if new_id.contains('/') {
            return None;
        }
        let mut sessions = self.sessions.write().await;
        let session = sessions.get(old_id)?;
        if matches!(session.origin, SessionOrigin::Remote(_)) {
            return None;
        }
        let mut session = sessions.remove(old_id)?;
        session.id = new_id.to_string();
        // Update tmux user variable and window name to new name
        if let Some(ref pane_id) = session.pane {
            let pane = pane_id.clone();
            let name = new_id.to_string();
            tokio::task::spawn_blocking(move || {
                tmux_var::set(&pane, &name);
                if crate::tmux::is_sole_pane(&pane) {
                    crate::tmux::rename_window(&pane, &name);
                }
            });
        }
        sessions.insert(new_id.to_string(), session.clone());
        self.persist_sessions_from(&sessions);
        drop(sessions);
        self.add_alias(old_id, new_id).await;
        // Re-key agent ref
        let mut agents = self.session_agents.write().await;
        if let Some(agent_ref) = agents.remove(old_id) {
            agents.insert(new_id.to_string(), agent_ref);
        }
        Some(session)
    }

    /// Update a local session's role, project_dir, and/or bulletin.
    ///
    /// Stamps `last_metadata_update` and persists. Returns the updated
    /// session, or `None` if the session doesn't exist or is remote.
    pub async fn update_session_metadata(
        &self,
        id: &str,
        role: Option<String>,
        project_dir: Option<String>,
        bulletin: Option<String>,
    ) -> Option<Session> {
        let mut sessions = self.sessions.write().await;
        let session = sessions.get_mut(id)?;
        if matches!(session.origin, SessionOrigin::Remote(_)) {
            return None;
        }
        if let Some(r) = role {
            session.metadata.role = Some(r);
        }
        if let Some(p) = project_dir {
            session.metadata.project_dir = Some(p);
        }
        if let Some(b) = bulletin {
            session.metadata.bulletin = Some(b);
        }
        session.metadata.last_metadata_update = Some(Utc::now());
        let snapshot = session.clone();
        self.persist_sessions_from(&sessions);
        drop(sessions);

        Some(snapshot)
    }

    /// Record an alias from an old session name to a new one.
    ///
    /// Also updates any existing aliases that pointed to `old_id` so they
    /// resolve directly to `new_id` (no chains).
    pub(crate) async fn add_alias(&self, old_id: &str, new_id: &str) {
        let mut aliases = self.session_aliases.write().await;
        // Update any alias that previously pointed to old_id
        for target in aliases.values_mut() {
            if *target == old_id {
                *target = new_id.to_string();
            }
        }
        aliases.insert(old_id.to_string(), new_id.to_string());
    }

    /// Look up what a stale session name was renamed to.
    ///
    /// Returns the current name only if that session still exists.
    pub async fn resolve_alias(&self, id: &str) -> Option<String> {
        let new_id = self.session_aliases.read().await.get(id)?.clone();
        if self.sessions.read().await.contains_key(&new_id) {
            Some(new_id)
        } else {
            None
        }
    }

    /// Query a session agent for its pending replies (RPC).
    pub async fn query_agent_pending_replies(&self, session_id: &str) -> Vec<PendingReply> {
        let agents = self.session_agents.read().await;
        if let Some(agent) = agents.get(session_id) {
            ractor::call!(agent, crate::session_agent::SessionMsg::GetPendingReplies)
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    pub async fn remove_session(&self, id: &str) -> Option<Session> {
        let mut sessions = self.sessions.write().await;
        let session = sessions.get(id)?;
        if matches!(session.origin, SessionOrigin::Remote(_)) {
            return None;
        }
        // Clear tmux user variable and re-enable automatic window naming
        if let Some(ref pane_id) = session.pane {
            let pane = pane_id.clone();
            tokio::task::spawn_blocking(move || {
                tmux_var::clear(&pane);
                crate::tmux::enable_automatic_rename(&pane);
            });
        }
        let removed = sessions.remove(id);
        self.persist_sessions_from(&sessions);
        drop(sessions);
        // Stop session agent
        if let Some(agent) = self.session_agents.write().await.remove(id) {
            agent.stop(None);
        }
        removed
    }

    /// Remove local sessions whose tmux panes have died.
    pub async fn reap_dead_sessions(&self) -> Vec<String> {
        let now = Utc::now();
        let grace_secs = REAPER_GRACE_SECS;
        let panes_to_check: Vec<(String, String)> = {
            let sessions = self.sessions.read().await;
            sessions
                .values()
                .filter(|s| {
                    matches!(s.origin, SessionOrigin::Local)
                        && s.pane.is_some()
                        && now.signed_duration_since(s.registered_at).num_seconds() > grace_secs
                })
                .map(|s| (s.id.clone(), s.pane.clone().unwrap()))
                .collect()
        };

        let dead_entries = tokio::task::spawn_blocking(move || {
            panes_to_check
                .into_iter()
                .filter(|(_, pane)| !crate::tmux::pane_alive(pane))
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();

        let dead_ids: Vec<String> = dead_entries.iter().map(|(id, _)| id.clone()).collect();

        if !dead_ids.is_empty() {
            // Clear tmux user variables and re-enable automatic window naming
            for (_, pane) in &dead_entries {
                let pane = pane.clone();
                tokio::task::spawn_blocking(move || {
                    tmux_var::clear(&pane);
                    crate::tmux::enable_automatic_rename(&pane);
                });
            }
            let mut sessions = self.sessions.write().await;
            for id in &dead_ids {
                sessions.remove(id);
                tracing::info!("reaped dead session: {id}");
            }
            self.persist_sessions_from(&sessions);
            drop(sessions);
            // Stop agents for reaped sessions
            let mut agents = self.session_agents.write().await;
            for id in &dead_ids {
                if let Some(agent) = agents.remove(id.as_str()) {
                    agent.stop(None);
                }
            }
        }
        // Clean up per-fire worktree panes that have died
        let perfire_to_check: Vec<(String, String)> = {
            let pf = self.perfire_worktree_panes.read().await;
            pf.iter()
                .map(|(pane, dir)| (pane.clone(), dir.clone()))
                .collect()
        };
        if !perfire_to_check.is_empty() {
            let dead_perfire = tokio::task::spawn_blocking(move || {
                perfire_to_check
                    .into_iter()
                    .filter(|(pane, _)| !crate::tmux::pane_alive(pane))
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();

            if !dead_perfire.is_empty() {
                let mut pf = self.perfire_worktree_panes.write().await;
                for (pane_id, project_dir) in dead_perfire {
                    pf.remove(&pane_id);
                    tracing::info!(
                        "per-fire worktree pane {pane_id} died, pruning worktrees in {project_dir}"
                    );
                    let _ = tokio::task::spawn_blocking(move || {
                        std::process::Command::new("git")
                            .args(["-C", &project_dir, "worktree", "prune"])
                            .status()
                    })
                    .await;
                }
            }
        }

        dead_ids
    }

    /// If local session count exceeds `max_local_sessions`, return the most
    /// idle sessions that should be closed to bring the count back to the limit.
    pub async fn collect_excess_idle_sessions(&self) -> Vec<String> {
        let max = self.settings.read().await.max_local_sessions as usize;
        if max == 0 {
            return vec![];
        }
        let sessions = self.sessions.read().await;
        let mut local: Vec<_> = sessions
            .values()
            .filter(|s| matches!(s.origin, SessionOrigin::Local))
            .collect();
        if local.len() <= max {
            return vec![];
        }
        // Sort by last_activity_at ascending (most idle first)
        local.sort_by_key(|s| s.last_activity_at);
        let excess = local.len() - max;
        local[..excess].iter().map(|s| s.id.clone()).collect()
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

    pub async fn cached_claude_panes(&self) -> Vec<crate::tmux::TmuxPane> {
        self.cached_claude_panes.read().await.clone()
    }

    /// Broadcast removal/announcement and spawn session agent after registration.
    pub async fn announce_and_activate(
        self: &Arc<Self>,
        session: &Session,
        replaced: Option<&str>,
    ) {
        if let Some(old_id) = replaced {
            let msg = crate::protocol::WireMessage::SessionRenamed {
                old_id: old_id.to_string(),
                new_id: session.id.clone(),
                daemon_id: self.config.npub.clone(),
                daemon_name: self.config.name.clone(),
                metadata: Some(session.metadata.clone()),
            };
            crate::transport::broadcast(self, &msg).await;
        } else if self.is_session_networked(session) {
            let msg = crate::protocol::WireMessage::SessionAnnounce {
                id: session.id.clone(),
                daemon_id: self.config.npub.clone(),
                daemon_name: self.config.name.clone(),
                metadata: Some(session.metadata.clone()),
            };
            crate::transport::broadcast(self, &msg).await;
        }

        if let Some(ref pane_id) = session.pane {
            self.spawn_session_agent(&session.id, pane_id).await;
        }
    }

    /// Scan tmux for Claude panes, update cache, and auto-register unregistered ones.
    pub async fn scan_and_autoregister_panes(self: &Arc<Self>) {
        let panes = match tokio::task::spawn_blocking(crate::tmux::find_claude_panes)
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
        *self.cached_claude_panes.write().await = panes.clone();

        let auto_register = self.settings.read().await.auto_register;
        if !auto_register {
            return;
        }

        // Build lookup tables from current sessions (single lock acquisition)
        let (registered_panes, id_to_pane) = {
            let sessions = self.sessions.read().await;
            let registered: std::collections::HashSet<String> = sessions
                .values()
                .filter(|s| matches!(s.origin, SessionOrigin::Local))
                .filter_map(|s| s.pane.clone())
                .collect();
            let id_to_pane: std::collections::HashMap<String, Option<String>> = sessions
                .iter()
                .map(|(id, s)| (id.clone(), s.pane.clone()))
                .collect();
            (registered, id_to_pane)
        };

        for pane in &panes {
            if registered_panes.contains(&pane.pane_id) {
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

            // Resolve name conflicts using pre-computed map (no lock re-acquisition)
            let mut id = base_id.clone();
            let mut suffix = 2u32;
            while let Some(existing_pane) = id_to_pane.get(&id) {
                if existing_pane.as_deref() == Some(pane.pane_id.as_str()) {
                    break; // Same pane, register_session handles idempotent update
                }
                id = format!("{base_id}-{suffix}");
                suffix += 1;
                if suffix > 100 {
                    tracing::warn!("could not find available name for pane {}", pane.pane_id);
                    break;
                }
            }

            let description = crate::api::extract_project_description(project_root);
            let metadata = SessionMetadata {
                project_dir: Some(project_root.to_string()),
                role: Some(format!("working on {basename}")),
                project_description: description,
                ..Default::default()
            };

            tracing::info!("auto-registering pane {} as '{id}'", pane.pane_id);
            let result = self
                .register_session(id.clone(), Some(pane.pane_id.clone()), metadata)
                .await;

            if let RegisterResult::Ok { session, replaced } = result {
                self.announce_and_activate(&session, replaced.as_deref())
                    .await;
            }
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
            if now.duration_since(*last) < std::time::Duration::from_secs(30) {
                return false;
            }
        }
        map.insert(daemon_id.to_string(), now);
        true
    }

    /// Register a oneshot sender for a pending remote command result.
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
        let sessions = self.sessions.read().await;
        let mut entries: Vec<(&str, bool, Option<&str>, Option<&str>)> = sessions
            .values()
            .filter(|s| matches!(s.origin, SessionOrigin::Local))
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
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // --- Pure functions ---

    #[test]
    fn remote_session_key_format() {
        assert_eq!(remote_session_key("daemon1", "sess"), "daemon1/sess");
    }

    #[test]
    fn strip_remote_prefix_with_slash() {
        assert_eq!(strip_remote_prefix("daemon1/sess"), "sess");
    }

    #[test]
    fn strip_remote_prefix_no_slash() {
        assert_eq!(strip_remote_prefix("local-id"), "local-id");
    }

    #[test]
    fn strip_remote_prefix_multiple_slashes() {
        assert_eq!(strip_remote_prefix("a/b/c"), "b/c");
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
    async fn register_session_basic() {
        let state = AppState::new(test_config());
        let result = state
            .register_session("s1".into(), Some("%1".into()), SessionMetadata::default())
            .await;
        let RegisterResult::Ok { session, .. } = result else {
            panic!("expected Ok");
        };
        assert_eq!(session.id, "s1");
        assert_eq!(session.pane.as_deref(), Some("%1"));

        let sessions = state.sessions.read().await;
        assert_eq!(sessions.len(), 1);
        assert!(sessions.contains_key("s1"));
    }

    #[tokio::test]
    async fn register_session_dedup_by_pane() {
        let state = AppState::new(test_config());
        let r1 = state
            .register_session("old".into(), Some("%1".into()), SessionMetadata::default())
            .await;
        let RegisterResult::Ok { replaced, .. } = r1 else {
            panic!("expected Ok");
        };
        assert!(replaced.is_none());

        let r2 = state
            .register_session("new".into(), Some("%1".into()), SessionMetadata::default())
            .await;
        let RegisterResult::Ok { replaced, .. } = r2 else {
            panic!("expected Ok");
        };
        assert_eq!(replaced.as_deref(), Some("old"));

        let sessions = state.sessions.read().await;
        assert_eq!(sessions.len(), 1);
        assert!(sessions.contains_key("new"));
        assert!(!sessions.contains_key("old"));
    }

    #[tokio::test]
    async fn register_session_same_id_different_pane_conflicts() {
        let state = AppState::new(test_config());
        state
            .register_session("s1".into(), Some("%1".into()), SessionMetadata::default())
            .await;
        let r2 = state
            .register_session("s1".into(), Some("%2".into()), SessionMetadata::default())
            .await;

        assert!(
            matches!(r2, RegisterResult::Conflict { existing_pane } if existing_pane == "%1"),
            "same ID on different pane should conflict"
        );

        // Original session should be unchanged
        let sessions = state.sessions.read().await;
        assert_eq!(sessions.len(), 1);
        let s = sessions.get("s1").unwrap();
        assert_eq!(s.pane.as_deref(), Some("%1"));
    }

    #[tokio::test]
    async fn register_session_same_id_same_pane_updates() {
        let state = AppState::new(test_config());
        state
            .register_session(
                "s1".into(),
                Some("%1".into()),
                SessionMetadata {
                    vim_mode: false,
                    ..Default::default()
                },
            )
            .await;
        let r2 = state
            .register_session(
                "s1".into(),
                Some("%1".into()),
                SessionMetadata {
                    vim_mode: true,
                    ..Default::default()
                },
            )
            .await;

        let RegisterResult::Ok { session, .. } = r2 else {
            panic!("same ID + same pane should succeed");
        };
        assert!(session.metadata.vim_mode);
    }

    #[tokio::test]
    async fn rename_session_basic() {
        let state = AppState::new(test_config());
        state
            .register_session("old".into(), Some("%1".into()), SessionMetadata::default())
            .await;
        let renamed = state.rename_session("old", "new").await;
        assert!(renamed.is_some());
        assert_eq!(renamed.unwrap().id, "new");

        let sessions = state.sessions.read().await;
        assert!(!sessions.contains_key("old"));
        assert!(sessions.contains_key("new"));
    }

    #[tokio::test]
    async fn rename_session_rejects_slash() {
        let state = AppState::new(test_config());
        state
            .register_session("s1".into(), Some("%1".into()), SessionMetadata::default())
            .await;
        assert!(state.rename_session("s1", "has/slash").await.is_none());
        assert!(state.sessions.read().await.contains_key("s1"));
    }

    #[tokio::test]
    async fn rename_nonexistent_returns_none() {
        let state = AppState::new(test_config());
        assert!(state.rename_session("nope", "new").await.is_none());
    }

    #[tokio::test]
    async fn remove_session_basic() {
        let state = AppState::new(test_config());
        state
            .register_session("s1".into(), Some("%1".into()), SessionMetadata::default())
            .await;
        let removed = state.remove_session("s1").await;
        assert!(removed.is_some());
        assert!(state.sessions.read().await.is_empty());
    }

    #[tokio::test]
    async fn remove_nonexistent_returns_none() {
        let state = AppState::new(test_config());
        assert!(state.remove_session("nope").await.is_none());
    }

    #[tokio::test]
    async fn remove_remote_session_returns_none() {
        let state = AppState::new(test_config());
        {
            let mut sessions = state.sessions.write().await;
            sessions.insert(
                "remote/s1".into(),
                test_session(
                    "remote/s1",
                    None,
                    SessionOrigin::Remote("remote".into()),
                    SessionMetadata::default(),
                ),
            );
        }
        assert!(state.remove_session("remote/s1").await.is_none());
        assert_eq!(state.sessions.read().await.len(), 1);
    }

    /// Helper to build a Session for tests, filling in `last_activity_at` automatically.
    fn test_session(
        id: &str,
        pane: Option<&str>,
        origin: SessionOrigin,
        metadata: SessionMetadata,
    ) -> Session {
        Session {
            id: id.into(),
            pane: pane.map(Into::into),
            origin,
            registered_at: Utc::now(),
            last_activity_at: Utc::now(),
            metadata,
            block_interactive: false,
        }
    }

    #[tokio::test]
    async fn resolve_alias_after_rename() {
        let state = AppState::new(test_config());
        state
            .register_session("old".into(), Some("%1".into()), SessionMetadata::default())
            .await;
        state.rename_session("old", "new").await;
        assert_eq!(state.resolve_alias("old").await.as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn resolve_alias_after_dedup() {
        let state = AppState::new(test_config());
        state
            .register_session("old".into(), Some("%1".into()), SessionMetadata::default())
            .await;
        state
            .register_session("new".into(), Some("%1".into()), SessionMetadata::default())
            .await;
        assert_eq!(state.resolve_alias("old").await.as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn resolve_alias_chain_flattened() {
        let state = AppState::new(test_config());
        state
            .register_session("a".into(), Some("%1".into()), SessionMetadata::default())
            .await;
        state.rename_session("a", "b").await;
        state.rename_session("b", "c").await;
        // Both old names resolve directly to the final name
        assert_eq!(state.resolve_alias("a").await.as_deref(), Some("c"));
        assert_eq!(state.resolve_alias("b").await.as_deref(), Some("c"));
    }

    #[tokio::test]
    async fn resolve_alias_returns_none_if_target_gone() {
        let state = AppState::new(test_config());
        state
            .register_session("old".into(), Some("%1".into()), SessionMetadata::default())
            .await;
        state.rename_session("old", "new").await;
        state.remove_session("new").await;
        // Target no longer exists, alias should not resolve
        assert!(state.resolve_alias("old").await.is_none());
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

    // --- Networked filtering ---

    #[test]
    fn is_session_networked_true() {
        let state = AppState::new(test_config());
        let session = test_session(
            "s1",
            Some("%1"),
            SessionOrigin::Local,
            SessionMetadata {
                networked: true,
                ..Default::default()
            },
        );
        assert!(state.is_session_networked(&session));
    }

    #[test]
    fn is_session_networked_false() {
        let state = AppState::new(test_config());
        let session = test_session(
            "s1",
            Some("%1"),
            SessionOrigin::Local,
            SessionMetadata {
                networked: false,
                ..Default::default()
            },
        );
        assert!(!state.is_session_networked(&session));
    }

    #[tokio::test]
    async fn local_session_hash_changes_on_networked_toggle() {
        let state = AppState::new(test_config());
        state
            .register_session("s1".into(), Some("%1".into()), SessionMetadata::default())
            .await;

        let hash_networked = state.local_session_hash().await;

        // Toggle s1 to non-networked
        {
            let mut sessions = state.sessions.write().await;
            sessions.get_mut("s1").unwrap().metadata.networked = false;
        }
        let hash_not_networked = state.local_session_hash().await;

        assert_ne!(hash_networked, hash_not_networked);
    }

    #[tokio::test]
    async fn disconnect_node_removes_sessions() {
        let state = AppState::new(test_config());
        // Add a remote session
        {
            let mut sessions = state.sessions.write().await;
            sessions.insert(
                "remote/s1".into(),
                test_session(
                    "remote/s1",
                    None,
                    SessionOrigin::Remote("npub1remote".into()),
                    SessionMetadata::default(),
                ),
            );
            sessions.insert(
                "remote/s2".into(),
                test_session(
                    "remote/s2",
                    None,
                    SessionOrigin::Remote("npub1remote".into()),
                    SessionMetadata::default(),
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
        assert!(state.sessions.read().await.is_empty());
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

    // --- Metadata staleness ---

    #[test]
    fn is_stale_when_never_set() {
        let meta = SessionMetadata::default();
        assert!(meta.is_stale());
    }

    #[test]
    fn is_stale_when_recent() {
        let meta = SessionMetadata {
            last_metadata_update: Some(Utc::now()),
            ..Default::default()
        };
        assert!(!meta.is_stale());
    }

    #[test]
    fn is_stale_when_old() {
        let old = Utc::now() - chrono::Duration::seconds(METADATA_STALE_SECS + 1);
        let meta = SessionMetadata {
            last_metadata_update: Some(old),
            ..Default::default()
        };
        assert!(meta.is_stale());
    }

    #[test]
    fn backward_compat_missing_last_metadata_update() {
        // Old JSON without last_metadata_update should deserialize with None
        let json = r#"{"vim_mode": false, "networked": true}"#;
        let meta: SessionMetadata = serde_json::from_str(json).unwrap();
        assert!(meta.last_metadata_update.is_none());
        assert!(meta.is_stale());
    }

    #[tokio::test]
    async fn register_session_stamps_metadata_update() {
        let state = AppState::new(test_config());
        let result = state
            .register_session("s1".into(), Some("%1".into()), SessionMetadata::default())
            .await;
        let RegisterResult::Ok { session, .. } = result else {
            panic!("expected Ok");
        };
        assert!(session.metadata.last_metadata_update.is_some());
        assert!(!session.metadata.is_stale());
    }

    #[tokio::test]
    async fn update_session_metadata_sets_role_and_stamps() {
        let state = AppState::new(test_config());
        state
            .register_session("s1".into(), Some("%1".into()), SessionMetadata::default())
            .await;

        let updated = state
            .update_session_metadata("s1", Some("debugging auth".into()), None, None)
            .await;
        assert!(updated.is_some());
        let s = updated.unwrap();
        assert_eq!(s.metadata.role.as_deref(), Some("debugging auth"));
        assert!(!s.metadata.is_stale());
    }

    #[tokio::test]
    async fn update_session_metadata_remote_returns_none() {
        let state = AppState::new(test_config());
        {
            let mut sessions = state.sessions.write().await;
            sessions.insert(
                "remote/s1".into(),
                test_session(
                    "remote/s1",
                    None,
                    SessionOrigin::Remote("remote".into()),
                    SessionMetadata::default(),
                ),
            );
        }
        assert!(
            state
                .update_session_metadata("remote/s1", Some("role".into()), None, None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn update_session_metadata_nonexistent_returns_none() {
        let state = AppState::new(test_config());
        assert!(
            state
                .update_session_metadata("nope", Some("role".into()), None, None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn local_session_hash_changes_on_role_update() {
        let state = AppState::new(test_config());
        state
            .register_session("s1".into(), Some("%1".into()), SessionMetadata::default())
            .await;

        let hash_before = state.local_session_hash().await;

        state
            .update_session_metadata("s1", Some("new role".into()), None, None)
            .await;

        let hash_after = state.local_session_hash().await;
        assert_ne!(hash_before, hash_after);
    }

    #[tokio::test]
    async fn update_metadata_sets_bulletin() {
        let state = AppState::new(test_config());
        state
            .register_session("s1".into(), Some("%1".into()), SessionMetadata::default())
            .await;

        let result = state
            .update_session_metadata("s1", None, None, Some("offering review".into()))
            .await;
        assert!(result.is_some());
        let sessions = state.sessions.read().await;
        assert_eq!(
            sessions["s1"].metadata.bulletin.as_deref(),
            Some("offering review")
        );
    }

    // --- collect_excess_idle_sessions ---

    #[tokio::test]
    async fn excess_idle_disabled_when_zero() {
        let state = AppState::new(test_config());
        // max_local_sessions defaults to 0 (disabled)
        state
            .register_session("s1".into(), Some("%1".into()), SessionMetadata::default())
            .await;
        assert!(state.collect_excess_idle_sessions().await.is_empty());
    }

    #[tokio::test]
    async fn excess_idle_no_eviction_at_limit() {
        let state = AppState::new(test_config());
        state.settings.write().await.max_local_sessions = 2;
        state
            .register_session("s1".into(), Some("%1".into()), SessionMetadata::default())
            .await;
        state
            .register_session("s2".into(), Some("%2".into()), SessionMetadata::default())
            .await;
        assert!(state.collect_excess_idle_sessions().await.is_empty());
    }

    #[tokio::test]
    async fn excess_idle_evicts_most_idle() {
        let state = AppState::new(test_config());
        state.settings.write().await.max_local_sessions = 2;

        // Insert 3 sessions with controlled last_activity_at
        {
            let mut sessions = state.sessions.write().await;
            let mut old = test_session(
                "old",
                Some("%1"),
                SessionOrigin::Local,
                SessionMetadata::default(),
            );
            old.last_activity_at = Utc::now() - chrono::Duration::hours(3);
            sessions.insert("old".into(), old);

            let mut mid = test_session(
                "mid",
                Some("%2"),
                SessionOrigin::Local,
                SessionMetadata::default(),
            );
            mid.last_activity_at = Utc::now() - chrono::Duration::hours(1);
            sessions.insert("mid".into(), mid);

            let fresh = test_session(
                "fresh",
                Some("%3"),
                SessionOrigin::Local,
                SessionMetadata::default(),
            );
            sessions.insert("fresh".into(), fresh);
        }

        let evicted = state.collect_excess_idle_sessions().await;
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0], "old");
    }

    #[tokio::test]
    async fn excess_idle_ignores_remote_and_human() {
        let state = AppState::new(test_config());
        state.settings.write().await.max_local_sessions = 1;

        // 1 local + 1 remote + 1 human = only 1 local, at limit
        {
            let mut sessions = state.sessions.write().await;
            sessions.insert(
                "local".into(),
                test_session(
                    "local",
                    Some("%1"),
                    SessionOrigin::Local,
                    SessionMetadata::default(),
                ),
            );
            sessions.insert(
                "remote/r1".into(),
                test_session(
                    "remote/r1",
                    None,
                    SessionOrigin::Remote("npub1x".into()),
                    SessionMetadata::default(),
                ),
            );
            sessions.insert(
                "human".into(),
                test_session(
                    "human",
                    None,
                    SessionOrigin::Human("npub1h".into()),
                    SessionMetadata::default(),
                ),
            );
        }

        assert!(state.collect_excess_idle_sessions().await.is_empty());
    }
}
