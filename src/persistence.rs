use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use serde::de::DeserializeOwned;

use crate::scheduler::{ScheduledTask, TaskRun};
use crate::state::{Session, SessionMetadata, SessionOrigin};

/// Load a JSON file, returning `default` if the file doesn't exist.
fn load_json<T: DeserializeOwned>(path: &Path, default: T) -> Result<T> {
    match std::fs::read_to_string(path) {
        Ok(data) => Ok(serde_json::from_str(&data)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(default),
        Err(e) => Err(e.into()),
    }
}

/// Atomically write JSON to a file (write to .tmp, then rename).
fn save_json<T: Serialize>(path: &Path, value: &T, pretty: bool) -> Result<()> {
    let data = if pretty {
        serde_json::to_string_pretty(value)?
    } else {
        serde_json::to_string(value)?
    };
    atomic_write(path, data.as_bytes())
}

/// On-disk representation of a local session for restart recovery.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedSession {
    pub id: String,
    pub pane: Option<String>,
    pub registered_at: DateTime<Utc>,
    #[serde(default = "Utc::now")]
    pub last_activity_at: DateTime<Utc>,
    pub metadata: SessionMetadata,
}

pub const SESSION_STATE_VERSION: u32 = 2;

/// Versioned atomic snapshot of local sessions and lifecycle authority.
#[derive(Debug, Serialize, Deserialize)]
pub struct PersistedLifecycleState {
    pub version: u32,
    pub sessions: Vec<PersistedSession>,
    #[serde(default)]
    pub dormant_sessions: BTreeMap<String, crate::daemon_protocol::DormantSession>,
    #[serde(default)]
    pub incarnation_high_water: crate::daemon_protocol::SessionIncarnation,
    #[serde(default)]
    pub lifecycle_leases: BTreeMap<String, crate::daemon_protocol::LifecycleLease>,
    /// Outstanding reply obligations, keyed by the session that owes them.
    ///
    /// `serde(default)` so a `sessions.json` written before this field existed
    /// still loads; it simply restores with no obligations, which is the old
    /// behaviour.
    #[serde(default)]
    pub pending_replies: BTreeMap<String, Vec<crate::daemon_protocol::PendingReplyEntry>>,
}

impl Default for PersistedLifecycleState {
    fn default() -> Self {
        Self {
            version: SESSION_STATE_VERSION,
            sessions: Vec::new(),
            dormant_sessions: BTreeMap::new(),
            incarnation_high_water: Default::default(),
            lifecycle_leases: BTreeMap::new(),
            pending_replies: BTreeMap::new(),
        }
    }
}

impl PersistedLifecycleState {
    pub fn new(
        sessions: Vec<PersistedSession>,
        dormant_sessions: BTreeMap<String, crate::daemon_protocol::DormantSession>,
        incarnation_high_water: crate::daemon_protocol::SessionIncarnation,
        lifecycle_leases: BTreeMap<String, crate::daemon_protocol::LifecycleLease>,
    ) -> Self {
        Self {
            version: SESSION_STATE_VERSION,
            sessions,
            dormant_sessions,
            incarnation_high_water,
            lifecycle_leases,
            pending_replies: BTreeMap::new(),
        }
        .normalized()
    }

    /// Attach outstanding reply obligations to a snapshot before saving.
    #[must_use]
    pub fn with_pending_replies(
        mut self,
        pending_replies: BTreeMap<String, Vec<crate::daemon_protocol::PendingReplyEntry>>,
    ) -> Self {
        self.pending_replies = pending_replies;
        self
    }

