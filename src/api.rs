use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;
use serde_json::json;

use crate::scheduler;
use crate::state::SharedState;
use crate::tmux;
use crate::transport;

pub async fn status(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let sessions = state.sessions.read().await;
    let peers = state.peers.read().await;
    let transports = state.transports().await;

    let sessions_list: Vec<_> = sessions
        .values()
        .map(|s| {
            json!({
                "id": s.id,
                "pane": s.pane,
                "origin": match &s.origin {
                    crate::state::SessionOrigin::Local => "local",
                    crate::state::SessionOrigin::Remote(_) => "remote",
                },
                "vim_mode": s.metadata.vim_mode,
                "project_dir": s.metadata.project_dir,
                "role": s.metadata.role,
            })
        })
        .collect();

    let peers_list: Vec<_> = peers
        .values()
        .map(|p| {
            json!({
                "name": p.name,
                "daemon_id": p.daemon_id,
            })
        })
        .collect();

    let transports_list: Vec<_> = transports
        .values()
        .map(|t| {
            json!({
                "name": t.transport_name(),
                "ready": t.is_ready(),
                "endpoint_id": t.endpoint_id(),
            })
        })
        .collect();

    // Deprecated compat: "transport" = first transport name, "endpoint_id" = first endpoint
    let first_transport = transports.values().next();
    let compat_transport = first_transport.map(|t| t.transport_name());
    let compat_endpoint_id = first_transport.and_then(|t| t.endpoint_id());

    let claude_panes: Vec<_> = tmux::find_claude_panes()
        .unwrap_or_default()
        .into_iter()
        .map(|p| json!({ "pane_id": p.pane_id, "session": p.session_name }))
        .collect();

    Json(json!({
        "daemon": state.config.name,
        "port": state.config.port,
        "transports": transports_list,
        "transport": compat_transport,
        "endpoint_id": compat_endpoint_id,
        "sessions": sessions_list,
        "peers": peers_list,
        "claude_panes": claude_panes,
    }))
}

#[derive(Deserialize, Default)]
pub struct TicketQuery {
    /// Relay URLs for nostr transport (?relay=url1&relay=url2 or comma-separated).
    #[serde(default, deserialize_with = "deserialize_string_or_seq")]
    relay: Vec<String>,
}

/// Accept a single string or a sequence for query params.
///
/// `serde_urlencoded` (used by axum's `Query`) cannot deserialize repeated
/// query keys (`?relay=a&relay=b`) into `Vec<String>`. This deserializer
/// accepts a single string and wraps it in a vec instead of failing.
fn deserialize_string_or_seq<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct StringOrSeq;

    impl<'de> de::Visitor<'de> for StringOrSeq {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a string or sequence of strings")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Vec<String>, E> {
            Ok(vec![v.to_string()])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<String>, A::Error> {
            let mut v = Vec::new();
            while let Some(s) = seq.next_element()? {
                v.push(s);
            }
            Ok(v)
        }
    }

    deserializer.deserialize_any(StringOrSeq)
}

pub async fn ticket(
    State(state): State<SharedState>,
    Query(query): Query<TicketQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let t = if !query.relay.is_empty() {
        match crate::nostr_transport::ensure_active(&state, query.relay).await {
            Ok(t) => t,
            Err(e) => {
                let msg = format!("failed to start nostr transport: {e}");
                tracing::error!("{msg}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": msg })),
                );
            }
        }
    } else {
        let Some(t) = state.transport_by_name("nostr").await else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "nostr transport is not active" })),
            );
        };
        t
    };
    match t.ticket_string() {
        Some(ticket) => (
            StatusCode::OK,
            Json(json!({
                "ticket": ticket,
                "endpoint_id": t.endpoint_id(),
                "transport": "nostr",
            })),
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "nostr transport not ready" })),
        ),
    }
}

#[derive(Deserialize, Default)]
pub struct RegenerateQuery {
    confirm: Option<bool>,
}

