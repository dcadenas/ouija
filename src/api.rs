use std::path::Path;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;
use serde_json::json;

use crate::scheduler;
use crate::state::SharedState;
use crate::tmux;
use crate::transport;

/// Extract a short project description from a project directory.
///
/// Tries in order: `Cargo.toml` description field, `package.json` description,
/// first non-heading non-empty line of `README.md` (truncated to 200 chars).
pub(crate) fn extract_project_description(project_dir: &str) -> Option<String> {
    let dir = Path::new(project_dir);

    // Try Cargo.toml
    if let Ok(contents) = std::fs::read_to_string(dir.join("Cargo.toml")) {
        for line in contents.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("description") {
                let rest = rest.trim_start();
                if let Some(rest) = rest.strip_prefix('=') {
                    let val = rest.trim().trim_matches('"');
                    if !val.is_empty() {
                        return Some(val.to_string());
                    }
                }
            }
        }
    }

    // Try package.json
    if let Ok(contents) = std::fs::read_to_string(dir.join("package.json")) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents) {
            if let Some(desc) = json["description"].as_str() {
                if !desc.is_empty() {
                    return Some(desc.to_string());
                }
            }
        }
    }

    // Try README.md — first non-heading, non-empty line
    if let Ok(contents) = std::fs::read_to_string(dir.join("README.md")) {
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let truncated = if trimmed.len() > 200 {
                format!("{}...", &trimmed[..200])
            } else {
                trimmed.to_string()
            };
            return Some(truncated);
        }
    }

    None
}

