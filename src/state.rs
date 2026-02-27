use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::config::OuijaConfig;
use crate::persistence::OuijaSettings;
use crate::scheduler::{ScheduledTask, TaskRun};
use crate::transport::Transport;

/// Named transport map keyed by transport name (e.g. "nostr").
type TransportMap = HashMap<String, Arc<dyn Transport>>;

/// The pane is already registered under a different session ID.
#[derive(Debug)]
pub struct AlreadyRegistered(pub String);

impl std::fmt::Display for AlreadyRegistered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pane already registered as '{}'", self.0)
    }
}

impl std::error::Error for AlreadyRegistered {}

/// A peer with this npub is already connected.
#[derive(Debug)]
pub struct DuplicatePeer(pub String);

impl std::fmt::Display for DuplicatePeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for DuplicatePeer {}

pub type SharedState = Arc<AppState>;

pub struct AppState {
    pub config: OuijaConfig,
    pub sessions: RwLock<HashMap<String, Session>>,
    pub peers: RwLock<HashMap<String, PeerInfo>>,
    pub message_log: RwLock<VecDeque<LogEntry>>,
    pub log_file: PathBuf,
    transports: RwLock<TransportMap>,
    pub settings: RwLock<OuijaSettings>,
    pub scheduled_tasks: RwLock<HashMap<String, ScheduledTask>>,
    pub task_runs: RwLock<VecDeque<TaskRun>>,
    /// Per-pane serialization locks to prevent concurrent injections.
    pane_locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Serializes log file writes to prevent interleaved lines.
    log_file_lock: std::sync::Mutex<()>,
    /// Serializes task_runs.jsonl writes.
    task_run_log_lock: std::sync::Mutex<()>,
    /// Connected remote daemon npubs, prevents duplicate connections.
    /// Maps npub -> peer name.
    connected_npubs: std::sync::Mutex<HashMap<String, String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SessionMetadata {
    #[serde(default)]
    pub vim_mode: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Session {
    pub id: String,
    pub pane: Option<String>,
    pub origin: SessionOrigin,
    pub registered_at: DateTime<Utc>,
    pub metadata: SessionMetadata,
}

#[derive(Clone, Debug, Serialize)]
pub enum SessionOrigin {
    Local,
    Remote(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct PeerInfo {
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
    pub fn new(config: OuijaConfig) -> SharedState {
        let log_file = config.data_dir.join("messages.jsonl");
        let settings = crate::persistence::load_settings(&config.data_dir).unwrap_or_default();
        let scheduled_tasks =
            crate::persistence::load_tasks(&config.data_dir).unwrap_or_default();
        Arc::new(Self {
            config,
            sessions: RwLock::new(HashMap::new()),
            peers: RwLock::new(HashMap::new()),
            message_log: RwLock::new(VecDeque::with_capacity(MAX_LOG)),
            log_file,
            transports: RwLock::new(HashMap::new()),
            settings: RwLock::new(settings),
            scheduled_tasks: RwLock::new(scheduled_tasks),
            task_runs: RwLock::new(VecDeque::with_capacity(MAX_TASK_RUNS)),
            pane_locks: std::sync::Mutex::new(HashMap::new()),
            log_file_lock: std::sync::Mutex::new(()),
            task_run_log_lock: std::sync::Mutex::new(()),
            connected_npubs: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Register a connected peer by npub.
    ///
    /// Returns the existing peer name if this npub is already connected.
    pub fn try_add_peer(&self, npub: &str, name: &str) -> Result<(), DuplicatePeer> {
        let mut connected = self.connected_npubs.lock().expect("connected_npubs poisoned");
        if let Some(existing) = connected.get(npub) {
            return Err(DuplicatePeer(existing.clone()));
        }
        connected.insert(npub.to_string(), name.to_string());
        Ok(())
    }

    /// Get or create a per-pane serialization lock.
    pub fn pane_lock(&self, pane: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.pane_locks.lock().expect("pane_locks poisoned");
        locks
            .entry(pane.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
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

    pub async fn register_session(
        &self,
        id: String,
        pane: Option<String>,
        metadata: SessionMetadata,
    ) -> Result<Session, AlreadyRegistered> {
        let mut sessions = self.sessions.write().await;

        // Reject if this pane is already registered under a different ID.
        // Use /api/rename (or peer_send to rename) to change session names.
        if let Some(ref pane_id) = pane {
            let old_key = sessions
                .iter()
                .find(|(key, s)| {
                    *key != &id
                        && matches!(s.origin, SessionOrigin::Local)
                        && s.pane.as_deref() == Some(pane_id)
                })
                .map(|(key, _)| key.clone());
            if let Some(old_key) = old_key {
                return Err(AlreadyRegistered(old_key));
            }
        }

        let session = Session {
            id: id.clone(),
            pane,
            origin: SessionOrigin::Local,
            registered_at: Utc::now(),
            metadata,
        };
        sessions.insert(id, session.clone());
        self.persist_sessions_from(&sessions);
        Ok(session)
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
        sessions.insert(new_id.to_string(), session.clone());
        self.persist_sessions_from(&sessions);
        Some(session)
    }

    pub async fn remove_session(&self, id: &str) -> Option<Session> {
        let mut sessions = self.sessions.write().await;
        let session = sessions.get(id)?;
        if matches!(session.origin, SessionOrigin::Remote(_)) {
            return None;
        }
        let removed = sessions.remove(id);
        self.persist_sessions_from(&sessions);
        removed
    }

    /// Remove local sessions whose tmux panes have died.
    pub async fn reap_dead_sessions(&self) -> Vec<String> {
        let panes_to_check: Vec<(String, String)> = {
            let sessions = self.sessions.read().await;
            sessions
                .values()
                .filter(|s| matches!(s.origin, SessionOrigin::Local) && s.pane.is_some())
                .map(|s| (s.id.clone(), s.pane.clone().unwrap()))
                .collect()
        };

        let dead_ids = tokio::task::spawn_blocking(move || {
            panes_to_check
                .into_iter()
                .filter(|(_, pane)| !crate::tmux::pane_alive(pane))
                .map(|(id, _)| id)
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();

        if !dead_ids.is_empty() {
            let mut sessions = self.sessions.write().await;
            for id in &dead_ids {
                sessions.remove(id);
                tracing::info!("reaped dead session: {id}");
            }
            self.persist_sessions_from(&sessions);
        }
        dead_ids
    }

    fn persist_sessions_from(&self, sessions: &HashMap<String, Session>) {
        let persisted: Vec<_> = sessions
            .values()
            .filter_map(crate::persistence::PersistedSession::from_session)
            .collect();
        if let Err(e) = crate::persistence::save_sessions(&self.config.data_dir, &persisted) {
            tracing::warn!("failed to persist sessions: {e}");
        }
    }

    // --- Scheduled Tasks ---

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
            let _guard = self.task_run_log_lock.lock().expect("task_run_log_lock poisoned");
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
mod tests {
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

    // --- AppState async tests ---

    fn test_config() -> OuijaConfig {
        let dir = tempfile::tempdir().unwrap();
        OuijaConfig {
            name: "test".into(),
            data_dir: dir.keep(),
            port: 0,
            npub: "npub1test".into(),
        }
    }

    #[tokio::test]
    async fn register_session_basic() {
        let state = AppState::new(test_config());
        let session = state
            .register_session("s1".into(), Some("%1".into()), SessionMetadata::default())
            .await
            .unwrap();
        assert_eq!(session.id, "s1");
        assert_eq!(session.pane.as_deref(), Some("%1"));

        let sessions = state.sessions.read().await;
        assert_eq!(sessions.len(), 1);
        assert!(sessions.contains_key("s1"));
    }

    #[tokio::test]
    async fn register_session_dedup_by_pane() {
        let state = AppState::new(test_config());
        state
            .register_session("old".into(), Some("%1".into()), SessionMetadata::default())
            .await
            .unwrap();
        let err = state
            .register_session("new".into(), Some("%1".into()), SessionMetadata::default())
            .await
            .unwrap_err();
        assert_eq!(err.0, "old");

        let sessions = state.sessions.read().await;
        assert_eq!(sessions.len(), 1);
        assert!(sessions.contains_key("old"));
        assert!(!sessions.contains_key("new"));
    }

    #[tokio::test]
    async fn register_session_same_id_overwrites() {
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
            .await
            .unwrap();
        state
            .register_session(
                "s1".into(),
                Some("%2".into()),
                SessionMetadata {
                    vim_mode: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let sessions = state.sessions.read().await;
        assert_eq!(sessions.len(), 1);
        let s = sessions.get("s1").unwrap();
        assert_eq!(s.pane.as_deref(), Some("%2"));
        assert!(s.metadata.vim_mode);
    }

    #[tokio::test]
    async fn rename_session_basic() {
        let state = AppState::new(test_config());
        state
            .register_session("old".into(), Some("%1".into()), SessionMetadata::default())
            .await
            .unwrap();
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
            .await
            .unwrap();
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
            .await
            .unwrap();
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
                Session {
                    id: "remote/s1".into(),
                    pane: None,
                    origin: SessionOrigin::Remote("remote".into()),
                    registered_at: Utc::now(),
                    metadata: SessionMetadata::default(),
                },
            );
        }
        assert!(state.remove_session("remote/s1").await.is_none());
        assert_eq!(state.sessions.read().await.len(), 1);
    }

    #[tokio::test]
    async fn log_message_caps_at_max() {
        let state = AppState::new(test_config());
        for i in 0..150 {
            state
                .log_message(
                    "from".into(),
                    "to".into(),
                    format!("msg {i}"),
                    true,
                    "test",
                )
                .await;
        }
        let log = state.message_log.read().await;
        assert_eq!(log.len(), MAX_LOG);
    }
}