pub async fn regenerate_ticket(
    State(state): State<SharedState>,
    Query(query): Query<RegenerateQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(t) = state.transport_by_name("nostr").await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "nostr transport is not active" })),
        );
    };

    if query.confirm != Some(true) {
        return (
            StatusCode::OK,
            Json(json!({
                "warning": "This will destroy your nostr identity (nsec). All peers must re-connect. Add ?confirm=true to proceed.",
                "transport": "nostr",
            })),
        );
    }

    match t.regenerate(&state.config.data_dir).await {
        Ok(ticket) => (
            StatusCode::OK,
            Json(json!({ "ticket": ticket, "transport": "nostr" })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

#[derive(Deserialize)]
pub struct ConnectBody {
    ticket: String,
    name: Option<String>,
}

pub async fn connect(
    State(state): State<SharedState>,
    Json(body): Json<ConnectBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Strip #secret suffix for validation — the nprofile is before the '#'
    let nprofile_part = body.ticket.split_once('#').map_or(body.ticket.as_str(), |(left, _)| left);
    if !nprofile_part.starts_with("nprofile1") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "ticket must be an nprofile1 string" })),
        );
    }

    tracing::info!("connect request received (ticket len={})", body.ticket.len());

    // Check for duplicate connection by npub
    let peer_npub = extract_npub(&body.ticket);
    if let Some(ref npub) = peer_npub {
        let peer_name = body.name.as_deref().unwrap_or(&npub[..16.min(npub.len())]);
        if let Err(existing) = state.try_add_peer(npub, peer_name) {
            let msg = format!("already connected to this daemon as '{existing}'");
            tracing::info!("connect rejected: {msg}");
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": msg })),
            );
        }
    }

    // Lazily activate nostr transport using relays from the nprofile
    let t = if let Some(t) = state.transport_by_name("nostr").await {
        t
    } else {
        let relays = extract_nprofile_relays(&body.ticket);
        match crate::nostr_transport::ensure_active(&state, relays).await {
            Ok(t) => t,
            Err(e) => {
                let msg = format!("failed to start nostr transport: {e}");
                tracing::error!("{msg}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": msg })),
                );
            }
        }
    };

    let connect_fut = t.connect(&body.ticket, state.clone(), true);
    match tokio::time::timeout(std::time::Duration::from_secs(10), connect_fut).await {
        Err(_) => {
            tracing::warn!("connect timed out after 10s waiting for peer");
            return (
                StatusCode::GATEWAY_TIMEOUT,
                Json(json!({ "error": "connect timed out waiting for peer" })),
            );
        }
        Ok(Err(e)) => {
            tracing::error!("connect failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("connect failed: {e}") })),
            );
        }
        Ok(Ok(())) => {}
    }

    if let Err(e) =
        crate::persistence::add_connection(&state.config.data_dir, &body.ticket, body.name.as_deref(), peer_npub.as_deref())
    {
        tracing::warn!("failed to persist connection: {e}");
    }
    transport::broadcast_local_sessions(&state).await;
    tracing::info!("peer connected successfully via nostr");
    (
        StatusCode::OK,
        Json(json!({ "status": "connected", "transport": "nostr" })),
    )
}

/// Strip the `#secret` suffix from a ticket, returning just the nprofile.
fn strip_ticket_secret(ticket: &str) -> &str {
    ticket.split_once('#').map_or(ticket, |(left, _)| left)
}

/// Extract relay URLs from an nprofile bech32 string.
fn extract_nprofile_relays(ticket: &str) -> Vec<String> {
    use nostr_sdk::prelude::*;
    Nip19Profile::from_bech32(strip_ticket_secret(ticket))
        .map(|p| p.relays.into_iter().map(|r| r.to_string()).collect())
        .unwrap_or_default()
}

/// Extract the daemon npub from an nprofile ticket.
pub fn extract_npub(ticket: &str) -> Option<String> {
    use nostr_sdk::prelude::*;
    Nip19Profile::from_bech32(strip_ticket_secret(ticket))
        .ok()
        .and_then(|p| p.public_key.to_bech32().ok())
}