pub async fn status(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let sessions = state.sessions.read().await;
    let nodes = state.nodes.read().await;
    let transports = state.transports().await;

    let sessions_list: Vec<_> = sessions
        .values()
        .map(|s| {
            json!({
                "id": s.id,
                "pane": s.pane,
                "origin": s.origin.label(),
                "vim_mode": s.metadata.vim_mode,
                "project_dir": s.metadata.project_dir,
                "role": s.metadata.role,
                "bulletin": s.metadata.bulletin,
                "networked": s.metadata.networked,
                "worktree": s.metadata.worktree,
                "last_metadata_update": s.metadata.last_metadata_update,
                "stale": s.metadata.is_stale(),
            })
        })
        .collect();

    let nodes_list: Vec<_> = nodes
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

    let claude_panes: Vec<_> = state
        .cached_claude_panes()
        .await
        .into_iter()
        .map(|p| json!({ "pane_id": p.pane_id, "session": p.session_name }))
        .collect();

    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "daemon": state.config.name,
        "daemon_id": state.config.npub,
        "port": state.config.port,
        "transports": transports_list,
        "transport": compat_transport,
        "endpoint_id": compat_endpoint_id,
        "sessions": sessions_list,
        "nodes": nodes_list,
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
    match t.ticket_string().await {
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
                "warning": "This will destroy your nostr identity (nsec). All nodes must re-connect. Add ?confirm=true to proceed.",
                "transport": "nostr",
            })),
        );
    }

    match t
        .regenerate(&state.config.config_dir, &state.config.data_dir)
        .await
    {
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

#[derive(Debug, Deserialize)]
pub struct ConnectBody {
    ticket: String,
    name: Option<String>,
}

pub async fn connect(
    State(state): State<SharedState>,
    Json(body): Json<ConnectBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Strip #secret suffix for validation — the nprofile is before the '#'
    let nprofile_part = body
        .ticket
        .split_once('#')
        .map_or(body.ticket.as_str(), |(left, _)| left);
    if !nprofile_part.starts_with("nprofile1") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "ticket must be an nprofile1 string" })),
        );
    }

    tracing::info!(
        "connect request received (ticket len={})",
        body.ticket.len()
    );

    // Check for duplicate connection by npub
    let peer_npub = extract_npub(&body.ticket);
    if let Some(ref npub) = peer_npub {
        let node_name = body.name.as_deref().unwrap_or(&npub[..16.min(npub.len())]);
        if let Err(existing) = state.try_add_node(npub, node_name) {
            let msg = format!("already connected to this daemon as '{existing}'");
            tracing::info!("connect rejected: {msg}");
            return (StatusCode::CONFLICT, Json(json!({ "error": msg })));
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

    if let Err(e) = crate::persistence::add_connection(
        &state.config.data_dir,
        &body.ticket,
        body.name.as_deref(),
        peer_npub.as_deref(),
    ) {
        tracing::warn!("failed to persist connection: {e}");
    }

    // Don't broadcast sessions here — the remote peer may not have processed
    // our ConnectRequest yet, so it would reject the SessionList as unauthorized.
    // Session exchange happens naturally: once the peer authorizes us, it broadcasts
    // its sessions; we process them as a new peer and broadcast ours back.
    // The periodic 5s broadcast in the main loop also provides resilience.
    tracing::info!("node connected successfully via nostr");
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

#[derive(Debug, Deserialize)]
pub struct RegisterBody {
    id: String,
    pane: Option<String>,
    #[serde(default)]
    vim_mode: bool,
    project_dir: Option<String>,
    role: Option<String>,
    bulletin: Option<String>,
    /// Defaults to true if omitted.
    #[serde(default)]
    networked: Option<bool>,
    claude_session_id: Option<String>,
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
    let project_description = body
        .project_dir
        .as_deref()
        .and_then(extract_project_description);
    let metadata = crate::state::SessionMetadata {
        vim_mode: body.vim_mode,
        project_dir: body.project_dir,
        role: body.role,
        bulletin: body.bulletin,
        networked: body.networked.unwrap_or(true),
        claude_session_id: body.claude_session_id,
        project_description,
        ..Default::default()
    };
    if let Some(ref p) = body.pane {
        if !crate::tmux::pane_alive(p) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("pane {p} does not exist") })),
            );
        }
    }
    // Try the requested ID first; on conflict (same name, different pane),
    // auto-suffix with -2, -3, etc. instead of returning 409.
    let base_id = body.id;
    let mut id = base_id.clone();
    let mut suffix = 2u32;
    let (session, replaced) = loop {
        let result = state
            .register_session(id.clone(), body.pane.clone(), metadata.clone())
            .await;
        match result {
            crate::state::RegisterResult::Ok { session, replaced } => {
                break (session, replaced);
            }
            crate::state::RegisterResult::Conflict { .. } => {
                id = format!("{base_id}-{suffix}");
                suffix += 1;
                if suffix > 100 {
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({
                            "error": "could not find available session name",
                        })),
                    );
                }
            }
        }
    };

    state
        .announce_and_activate(&session, replaced.as_deref())
        .await;

    (
        StatusCode::OK,
        Json(json!({
            "registered": session.id,
            "pane": session.pane,
        })),
    )
}

#[derive(Debug, Deserialize)]
pub struct SendBody {
    from: String,
    to: String,
    message: String,
    #[serde(default)]
    expects_reply: bool,
}