    fn normalized(mut self) -> Self {
        let session_max = self
            .sessions
            .iter()
            .map(|session| session.metadata.session_incarnation)
            .max()
            .unwrap_or_default();
        let dormant_max = self
            .dormant_sessions
            .values()
            .flat_map(|dormant| {
                [
                    dormant.prior_owner.incarnation,
                    dormant.metadata.session_incarnation,
                ]
            })
            .max()
            .unwrap_or_default();
        let lease_max = self
            .lifecycle_leases
            .values()
            .flat_map(|lease| {
                std::iter::once(lease.owner.incarnation)
                    .chain(
                        lease
                            .inert_pane_owner
                            .as_ref()
                            .map(|owner| owner.incarnation),
                    )
                    .chain(
                        lease
                            .project_dir_owner
                            .as_ref()
                            .map(|owner| owner.incarnation),
                    )
                    .chain(
                        lease
                            .backend_session_owner
                            .as_ref()
                            .map(|owner| owner.incarnation),
                    )
                    .chain(
                        lease
                            .restart_target_owner
                            .as_ref()
                            .map(|owner| owner.incarnation),
                    )
                    .chain(
                        lease
                            .restart_previous
                            .as_ref()
                            .map(|session| session.metadata.session_incarnation),
                    )
            })
            .max()
            .unwrap_or_default();
        self.incarnation_high_water = self
            .incarnation_high_water
            .max(session_max)
            .max(dormant_max)
            .max(lease_max);
        self
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PersistedLifecycleStateFile {
    Versioned(PersistedLifecycleState),
    Legacy(Vec<PersistedSession>),
}

impl PersistedSession {
    /// Convert a live session to its persisted form (local only).
    pub fn from_session(session: &Session) -> Option<Self> {
        // Only persist Local sessions; Remote and Human are restored differently.
        if !matches!(session.origin, SessionOrigin::Local) {
            return None;
        }
        Some(Self {
            id: session.id.clone(),
            pane: session.pane.clone(),
            registered_at: session.registered_at,
            last_activity_at: session.last_activity_at,
            metadata: session.metadata.clone(),
        })
    }
}

/// On-disk representation of a remote node connection.
#[derive(Debug, Serialize, Deserialize)]
pub struct PersistedConnection {
    pub ticket: String,
    pub connected_at: DateTime<Utc>,
    #[serde(default)]
    pub node_name: Option<String>,
    #[serde(default)]
    pub daemon_npub: Option<String>,
}

// --- Sessions ---

/// Load persisted sessions from `sessions.json`.
///
/// # Errors
///
/// Returns an error if the file exists but contains invalid JSON.
pub fn load_sessions(data_dir: &Path) -> Result<PersistedLifecycleState> {
    let path = data_dir.join("sessions.json");
    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PersistedLifecycleState::default());
        }
        Err(error) => return Err(error.into()),
    };
    let mut state = match serde_json::from_str::<PersistedLifecycleStateFile>(&data)? {
        PersistedLifecycleStateFile::Versioned(mut state) => match state.version {
            1 => {
                state.version = SESSION_STATE_VERSION;
                state.dormant_sessions.clear();
                migrate_legacy_project_identities(&mut state.sessions);
                state
            }
            SESSION_STATE_VERSION => state,
            version => {
                anyhow::bail!(
                    "unsupported sessions.json version {} (expected 1 or {})",
                    version,
                    SESSION_STATE_VERSION
                );
            }
        },
        PersistedLifecycleStateFile::Legacy(mut sessions) => {
            migrate_legacy_project_identities(&mut sessions);
            PersistedLifecycleState::new(
                sessions,
                BTreeMap::new(),
                Default::default(),
                BTreeMap::new(),
            )
        }
    };
    validate_session_authority(&state)?;
    for (session_id, lease) in &state.lifecycle_leases {
        if lease.owner.session_id != *session_id {
            anyhow::bail!(
                "lifecycle lease key '{}' does not match owner '{}'",
                session_id,
                lease.owner.session_id
            );
        }
        if let Some(inert_owner) = &lease.inert_pane_owner
            && inert_owner.session_id != *session_id
        {
            anyhow::bail!(
                "lifecycle lease key '{}' does not match inert pane owner '{}'",
                session_id,
                inert_owner.session_id
            );
        }
        if let Some(project_dir_owner) = &lease.project_dir_owner
            && project_dir_owner.session_id != *session_id
        {
            anyhow::bail!(
                "lifecycle lease key '{}' does not match project directory owner '{}'",
                session_id,
                project_dir_owner.session_id
            );
        }
        if let Some(backend_session_owner) = &lease.backend_session_owner
            && backend_session_owner.session_id != *session_id
        {
            anyhow::bail!(
                "lifecycle lease key '{}' does not match backend session owner '{}'",
                session_id,
                backend_session_owner.session_id
            );
        }
        if let Some(backend_session_owner) = &lease.backend_session_owner {
            let expected_owner = match lease.phase {
                crate::daemon_protocol::LifecyclePhase::Stopping => Some(&lease.owner),
                crate::daemon_protocol::LifecyclePhase::Restarting => {
                    lease.restart_target_owner.as_ref()
                }
                crate::daemon_protocol::LifecyclePhase::Starting => None,
            };
            if expected_owner != Some(backend_session_owner) {
                anyhow::bail!(
                    "lifecycle lease '{}' backend cleanup owner does not match its lifecycle phase",
                    session_id
                );
            }
        }
        let backend_claim_fields = [
            lease.backend.is_some(),
            lease.backend_session_id.is_some(),
            lease.backend_session_owner.is_some(),
        ];
        if backend_claim_fields.iter().any(|present| *present)
            && !backend_claim_fields.iter().all(|present| *present)
        {
            anyhow::bail!(
                "lifecycle lease '{}' has an incomplete backend abort claim",
                session_id
            );
        }
        if lease.backend.is_some()
            && !matches!(
                lease.phase,
                crate::daemon_protocol::LifecyclePhase::Stopping
                    | crate::daemon_protocol::LifecyclePhase::Restarting
            )
        {
            anyhow::bail!(
                "lifecycle lease '{}' has a backend cleanup claim in an invalid phase",
                session_id
            );
        }
        if lease.restart_target_owner.is_some() != lease.restart_previous.is_some() {
            anyhow::bail!(
                "lifecycle lease '{}' has an incomplete restart target claim",
                session_id
            );
        }
        if let (Some(target), Some(previous)) =
            (&lease.restart_target_owner, &lease.restart_previous)
        {
            if lease.phase != crate::daemon_protocol::LifecyclePhase::Restarting {
                anyhow::bail!(
                    "non-restarting lifecycle lease '{}' has a restart target claim",
                    session_id
                );
            }
            if target.session_id != *session_id
                || previous.id != *session_id
                || previous.owner() != lease.owner
                || !matches!(previous.origin, crate::daemon_protocol::Origin::Local)
                || target.incarnation <= lease.owner.incarnation
            {
                anyhow::bail!(
                    "lifecycle lease '{}' has inconsistent restart ownership",
                    session_id
                );
            }
        }
        if lease.project_dir.is_some() != lease.project_dir_owner.is_some() {
            anyhow::bail!(
                "lifecycle lease '{}' has an incomplete project directory claim",
                session_id
            );
        }
        if lease.project_dir_cleanup_on_abandon && lease.project_dir.is_none() {
            anyhow::bail!(
                "lifecycle lease '{}' grants cleanup without a project directory claim",
                session_id
            );
        }
    }
    state = state.normalized();
    Ok(state)
}

fn migrate_legacy_project_identities(sessions: &mut [PersistedSession]) {
    for session in sessions {
        if session.metadata.canonical_project_identity.is_some() {
            continue;
        }
        let Some(project_dir) = session.metadata.project_dir.as_deref() else {
            continue;
        };
        let path = Path::new(project_dir);
        if !path.is_absolute() || path.parent().is_none() || !path.is_dir() {
            continue;
        }
        let Ok(identity) = crate::project_identity::resolve_project_identity(project_dir) else {
            continue;
        };
        session.metadata.project_dir = Some(identity.project_dir);
        session.metadata.canonical_project_identity = Some(identity.canonical_repository);
    }
}

fn validate_session_authority(state: &PersistedLifecycleState) -> Result<()> {
    use crate::daemon_protocol::{ResourceOwner, SessionMeta};

    fn complete_pair(metadata: &SessionMeta) -> Result<Option<(String, String)>> {
        match (
            metadata.backend.as_deref(),
            metadata.backend_session_id.as_deref(),
        ) {
            (None, None) => Ok(None),
            (Some(backend), Some(backend_session_id))
                if !backend.is_empty() && !backend_session_id.is_empty() =>
            {
                Ok(Some((backend.to_string(), backend_session_id.to_string())))
            }
            _ => anyhow::bail!("session metadata has an incomplete backend pair"),
        }
    }

    fn claim_pair(
        claims: &mut BTreeMap<(String, String), ResourceOwner>,
        pair: Option<(String, String)>,
        owner: ResourceOwner,
    ) -> Result<()> {
        let Some(pair) = pair else {
            return Ok(());
        };
        if let Some(existing) = claims.get(&pair)
            && existing != &owner
        {
            anyhow::bail!(
                "backend pair ({}, {}) is assigned to distinct owners '{}' and '{}'",
                pair.0,
                pair.1,
                existing.session_id,
                owner.session_id
            );
        }
        claims.insert(pair, owner);
        Ok(())
    }

    fn usable_project(value: &str) -> bool {
        value.starts_with('/') && value != "/"
    }

    let mut live_ids = BTreeSet::new();
    let mut backend_claims = BTreeMap::new();
    for session in &state.sessions {
        if !live_ids.insert(session.id.as_str()) {
            anyhow::bail!("duplicate persisted live session ID '{}'", session.id);
        }
        let owner = ResourceOwner {
            session_id: session.id.clone(),
            incarnation: session.metadata.session_incarnation,
        };
        claim_pair(
            &mut backend_claims,
            match (
                session.metadata.backend.as_deref(),
                session.metadata.backend_session_id.as_deref(),
            ) {
                (None, None) => None,
                (Some(backend), Some(backend_session_id))
                    if !backend.is_empty() && !backend_session_id.is_empty() =>
                {
                    Some((backend.to_string(), backend_session_id.to_string()))
                }
                // Live compatibility rows may predate a complete backend
                // identity or represent an abandoned staged launch. They
                // remain loadable, but an incomplete pair reserves nothing:
                // dormancy, recovery, and self-claim paths independently
                // require a complete pair before granting authority.
                _ => None,
            },
            owner,
        )?;
    }

    for (key, dormant) in &state.dormant_sessions {
        if live_ids.contains(key.as_str()) {
            anyhow::bail!("session ID '{key}' is both live and dormant");
        }
        if key != &dormant.id
            || key != &dormant.prior_owner.session_id
            || dormant.metadata.session_incarnation != dormant.prior_owner.incarnation
        {
            anyhow::bail!("dormant session key '{key}' disagrees with its embedded owner");
        }
        let actual_project = dormant.metadata.project_dir.as_deref().unwrap_or_default();
        if !usable_project(actual_project)
            || !usable_project(&dormant.canonical_project_identity)
            || dormant.metadata.canonical_project_identity.as_deref()
                != Some(dormant.canonical_project_identity.as_str())
        {
            anyhow::bail!("dormant session '{key}' has unsafe or inconsistent project identity");
        }
        if dormant.metadata.active_context_segment_started_at.is_some() {
            anyhow::bail!("dormant session '{key}' has an open active-context segment");
        }
        claim_pair(
            &mut backend_claims,
            complete_pair(&dormant.metadata)?,
            dormant.prior_owner.clone(),
        )?;
    }

    for lease in state.lifecycle_leases.values() {
        if let (Some(backend), Some(backend_session_id), Some(owner)) = (
            lease.backend.as_deref(),
            lease.backend_session_id.as_deref(),
            lease.backend_session_owner.as_ref(),
        ) {
            if backend.is_empty() || backend_session_id.is_empty() {
                anyhow::bail!(
                    "lifecycle lease '{}' has an incomplete backend pair",
                    lease.owner.session_id
                );
            }
            claim_pair(
                &mut backend_claims,
                Some((backend.to_string(), backend_session_id.to_string())),
                owner.clone(),
            )?;
        }
        if let Some(previous) = lease.restart_previous.as_deref() {
            claim_pair(
                &mut backend_claims,
                complete_pair(&previous.metadata)?,
                previous.owner(),
            )?;
        }
    }

    Ok(())
}