#[derive(Deserialize)]
pub struct RegisterBody {
    id: String,
    pane: Option<String>,
    #[serde(default)]
    vim_mode: bool,
    project_dir: Option<String>,
    role: Option<String>,
}

pub async fn register(
    State(state): State<SharedState>,
    Json(body): Json<RegisterBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.id.contains('/') {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "session ID must not contain '/'" })),
        );
    }
    let metadata = crate::state::SessionMetadata {
        vim_mode: body.vim_mode,
        project_dir: body.project_dir,
        role: body.role,
    };
    let session = match state.register_session(body.id, body.pane, metadata).await {
        Ok(session) => session,
        Err(e) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": format!("pane already registered as '{}'", e.0) })),
            );
        }
    };

    // Announce to peers if connected
    let msg = crate::protocol::WireMessage::SessionAnnounce {
        id: session.id.clone(),
        daemon_id: state.config.npub.clone(),
        daemon_name: state.config.name.clone(),
        metadata: Some(session.metadata.clone()),
    };
    transport::broadcast(&state, &msg).await;

    (
        StatusCode::OK,
        Json(json!({
            "registered": session.id,
            "pane": session.pane,
        })),
    )
}

#[derive(Deserialize)]
pub struct SendBody {
    from: String,
    to: String,
    message: String,
}

pub async fn send_msg(
    State(state): State<SharedState>,
    Json(body): Json<SendBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let sessions = state.sessions.read().await;
    let target = sessions.get(&body.to).cloned();
    drop(sessions);

    match target {
        Some(session) => match &session.origin {
            crate::state::SessionOrigin::Local => {
                if let Some(pane) = &session.pane {
                    let formatted = tmux::format_peer_message(&body.from, &body.message);
                    let pane = pane.clone();
                    let vim_mode = session.metadata.vim_mode;
                    let lock = state.pane_lock(&pane);
                    let _guard = lock.lock().await;
                    match tokio::task::spawn_blocking(move || {
                        tmux::inject(&pane, &formatted, vim_mode)
                    })
                    .await
                    {
                        Ok(Ok(())) => {
                            state
                                .log_message(
                                    body.from, body.to, body.message, true, "tmux",
                                )
                                .await;
                            (StatusCode::OK, Json(json!({ "status": "delivered", "method": "tmux" })))
                        }
                        Ok(Err(e)) => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": e.to_string() })),
                        ),
                        Err(e) => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": e.to_string() })),
                        ),
                    }
                } else {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": "session has no tmux pane" })),
                    )
                }
            }
            crate::state::SessionOrigin::Remote(_) => {
                let wire_to =
                    crate::state::strip_remote_prefix(&body.to).to_string();
                let wire_msg = crate::protocol::WireMessage::PeerSend {
                    from: body.from.clone(),
                    to: wire_to,
                    message: body.message.clone(),
                };
                if transport::broadcast(&state, &wire_msg).await {
                    state
                        .log_message(
                            body.from, body.to, body.message, true, "gossip",
                        )
                        .await;
                    (StatusCode::OK, Json(json!({ "status": "sent", "method": "gossip" })))
                } else {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({ "error": "P2P not connected" })),
                    )
                }
            }
        },
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("session '{}' not found", body.to) })),
        ),
    }
}

#[derive(Deserialize)]
pub struct RenameBody {
    old_id: String,
    new_id: String,
}

pub async fn rename(
    State(state): State<SharedState>,
    Json(body): Json<RenameBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.new_id.contains('/') {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "session ID must not contain '/'" })),
        );
    }
    match state.rename_session(&body.old_id, &body.new_id).await {
        Some(session) => {
            let remove_msg = crate::protocol::WireMessage::SessionRemove {
                id: body.old_id.clone(),
                daemon_id: state.config.npub.clone(),
                daemon_name: state.config.name.clone(),
            };
            transport::broadcast(&state, &remove_msg).await;
            let announce_msg = crate::protocol::WireMessage::SessionAnnounce {
                id: session.id.clone(),
                daemon_id: state.config.npub.clone(),
                daemon_name: state.config.name.clone(),
                metadata: Some(session.metadata.clone()),
            };
            transport::broadcast(&state, &announce_msg).await;
            (
                StatusCode::OK,
                Json(json!({ "renamed": body.old_id, "to": body.new_id })),
            )
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("session '{}' not found", body.old_id) })),
        ),
    }
}