pub async fn send_msg(
    State(state): State<SharedState>,
    Json(body): Json<SendBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.from == body.to {
        let sessions = state.sessions.read().await;
        let suggestions: Vec<&str> = sessions
            .keys()
            .filter(|k| {
                k.ends_with(&format!("/{}", body.to)) || k.starts_with(&format!("{}/", body.to))
            })
            .map(|k| k.as_str())
            .collect();
        let hint = if suggestions.is_empty() {
            "If you meant a remote session, use the full node-prefixed name (e.g. 'node/session'). Run session_list to see all available targets.".to_string()
        } else {
            format!(
                "Did you mean one of these remote sessions? {} — use session_list to check.",
                suggestions.join(", ")
            )
        };
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("cannot send a message to yourself. {hint}") })),
        );
    }
    let sessions = state.sessions.read().await;
    let target = sessions.get(&body.to).cloned();
    drop(sessions);

    match target {
        Some(session) => match &session.origin {
            crate::state::SessionOrigin::Local => {
                if let Some(pane) = &session.pane {
                    let formatted =
                        tmux::format_session_message(&body.from, &body.message, body.expects_reply);
                    let vim_mode = session.metadata.vim_mode;
                    match tmux::locked_inject(&state, pane, &formatted, vim_mode).await {
                        Ok(()) => {
                            if body.expects_reply {
                                state
                                    .notify_agent(
                                        &body.to,
                                        crate::session_agent::SessionMsg::MessageDelivered {
                                            from: body.from.clone(),
                                            message: body.message.clone(),
                                            expects_reply: true,
                                        },
                                    )
                                    .await;
                            }
                            state
                                .notify_agent(
                                    &body.from,
                                    crate::session_agent::SessionMsg::ReplySent {
                                        to: body.to.clone(),
                                    },
                                )
                                .await;
                            state
                                .log_message(body.from, body.to, body.message, true, "tmux")
                                .await;
                            (
                                StatusCode::OK,
                                Json(json!({ "status": "delivered", "method": "tmux" })),
                            )
                        }
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
                let wire_to = crate::state::strip_remote_prefix(&body.to).to_string();
                let wire_msg = crate::protocol::WireMessage::SessionSend {
                    from: body.from.clone(),
                    to: wire_to.clone(),
                    message: body.message.clone(),
                    expects_reply: body.expects_reply,
                };
                if transport::broadcast(&state, &wire_msg).await {
                    // Clear pending reply using stripped name (inbound stores short name)
                    state
                        .notify_agent(
                            &body.from,
                            crate::session_agent::SessionMsg::ReplySent {
                                to: wire_to.clone(),
                            },
                        )
                        .await;
                    state
                        .log_message(body.from, body.to, body.message, true, "nostr")
                        .await;
                    (
                        StatusCode::OK,
                        Json(json!({ "status": "sent", "method": "nostr" })),
                    )
                } else {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({ "error": "P2P not connected" })),
                    )
                }
            }
            crate::state::SessionOrigin::Human(npub) => {
                let formatted = format!("[from {}]: {}", body.from, body.message);
                match crate::nostr_transport::send_plain_dm(&state, npub, &formatted).await {
                    Ok(()) => {
                        state
                            .log_message(body.from, body.to, body.message, true, "nostr-dm")
                            .await;
                        (
                            StatusCode::OK,
                            Json(json!({ "status": "delivered", "method": "nostr-dm" })),
                        )
                    }
                    Err(e) => (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({ "error": format!("DM delivery failed: {e}") })),
                    ),
                }
            }
        },
        None => {
            if let Some(new_id) = state.resolve_alias(&body.to).await {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({
                        "error": format!("session '{}' was renamed to '{}'", body.to, new_id),
                        "renamed_to": new_id,
                    })),
                )
            } else {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": format!("session '{}' not found", body.to) })),
                )
            }
        }
    }
}

#[derive(Debug, Deserialize)]
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
            // Notify peers: remove old name, announce new if networked
            let remove_msg = crate::protocol::WireMessage::SessionRemove {
                id: body.old_id.clone(),
                daemon_id: state.config.npub.clone(),
                daemon_name: state.config.name.clone(),
            };
            transport::broadcast(&state, &remove_msg).await;
            if state.is_session_networked(&session) {
                let announce_msg = crate::protocol::WireMessage::SessionAnnounce {
                    id: session.id.clone(),
                    daemon_id: state.config.npub.clone(),
                    daemon_name: state.config.name.clone(),
                    metadata: Some(session.metadata.clone()),
                };
                transport::broadcast(&state, &announce_msg).await;
            }
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

#[derive(Debug, Deserialize)]
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
            (StatusCode::OK, Json(json!({ "removed": body.id })))
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("session '{}' not found", body.id) })),
        ),
    }
}

#[derive(Debug, Deserialize)]
pub struct SessionUpdateBody {
    id: String,
    networked: Option<bool>,
    role: Option<String>,
    project_dir: Option<String>,
    bulletin: Option<String>,
}