/// Atomically write sessions to `sessions.json`.
///
/// # Errors
///
/// Returns an error if serialization or file I/O fails.
pub fn save_sessions(data_dir: &Path, state: &PersistedLifecycleState) -> Result<()> {
    save_json(&data_dir.join("sessions.json"), state, false)
}

// --- Connections ---

/// Load persisted connections from `connections.json`.
///
/// # Errors
///
/// Returns an error if the file exists but contains invalid JSON.
pub fn load_connections(data_dir: &Path) -> Result<Vec<PersistedConnection>> {
    load_json(&data_dir.join("connections.json"), vec![])
}

/// Add or update a connection, deduplicating by npub or ticket.
///
/// # Errors
///
/// Returns an error if reading or writing `connections.json` fails.
pub fn add_connection(
    data_dir: &Path,
    ticket: &str,
    node_name: Option<&str>,
    daemon_npub: Option<&str>,
) -> Result<()> {
    let mut conns = load_connections(data_dir).unwrap_or_default();
    // Dedup by npub (preferred) or by ticket string (fallback)
    if let Some(npub) = daemon_npub {
        conns.retain(|c| c.daemon_npub.as_deref() != Some(npub));
    }
    conns.retain(|c| c.ticket != ticket);
    conns.push(PersistedConnection {
        ticket: ticket.to_string(),
        connected_at: Utc::now(),
        node_name: node_name.map(String::from),
        daemon_npub: daemon_npub.map(String::from),
    });
    save_json(&data_dir.join("connections.json"), &conns, false)
}

/// Remove the `connections.json` file, if it exists.
///
/// # Errors
///
/// Returns an error if file removal fails for reasons other than not found.
pub fn clear_connections(data_dir: &Path) -> Result<()> {
    match std::fs::remove_file(data_dir.join("connections.json")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// --- Settings ---

/// Configuration for a human Nostr user who can interact via DMs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HumanSession {
    pub npub: String,
    pub name: String,
    #[serde(default)]
    pub default_session: Option<String>,
    /// Whether the welcome message has been sent.
    #[serde(default)]
    pub welcomed: bool,
}

/// LLM router configuration for dispatching bare-text human DMs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouterConfig {
    /// Explicit API key. If absent, falls back to `ROUTER_API_KEY` or `GEMINI_API_KEY` env var.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_router_model")]
    pub model: String,
    #[serde(default = "default_router_base_url")]
    pub base_url: String,
}

fn default_router_model() -> String {
    "gemini-2.5-flash".to_string()
}

fn default_router_base_url() -> String {
    "https://generativelanguage.googleapis.com/v1beta/openai".to_string()
}

/// User-configurable daemon settings persisted in `settings.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OuijaSettings {
    /// Backend used when a launch or legacy session has no backend selection.
    #[serde(default = "default_backend")]
    pub default_backend: String,
    #[serde(default = "default_true")]
    pub auto_register: bool,
    #[serde(default)]
    pub human_sessions: Vec<HumanSession>,
    /// Base directory for projects (e.g. ~/code). Used by /start to resolve session dirs.
    #[serde(default)]
    pub projects_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub router: Option<RouterConfig>,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_reaper_interval")]
    pub reaper_interval_secs: u64,
    /// Max local sessions before the most idle are auto-closed. 0 = disabled.
    #[serde(default)]
    pub max_local_sessions: u64,
    /// Optional Claude Code permission mode passed as `--permission-mode`.
    /// When unset, Claude Code uses its own settings/defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_permission_mode: Option<String>,
    /// Optional CODEX_HOME override for Ouija-launched Codex sessions.
    /// When unset, Codex uses its own default (`$CODEX_HOME` or `~/.codex`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<String>,
    /// Optional Codex model aliases. Keys are the model names users pass to
    /// Ouija (for example `gemini`); values describe how Codex should launch.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub codex_model_routes: HashMap<String, CodexModelRoute>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodexModelRoute {
    /// Model name passed to Codex. If unset, the user's alias is passed through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// CODEX_HOME used for this route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_backend() -> String {
    "opencode".to_string()
}

/// Default idle timeout before a session is considered stale (seconds).
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 900;
/// Default interval between reaper sweeps (seconds).
const DEFAULT_REAPER_INTERVAL_SECS: u64 = 5;

fn default_idle_timeout() -> u64 {
    DEFAULT_IDLE_TIMEOUT_SECS
}

fn default_reaper_interval() -> u64 {
    DEFAULT_REAPER_INTERVAL_SECS
}

impl Default for OuijaSettings {
    fn default() -> Self {
        Self {
            default_backend: default_backend(),
            auto_register: true,
            human_sessions: Vec::new(),
            projects_dir: None,
            router: None,
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            reaper_interval_secs: DEFAULT_REAPER_INTERVAL_SECS,
            max_local_sessions: 0,
            claude_permission_mode: None,
            codex_home: None,
            codex_model_routes: HashMap::new(),
        }
    }
}