#[derive(Deserialize)]
pub struct RemoveBody {
    id: String,
}

pub async fn remove(
    State(state): State<SharedState>,
    Json(body): Json<RemoveBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.remove_session(&body.id).await {
        Some(_) => {
            let msg = crate::protocol::WireMessage::SessionRemove {
                id: body.id.clone(),
                daemon_id: state.config.npub.clone(),
                daemon_name: state.config.name.clone(),
            };
            transport::broadcast(&state, &msg).await;
            (
                StatusCode::OK,
                Json(json!({ "removed": body.id })),
            )
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("session '{}' not found", body.id) })),
        ),
    }
}

#[derive(Deserialize)]
pub struct InjectBody {
    pane: String,
    message: String,
    #[serde(default)]
    vim_mode: bool,
}

pub async fn inject(
    State(state): State<SharedState>,
    Json(body): Json<InjectBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let pane = body.pane;
    let message = body.message;
    let vim_mode = body.vim_mode;
    let lock = state.pane_lock(&pane);
    let _guard = lock.lock().await;
    match tokio::task::spawn_blocking(move || tmux::inject(&pane, &message, vim_mode)).await {
        Ok(Ok(())) => (StatusCode::OK, Json(json!({ "status": "injected" }))),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

// --- Peers ---

pub async fn peers(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let connected = state.peers.read().await;

    let mut entries: Vec<serde_json::Value> = connected
        .values()
        .map(|p| {
            json!({
                "name": p.name,
                "status": "connected",
                "transport": null,
                "since": p.connected_at.format("%H:%M:%S").to_string(),
            })
        })
        .collect();

    // Add saved (persisted) connections that aren't currently connected
    let connected_names: std::collections::HashSet<&str> =
        connected.values().map(|p| p.name.as_str()).collect();

    if let Ok(conns) = crate::persistence::load_connections(&state.config.data_dir) {
        for conn in &conns {
            if let Some(name) = &conn.peer_name
                && connected_names.contains(name.as_str())
            {
                continue;
            }
            entries.push(json!({
                "name": conn.peer_name,
                "status": "saved",
                "transport": "nostr",
                "since": conn.connected_at.format("%Y-%m-%d").to_string(),
            }));
        }
    }

    Json(json!({ "peers": entries }))
}

// --- Settings ---

pub async fn get_settings(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let settings = state.settings.read().await;
    Json(json!({
        "auto_register": settings.auto_register,
    }))
}

#[derive(Deserialize)]
pub struct SettingsUpdateBody {
    auto_register: Option<bool>,
}

pub async fn update_settings(
    State(state): State<SharedState>,
    Json(body): Json<SettingsUpdateBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut settings = state.settings.write().await;
    if let Some(v) = body.auto_register {
        settings.auto_register = v;
    }
    if let Err(e) = crate::persistence::save_settings(&state.config.data_dir, &settings) {
        tracing::warn!("failed to save settings: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to save: {e}") })),
        );
    }
    (StatusCode::OK, Json(json!({ "status": "saved", "settings": { "auto_register": settings.auto_register } })))
}

pub async fn get_relays(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let relays = crate::nostr_transport::load_relays(&state.config.data_dir);
    Json(json!({ "relays": relays }))
}

#[derive(Deserialize)]
pub struct RelaysUpdateBody {
    relays: Vec<String>,
}

pub async fn update_relays(
    State(state): State<SharedState>,
    Json(body): Json<RelaysUpdateBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Validate relay URLs
    let relays: Vec<String> = body
        .relays
        .into_iter()
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .collect();

    if let Err(e) = crate::nostr_transport::save_relays(&state.config.data_dir, &relays) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to save: {e}") })),
        );
    }
    (StatusCode::OK, Json(json!({ "status": "saved", "relays": relays })))
}

// --- Scheduled Tasks ---