pub async fn update_session(
    State(state): State<SharedState>,
    Json(body): Json<SessionUpdateBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut sessions = state.sessions.write().await;
    let Some(session) = sessions.get_mut(&body.id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("session '{}' not found", body.id) })),
        );
    };
    if matches!(session.origin, crate::state::SessionOrigin::Remote(_)) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "cannot update remote session" })),
        );
    }
    let mut changed = false;
    if let Some(v) = body.networked {
        if session.metadata.networked != v {
            session.metadata.networked = v;
            changed = true;
        }
    }
    let mut metadata_changed = false;
    if let Some(r) = body.role {
        session.metadata.role = Some(r);
        metadata_changed = true;
    }
    if let Some(p) = body.project_dir {
        session.metadata.project_dir = Some(p);
        metadata_changed = true;
    }
    if let Some(b) = body.bulletin {
        session.metadata.bulletin = Some(b);
        metadata_changed = true;
    }
    if metadata_changed {
        session.metadata.last_metadata_update = Some(chrono::Utc::now());
        changed = true;
    }
    let session_snapshot = session.clone();
    if changed {
        state.persist_sessions_from(&sessions);
    }
    drop(sessions);

    // Re-broadcast so peers see the updated session list
    if changed {
        transport::broadcast_local_sessions(&state).await;
    }

    (
        StatusCode::OK,
        Json(json!({
            "updated": session_snapshot.id,
            "networked": session_snapshot.metadata.networked,
            "role": session_snapshot.metadata.role,
            "bulletin": session_snapshot.metadata.bulletin,
            "project_dir": session_snapshot.metadata.project_dir,
        })),
    )
}

#[derive(Debug, Deserialize)]
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
    match tmux::locked_inject(&state, &body.pane, &body.message, body.vim_mode).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "injected" }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

// --- Nodes ---

pub async fn nodes(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let connected = state.nodes.read().await;

    // Self entry first
    let self_entry = json!({
        "name": state.config.name,
        "npub": state.config.npub,
        "status": "self",
        "transport": null,
        "since": null,
    });

    let mut entries: Vec<serde_json::Value> = vec![self_entry];

    for p in connected.values() {
        entries.push(json!({
            "name": p.name,
            "npub": p.daemon_id,
            "status": "connected",
            "transport": null,
            "since": p.connected_at.format("%H:%M:%S").to_string(),
        }));
    }

    // Add saved (persisted) connections that aren't currently connected
    let connected_names: std::collections::HashSet<&str> =
        connected.values().map(|p| p.name.as_str()).collect();

    if let Ok(conns) = crate::persistence::load_connections(&state.config.data_dir) {
        for conn in &conns {
            if let Some(name) = &conn.node_name
                && connected_names.contains(name.as_str())
            {
                continue;
            }
            entries.push(json!({
                "name": conn.node_name,
                "npub": conn.daemon_npub,
                "status": "saved",
                "transport": "nostr",
                "since": conn.connected_at.format("%Y-%m-%d").to_string(),
            }));
        }
    }

    Json(json!({ "nodes": entries }))
}

#[derive(Debug, Deserialize)]
pub struct DisconnectNodeBody {
    daemon_id: String,
}

pub async fn disconnect_node(
    State(state): State<SharedState>,
    Json(body): Json<DisconnectNodeBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let removed = state.disconnect_node(&body.daemon_id).await;
    (
        StatusCode::OK,
        Json(json!({
            "disconnected": body.daemon_id,
            "sessions_removed": removed,
        })),
    )
}

// --- Settings ---

pub async fn get_settings(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let settings = state.settings.read().await;
    Json(json!({
        "auto_register": settings.auto_register,
    }))
}

#[derive(Debug, Deserialize)]
pub struct SettingsUpdateBody {
    auto_register: Option<bool>,
    projects_dir: Option<String>,
    idle_timeout_secs: Option<u64>,
    reaper_interval_secs: Option<u64>,
}

pub async fn update_settings(
    State(state): State<SharedState>,
    Json(body): Json<SettingsUpdateBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut settings = state.settings.write().await;
    if let Some(v) = body.auto_register {
        settings.auto_register = v;
    }
    if let Some(v) = body.projects_dir {
        settings.projects_dir = Some(v);
    }
    if let Some(v) = body.idle_timeout_secs {
        settings.idle_timeout_secs = v;
    }
    if let Some(v) = body.reaper_interval_secs {
        settings.reaper_interval_secs = v;
    }
    if let Err(e) = crate::persistence::save_settings(&state.config.config_dir, &settings) {
        tracing::warn!("failed to save settings: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to save: {e}") })),
        );
    }
    (
        StatusCode::OK,
        Json(json!({
            "status": "saved",
            "settings": {
                "auto_register": settings.auto_register,
                "projects_dir": settings.projects_dir,
            }
        })),
    )
}