/// Load settings from `settings.json`, using defaults if missing.
///
/// # Errors
///
/// Returns an error if the file exists but contains invalid JSON.
pub fn load_settings(data_dir: &Path) -> Result<OuijaSettings> {
    load_json(&data_dir.join("settings.json"), OuijaSettings::default())
}

/// Atomically write settings to `settings.json` (pretty-printed).
///
/// # Errors
///
/// Returns an error if serialization or file I/O fails.
pub fn save_settings(data_dir: &Path, settings: &OuijaSettings) -> Result<()> {
    save_json(&data_dir.join("settings.json"), settings, true)
}

// --- Scheduled Tasks ---

/// Load scheduled tasks from `tasks.json` into a map keyed by ID.
///
/// # Errors
///
/// Returns an error if the file exists but contains invalid JSON.
pub fn load_tasks(data_dir: &Path) -> Result<HashMap<String, ScheduledTask>> {
    let tasks: Vec<ScheduledTask> = load_json(&data_dir.join("tasks.json"), vec![])?;
    Ok(crate::scheduler::tasks_to_map(tasks))
}

/// Atomically write scheduled tasks to `tasks.json`.
///
/// # Errors
///
/// Returns an error if serialization or file I/O fails.
pub fn save_tasks(data_dir: &Path, tasks: &HashMap<String, ScheduledTask>) -> Result<()> {
    let list: Vec<&ScheduledTask> = tasks.values().collect();
    save_json(&data_dir.join("tasks.json"), &list, false)
}