pub async fn list_tasks(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let tasks = state.scheduled_tasks.read().await;
    let mut list: Vec<&scheduler::ScheduledTask> = tasks.values().collect();
    list.sort_by_key(|t| &t.created_at);
    let entries: Vec<serde_json::Value> = list
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "name": t.name,
                "cron": t.cron,
                "target_session": t.target_session,
                "message": t.message,
                "enabled": t.enabled,
                "next_run": t.next_run,
                "last_run": t.last_run,
                "last_status": t.last_status,
                "run_count": t.run_count,
                "project_dir": t.project_dir,
                "once": t.once,
            })
        })
        .collect();
    Json(json!({ "tasks": entries }))
}

#[derive(Deserialize)]
pub struct CreateTaskBody {
    name: String,
    cron: String,
    target_session: String,
    message: String,
    project_dir: Option<String>,
    #[serde(default)]
    once: Option<bool>,
}

pub async fn create_task(
    State(state): State<SharedState>,
    Json(body): Json<CreateTaskBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(e) = scheduler::validate_cron(&body.cron) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("invalid cron: {e}") })),
        );
    }

    let task = scheduler::new_task(
        body.name,
        body.cron,
        body.target_session,
        body.message,
        body.project_dir,
        body.once.unwrap_or(false),
    );

    let id = task.id.clone();
    state.add_task(task).await;

    (StatusCode::OK, Json(json!({ "created": id })))
}

#[derive(Deserialize)]
pub struct TaskIdBody {
    id: String,
}

pub async fn delete_task(
    State(state): State<SharedState>,
    Json(body): Json<TaskIdBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.remove_task(&body.id).await {
        Some(_) => (StatusCode::OK, Json(json!({ "deleted": body.id }))),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("task '{}' not found", body.id) })),
        ),
    }
}

pub async fn enable_task(
    State(state): State<SharedState>,
    Json(body): Json<TaskIdBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let tasks = state.scheduled_tasks.read().await;
    if !tasks.contains_key(&body.id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("task '{}' not found", body.id) })),
        );
    }
    drop(tasks);
    state
        .update_task(&body.id, |t| {
            t.enabled = true;
            t.next_run = scheduler::compute_next_run(&t.cron);
        })
        .await;
    (StatusCode::OK, Json(json!({ "enabled": body.id })))
}

pub async fn disable_task(
    State(state): State<SharedState>,
    Json(body): Json<TaskIdBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let tasks = state.scheduled_tasks.read().await;
    if !tasks.contains_key(&body.id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("task '{}' not found", body.id) })),
        );
    }
    drop(tasks);
    state
        .update_task(&body.id, |t| {
            t.enabled = false;
            t.next_run = None;
        })
        .await;
    (StatusCode::OK, Json(json!({ "disabled": body.id })))
}

pub async fn trigger_task(
    State(state): State<SharedState>,
    Json(body): Json<TaskIdBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    {
        let tasks = state.scheduled_tasks.read().await;
        if !tasks.contains_key(&body.id) {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("task '{}' not found", body.id) })),
            );
        }
    }
    scheduler::execute_task(&state, &body.id).await;
    (StatusCode::OK, Json(json!({ "triggered": body.id })))
}

#[derive(Deserialize, Default)]
pub struct TaskRunsQuery {
    task: Option<String>,
}

pub async fn list_task_runs(
    State(state): State<SharedState>,
    Query(query): Query<TaskRunsQuery>,
) -> Json<serde_json::Value> {
    let runs = state.task_runs.read().await;
    let entries: Vec<serde_json::Value> = runs
        .iter()
        .rev()
        .filter(|r| {
            query
                .task
                .as_ref()
                .is_none_or(|id| r.task_id == *id)
        })
        .take(50)
        .map(|r| {
            json!({
                "task_id": r.task_id,
                "task_name": r.task_name,
                "timestamp": r.timestamp,
                "status": r.status,
                "error": r.error,
                "target_session": r.target_session,
                "revived_pane": r.revived_pane,
            })
        })
        .collect();
    Json(json!({ "runs": entries }))
}