/// Bulk-set `networked` on all local sessions.
pub async fn bulk_update_sessions(
    State(state): State<SharedState>,
    Json(body): Json<BulkSessionUpdateBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut sessions = state.sessions.write().await;
    let mut count = 0;
    for session in sessions.values_mut() {
        if matches!(session.origin, crate::state::SessionOrigin::Local) {
            if let Some(v) = body.networked {
                if session.metadata.networked != v {
                    session.metadata.networked = v;
                    count += 1;
                }
            }
        }
    }
    if count > 0 {
        state.persist_sessions_from(&sessions);
    }
    drop(sessions);
    if count > 0 {
        transport::broadcast_local_sessions(&state).await;
    }
    (StatusCode::OK, Json(json!({ "updated": count })))
}

#[derive(Debug, Deserialize)]
pub struct BulkSessionUpdateBody {
    networked: Option<bool>,
}

pub async fn get_relays(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let relays = crate::nostr_transport::load_relays(&state.config.data_dir);
    Json(json!({ "relays": relays }))
}

#[derive(Debug, Deserialize)]
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
    (
        StatusCode::OK,
        Json(json!({ "status": "saved", "relays": relays })),
    )
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
                "claude_session_id": t.claude_session_id,
                "on_fire": t.on_fire,
            })
        })
        .collect();
    Json(json!({ "tasks": entries }))
}

#[derive(Debug, Deserialize)]
pub struct CreateTaskBody {
    name: String,
    cron: String,
    target_session: Option<String>,
    message: String,
    project_dir: Option<String>,
    #[serde(default)]
    once: Option<bool>,
    claude_session_id: Option<String>,
    #[serde(default)]
    on_fire: Option<crate::scheduler::OnFire>,
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
        body.claude_session_id,
        body.on_fire.unwrap_or_default(),
    );

    let id = task.id.clone();
    state.add_task(task).await;

    (StatusCode::OK, Json(json!({ "created": id })))
}

#[derive(Debug, Deserialize)]
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
        .filter(|r| query.task.as_ref().is_none_or(|id| r.task_id == *id))
        .take(50)
        .map(|r| {
            json!({
                "task_id": r.task_id,
                "task_name": r.task_name,
                "timestamp": r.timestamp,
                "status": r.status,
                "error": r.error,
                "session_name": r.session_name,
                "revived_pane": r.revived_pane,
            })
        })
        .collect();
    Json(json!({ "runs": entries }))
}

// --- Human sessions ---

#[derive(Deserialize)]
pub struct AddHumanBody {
    pub npub: String,
    pub name: String,
    #[serde(default)]
    pub admin: bool,
    pub default_session: Option<String>,
}

pub async fn add_human(
    State(state): State<SharedState>,
    Json(body): Json<AddHumanBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let name = body.name.trim().to_string();
    if name.is_empty() || name.contains('/') {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid name" })),
        );
    }

    // Reject if name conflicts with an existing non-human session
    {
        let sessions = state.sessions.read().await;
        if sessions
            .get(&name)
            .is_some_and(|s| !matches!(s.origin, crate::state::SessionOrigin::Human(_)))
        {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "name conflicts with existing session" })),
            );
        }
    }

    let mut settings = state.settings.write().await;
    if settings.human_sessions.iter().any(|h| h.name == name) {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "human session already exists" })),
        );
    }

    let human = crate::persistence::HumanSession {
        npub: body.npub.clone(),
        name: name.clone(),
        admin: body.admin,
        default_session: body.default_session,
        welcomed: false,
    };
    settings.human_sessions.push(human);

    if let Err(e) = crate::persistence::save_settings(&state.config.config_dir, &settings) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to save: {e}") })),
        );
    }
    drop(settings);

    // Register the session
    let mut sessions = state.sessions.write().await;
    sessions
        .entry(name.clone())
        .or_insert_with(|| crate::state::Session {
            id: name.clone(),
            pane: None,
            origin: crate::state::SessionOrigin::Human(body.npub.clone()),
            registered_at: chrono::Utc::now(),
            metadata: crate::state::SessionMetadata {
                role: Some("human".to_string()),
                networked: false,
                ..Default::default()
            },
            block_interactive: false,
        });

    (
        StatusCode::OK,
        Json(json!({ "status": "added", "name": name })),
    )
}