/// Append a task run record to `task_runs.jsonl`.
///
/// # Errors
///
/// Returns an error if serialization or file I/O fails.
pub fn append_task_run(data_dir: &Path, run: &TaskRun) -> Result<()> {
    let path = data_dir.join("task_runs.jsonl");
    let line = serde_json::to_string(run)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    use std::io::Write;
    writeln!(f, "{line}")?;
    Ok(())
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Session, SessionMetadata, SessionOrigin};

    fn make_local_session(id: &str, pane: Option<&str>) -> Session {
        Session {
            id: id.to_string(),
            pane: pane.map(|s| s.to_string()),
            origin: SessionOrigin::Local,
            registered_at: Utc::now(),
            last_activity_at: Utc::now(),
            metadata: SessionMetadata::default(),
        }
    }

    fn make_remote_session(id: &str, daemon: &str) -> Session {
        Session {
            id: id.to_string(),
            pane: None,
            origin: SessionOrigin::Remote(daemon.to_string()),
            registered_at: Utc::now(),
            last_activity_at: Utc::now(),
            metadata: SessionMetadata::default(),
        }
    }

    fn make_persisted_local(
        id: &str,
        incarnation: u64,
        backend: Option<(&str, &str)>,
    ) -> PersistedSession {
        let (backend, backend_session_id) = backend
            .map(|(backend, session_id)| (Some(backend.into()), Some(session_id.into())))
            .unwrap_or_default();
        PersistedSession {
            id: id.into(),
            pane: Some(format!("%{incarnation}")),
            registered_at: Utc::now(),
            last_activity_at: Utc::now(),
            metadata: SessionMetadata {
                backend,
                backend_session_id,
                session_incarnation: crate::daemon_protocol::SessionIncarnation(incarnation),
                ..Default::default()
            },
        }
    }

    fn make_dormant(
        id: &str,
        incarnation: u64,
        backend: &str,
        backend_session_id: &str,
        project_dir: &str,
        canonical_project_identity: &str,
    ) -> crate::daemon_protocol::DormantSession {
        crate::daemon_protocol::DormantSession {
            id: id.into(),
            prior_owner: crate::daemon_protocol::ResourceOwner {
                session_id: id.into(),
                incarnation: crate::daemon_protocol::SessionIncarnation(incarnation),
            },
            metadata: crate::daemon_protocol::SessionMeta {
                project_dir: Some(project_dir.into()),
                canonical_project_identity: Some(canonical_project_identity.into()),
                backend: Some(backend.into()),
                backend_session_id: Some(backend_session_id.into()),
                session_incarnation: crate::daemon_protocol::SessionIncarnation(incarnation),
                ..Default::default()
            },
            canonical_project_identity: canonical_project_identity.into(),
            dormant_at: 1_753_920_123,
            source: crate::daemon_protocol::DormancySource::Reaped,
        }
    }

    fn save_raw_snapshot(
        dir: &tempfile::TempDir,
        sessions: Vec<PersistedSession>,
        dormant_sessions: BTreeMap<String, crate::daemon_protocol::DormantSession>,
        lifecycle_leases: BTreeMap<String, crate::daemon_protocol::LifecycleLease>,
    ) {
        let snapshot = serde_json::json!({
            "version": SESSION_STATE_VERSION,
            "sessions": sessions,
            "dormant_sessions": dormant_sessions,
            "incarnation_high_water": 0,
            "lifecycle_leases": lifecycle_leases,
        });
        std::fs::write(
            dir.path().join("sessions.json"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
    }

    // --- PersistedSession::from_session ---

    #[test]
    fn from_session_local_succeeds() {
        let session = make_local_session("test", Some("%1"));
        let persisted = PersistedSession::from_session(&session);
        assert!(persisted.is_some());
        let p = persisted.unwrap();
        assert_eq!(p.id, "test");
        assert_eq!(p.pane.as_deref(), Some("%1"));
    }

    #[test]
    fn from_session_remote_returns_none() {
        let session = make_remote_session("test", "remote-daemon");
        assert!(PersistedSession::from_session(&session).is_none());
    }

    // --- Sessions ---

    #[test]
    fn sessions_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = vec![
            PersistedSession {
                id: "a".into(),
                pane: Some("%1".into()),
                registered_at: Utc::now(),
                last_activity_at: Utc::now(),
                metadata: SessionMetadata::default(),
            },
            PersistedSession {
                id: "b".into(),
                pane: None,
                registered_at: Utc::now(),
                last_activity_at: Utc::now(),
                metadata: SessionMetadata {
                    vim_mode: true,
                    project_dir: Some("/tmp".into()),
                    role: Some("dev".into()),
                    ..Default::default()
                },
            },
        ];
        let owner = crate::daemon_protocol::ResourceOwner {
            session_id: "pending".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(30),
        };
        let state = PersistedLifecycleState::new(
            sessions,
            BTreeMap::new(),
            crate::daemon_protocol::SessionIncarnation(31),
            BTreeMap::from([(
                owner.session_id.clone(),
                crate::daemon_protocol::LifecycleLease {
                    owner: owner.clone(),
                    phase: crate::daemon_protocol::LifecyclePhase::Starting,
                    backend: None,
                    backend_session_id: None,
                    backend_session_owner: None,
                    restart_target_owner: None,
                    restart_previous: None,
                    project_dir: None,
                    project_dir_owner: None,
                    project_dir_cleanup_on_abandon: false,
                    inert_pane: Some("%42".into()),
                    inert_pane_owner: Some(owner.clone()),
                },
            )]),
        );
        save_sessions(dir.path(), &state).unwrap();
        let loaded = load_sessions(dir.path()).unwrap();
        assert_eq!(loaded.version, SESSION_STATE_VERSION);
        assert_eq!(
            loaded.incarnation_high_water,
            crate::daemon_protocol::SessionIncarnation(31)
        );
        assert_eq!(loaded.lifecycle_leases["pending"].owner, owner);
        assert_eq!(
            loaded.lifecycle_leases["pending"].inert_pane.as_deref(),
            Some("%42")
        );
        assert_eq!(
            loaded.lifecycle_leases["pending"].inert_pane_owner.as_ref(),
            Some(&owner)
        );
        assert_eq!(loaded.sessions.len(), 2);
        assert_eq!(loaded.sessions[0].id, "a");
        assert_eq!(loaded.sessions[1].id, "b");
        assert!(loaded.dormant_sessions.is_empty());
        assert!(loaded.sessions[1].metadata.vim_mode);
        assert_eq!(
            loaded.sessions[1].metadata.project_dir.as_deref(),
            Some("/tmp")
        );
    }

    #[test]
    fn active_context_provisional_marker_is_backward_compatible_and_round_trips() {
        // Break caught: sessions.json written before provisional accounting
        // must restore as finalized, while staged targets must remain marked.
        let legacy: SessionMetadata = serde_json::from_str("{}").unwrap();
        assert!(!legacy.active_context_accounting_provisional);

        let staged = SessionMetadata {
            active_context_accounting_provisional: true,
            ..Default::default()
        };
        let decoded: SessionMetadata =
            serde_json::from_str(&serde_json::to_string(&staged).unwrap()).unwrap();
        assert!(decoded.active_context_accounting_provisional);
    }

    #[test]
    fn load_sessions_migrates_legacy_array_and_derives_high_water() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = vec![PersistedSession {
            id: "legacy".into(),
            pane: Some("%9".into()),
            registered_at: Utc::now(),
            last_activity_at: Utc::now(),
            metadata: SessionMetadata {
                session_incarnation: crate::daemon_protocol::SessionIncarnation(17),
                ..Default::default()
            },
        }];
        std::fs::write(
            dir.path().join("sessions.json"),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let loaded = load_sessions(dir.path()).unwrap();

        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(loaded.sessions[0].id, "legacy");
        assert_eq!(
            loaded.incarnation_high_water,
            crate::daemon_protocol::SessionIncarnation(17)
        );
        assert!(loaded.dormant_sessions.is_empty());
        assert!(loaded.lifecycle_leases.is_empty());
    }

    #[test]
    fn load_sessions_migrates_version_one_to_empty_dormancy() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = serde_json::json!({
            "version": 1,
            "sessions": [make_persisted_local("legacy-v1", 18, None)],
            "incarnation_high_water": 17,
            "lifecycle_leases": {},
        });
        std::fs::write(
            dir.path().join("sessions.json"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();

        let loaded = load_sessions(dir.path()).unwrap();

        assert_eq!(loaded.version, SESSION_STATE_VERSION);
        assert_eq!(loaded.sessions[0].id, "legacy-v1");
        assert!(loaded.dormant_sessions.is_empty());
        assert_eq!(
            loaded.incarnation_high_water,
            crate::daemon_protocol::SessionIncarnation(18)
        );
    }

    #[test]
    fn load_sessions_v1_derives_only_safe_existing_project_identity() {
        let dir = tempfile::tempdir().unwrap();
        let existing_project = dir.path().join("existing-project");
        std::fs::create_dir(&existing_project).unwrap();
        let mut safe = make_persisted_local("safe", 18, None);
        safe.metadata.project_dir = Some(existing_project.to_string_lossy().into_owned());
        let mut unsafe_root = make_persisted_local("unsafe-root", 19, None);
        unsafe_root.metadata.project_dir = Some("/".into());
        let snapshot = serde_json::json!({
            "version": 1,
            "sessions": [safe, unsafe_root],
            "incarnation_high_water": 17,
            "lifecycle_leases": {},
        });
        std::fs::write(
            dir.path().join("sessions.json"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();

        let loaded = load_sessions(dir.path()).unwrap();

        let safe = loaded
            .sessions
            .iter()
            .find(|session| session.id == "safe")
            .unwrap();
        let expected = existing_project
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            safe.metadata.project_dir.as_deref(),
            Some(expected.as_str())
        );
        assert_eq!(
            safe.metadata.canonical_project_identity.as_deref(),
            Some(expected.as_str())
        );
        let unsafe_root = loaded
            .sessions
            .iter()
            .find(|session| session.id == "unsafe-root")
            .unwrap();
        assert_eq!(unsafe_root.metadata.project_dir.as_deref(), Some("/"));
        assert!(unsafe_root.metadata.canonical_project_identity.is_none());
    }

    #[test]
    fn sessions_v2_round_trip_preserves_dormancy_metadata_and_accounting() {
        let dir = tempfile::tempdir().unwrap();
        let mut dormant = make_dormant(
            "rootfix",
            41,
            "codex-cli",
            "019fb5e7-1fd4-7861-bd29-6a4860a3be75",
            "/tmp/worktrees/rootfix",
            "/tmp/repository",
        );
        dormant.dormant_at = 1_753_920_456;
        dormant.source = crate::daemon_protocol::DormancySource::TrustedSessionEnd;
        dormant.metadata.fresh_context_after_active_secs = Some(3_600);
        dormant.metadata.active_context_accumulated_secs = 901;
        dormant.metadata.active_context_segment_started_at = None;
        dormant.metadata.active_context_restart_due = false;
        dormant.metadata.active_context_accounting_provisional = true;
        let state = PersistedLifecycleState::new(
            vec![],
            BTreeMap::from([("rootfix".into(), dormant.clone())]),
            crate::daemon_protocol::SessionIncarnation(40),
            BTreeMap::new(),
        );

        save_sessions(dir.path(), &state).unwrap();
        let loaded = load_sessions(dir.path()).unwrap();

        assert_eq!(loaded.version, 2);
        assert_eq!(loaded.dormant_sessions["rootfix"], dormant);
        assert_eq!(
            loaded.incarnation_high_water,
            crate::daemon_protocol::SessionIncarnation(41)
        );
    }

    #[test]
    fn dormant_prior_owner_advances_high_water() {
        let state = PersistedLifecycleState::new(
            vec![make_persisted_local("live", 4, None)],
            BTreeMap::from([(
                "parked".into(),
                make_dormant(
                    "parked",
                    77,
                    "codex-cli",
                    "thread-parked",
                    "/tmp/worktree",
                    "/tmp/repository",
                ),
            )]),
            crate::daemon_protocol::SessionIncarnation(3),
            BTreeMap::new(),
        );

        assert_eq!(
            state.incarnation_high_water,
            crate::daemon_protocol::SessionIncarnation(77)
        );
    }

    #[test]
    fn load_sessions_rejects_malformed_dormant_identity() {
        for mutation in ["key", "id", "owner", "incarnation", "canonical"] {
            let dir = tempfile::tempdir().unwrap();
            let mut dormant = make_dormant(
                "parked",
                10,
                "codex-cli",
                "thread-parked",
                "/tmp/worktree",
                "/tmp/repository",
            );
            let key = match mutation {
                "key" => "wrong-key",
                "id" => {
                    dormant.id = "wrong-id".into();
                    "parked"
                }
                "owner" => {
                    dormant.prior_owner.session_id = "wrong-owner".into();
                    "parked"
                }
                "incarnation" => {
                    dormant.metadata.session_incarnation =
                        crate::daemon_protocol::SessionIncarnation(11);
                    "parked"
                }
                "canonical" => {
                    dormant.metadata.canonical_project_identity = Some("/tmp/other".into());
                    "parked"
                }
                _ => unreachable!(),
            };
            save_raw_snapshot(
                &dir,
                vec![],
                BTreeMap::from([(key.into(), dormant)]),
                BTreeMap::new(),
            );

            assert!(
                load_sessions(dir.path()).is_err(),
                "mutation {mutation} must be rejected"
            );
        }
    }

    #[test]
    fn load_sessions_allows_incomplete_live_compatibility_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = make_persisted_local("legacy-live", 9, None);
        session.metadata.backend = Some("codex-cli".into());
        save_raw_snapshot(&dir, vec![session], BTreeMap::new(), BTreeMap::new());

        let loaded = load_sessions(dir.path()).unwrap();

        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(
            loaded.sessions[0].metadata.backend.as_deref(),
            Some("codex-cli")
        );
        assert!(
            loaded.sessions[0].metadata.backend_session_id.is_none(),
            "loading must not invent the missing half of a live compatibility pair"
        );
    }

    #[test]
    fn load_sessions_rejects_incomplete_dormant_backend_pair() {
        let dir = tempfile::tempdir().unwrap();
        let mut dormant = make_dormant(
            "parked",
            10,
            "codex-cli",
            "thread-parked",
            "/tmp/worktree",
            "/tmp/repository",
        );
        dormant.metadata.backend_session_id = None;
        save_raw_snapshot(
            &dir,
            vec![],
            BTreeMap::from([("parked".into(), dormant)]),
            BTreeMap::new(),
        );

        assert!(load_sessions(dir.path()).is_err());
    }

    #[test]
    fn load_sessions_rejects_incomplete_lifecycle_backend_claim() {
        let dir = tempfile::tempdir().unwrap();
        let owner = crate::daemon_protocol::ResourceOwner {
            session_id: "pending-stop".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(12),
        };
        save_raw_snapshot(
            &dir,
            vec![],
            BTreeMap::new(),
            BTreeMap::from([(
                owner.session_id.clone(),
                crate::daemon_protocol::LifecycleLease {
                    owner,
                    phase: crate::daemon_protocol::LifecyclePhase::Stopping,
                    backend: Some("opencode".into()),
                    backend_session_id: None,
                    backend_session_owner: None,
                    restart_target_owner: None,
                    restart_previous: None,
                    project_dir: None,
                    project_dir_owner: None,
                    project_dir_cleanup_on_abandon: false,
                    inert_pane: None,
                    inert_pane_owner: None,
                },
            )]),
        );

        assert!(load_sessions(dir.path()).is_err());
    }

    #[test]
    fn load_sessions_rejects_unsafe_or_active_dormant_rows() {
        for mutation in [
            "missing-actual",
            "root-actual",
            "root-canonical",
            "open-segment",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut dormant = make_dormant(
                "parked",
                10,
                "codex-cli",
                "thread-parked",
                "/tmp/worktree",
                "/tmp/repository",
            );
            match mutation {
                "missing-actual" => dormant.metadata.project_dir = None,
                "root-actual" => dormant.metadata.project_dir = Some("/".into()),
                "root-canonical" => {
                    dormant.canonical_project_identity = "/".into();
                    dormant.metadata.canonical_project_identity = Some("/".into());
                }
                "open-segment" => dormant.metadata.active_context_segment_started_at = Some(123),
                _ => unreachable!(),
            }
            save_raw_snapshot(
                &dir,
                vec![],
                BTreeMap::from([("parked".into(), dormant)]),
                BTreeMap::new(),
            );

            assert!(
                load_sessions(dir.path()).is_err(),
                "mutation {mutation} must be rejected"
            );
        }
    }

    #[test]
    fn load_sessions_rejects_live_dormant_id_collision() {
        let dir = tempfile::tempdir().unwrap();
        save_raw_snapshot(
            &dir,
            vec![make_persisted_local(
                "rootfix",
                9,
                Some(("codex-cli", "thread-live")),
            )],
            BTreeMap::from([(
                "rootfix".into(),
                make_dormant(
                    "rootfix",
                    10,
                    "codex-cli",
                    "thread-dormant",
                    "/tmp/worktree",
                    "/tmp/repository",
                ),
            )]),
            BTreeMap::new(),
        );

        assert!(load_sessions(dir.path()).is_err());
    }

    #[test]
    fn load_sessions_rejects_backend_pair_collisions_across_authority() {
        let pair = ("codex-cli", "shared-thread");
        for conflict in [
            "live-live",
            "live-dormant",
            "dormant-dormant",
            "dormant-lease",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut live = vec![];
            let mut dormant_sessions = BTreeMap::new();
            let mut leases = BTreeMap::new();
            match conflict {
                "live-live" => {
                    live.push(make_persisted_local("live-a", 1, Some(pair)));
                    live.push(make_persisted_local("live-b", 2, Some(pair)));
                }
                "live-dormant" => {
                    live.push(make_persisted_local("live", 1, Some(pair)));
                    dormant_sessions.insert(
                        "parked".into(),
                        make_dormant(
                            "parked",
                            2,
                            pair.0,
                            pair.1,
                            "/tmp/worktree",
                            "/tmp/repository",
                        ),
                    );
                }
                "dormant-dormant" => {
                    for (id, incarnation) in [("parked-a", 1), ("parked-b", 2)] {
                        dormant_sessions.insert(
                            id.into(),
                            make_dormant(
                                id,
                                incarnation,
                                pair.0,
                                pair.1,
                                &format!("/tmp/{id}"),
                                "/tmp/repository",
                            ),
                        );
                    }
                }
                "dormant-lease" => {
                    dormant_sessions.insert(
                        "parked".into(),
                        make_dormant(
                            "parked",
                            1,
                            pair.0,
                            pair.1,
                            "/tmp/worktree",
                            "/tmp/repository",
                        ),
                    );
                    let owner = crate::daemon_protocol::ResourceOwner {
                        session_id: "lease".into(),
                        incarnation: crate::daemon_protocol::SessionIncarnation(2),
                    };
                    leases.insert(
                        "lease".into(),
                        crate::daemon_protocol::LifecycleLease {
                            owner: owner.clone(),
                            phase: crate::daemon_protocol::LifecyclePhase::Stopping,
                            backend: Some(pair.0.into()),
                            backend_session_id: Some(pair.1.into()),
                            backend_session_owner: Some(owner),
                            restart_target_owner: None,
                            restart_previous: None,
                            project_dir: None,
                            project_dir_owner: None,
                            project_dir_cleanup_on_abandon: false,
                            inert_pane: None,
                            inert_pane_owner: None,
                        },
                    );
                }
                _ => unreachable!(),
            }
            save_raw_snapshot(&dir, live, dormant_sessions, leases);

            assert!(
                load_sessions(dir.path()).is_err(),
                "conflict {conflict} must be rejected"
            );
        }
    }

    #[test]
    fn restart_lease_round_trip_advances_high_water_through_target() {
        let dir = tempfile::tempdir().unwrap();
        let incumbent = crate::daemon_protocol::ResourceOwner {
            session_id: "worker".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(5),
        };
        let target = crate::daemon_protocol::ResourceOwner {
            session_id: "worker".into(),
            incarnation: crate::daemon_protocol::SessionIncarnation(9),
        };
        let previous = crate::daemon_protocol::SessionEntry {
            id: "worker".into(),
            pane: Some("%2".into()),
            metadata: crate::daemon_protocol::SessionMeta {
                backend: Some("opencode".into()),
                backend_session_id: Some("ses_incumbent".into()),
                session_incarnation: incumbent.incarnation,
                ..Default::default()
            },
            ..Default::default()
        };
        let state = PersistedLifecycleState::new(
            vec![],
            BTreeMap::new(),
            incumbent.incarnation,
            BTreeMap::from([(
                "worker".into(),
                crate::daemon_protocol::LifecycleLease {
                    owner: incumbent.clone(),
                    phase: crate::daemon_protocol::LifecyclePhase::Restarting,
                    backend: None,
                    backend_session_id: None,
                    backend_session_owner: None,
                    restart_target_owner: Some(target.clone()),
                    restart_previous: Some(Box::new(previous.clone())),
                    project_dir: None,
                    project_dir_owner: None,
                    project_dir_cleanup_on_abandon: false,
                    inert_pane: None,
                    inert_pane_owner: None,
                },
            )]),
        );

        save_sessions(dir.path(), &state).unwrap();
        let loaded = load_sessions(dir.path()).unwrap();

        assert_eq!(loaded.incarnation_high_water, target.incarnation);
        assert_eq!(
            loaded.lifecycle_leases["worker"]
                .restart_target_owner
                .as_ref(),
            Some(&target)
        );
        assert_eq!(
            loaded.lifecycle_leases["worker"]
                .restart_previous
                .as_deref(),
            Some(&previous)
        );
    }

    #[test]
    fn pending_replies_survive_a_save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let entry = crate::daemon_protocol::PendingReplyEntry {
            msg_id: 42,
            from: "turnero".into(),
            message: "which shape do you want?".into(),
            received_at: 1_700_000_000,
            last_activity: 1_700_000_001,
            in_progress: true,
        };
        let snapshot = PersistedLifecycleState::new(
            vec![],
            BTreeMap::new(),
            crate::daemon_protocol::SessionIncarnation(1),
            BTreeMap::new(),
        )
        .with_pending_replies(BTreeMap::from([(
            "worker".to_string(),
            vec![entry.clone()],
        )]));

        save_sessions(dir.path(), &snapshot).unwrap();
        let loaded = load_sessions(dir.path()).unwrap();

        // The body matters: it is what a cold coordinator needs to answer
        // without reading the recipient's backend transcript.
        assert_eq!(loaded.pending_replies["worker"], vec![entry]);
    }

    #[test]
    fn load_sessions_defaults_pending_replies_when_absent() {
        // sessions.json written before pending_replies existed must still load.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("sessions.json"),
            serde_json::json!({
                "version": SESSION_STATE_VERSION,
                "sessions": [],
            })
            .to_string(),
        )
        .unwrap();

        let loaded = load_sessions(dir.path()).unwrap();
        assert!(loaded.pending_replies.is_empty());
    }

    #[test]
    fn load_sessions_rejects_mismatched_lease_owner_key() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = PersistedLifecycleState {
            version: SESSION_STATE_VERSION,
            sessions: vec![],
            dormant_sessions: BTreeMap::new(),
            incarnation_high_water: crate::daemon_protocol::SessionIncarnation(5),
            pending_replies: BTreeMap::new(),
            lifecycle_leases: BTreeMap::from([(
                "wrong-key".into(),
                crate::daemon_protocol::LifecycleLease {
                    owner: crate::daemon_protocol::ResourceOwner {
                        session_id: "actual-owner".into(),
                        incarnation: crate::daemon_protocol::SessionIncarnation(5),
                    },
                    phase: crate::daemon_protocol::LifecyclePhase::Starting,
                    backend: None,
                    backend_session_id: None,
                    backend_session_owner: None,
                    restart_target_owner: None,
                    restart_previous: None,
                    project_dir: None,
                    project_dir_owner: None,
                    project_dir_cleanup_on_abandon: false,
                    inert_pane: None,
                    inert_pane_owner: None,
                },
            )]),
        };
        std::fs::write(
            dir.path().join("sessions.json"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();

        assert!(load_sessions(dir.path()).is_err());
    }

    #[test]
    fn load_sessions_rejects_mismatched_inert_pane_owner_key() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = PersistedLifecycleState {
            version: SESSION_STATE_VERSION,
            sessions: vec![],
            dormant_sessions: BTreeMap::new(),
            incarnation_high_water: crate::daemon_protocol::SessionIncarnation(6),
            pending_replies: BTreeMap::new(),
            lifecycle_leases: BTreeMap::from([(
                "pending".into(),
                crate::daemon_protocol::LifecycleLease {
                    owner: crate::daemon_protocol::ResourceOwner {
                        session_id: "pending".into(),
                        incarnation: crate::daemon_protocol::SessionIncarnation(5),
                    },
                    phase: crate::daemon_protocol::LifecyclePhase::Starting,
                    backend: None,
                    backend_session_id: None,
                    backend_session_owner: None,
                    restart_target_owner: None,
                    restart_previous: None,
                    project_dir: None,
                    project_dir_owner: None,
                    project_dir_cleanup_on_abandon: false,
                    inert_pane: Some("%1".into()),
                    inert_pane_owner: Some(crate::daemon_protocol::ResourceOwner {
                        session_id: "other".into(),
                        incarnation: crate::daemon_protocol::SessionIncarnation(6),
                    }),
                },
            )]),
        };
        std::fs::write(
            dir.path().join("sessions.json"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();

        assert!(load_sessions(dir.path()).is_err());
    }

    #[test]
    fn load_sessions_rejects_unsupported_envelope_version() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = serde_json::json!({
            "version": SESSION_STATE_VERSION + 1,
            "sessions": [],
            "incarnation_high_water": 0,
            "lifecycle_leases": {}
        });
        std::fs::write(
            dir.path().join("sessions.json"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();

        assert!(load_sessions(dir.path()).is_err());
    }

    #[test]
    fn load_sessions_missing_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_sessions(dir.path()).unwrap().sessions.is_empty());
    }

    #[test]
    fn load_sessions_corrupt_json_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sessions.json"), "{bad").unwrap();
        assert!(load_sessions(dir.path()).is_err());
    }

    // --- Connections ---

    #[test]
    fn connections_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        add_connection(dir.path(), "ticket-abc", None, None).unwrap();
        add_connection(dir.path(), "ticket-def", Some("remote1"), None).unwrap();
        let loaded = load_connections(dir.path()).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].ticket, "ticket-abc");
        assert!(loaded[0].node_name.is_none());
        assert_eq!(loaded[1].ticket, "ticket-def");
        assert_eq!(loaded[1].node_name.as_deref(), Some("remote1"));
    }

    #[test]
    fn add_connection_deduplicates() {
        let dir = tempfile::tempdir().unwrap();
        add_connection(dir.path(), "ticket-abc", None, None).unwrap();
        add_connection(dir.path(), "ticket-abc", None, None).unwrap();
        let loaded = load_connections(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
    }

    #[test]
    fn node_name_backward_compat() {
        let dir = tempfile::tempdir().unwrap();
        // Simulate old format without node_name field
        let old_json = r#"[{"ticket":"old-ticket","connected_at":"2025-01-01T00:00:00Z"}]"#;
        std::fs::write(dir.path().join("connections.json"), old_json).unwrap();
        let loaded = load_connections(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].node_name.is_none());
    }

    #[test]
    fn load_connections_missing_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_connections(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn clear_connections_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        add_connection(dir.path(), "ticket-abc", None, None).unwrap();
        assert!(dir.path().join("connections.json").exists());
        clear_connections(dir.path()).unwrap();
        assert!(!dir.path().join("connections.json").exists());
    }

    #[test]
    fn clear_connections_no_file_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        clear_connections(dir.path()).unwrap();
    }

    // --- Settings ---

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let settings = OuijaSettings {
            auto_register: false,
            ..Default::default()
        };
        save_settings(dir.path(), &settings).unwrap();
        let loaded = load_settings(dir.path()).unwrap();
        assert!(!loaded.auto_register);
    }

    #[test]
    fn load_settings_missing_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let settings = load_settings(dir.path()).unwrap();
        assert!(settings.auto_register);
    }

    #[test]
    fn load_settings_empty_object_uses_field_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.json"), "{}").unwrap();
        let settings = load_settings(dir.path()).unwrap();
        assert!(settings.auto_register);
        assert_eq!(
            serde_json::to_value(settings).unwrap()["default_backend"],
            "opencode"
        );
    }

    #[test]
    fn default_backend_round_trip_preserves_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"auto_register":false,"idle_timeout_secs":321,"default_backend":"claude-code"}"#,
        )
        .unwrap();

        let settings = load_settings(dir.path()).unwrap();
        let serialized = serde_json::to_value(settings).unwrap();

        assert_eq!(serialized["default_backend"], "claude-code");
        assert_eq!(serialized["auto_register"], false);
        assert_eq!(serialized["idle_timeout_secs"], 321);
    }

    #[test]
    fn human_sessions_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let settings = OuijaSettings {
            auto_register: true,
            human_sessions: vec![HumanSession {
                npub: "npub1abc".into(),
                name: "daniel".into(),
                default_session: Some("ouija".into()),
                welcomed: false,
            }],
            ..Default::default()
        };
        save_settings(dir.path(), &settings).unwrap();
        let loaded = load_settings(dir.path()).unwrap();
        assert_eq!(loaded.human_sessions.len(), 1);
        assert_eq!(loaded.human_sessions[0].name, "daniel");
        assert_eq!(loaded.human_sessions[0].npub, "npub1abc");
        assert!(!loaded.human_sessions[0].welcomed);
        assert_eq!(
            loaded.human_sessions[0].default_session.as_deref(),
            Some("ouija")
        );
    }

    #[test]
    fn human_sessions_default_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.json"), "{}").unwrap();
        let settings = load_settings(dir.path()).unwrap();
        assert!(settings.human_sessions.is_empty());
    }

    // --- RouterConfig ---

    #[test]
    fn router_config_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let settings = OuijaSettings {
            router: Some(RouterConfig {
                api_key: Some("sk-test-123".into()),
                model: "gemini-2.5-flash".into(),
                base_url: "https://generativelanguage.googleapis.com/v1beta/openai".into(),
            }),
            ..Default::default()
        };
        save_settings(dir.path(), &settings).unwrap();
        let loaded = load_settings(dir.path()).unwrap();
        let router = loaded.router.unwrap();
        assert_eq!(router.api_key.as_deref(), Some("sk-test-123"));
        assert_eq!(router.model, "gemini-2.5-flash");
    }

    #[test]
    fn router_none_backward_compat() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.json"), "{}").unwrap();
        let settings = load_settings(dir.path()).unwrap();
        assert!(settings.router.is_none());
    }

    #[test]
    fn router_config_uses_defaults() {
        let json = r#"{"router":{"api_key":"sk-test"}}"#;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.json"), json).unwrap();
        let settings = load_settings(dir.path()).unwrap();
        let router = settings.router.unwrap();
        assert_eq!(router.api_key.as_deref(), Some("sk-test"));
        assert_eq!(router.model, "gemini-2.5-flash");
        assert_eq!(
            router.base_url,
            "https://generativelanguage.googleapis.com/v1beta/openai"
        );
    }

    // --- Idle Timeout ---

    #[test]
    fn idle_timeout_default() {
        let settings: OuijaSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.idle_timeout_secs, 900);
    }

    #[test]
    fn idle_timeout_custom() {
        let settings: OuijaSettings = serde_json::from_str(r#"{"idle_timeout_secs":600}"#).unwrap();
        assert_eq!(settings.idle_timeout_secs, 600);
    }

    // --- Scheduled Tasks ---

    #[test]
    fn tasks_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut tasks = HashMap::new();
        let task = ScheduledTask {
            id: "a1b2c3d4".into(),
            name: "test".into(),
            cron: "*/5 * * * *".into(),
            target_session: Some("web".into()),
            prompt: None,
            reminder: None,
            enabled: true,
            created_at: Utc::now(),
            next_run: None,
            last_run: None,
            last_status: None,
            run_count: 0,
            project_dir: None,
            backend: None,
            model: None,
            effort: None,
            once: false,
            backend_session_id: None,
            on_fire: crate::scheduler::OnFire::ContinueSession,
        };
        tasks.insert(task.id.clone(), task);
        save_tasks(dir.path(), &tasks).unwrap();
        let loaded = load_tasks(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key("a1b2c3d4"));
    }

    #[test]
    fn load_tasks_missing_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_tasks(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn append_task_run_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let run = TaskRun {
            task_id: "abc".into(),
            task_name: "test".into(),
            timestamp: Utc::now(),
            status: crate::scheduler::TaskRunStatus::Ok,
            error: None,
            session_name: "web".into(),
            revived_pane: None,
        };
        append_task_run(dir.path(), &run).unwrap();
        assert!(dir.path().join("task_runs.jsonl").exists());
        let content = std::fs::read_to_string(dir.path().join("task_runs.jsonl")).unwrap();
        assert!(content.contains("abc"));
    }
}