#[derive(Deserialize)]
pub struct RemoveHumanBody {
    pub name: String,
}

pub async fn remove_human(
    State(state): State<SharedState>,
    Json(body): Json<RemoveHumanBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut settings = state.settings.write().await;
    let before = settings.human_sessions.len();
    settings.human_sessions.retain(|h| h.name != body.name);
    if settings.human_sessions.len() == before {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" })));
    }

    if let Err(e) = crate::persistence::save_settings(&state.config.config_dir, &settings) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to save: {e}") })),
        );
    }
    drop(settings);

    // Remove the session
    let mut sessions = state.sessions.write().await;
    if sessions
        .get(&body.name)
        .is_some_and(|s| matches!(s.origin, crate::state::SessionOrigin::Human(_)))
    {
        sessions.remove(&body.name);
    }

    (StatusCode::OK, Json(json!({ "status": "removed" })))
}

pub async fn list_humans(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let settings = state.settings.read().await;
    let humans: Vec<serde_json::Value> = settings
        .human_sessions
        .iter()
        .map(|h| {
            json!({
                "name": h.name,
                "npub": h.npub,
                "admin": h.admin,
                "default_session": h.default_session,
            })
        })
        .collect();
    Json(json!({ "humans": humans }))
}

// --- Session lifecycle ---

#[derive(Debug, Deserialize)]
pub struct SessionNameBody {
    name: String,
    #[serde(default)]
    fresh: Option<bool>,
    #[serde(default)]
    worktree: Option<bool>,
    #[serde(default)]
    project_dir: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
}

pub async fn kill_session(
    State(state): State<SharedState>,
    Json(body): Json<SessionNameBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let result = crate::nostr_transport::admin_kill_session(&state, &body.name).await;
    (StatusCode::OK, Json(json!({ "result": result })))
}

pub async fn start_session(
    State(state): State<SharedState>,
    Json(body): Json<SessionNameBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let result = crate::nostr_transport::admin_start_session(
        &state,
        &body.name,
        body.worktree,
        body.project_dir.as_deref(),
        body.prompt.as_deref(),
    )
    .await;
    (StatusCode::OK, Json(json!({ "result": result })))
}

pub async fn restart_session(
    State(state): State<SharedState>,
    Json(body): Json<SessionNameBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let fresh = body.fresh.unwrap_or(false);
    let result = crate::nostr_transport::admin_restart_session(
        &state,
        &body.name,
        fresh,
        body.prompt.as_deref(),
    )
    .await;
    (StatusCode::OK, Json(json!({ "result": result })))
}

pub async fn get_block_interactive(
    State(state): State<SharedState>,
    axum::extract::Path(pane): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    let pane_id = format!("%{pane}");
    let sessions = state.sessions.read().await;
    let blocked = sessions
        .values()
        .find(|s| s.pane.as_deref() == Some(&pane_id))
        .is_some_and(|s| s.block_interactive);
    Json(json!({ "block_interactive": blocked }))
}

pub async fn clear_block_interactive(
    State(state): State<SharedState>,
    axum::extract::Path(pane): axum::extract::Path<String>,
) -> StatusCode {
    let pane_id = format!("%{pane}");
    let mut sessions = state.sessions.write().await;
    if let Some(session) = sessions
        .values_mut()
        .find(|s| s.pane.as_deref() == Some(&pane_id))
    {
        session.block_interactive = false;
    }
    StatusCode::OK
}

pub async fn get_pending_replies(
    State(state): State<SharedState>,
    axum::extract::Path(pane): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    let pane_id = format!("%{pane}");
    let session_id = {
        let sessions = state.sessions.read().await;
        sessions
            .values()
            .find(|s| s.pane.as_deref() == Some(&pane_id))
            .map(|s| s.id.clone())
    };
    let replies = if let Some(id) = session_id {
        state.query_agent_pending_replies(&id).await
    } else {
        Vec::new()
    };
    let list: Vec<_> = replies
        .iter()
        .map(|r| json!({ "from": r.from, "message": r.message, "received_at": r.received_at }))
        .collect();
    Json(json!({ "pending_replies": list, "count": list.len() }))
}

pub async fn delete_pending_reply(
    State(state): State<SharedState>,
    axum::extract::Path((pane, from)): axum::extract::Path<(String, String)>,
) -> StatusCode {
    let pane_id = format!("%{pane}");
    let sessions = state.sessions.read().await;
    let session_id = sessions
        .iter()
        .find(|(_, s)| s.pane.as_deref() == Some(&pane_id))
        .map(|(id, _)| id.clone());
    drop(sessions);
    if let Some(id) = session_id {
        state
            .notify_agent(
                &id,
                crate::session_agent::SessionMsg::ClearPendingReply { from: from.clone() },
            )
            .await;
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

pub async fn session_stopped(
    State(state): State<SharedState>,
    axum::extract::Path(pane): axum::extract::Path<String>,
) -> StatusCode {
    let pane_id = format!("%{pane}");
    let session_id = {
        let sessions = state.sessions.read().await;
        sessions
            .values()
            .find(|s| s.pane.as_deref() == Some(&pane_id))
            .map(|s| s.id.clone())
    };
    if let Some(id) = session_id {
        state
            .notify_agent(&id, crate::session_agent::SessionMsg::Stopped)
            .await;
    }
    StatusCode::OK
}

pub async fn session_active(
    State(state): State<SharedState>,
    axum::extract::Path(pane): axum::extract::Path<String>,
) -> StatusCode {
    let pane_id = format!("%{pane}");
    let mut sessions = state.sessions.write().await;
    let session_id = sessions
        .values_mut()
        .find(|s| s.pane.as_deref() == Some(&pane_id))
        .map(|s| {
            s.block_interactive = false;
            s.id.clone()
        });
    drop(sessions);
    if let Some(id) = session_id {
        state
            .notify_agent(&id, crate::session_agent::SessionMsg::Active)
            .await;
    }
    StatusCode::OK
}

pub async fn list_projects(
    State(state): State<SharedState>,
) -> axum::Json<Vec<crate::project_index::ProjectInfo>> {
    let index = state.project_index.read().await;
    let mut projects: Vec<_> = index.values().cloned().collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name));
    axum::Json(projects)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_from_cargo_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"foo\"\ndescription = \"A test crate\"\n",
        )
        .unwrap();
        let desc = extract_project_description(dir.path().to_str().unwrap());
        assert_eq!(desc.as_deref(), Some("A test crate"));
    }

    #[test]
    fn extract_from_package_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"foo","description":"A JS project"}"#,
        )
        .unwrap();
        let desc = extract_project_description(dir.path().to_str().unwrap());
        assert_eq!(desc.as_deref(), Some("A JS project"));
    }

    #[test]
    fn extract_from_readme() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("README.md"),
            "# My Project\n\nThis is a great project.\n",
        )
        .unwrap();
        let desc = extract_project_description(dir.path().to_str().unwrap());
        assert_eq!(desc.as_deref(), Some("This is a great project."));
    }

    #[test]
    fn extract_cargo_toml_preferred_over_readme() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\ndescription = \"From cargo\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("README.md"), "# Title\n\nFrom readme\n").unwrap();
        let desc = extract_project_description(dir.path().to_str().unwrap());
        assert_eq!(desc.as_deref(), Some("From cargo"));
    }

    #[test]
    fn extract_missing_files_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(extract_project_description(dir.path().to_str().unwrap()).is_none());
    }
}
