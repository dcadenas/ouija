use std::net::SocketAddr;
use std::path::Path;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Json;
use serde::Deserialize;
use serde_json::json;

use crate::scheduler;
use crate::state::SharedState;
use crate::tmux;
use crate::transport;

/// Max description length before truncation.
const MAX_DESCRIPTION_LEN: usize = 200;
/// Max characters of npub to display as fallback node name.
const NPUB_DISPLAY_LEN: usize = 16;
/// Timeout for peer connect handshake.
const CONNECT_TIMEOUT_SECS: u64 = 10;
/// Max task runs to return in the list endpoint.
const MAX_TASK_RUNS_RETURNED: usize = 50;

/// Normalize a user-supplied optional string: trim whitespace and treat
/// empty/whitespace-only strings as absent.
///
/// Applied at the API boundary on fields like `model` and `effort` where
/// `Some("")` is always a mistake (serialized form of a CLI flag without a
/// value, or a JSON client passing an empty placeholder) and must not flow
/// through as if it were an explicit override.
pub(crate) fn normalize_optional_string(input: Option<String>) -> Option<String> {
    input
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

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
            let truncated = if trimmed.len() > MAX_DESCRIPTION_LEN {
                format!("{}...", &trimmed[..MAX_DESCRIPTION_LEN])
            } else {
                trimmed.to_string()
            };
            return Some(truncated);
        }
    }

    None
}

/// Return status of a single session by name.
pub async fn get_session(
    State(state): State<SharedState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let proto = state.protocol.read().await;
    match proto.sessions.get(&name) {
        Some(s) => {
            let stale = s.metadata.is_stale();
            (
                StatusCode::OK,
                Json(json!({
                    "id": s.id,
                    "pane": s.pane,
                    "origin": s.origin.label(),
                    "vim_mode": s.metadata.vim_mode,
                    "project_dir": s.metadata.project_dir,
                    "role": s.metadata.role,
                    "bulletin": s.metadata.bulletin,
                    "networked": s.metadata.networked,
                    "worktree": s.metadata.worktree,
                    "model": s.metadata.model,
                    "effort": s.metadata.effort,
                    "last_metadata_update": s.metadata.last_metadata_update,
                    "stale": stale,
                    "backend_session_id": s.metadata.backend_session_id,
                    "backend": s.metadata.backend,
                    "reminder": s.metadata.reminder,
                    "prompt": s.metadata.prompt,
                    "iteration": s.metadata.iteration,
                    "iteration_log": s.metadata.iteration_log,
                    "last_iteration_at": s.metadata.last_iteration_at,
                    "worktree_present": s.metadata.worktree_present,
                })),
            )
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("session '{}' not found", name)})),
        ),
    }
}

/// Return daemon status, sessions, nodes, and transport info.
pub async fn status(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let proto = state.protocol.read().await;
    let nodes = state.nodes.read().await;
    let transports = state.transports().await;

    let sessions_list: Vec<_> = proto
        .sessions
        .values()
        .map(|s| {
            let stale = s.metadata.is_stale();
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
                "model": s.metadata.model,
                "effort": s.metadata.effort,
                "last_metadata_update": s.metadata.last_metadata_update,
                "stale": stale,
                "backend_session_id": s.metadata.backend_session_id,
                "backend": s.metadata.backend,
                "reminder": s.metadata.reminder,
                "prompt": s.metadata.prompt,
                "iteration": s.metadata.iteration,
                "iteration_log": s.metadata.iteration_log,
                "last_iteration_at": s.metadata.last_iteration_at,
                "worktree_present": s.metadata.worktree_present,
            })
        })
        .collect();
    drop(proto);

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

    let assistant_panes: Vec<_> = state
        .cached_assistant_panes()
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
        "assistant_panes": assistant_panes,
    }))
}

#[derive(Debug, Deserialize, Default)]
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

/// Generate a connect ticket for remote peer pairing.
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

#[derive(Debug, Deserialize, Default)]
pub struct RegenerateQuery {
    confirm: Option<bool>,
}

/// Regenerate the connect secret and return a new ticket.
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

/// Initiate a Nostr connection to a remote peer via ticket.
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
        let node_name = body
            .name
            .as_deref()
            .unwrap_or(&npub[..NPUB_DISPLAY_LEN.min(npub.len())]);
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
    match tokio::time::timeout(
        std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS),
        connect_fut,
    )
    .await
    {
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
    #[serde(alias = "claude_session_id")]
    backend_session_id: Option<String>,
    /// Which coding assistant backend to use (e.g. "claude-code", "codex").
    #[serde(default)]
    backend: Option<String>,
    /// Reminder text re-injected on idle.
    #[serde(default)]
    reminder: Option<String>,
}

/// Parse `/proc/net/tcp` to find the socket inode whose *local* endpoint
/// matches `needle` (an address string in the kernel's little-endian hex
/// format, e.g. `0100007F:8012`). Pure function, unit-testable.
fn parse_tcp_inode_for_local(tcp_table: &str, needle: &str) -> Option<u64> {
    for line in tcp_table.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 10 && cols[1] == needle {
            return cols[9].parse().ok();
        }
    }
    None
}

/// Format a `SocketAddr` into the `AABBCCDD:EEFF` encoding used by
/// `/proc/net/tcp` (IPv4 only; IPv6 not supported because the daemon binds
/// loopback). Returns None for IPv6 peers.
fn needle_for_loopback_peer(peer: SocketAddr) -> Option<String> {
    let std::net::IpAddr::V4(v4) = peer.ip() else {
        return None;
    };
    let o = v4.octets();
    Some(format!(
        "{:02X}{:02X}{:02X}{:02X}:{:04X}",
        o[3],
        o[2],
        o[1],
        o[0],
        peer.port()
    ))
}

/// Resolve the PID + cmdline of a local TCP peer by walking `/proc`. Linux
/// only; returns None on other platforms or when resolution fails (socket
/// already closed, permission denied, peer is IPv6, etc.).
#[cfg(target_os = "linux")]
fn resolve_loopback_peer(peer: SocketAddr) -> Option<String> {
    let needle = needle_for_loopback_peer(peer)?;
    let tcp_table = std::fs::read_to_string("/proc/net/tcp").ok()?;
    let inode = parse_tcp_inode_for_local(&tcp_table, &needle)?;
    let socket_target = format!("socket:[{inode}]");

    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let pid = match name.to_str() {
            Some(s) if s.chars().all(|c| c.is_ascii_digit()) => s,
            _ => continue,
        };
        let fd_dir = entry.path().join("fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        for fd in fds.flatten() {
            if let Ok(link) = std::fs::read_link(fd.path())
                && link.to_str() == Some(socket_target.as_str())
            {
                let cmdline = std::fs::read_to_string(entry.path().join("cmdline"))
                    .unwrap_or_default()
                    .replace('\0', " ")
                    .trim_end()
                    .to_string();
                return Some(format!("pid={pid} cmd={cmdline:?}"));
            }
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn resolve_loopback_peer(_peer: SocketAddr) -> Option<String> {
    None
}

/// Register a new local session with optional metadata.
pub async fn register(
    State(state): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body_bytes: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    // Diagnostic (issue #14): log peer address, User-Agent, the raw JSON body
    // (preserves unknown fields), and — on Linux — the caller PID+cmdline
    // resolved via /proc/net/tcp + /proc/<pid>/fd walk. Resolving inside the
    // handler catches the caller before TIME_WAIT loses the socket→PID mapping.
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    let raw_body = String::from_utf8_lossy(&body_bytes);
    let caller = resolve_loopback_peer(peer).unwrap_or_else(|| "pid=unknown".to_string());
    tracing::info!(
        target: "ouija::api::register",
        peer = %peer,
        user_agent = %user_agent,
        caller = %caller,
        "/api/register: raw_body={}",
        raw_body,
    );

    let body: RegisterBody = match serde_json::from_slice(&body_bytes) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid JSON: {e}") })),
            );
        }
    };

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
        backend_session_id: body.backend_session_id,
        backend: body.backend,
        project_description,
        reminder: body.reminder,
        ..Default::default()
    };
    if let Some(ref p) = body.pane {
        let names = state.backends.all_process_names();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        if !crate::tmux::pane_alive(p, &refs) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("pane {p} does not exist") })),
            );
        }
    }
    // Auto-detect backend from pane process tree if not explicitly provided
    let backend = match metadata.backend {
        Some(ref b) => Some(b.clone()),
        None => match body.pane {
            Some(ref p) => state.detect_backend_in_pane(p).await,
            None => None,
        },
    };
    let proto_meta = crate::daemon_protocol::SessionMeta {
        project_dir: metadata.project_dir.clone(),
        role: metadata.role.clone(),
        bulletin: metadata.bulletin.clone(),
        networked: metadata.networked,
        worktree: metadata.worktree,
        vim_mode: metadata.vim_mode,
        backend,
        backend_session_id: metadata.backend_session_id.clone(),
        reminder: metadata.reminder.clone(),
        ..Default::default()
    };
    let effects = state
        .apply_and_execute(crate::daemon_protocol::Event::Register {
            id: body.id.clone(),
            pane: body.pane.clone(),
            metadata: proto_meta,
        })
        .await;
    let (session_id, _replaced) = match effects.iter().find_map(|e| match e {
        crate::daemon_protocol::Effect::RegisterOk {
            session_id,
            replaced,
        } => Some((session_id.clone(), replaced.clone())),
        _ => None,
    }) {
        Some(ok) => ok,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "unexpected register result" })),
            );
        }
    };

    (
        StatusCode::OK,
        Json(json!({
            "registered": session_id,
            "pane": body.pane,
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
    #[serde(default)]
    responds_to: Option<u64>,
    #[serde(default)]
    done: bool,
}

/// Send a message from one session to another.
pub async fn send_msg(
    State(state): State<SharedState>,
    Json(body): Json<SendBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.from == body.to {
        let suffix = format!("/{}", body.to);
        let prefix = format!("{}/", body.to);
        let proto = state.protocol.read().await;
        let suggestions: Vec<&str> = proto
            .sessions
            .keys()
            .filter(|k| k.ends_with(&suffix) || k.starts_with(&prefix))
            .map(|k| k.as_str())
            .collect();
        let hint = if suggestions.is_empty() {
            "If you meant a remote session, use the full node-prefixed name (e.g. 'node/session'). GET /api/status to see all available targets.".to_string()
        } else {
            format!(
                "Did you mean one of these remote sessions? {} — GET /api/status to check.",
                suggestions.join(", ")
            )
        };
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("cannot send a message to yourself. {hint}") })),
        );
    }
    if state.is_soft_restart_in_progress(&body.to) {
        return (
            StatusCode::CONFLICT,
            Json(
                json!({ "error": format!("soft restart is in progress for session '{}'", body.to) }),
            ),
        );
    }
    let from = body.from.clone();
    let (effects, rollback) = {
        let mut proto = state.protocol.write().await;
        let mut rollback = FailedSendRollback::capture(&proto, &from, body.responds_to, body.done);
        let effects = proto.apply(crate::daemon_protocol::Event::Send {
            from: body.from,
            to: body.to,
            message: body.message,
            expects_reply: body.expects_reply,
            responds_to: body.responds_to,
            done: body.done,
        });
        rollback.capture_after_send(&proto);
        rollback.reserve_sender_state_after_send(&mut proto);
        (effects, rollback)
    };

    if let Some((reason, renamed_to)) = effects.iter().find_map(|e| match e {
        crate::daemon_protocol::Effect::SendFailed {
            reason, renamed_to, ..
        } => Some((reason.clone(), renamed_to.clone())),
        _ => None,
    }) {
        rollback_failed_delivery(&state, &effects, rollback).await;
        let mut body = json!({ "error": reason });
        if let Some(new_id) = renamed_to {
            body["renamed_to"] = json!(new_id);
        }
        return (StatusCode::NOT_FOUND, Json(body));
    }

    let delivery_outcome = match execute_send_effects_for_api(&state, &effects).await {
        Ok(outcome) => outcome,
        Err(e) => {
            rollback_failed_delivery(&state, &effects, rollback).await;
            let method = effects.iter().find_map(|effect| match effect {
                crate::daemon_protocol::Effect::SendDelivered { method, .. } => {
                    Some(method.clone())
                }
                _ => None,
            });
            let mut body = json!({ "error": e.to_string() });
            if let Some(method) = method {
                body["method"] = json!(method);
            }
            return (StatusCode::BAD_GATEWAY, Json(body));
        }
    };

    match &delivery_outcome {
        crate::state::DeliveryOutcome::Accepted => {
            finalize_successful_delivery(&state, rollback).await;
        }
        crate::state::DeliveryOutcome::Rejected(reason) => {
            rollback_failed_delivery(&state, &effects, rollback).await;
            let method = effects.iter().find_map(|effect| match effect {
                crate::daemon_protocol::Effect::SendDelivered { method, .. } => {
                    Some(method.clone())
                }
                _ => None,
            });
            let mut body = json!({ "error": reason });
            if let Some(method) = method {
                body["method"] = json!(method);
            }
            return (StatusCode::BAD_GATEWAY, Json(body));
        }
        crate::state::DeliveryOutcome::Ambiguous(_) => {}
    }

    if let Some((method, msg_id)) = effects.iter().find_map(|e| match e {
        crate::daemon_protocol::Effect::SendDelivered { method, msg_id, .. } => {
            Some((method.clone(), *msg_id))
        }
        _ => None,
    }) {
        let status = if matches!(
            delivery_outcome,
            crate::state::DeliveryOutcome::Ambiguous(_)
        ) {
            "unknown"
        } else if method == "http" {
            "accepted"
        } else {
            "delivered"
        };
        (
            StatusCode::OK,
            Json(json!({
                "status": status,
                "method": method,
                "msg_id": msg_id,
            })),
        )
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "unexpected send result" })),
        )
    }
}

struct FailedSendRollback {
    sender_id: String,
    pending_reply_before_send: Option<crate::daemon_protocol::PendingReplyEntry>,
    pending_reply_after_send: Option<Option<crate::daemon_protocol::PendingReplyEntry>>,
    sender_reminder: Option<Option<String>>,
    sender_reminder_after_send: Option<Option<String>>,
    sender_state_reserved: bool,
    done: bool,
}

impl FailedSendRollback {
    fn capture(
        proto: &crate::daemon_protocol::DaemonState,
        sender_id: &str,
        responds_to: Option<u64>,
        done: bool,
    ) -> Self {
        let pending_reply_before_send = responds_to.and_then(|msg_id| {
            proto
                .pending_replies
                .get(sender_id)
                .and_then(|pending| pending.iter().find(|entry| entry.msg_id == msg_id).cloned())
        });
        Self {
            sender_id: sender_id.to_string(),
            pending_reply_before_send,
            pending_reply_after_send: None,
            sender_reminder: done.then(|| {
                proto
                    .sessions
                    .get(sender_id)
                    .and_then(|session| session.metadata.reminder.clone())
            }),
            sender_reminder_after_send: None,
            sender_state_reserved: false,
            done,
        }
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

async fn rollback_failed_delivery(
    state: &SharedState,
    effects: &[crate::daemon_protocol::Effect],
    rollback: FailedSendRollback,
) {
    clear_pending_reply_for_failed_delivery(state, effects).await;
    if rollback.sender_state_reserved() {
        return;
    }

    let mut proto = state.protocol.write().await;
    if let Some(entry) = rollback.pending_reply_before_send {
        let current_entry = proto
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

async fn finalize_successful_delivery(state: &SharedState, rollback: FailedSendRollback) {
    if !rollback.done {
        return;
    }

    let mut proto = state.protocol.write().await;
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

async fn clear_pending_reply_for_failed_delivery(
    state: &SharedState,
    effects: &[crate::daemon_protocol::Effect],
) {
    let Some((to, msg_id, from)) = effects.iter().find_map(|effect| match effect {
        crate::daemon_protocol::Effect::SendDelivered {
            from, to, msg_id, ..
        } => Some((to.clone(), *msg_id, from.clone())),
        _ => None,
    }) else {
        return;
    };

    let mut proto = state.protocol.write().await;
    let Some(pending) = proto.pending_replies.get_mut(&to) else {
        return;
    };
    pending.retain(|entry| entry.msg_id != msg_id || entry.from != from);
    if pending.is_empty() {
        proto.pending_replies.remove(&to);
    }
}

async fn execute_send_effects_for_api(
    state: &SharedState,
    effects: &[crate::daemon_protocol::Effect],
) -> anyhow::Result<crate::state::DeliveryOutcome> {
    use crate::daemon_protocol::Effect;

    let recorded_method = effects.iter().find_map(|effect| match effect {
        Effect::SendDelivered { method, .. } => Some(method.as_str()),
        _ => None,
    });
    let recorded_http_delivery = effects.iter().find_map(|effect| match effect {
        Effect::SendDelivered { http_delivery, .. } => http_delivery.as_ref(),
        _ => None,
    });

    let mut outcome = crate::state::DeliveryOutcome::Accepted;

    for effect in effects {
        match effect {
            Effect::Broadcast(msg) => {
                crate::transport::broadcast(state, msg).await;
            }
            Effect::InjectMessage {
                session_id,
                pane,
                message,
                vim_mode,
                ..
            } => match recorded_method {
                Some("http") => {
                    let delivery = recorded_http_delivery.ok_or_else(|| {
                        anyhow::anyhow!(
                            "http delivery skipped: no recorded backend_session_id on send"
                        )
                    })?;
                    outcome = combine_delivery_outcome(
                        outcome,
                        deliver_http_message_outcome(state, delivery, message).await,
                    )
                }
                Some("tmux") => {
                    tmux::locked_inject_raw_tmux(state, session_id, pane, message, *vim_mode)
                        .await?;
                }
                _ => {
                    tmux::locked_inject(state, session_id, pane, message, *vim_mode).await?;
                }
            },
            Effect::DeliverHttpMessage {
                session_id: _,
                message,
                http_delivery,
                ..
            } => {
                let delivery = Some(http_delivery)
                    .or(recorded_http_delivery)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "http delivery skipped: no recorded backend_session_id on send"
                        )
                    })?;
                outcome = combine_delivery_outcome(
                    outcome,
                    deliver_http_message_outcome(state, delivery, message).await,
                );
            }
            Effect::SendToHuman { npub, message } => {
                let _ = crate::nostr_transport::send_plain_dm(state, npub, message).await;
            }
            Effect::LogMessage {
                from,
                to,
                message,
                delivered,
                transport,
            } => {
                state
                    .log_message(
                        from.clone(),
                        to.clone(),
                        message.clone(),
                        *delivered,
                        transport,
                    )
                    .await;
            }
            Effect::SendDelivered { .. } | Effect::SendFailed { .. } => {}
            _ => {
                tracing::debug!(?effect, "unexpected send effect in API executor");
            }
        }
    }

    Ok(outcome)
}

fn combine_delivery_outcome(
    left: crate::state::DeliveryOutcome,
    right: crate::state::DeliveryOutcome,
) -> crate::state::DeliveryOutcome {
    match (left, right) {
        (crate::state::DeliveryOutcome::Rejected(reason), _)
        | (_, crate::state::DeliveryOutcome::Rejected(reason)) => {
            crate::state::DeliveryOutcome::Rejected(reason)
        }
        (crate::state::DeliveryOutcome::Ambiguous(reason), _)
        | (_, crate::state::DeliveryOutcome::Ambiguous(reason)) => {
            crate::state::DeliveryOutcome::Ambiguous(reason)
        }
        (crate::state::DeliveryOutcome::Accepted, crate::state::DeliveryOutcome::Accepted) => {
            crate::state::DeliveryOutcome::Accepted
        }
    }
}

async fn deliver_http_message_outcome(
    state: &SharedState,
    delivery: &crate::daemon_protocol::HttpDeliverySnapshot,
    message: &str,
) -> crate::state::DeliveryOutcome {
    tmux::deliver_via_http(
        state,
        &delivery.backend_session_id,
        delivery.project_dir.as_deref(),
        message,
        delivery.model.as_deref(),
        delivery.effort.as_deref(),
    )
    .await
    .map(|()| crate::state::DeliveryOutcome::Accepted)
    .unwrap_or_else(|decision| match decision {
        crate::nostr_transport::PromptAsyncFallbackDecision::Ambiguous => {
            crate::state::DeliveryOutcome::Ambiguous(format!(
                "prompt_async request failed: {decision:?}"
            ))
        }
        crate::nostr_transport::PromptAsyncFallbackDecision::DefiniteNonAcceptance => {
            crate::state::DeliveryOutcome::Rejected(format!(
                "prompt_async request failed: {decision:?}"
            ))
        }
    })
}

#[derive(Debug, Deserialize)]
pub struct RenameBody {
    old_id: String,
    new_id: String,
}

/// Rename an existing session.
pub async fn rename(
    State(state): State<SharedState>,
    Json(body): Json<RenameBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let effects = state
        .apply_and_execute(crate::daemon_protocol::Event::Rename {
            old_id: body.old_id.clone(),
            new_id: body.new_id.clone(),
        })
        .await;
    if effects
        .iter()
        .any(|e| matches!(e, crate::daemon_protocol::Effect::RenameOk { .. }))
    {
        (
            StatusCode::OK,
            Json(json!({ "renamed": body.old_id, "to": body.new_id })),
        )
    } else {
        let reason = effects
            .iter()
            .find_map(|e| match e {
                crate::daemon_protocol::Effect::RenameFailed { reason } => Some(reason.clone()),
                _ => None,
            })
            .unwrap_or_else(|| format!("session '{}' not found", body.old_id));
        (StatusCode::NOT_FOUND, Json(json!({ "error": reason })))
    }
}

#[derive(Debug, Deserialize)]
pub struct RemoveBody {
    id: String,
    #[serde(default)]
    keep_worktree: Option<bool>,
}

/// Unregister a session by ID.
pub async fn remove(
    State(state): State<SharedState>,
    Json(body): Json<RemoveBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let effects = state
        .apply_and_execute(crate::daemon_protocol::Event::Remove {
            id: body.id.clone(),
            keep_worktree: body.keep_worktree.unwrap_or(false),
        })
        .await;
    if effects
        .iter()
        .any(|e| matches!(e, crate::daemon_protocol::Effect::RemoveOk { .. }))
    {
        (StatusCode::OK, Json(json!({ "removed": body.id })))
    } else {
        let reason = effects
            .iter()
            .find_map(|e| match e {
                crate::daemon_protocol::Effect::RemoveFailed { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .unwrap_or_else(|| format!("session '{}' not found", body.id));
        (StatusCode::NOT_FOUND, Json(json!({ "error": reason })))
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

/// Update a session's metadata (role, bulletin, project_dir, etc.).
pub async fn update_session(
    State(state): State<SharedState>,
    Json(body): Json<SessionUpdateBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Validate session exists and is not remote
    {
        let proto = state.protocol.read().await;
        let Some(session) = proto.sessions.get(&body.id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("session '{}' not found", body.id) })),
            );
        };
        if matches!(session.origin, crate::daemon_protocol::Origin::Remote(_)) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "cannot update remote session" })),
            );
        }
    }

    state
        .apply_and_execute(crate::daemon_protocol::Event::UpdateMetadata {
            id: body.id.clone(),
            role: body.role,
            bulletin: body.bulletin,
            project_dir: body.project_dir,
            networked: body.networked,
        })
        .await;

    let proto = state.protocol.read().await;
    let response = if let Some(s) = proto.sessions.get(&body.id) {
        json!({
            "updated": s.id,
            "networked": s.metadata.networked,
            "role": s.metadata.role,
            "bulletin": s.metadata.bulletin,
            "project_dir": s.metadata.project_dir,
        })
    } else {
        json!({ "updated": body.id })
    };

    (StatusCode::OK, Json(response))
}

#[derive(Debug, Deserialize)]
pub struct InjectBody {
    pane: String,
    message: String,
    #[serde(default)]
    vim_mode: bool,
}

/// Inject text into a tmux pane via the queued writer.
pub async fn inject(
    State(state): State<SharedState>,
    Json(body): Json<InjectBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let session_id = {
        let proto = state.protocol.read().await;
        proto
            .sessions
            .values()
            .find(|s| s.pane.as_deref() == Some(&body.pane))
            .map(|s| s.id.clone())
            .unwrap_or_else(|| "__pane_inject_default__".to_string())
    };
    match tmux::locked_inject_raw_tmux(
        &state,
        &session_id,
        &body.pane,
        &body.message,
        body.vim_mode,
    )
    .await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "status": "injected", "delivery": "tmux" })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

// --- Compact ---

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactBody {
    #[serde(default)]
    pub continuation: Option<String>,
}

/// Trigger backend-aware compaction. Callers pass the same `{continuation}` body
/// for every backend; the endpoint routes by `delivery_mode()`:
///
/// - TUI backends (e.g. Claude Code) receive the compact slash command via tmux,
///   and the continuation is parked on the session agent for the post-compact
///   hook to drain. `compacted: true` on the response means the compact command
///   was successfully queued; the backend performs the actual compaction
///   asynchronously. `continuation_delivered` is always `false` on this
///   branch because delivery (if any) happens later — after this response
///   returns — when the post-compact hook drains the parked continuation.
/// - HTTP backends (e.g. OpenCode) call `POST /session/:id/summarize` on the
///   opencode serve with `{providerID, modelID}` resolved from the session's
///   configured model (falling back to OpenCode's top-level `/config.model`). The
///   request is synchronous — it blocks until opencode's compaction loop
///   completes — so a 2xx response means the context has really been shrunk.
///   On success, the continuation (if any) is delivered as a fresh user turn
///   via `prompt_async`. `compacted: true` means the summarize call
///   succeeded; `continuation_delivered: <bool>` reports whether the
///   continuation turn landed on the session in this same request.
///
/// HTTP partial-success: if summarize succeeds but the continuation delivery
/// fails, the endpoint returns 200 with `{compacted: true,
/// continuation_delivered: false, error}` rather than 502. The compaction
/// side effect already happened — a 502 would tempt the caller to retry the
/// whole compact and pay for a second summarize LLM call.
///
/// Breaking changes vs. the prior version of this endpoint:
/// - Response envelope changed from `{status:"compact_triggered"}` to
///   `{status:"ok", compacted: <bool>, continuation_delivered: <bool>}`.
///   Callers asserting on the old literal must update.
/// - Request body is now strict: `CompactBody` rejects unknown fields (e.g. a
///   typo like `{"continuatino": "..."}` now returns 400 instead of silently
///   dropping the value).
///
/// Concurrency: the TUI branch rejects concurrent compact attempts with a
/// continuation so it cannot overwrite the in-flight caller's parked
/// continuation. The HTTP branch rejects any concurrent compact attempt for
/// the same Ouija session while summarize/prompt delivery is in flight.
pub async fn compact(
    State(state): State<SharedState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    Json(body): Json<CompactBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let (status, value) = compact_inner(&state, session_id, body).await;
    (status, Json(value))
}

/// Build the success envelope promised by [`compact`]'s docstring:
/// `{status, compacted, continuation_delivered}`, with an optional `error`
/// field appended for the HTTP partial-success case.
///
/// The shape must stay consistent across backends so a typed client
/// deserializer works uniformly on TUI and HTTP responses. On TUI the caller
/// always passes `continuation_delivered = false` because TUI delivery is
/// asynchronous: the continuation is parked on the session agent and the
/// post-compact hook drains it after this response has already returned, so
/// at response-construction time nothing has been synchronously delivered.
fn compact_success_body(continuation_delivered: bool, error: Option<String>) -> serde_json::Value {
    let mut body = json!({
        "status": "ok",
        "compacted": true,
        "continuation_delivered": continuation_delivered,
    });
    if let Some(err) = error {
        body["error"] = json!(err);
    }
    body
}

async fn compact_inner(
    state: &std::sync::Arc<crate::state::AppState>,
    session_id: String,
    body: CompactBody,
) -> (StatusCode, serde_json::Value) {
    // Normalize: trim surrounding whitespace so the same string reaches both
    // tmux paste and prompt_async, and treat empty/whitespace-only as None so
    // both branches apply the same "no continuation" rule.
    let continuation = body.continuation.and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    });

    // Read everything we need under a single lock acquisition so a racing
    // session mutation can't split backend_session_id from pane/project_dir
    // or flip the backend type between the lookup and the dispatch decision.
    let lookup = {
        let proto = state.protocol.read().await;
        match proto.sessions.get(&session_id) {
            Some(s) => SessionLookup {
                pane: s.pane.clone(),
                backend_session_id: s.metadata.backend_session_id.clone(),
                project_dir: s.metadata.project_dir.clone(),
                backend_name: s.metadata.backend.clone(),
                model: s.metadata.model.clone(),
                effort: s.metadata.effort.clone(),
                strong_opencode_binding: s.metadata.is_strong_opencode_binding(),
            },
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    json!({"error": format!("session '{}' not found", session_id)}),
                );
            }
        }
    };

    // Resolve the backend from the name captured above rather than re-reading
    // the protocol lock — prevents the branch decision from diverging from the
    // metadata it was taken on.
    let backend = match lookup.backend_name.as_deref() {
        Some(name) => state
            .backends
            .get(name)
            .unwrap_or_else(|| state.backends.default()),
        None => state.backends.default(),
    };

    match backend.delivery_mode() {
        crate::backend::DeliveryMode::TuiInjection => {
            let Some(pane) = lookup.pane else {
                return (
                    StatusCode::BAD_REQUEST,
                    json!({"error": "session has no pane (remote sessions cannot be compacted)"}),
                );
            };
            let Some(compact_cmd) = backend.compact_command().map(str::to_string) else {
                return (
                    StatusCode::BAD_REQUEST,
                    json!({"error": format!("backend '{}' does not support compact", backend.name())}),
                );
            };

            // Atomically acquire the compact slot. If another compact already parked a
            // continuation on this session, reject with 409 so the in-flight operation
            // isn't silently overwritten. Parking before injection is required so the
            // slot is reserved by the time /compact reaches the pane; the rollback below
            // releases the slot when injection fails synchronously so a later compact
            // doesn't see a stale continuation.
            let parked = if let Some(ref text) = continuation {
                let acquired = state
                    .try_set_pending_compact_continuation(&session_id, text.clone())
                    .await;
                if !acquired {
                    return (
                        StatusCode::CONFLICT,
                        json!({"error": "another compact continuation is already pending for this session"}),
                    );
                }
                true
            } else {
                false
            };

            if let Err(e) =
                tmux::locked_inject(state, &session_id, &pane, &compact_cmd, false).await
            {
                if parked {
                    // Rollback: drain what we just parked so a later compact doesn't
                    // splice this stale continuation into an unrelated turn. If the
                    // drain RPC comes back with nothing unexpectedly, log it — the
                    // slot may stay reserved and block future compacts until the
                    // agent is restarted.
                    if state
                        .drain_agent_compact_continuation(&session_id)
                        .await
                        .is_none()
                    {
                        tracing::warn!(
                            session = %session_id,
                            "rollback drain returned None after successful try-set; slot may be orphaned",
                        );
                    }
                }
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error": e.to_string()}),
                );
            }

            // On TUI, delivery (if any) happens asynchronously via the
            // post-compact hook draining the parked continuation, so nothing
            // has been delivered at response time — always `false` here.
            (StatusCode::OK, compact_success_body(false, None))
        }
        crate::backend::DeliveryMode::HttpApi { .. } => {
            let Some(backend_session_id) = lookup.backend_session_id else {
                return (
                    StatusCode::BAD_REQUEST,
                    json!({
                        "error": format!(
                            "session has no backend_session_id (backend '{}' not attached)",
                            backend.name()
                        )
                    }),
                );
            };
            if !lookup.strong_opencode_binding {
                return (
                    StatusCode::BAD_REQUEST,
                    json!({
                        "error": "opencode compact requires a strong managed backend binding; weak/adopted sessions cannot safely use HTTP-only summarize"
                    }),
                );
            }
            let Some(_compact_guard) =
                state.try_acquire_compact_in_progress(&format!("opencode:{backend_session_id}"))
            else {
                return (
                    StatusCode::CONFLICT,
                    json!({"error": "another compact operation is already in progress for this session"}),
                );
            };

            let Some((provider_id, model_id)) = resolve_opencode_compact_model(
                state,
                lookup.model.as_deref(),
                lookup.project_dir.as_deref(),
            )
            .await
            else {
                return (
                    StatusCode::BAD_REQUEST,
                    json!({
                        "error": "cannot resolve provider/model for summarize: session has no parseable `model` \
                                  (expected \"providerID/modelID\") and /config is unreachable \
                                  or has no top-level `model`. Configure the session with \
                                  `ouija spawn-session --model <p/m>` or set OpenCode's default model."
                    }),
                );
            };

            // Opencode runs /summarize synchronously — the request blocks
            // until the compaction LLM call and the follow-up prompt.loop
            // complete. That can take tens of seconds up to a few minutes
            // for a long session; 300s is a generous ceiling that matches
            // what the TUI path effectively allows by not timing out at all.
            let port = state.opencode_serve_port();
            let summarize_url =
                format!("http://127.0.0.1:{port}/session/{backend_session_id}/summarize");
            let summarize_body = json!({
                "providerID": provider_id,
                "modelID": model_id,
            });
            let mut summarize_req = state
                .http_client
                .post(&summarize_url)
                .json(&summarize_body)
                .timeout(std::time::Duration::from_secs(300));
            if let Some(dir) = lookup.project_dir.as_deref() {
                summarize_req = summarize_req.header("x-opencode-directory", dir);
            }
            match summarize_req.send().await {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => {
                    let status = r.status();
                    let text = r.text().await.unwrap_or_default();
                    return (
                        StatusCode::BAD_GATEWAY,
                        json!({"error": format!("opencode /summarize returned {status}: {text}")}),
                    );
                }
                Err(e) => {
                    return (
                        StatusCode::BAD_GATEWAY,
                        json!({"error": format!("opencode /summarize request failed: {e}")}),
                    );
                }
            }

            // Context is now compacted on the opencode server. If the caller
            // supplied a continuation, deliver it as a fresh user turn. A
            // delivery failure here is surfaced as 200 + continuation_delivered:
            // false (not 502) because the compaction side effect already
            // happened — a 502 would tempt the caller to retry the whole
            // compact, paying for a second summarize on an already-compacted
            // session.
            let continuation_delivered = if let Some(continuation) = continuation {
                match tmux::deliver_via_http(
                    state,
                    &backend_session_id,
                    lookup.project_dir.as_deref(),
                    &continuation,
                    lookup.model.as_deref(),
                    lookup.effort.as_deref(),
                )
                .await
                {
                    Ok(()) => true,
                    Err(decision) => {
                        tracing::warn!(
                            session = %session_id,
                            ?decision,
                            "continuation delivery failed after successful summarize"
                        );
                        return (
                            StatusCode::OK,
                            compact_success_body(
                                false,
                                Some(format!(
                                    "opencode continuation delivery failed: {decision:?}"
                                )),
                            ),
                        );
                    }
                }
            } else {
                false
            };

            (
                StatusCode::OK,
                compact_success_body(continuation_delivered, None),
            )
        }
    }
}

struct SessionLookup {
    pane: Option<String>,
    backend_session_id: Option<String>,
    project_dir: Option<String>,
    backend_name: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    strong_opencode_binding: bool,
}

// --- Nodes ---

/// List connected remote nodes with their sessions.
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

/// Disconnect a remote node and remove its sessions.
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

/// Return the current daemon settings.
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
    max_local_sessions: Option<u64>,
}

/// Patch daemon settings and persist to disk.
pub async fn update_settings(
    State(state): State<SharedState>,
    Json(body): Json<SettingsUpdateBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut settings = state.settings.write().await;
    if let Some(v) = body.auto_register {
        settings.auto_register = v;
    }
    let projects_dir_changed = body.projects_dir.is_some();
    if let Some(v) = body.projects_dir {
        settings.projects_dir = Some(v);
    }
    if let Some(v) = body.idle_timeout_secs {
        settings.idle_timeout_secs = v;
    }
    if let Some(v) = body.reaper_interval_secs {
        settings.reaper_interval_secs = v;
    }
    if let Some(v) = body.max_local_sessions {
        settings.max_local_sessions = v;
    }
    if let Err(e) = crate::persistence::save_settings(&state.config.config_dir, &settings) {
        tracing::warn!("failed to save settings: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to save: {e}") })),
        );
    }
    // Drop the write lock before spawning the refresh
    drop(settings);
    // Rebuild project index when projects_dir changes
    if projects_dir_changed {
        let s = state.clone();
        tokio::spawn(async move {
            crate::project_index::refresh_index(&s).await;
        });
    }
    let settings = state.settings.read().await;
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
    let mut count = 0;
    {
        let mut proto = state.protocol.write().await;
        for session in proto.sessions.values_mut() {
            if matches!(session.origin, crate::daemon_protocol::Origin::Local) {
                if let Some(v) = body.networked {
                    if session.metadata.networked != v {
                        session.metadata.networked = v;
                        count += 1;
                    }
                }
            }
        }
    }
    if count > 0 {
        transport::broadcast_local_sessions(&state).await;
    }
    (StatusCode::OK, Json(json!({ "updated": count })))
}

#[derive(Debug, Deserialize)]
pub struct BulkSessionUpdateBody {
    networked: Option<bool>,
}

/// Return the list of configured Nostr relay URLs.
pub async fn get_relays(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let relays = crate::nostr_transport::load_relays(&state.config.data_dir);
    Json(json!({ "relays": relays }))
}

#[derive(Debug, Deserialize)]
pub struct RelaysUpdateBody {
    relays: Vec<String>,
}

/// Replace the Nostr relay list and persist to disk.
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

/// List all scheduled tasks sorted by creation time.
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
                "enabled": t.enabled,
                "next_run": t.next_run,
                "last_run": t.last_run,
                "last_status": t.last_status,
                "run_count": t.run_count,
                "project_dir": t.project_dir,
                "once": t.once,
                "backend_session_id": t.backend_session_id,
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
    prompt: Option<String>,
    reminder: Option<String>,
    project_dir: Option<String>,
    #[serde(default)]
    once: Option<bool>,
    #[serde(alias = "claude_session_id")]
    backend_session_id: Option<String>,
    #[serde(default)]
    on_fire: Option<crate::scheduler::OnFire>,
}

/// Create a new scheduled task with a cron expression.
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

    let mut task = scheduler::new_task(
        body.name,
        body.cron,
        body.target_session,
        body.prompt,
        body.reminder,
        body.once.unwrap_or(false),
        body.backend_session_id,
        body.on_fire.unwrap_or_default(),
    );
    task.project_dir = body.project_dir;

    let id = task.id.clone();
    state.add_task(task).await;

    (StatusCode::OK, Json(json!({ "created": id })))
}

#[derive(Debug, Deserialize)]
pub struct TaskIdBody {
    id: String,
}

/// Delete a scheduled task by ID.
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

/// Enable a disabled scheduled task.
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

/// Disable a scheduled task without deleting it.
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

/// Immediately fire a scheduled task, ignoring its cron schedule.
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

#[derive(Debug, Deserialize, Default)]
pub struct TaskRunsQuery {
    task: Option<String>,
}

/// Return recent task execution history, newest first.
pub async fn list_task_runs(
    State(state): State<SharedState>,
    Query(query): Query<TaskRunsQuery>,
) -> Json<serde_json::Value> {
    let runs = state.task_runs.read().await;
    let entries: Vec<serde_json::Value> = runs
        .iter()
        .rev()
        .filter(|r| query.task.as_ref().is_none_or(|id| r.task_id == *id))
        .take(MAX_TASK_RUNS_RETURNED)
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

#[derive(Debug, Deserialize)]
pub struct AddHumanBody {
    pub npub: String,
    pub name: String,
    pub default_session: Option<String>,
}

/// Add or update a human Nostr session configuration.
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
        let proto = state.protocol.read().await;
        if proto
            .sessions
            .get(&name)
            .is_some_and(|s| !matches!(s.origin, crate::daemon_protocol::Origin::Human(_)))
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

    // Register the human session in protocol state
    {
        let mut proto = state.protocol.write().await;
        proto.sessions.entry(name.clone()).or_insert_with(|| {
            crate::daemon_protocol::SessionEntry {
                id: name.clone(),
                pane: None,
                origin: crate::daemon_protocol::Origin::Human(body.npub.clone()),
                metadata: crate::daemon_protocol::SessionMeta {
                    role: Some("human".to_string()),
                    networked: false,
                    ..Default::default()
                },
                ..Default::default()
            }
        });
    }

    (
        StatusCode::OK,
        Json(json!({ "status": "added", "name": name })),
    )
}

#[derive(Debug, Deserialize)]
pub struct RemoveHumanBody {
    pub name: String,
}

/// Remove a human session configuration by name.
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

    // Remove the session from protocol state
    {
        let mut proto = state.protocol.write().await;
        if proto
            .sessions
            .get(&body.name)
            .is_some_and(|s| matches!(s.origin, crate::daemon_protocol::Origin::Human(_)))
        {
            proto.sessions.remove(&body.name);
        }
    }

    (StatusCode::OK, Json(json!({ "status": "removed" })))
}

/// List configured human Nostr sessions.
pub async fn list_humans(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let settings = state.settings.read().await;
    let humans: Vec<serde_json::Value> = settings
        .human_sessions
        .iter()
        .map(|h| {
            json!({
                "name": h.name,
                "npub": h.npub,
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
    #[serde(default)]
    from: Option<String>,
    /// Which coding assistant backend to use (e.g. "claude-code", "codex").
    #[serde(default)]
    backend: Option<String>,
    /// Which LLM model to use.
    ///
    /// Passed through to the backend: for claude-code this becomes
    /// `claude --model <X>`; for opencode it is split on the first `/` into
    /// `providerID/modelID` and sent on each `prompt_async` body.
    #[serde(default)]
    model: Option<String>,
    /// Reasoning effort / variant for the model.
    ///
    /// For claude-code: passed as `claude --effort <X>`.
    /// For opencode: sent as `variant` on each `prompt_async` body.
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    reminder: Option<String>,
    /// Git branch name for worktree sessions. If omitted, defaults to the session name.
    #[serde(default)]
    branch: Option<String>,
    /// Base branch to create the worktree branch from. If omitted, branches from HEAD.
    #[serde(default)]
    base_branch: Option<String>,
    /// On kill, preserve the worktree directory instead of cleaning it up.
    /// Defaults to false (cleanup) when omitted.
    #[serde(default)]
    keep_worktree: Option<bool>,
    /// Opt-in to the data-destructive worktree reset on respawn.
    ///
    /// When the worktree dir already exists and `base_branch` is supplied,
    /// ouija used to unconditionally `git checkout -B <branch> <base>`,
    /// silently discarding every commit the branch was ahead of base
    /// (hub#528). The default is now `false`: ouija skips the reset and
    /// WARNs if the branch is ahead. Callers that *want* the reset (e.g. a
    /// legitimate "redraft from scratch" flow) must pass `force_reset=true`
    /// so the intent is explicit and auditable.
    #[serde(default)]
    force_reset: Option<bool>,
}

/// Return a warning message when the caller's request carries
/// destructive intent (`force_reset=true` or a `base_branch` override)
/// that the restart path cannot honor.
///
/// `/api/sessions/start` routes to `restart_session` when the named
/// session is already registered. `restart_session` reuses the existing
/// worktree dir from `SessionMeta.project_dir` as-is and does not call
/// `create_ouija_worktree`, so `base_branch` and `force_reset` have no
/// downstream hook to act on. This predicate centralizes the "dropped
/// intent" check so the API handler can `tracing::warn!` before routing
/// — making the drop auditable from daemon logs even when hub cannot
/// act on the return envelope (202 Accepted is sent before the work
/// runs, per the ExistingOrOtherDesignDecision recorded on hub#528).
///
/// Returns `None` when the body does not assert any restart-incompatible
/// intent. Returns `Some(msg)` with a single diagnostic line when it
/// does. Caller emits the warn; predicate stays pure for unit testing.
fn restart_drops_destructive_intent(body: &SessionNameBody) -> Option<String> {
    let mut dropped: Vec<&str> = Vec::new();
    if body.force_reset == Some(true) {
        dropped.push("force_reset=true");
    }
    if body.base_branch.is_some() {
        dropped.push("base_branch");
    }
    if dropped.is_empty() {
        return None;
    }
    Some(format!(
        "session '{}' is already registered, routing to restart_session \
         which cannot act on {}; destructive intent silently dropped. \
         File a ticket for a sync reset endpoint if this is load-bearing.",
        body.name,
        dropped.join(", ")
    ))
}

/// Kill the coding assistant process in a session's tmux pane.
pub async fn kill_session(
    State(state): State<SharedState>,
    Json(body): Json<SessionNameBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let result = if body.keep_worktree.unwrap_or(false) {
        crate::nostr_transport::kill_session_keep_worktree(&state, &body.name).await
    } else {
        crate::nostr_transport::kill_session(&state, &body.name).await
    };
    (StatusCode::OK, Json(json!({ "result": result })))
}

/// Prune stale sessions whose worktree is missing.
///
/// Default dry-run: returns IDs that would be pruned without removing.
/// With confirm=true: removes sessions via Remove { keep_worktree: true }
/// to avoid triggering CleanupWorktree on already-missing dirs.
pub async fn prune_stale_sessions(
    State(state): State<SharedState>,
    Json(body): Json<PruneStaleBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let stale_sessions: Vec<(String, String)> = {
        let proto = state.protocol.read().await;
        proto
            .sessions
            .values()
            .filter(|s| {
                matches!(s.origin, crate::daemon_protocol::Origin::Local)
                    && s.metadata.worktree_present == Some(false)
            })
            .filter_map(|s| {
                s.metadata
                    .project_dir
                    .as_ref()
                    .map(|d| (s.id.clone(), d.clone()))
            })
            .collect()
    };
    if !body.confirm {
        return (
            StatusCode::OK,
            Json(
                json!({ "dry_run": true, "would_prune": stale_sessions.iter().map(|(id, _)| id).cloned().collect::<Vec<_>>() }),
            ),
        );
    }
    let mut pruned = Vec::new();
    let mut errors = Vec::new();
    let mut already_gone = Vec::new();
    // Single batched apply: the handler runs each session's RemoveIfStale guard
    // under one write lock and coalesces Persist + BroadcastSessionList into
    // one of each, rather than N full state writes for N stale sessions.
    let input_ids: Vec<String> = stale_sessions.iter().map(|(id, _)| id.clone()).collect();
    let effects = state
        .apply_and_execute(crate::daemon_protocol::Event::PruneStale {
            sessions: stale_sessions,
        })
        .await;
    let pruned_set: std::collections::HashSet<String> = effects
        .iter()
        .filter_map(|e| match e {
            crate::daemon_protocol::Effect::RemoveOk { id } => Some(id.clone()),
            _ => None,
        })
        .collect();
    // Bucket failures via the structured RemoveFailureKind discriminator —
    // never via reason substring matching (which would misclassify any session
    // id or project_dir that happens to contain a substring like "not found").
    let already_gone_set: std::collections::HashSet<String> = effects
        .iter()
        .filter_map(|e| match e {
            crate::daemon_protocol::Effect::RemoveFailed { id, kind, .. }
                if *kind == crate::daemon_protocol::RemoveFailureKind::NotFound =>
            {
                Some(id.clone())
            }
            _ => None,
        })
        .collect();
    for id in input_ids {
        if pruned_set.contains(&id) {
            pruned.push(id);
        } else if already_gone_set.contains(&id) {
            tracing::debug!("session {} vanished between snapshot and prune", id);
            already_gone.push(id);
        } else {
            tracing::warn!(
                "failed to prune session {} (no longer stale or guard tripped)",
                id
            );
            errors.push(id);
        }
    }
    let response = if errors.is_empty() && already_gone.is_empty() {
        json!({ "dry_run": false, "pruned": pruned })
    } else {
        let mut obj = serde_json::Map::new();
        obj.insert("dry_run".into(), serde_json::Value::Bool(false));
        obj.insert(
            "pruned".into(),
            serde_json::Value::Array(pruned.into_iter().map(serde_json::Value::String).collect()),
        );
        if !errors.is_empty() {
            obj.insert(
                "errors".into(),
                serde_json::Value::Array(
                    errors.into_iter().map(serde_json::Value::String).collect(),
                ),
            );
        }
        if !already_gone.is_empty() {
            obj.insert(
                "already_gone".into(),
                serde_json::Value::Array(
                    already_gone
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
        }
        serde_json::Value::Object(obj)
    };
    (StatusCode::OK, Json(response))
}

#[derive(serde::Deserialize)]
pub struct PruneStaleBody {
    #[serde(default)]
    confirm: bool,
}

/// Start a new session in a tmux pane, optionally in a worktree.
pub async fn start_session(
    State(state): State<SharedState>,
    Json(body): Json<SessionNameBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Normalize at the boundary: `Some("")` or `Some("   ")` must not flow
    // through as an explicit override — it would clobber prev_metadata on
    // restart with an empty string and produce malformed CLI invocations.
    let mut body = body;
    body.model = normalize_optional_string(body.model);
    body.effort = normalize_optional_string(body.effort);

    // Return 202 immediately — all work (registration + boot) happens in background.
    let name = body.name.clone();
    let state2 = state.clone();
    tokio::spawn(async move {
        // If session already exists, restart with fresh context instead of failing.
        let exists = state2
            .protocol
            .read()
            .await
            .sessions
            .contains_key(&body.name);
        if exists {
            tracing::info!(
                "session '{}' exists, restarting with fresh context",
                body.name
            );
            if let Some(msg) = restart_drops_destructive_intent(&body) {
                tracing::warn!("{msg}");
            }
            let (_result, _msg_id) = crate::nostr_transport::restart_session(
                &state2,
                &body.name,
                true, // fresh
                body.prompt.as_deref(),
                body.from.as_deref(),
                None, // expects_reply not used for session start
                body.backend.as_deref(),
                body.model.as_deref(),
                body.effort.as_deref(),
                body.reminder.as_deref(),
            )
            .await;

            tracing::info!("async session restart complete: {}", body.name);
            return;
        }

        let (result, _prompt_msg_id) = crate::nostr_transport::start_session(
            &state2,
            &body.name,
            body.worktree,
            body.project_dir.as_deref(),
            body.prompt.as_deref(),
            body.from.as_deref(),
            None, // expects_reply not used for session start
            body.backend.as_deref(),
            body.model.as_deref(),
            body.effort.as_deref(),
            body.reminder.as_deref(),
            body.branch.as_deref(),
            body.base_branch.as_deref(),
            body.force_reset.unwrap_or(false),
        )
        .await;

        tracing::info!(
            "async session start complete: {}, result: {result}",
            body.name
        );
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({ "session": name, "status": "starting" })),
    )
}

/// Kill and restart a session, optionally with a fresh conversation.
pub async fn restart_session(
    State(state): State<SharedState>,
    Json(body): Json<SessionNameBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Normalize at the boundary; see start_session for rationale.
    let mut body = body;
    body.model = normalize_optional_string(body.model);
    body.effort = normalize_optional_string(body.effort);

    // restart_session shares SessionNameBody with start_session, so
    // `force_reset` and `base_branch` deserialize here too — but the
    // underlying `nostr_transport::restart_session` does not accept or
    // act on them (no create_ouija_worktree call; the worktree is
    // reused as-is from prev_metadata.project_dir). Warn-log the drop
    // so the caller's opt-in is visible in daemon logs. Same predicate
    // and same rationale as the /api/sessions/start exists branch
    // (hub#528 review).
    if let Some(msg) = restart_drops_destructive_intent(&body) {
        tracing::warn!("{msg}");
    }

    let fresh = body.fresh.unwrap_or(false);
    let (result, _prompt_msg_id) = crate::nostr_transport::restart_session(
        &state,
        &body.name,
        fresh,
        body.prompt.as_deref(),
        body.from.as_deref(),
        None, // expects_reply not used for session restart
        body.backend.as_deref(),
        body.model.as_deref(),
        body.effort.as_deref(),
        body.reminder.as_deref(),
    )
    .await;

    (StatusCode::OK, Json(json!({ "result": result })))
}

/// Check if interactive mode is currently blocked.
pub async fn get_block_interactive(
    State(_state): State<SharedState>,
    axum::extract::Path(_pane): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    // block_interactive is no longer tracked in protocol state
    Json(json!({ "block_interactive": false }))
}

/// Clear the interactive block flag (no-op, kept for compat).
pub async fn clear_block_interactive(
    State(_state): State<SharedState>,
    axum::extract::Path(_pane): axum::extract::Path<String>,
) -> StatusCode {
    // block_interactive is no longer tracked in protocol state
    StatusCode::OK
}

/// Resolve a pane URL segment to the session id registered on that pane.
///
/// Axum percent-decodes path segments, so a literal tmux pane id like `%74`
/// placed raw in the URL arrives here as `t` (0x74 == ASCII `t`). Callers
/// must therefore send the pane *suffix* (just the number). We tolerate an
/// optional leading `%` defensively so a future caller that correctly
/// URL-encodes the percent as `%25` (extracted value: `%74`) also works.
///
/// See issue #646: `/api/pane/{pane}/...` routes used to silently 404 on
/// raw `%`-prefixed pane ids, and one handler (get_pending_replies) even
/// masked the bug by returning `200 + []` on pane lookup miss.
fn resolve_pane_to_session(
    proto: &crate::daemon_protocol::DaemonState,
    raw: &str,
) -> Option<String> {
    let suffix = raw.strip_prefix('%').unwrap_or(raw);
    let pane_id = format!("%{suffix}");
    proto
        .sessions
        .values()
        .find(|s| s.pane.as_deref() == Some(&pane_id))
        .map(|s| s.id.clone())
}

/// Return pending reply entries for a session identified by pane.
///
/// Returns 404 when the pane is not registered: the old fail-open behaviour
/// (`200 + []` on miss) masked the silent-404 bug described above.
pub async fn get_pending_replies(
    State(state): State<SharedState>,
    axum::extract::Path(pane): axum::extract::Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let (status, value) = get_pending_replies_inner(&state, pane).await;
    (status, Json(value))
}

async fn get_pending_replies_inner(
    state: &SharedState,
    pane: String,
) -> (StatusCode, serde_json::Value) {
    let session_id = {
        let proto = state.protocol.read().await;
        resolve_pane_to_session(&proto, &pane)
    };
    let Some(id) = session_id else {
        return (
            StatusCode::NOT_FOUND,
            json!({ "error": format!("pane '{pane}' is not registered") }),
        );
    };
    let replies = state.query_agent_pending_replies(&id).await;
    let list: Vec<_> = replies
        .iter()
        .map(|r| json!({ "msg_id": r.msg_id, "from": r.from, "message": r.message, "received_at": r.received_at }))
        .collect();
    (
        StatusCode::OK,
        json!({ "pending_replies": list, "count": list.len() }),
    )
}

/// Clear a pending reply from a specific sender on a pane's session.
///
/// Returns a JSON acknowledgement on success so callers can distinguish
/// "actually cleared something" (`cleared >= 1`) from "pane exists but the
/// named sender had no pending slot" (`cleared: 0`). 404 means the pane is
/// not registered; the response body includes a JSON `error` field.
pub async fn delete_pending_reply(
    State(state): State<SharedState>,
    axum::extract::Path((pane, from)): axum::extract::Path<(String, String)>,
) -> (StatusCode, Json<serde_json::Value>) {
    let (status, value) = delete_pending_reply_inner(&state, pane, from).await;
    (status, Json(value))
}

async fn delete_pending_reply_inner(
    state: &SharedState,
    pane: String,
    from: String,
) -> (StatusCode, serde_json::Value) {
    let session_id = {
        let proto = state.protocol.read().await;
        resolve_pane_to_session(&proto, &pane)
    };
    let Some(id) = session_id else {
        return (
            StatusCode::NOT_FOUND,
            json!({ "error": format!("pane '{pane}' is not registered") }),
        );
    };
    let cleared = {
        let mut proto = state.protocol.write().await;
        proto.clear_pending_reply_from(&id, &from)
    };
    (StatusCode::OK, json!({ "cleared": cleared }))
}

/// Notify the session agent that the coding assistant has stopped in a pane.
///
/// Idempotent: returns 200 even when the pane is not registered. Hooks call
/// this on every Stop, and a transient pane-lookup miss should not surface
/// as an error to the hook script.
pub async fn session_stopped(
    State(state): State<SharedState>,
    axum::extract::Path(pane): axum::extract::Path<String>,
) -> StatusCode {
    let session_id = {
        let proto = state.protocol.read().await;
        resolve_pane_to_session(&proto, &pane)
    };
    if let Some(id) = session_id {
        state
            .notify_agent(&id, crate::session_agent::SessionMsg::Stopped)
            .await;
    }
    StatusCode::OK
}

/// Notify the session agent that the coding assistant is active in a pane.
///
/// Idempotent: see `session_stopped` for the 200-on-miss rationale.
pub async fn session_active(
    State(state): State<SharedState>,
    axum::extract::Path(pane): axum::extract::Path<String>,
) -> StatusCode {
    let session_id = {
        let proto = state.protocol.read().await;
        resolve_pane_to_session(&proto, &pane)
    };
    if let Some(id) = session_id {
        state
            .notify_agent(&id, crate::session_agent::SessionMsg::Active)
            .await;
    }
    StatusCode::OK
}

/// Deliver a pending prompt for the given session, if one is queued.
async fn deliver_pending_prompt(state: &SharedState, session_name: &str) -> bool {
    let pending = state.pending_prompts.lock().unwrap().remove(session_name);
    let Some(pending) = pending else {
        tracing::info!(
            session = session_name,
            "opencode pending prompt: none queued at readiness"
        );
        return false;
    };
    let pane_id = pending.pane_id.clone();
    let prompt = pending.prompt.clone();

    let (pane_still_registered, backend_session_matches, http_delivery) = {
        let proto = state.protocol.read().await;
        match proto.sessions.get(session_name) {
            Some(session) => (
                session.pane.as_deref() == Some(pane_id.as_str()),
                pending
                    .backend_session_id
                    .as_deref()
                    .is_none_or(|expected| {
                        session.metadata.backend_session_id.as_deref() == Some(expected)
                    }),
                session
                    .metadata
                    .is_strong_opencode_binding()
                    .then(|| session.metadata.http_delivery_snapshot())
                    .flatten(),
            ),
            None => (false, false, None),
        }
    };
    if !pane_still_registered {
        restore_pending_prompt_if_absent(state, session_name, pending);
        tracing::warn!(
            "readiness prompt delivery skipped for {session_name}: queued pane is no longer registered to the session"
        );
        return false;
    }
    if !backend_session_matches {
        restore_pending_prompt_if_absent(state, session_name, pending);
        tracing::warn!(
            "readiness prompt delivery skipped for {session_name}: queued OpenCode backend session is no longer current"
        );
        return false;
    }

    let used_http_delivery = http_delivery.is_some();
    let result = match http_delivery {
        Some(delivery) => match deliver_http_message_outcome(state, &delivery, &prompt).await {
            crate::state::DeliveryOutcome::Accepted => Ok(()),
            crate::state::DeliveryOutcome::Ambiguous(_) => {
                restore_pending_prompt_if_absent(state, session_name, pending);
                tracing::warn!(
                    "readiness prompt HTTP delivery failed ambiguously for {session_name}; not retrying via raw tmux"
                );
                return false;
            }
            crate::state::DeliveryOutcome::Rejected(reason) => Err(anyhow::anyhow!(reason)),
        },
        None => {
            deliver_pending_prompt_via_raw_tmux(
                state,
                session_name,
                &pane_id,
                &prompt,
                pending.backend_session_id.as_deref(),
            )
            .await
        }
    };

    match result {
        Ok(()) => {
            tracing::info!("delivered queued prompt to {session_name} via readiness signal");
            true
        }
        Err(e) if used_http_delivery => {
            tracing::warn!(
                "readiness prompt HTTP delivery failed for {session_name}, trying raw tmux fallback: {e}"
            );
            match deliver_pending_prompt_via_raw_tmux(
                state,
                session_name,
                &pane_id,
                &prompt,
                pending.backend_session_id.as_deref(),
            )
            .await
            {
                Ok(()) => {
                    tracing::info!(
                        "delivered queued prompt to {session_name} via raw tmux fallback"
                    );
                    true
                }
                Err(fallback_error) => {
                    restore_pending_prompt_if_absent(state, session_name, pending.clone());
                    schedule_pending_prompt_retry(state, session_name, pending);
                    tracing::warn!(
                        "readiness prompt fallback failed for {session_name}: {fallback_error}"
                    );
                    false
                }
            }
        }
        Err(e) => {
            restore_pending_prompt_if_absent(state, session_name, pending.clone());
            schedule_pending_prompt_retry(state, session_name, pending);
            tracing::warn!("readiness prompt raw tmux delivery failed for {session_name}: {e}");
            false
        }
    }
}

#[cfg(test)]
const PENDING_PROMPT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(10);
#[cfg(not(test))]
const PENDING_PROMPT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(10);
const PENDING_PROMPT_MAX_RETRIES: u8 = 3;

fn schedule_pending_prompt_retry(
    state: &SharedState,
    session_name: &str,
    pending_prompt: crate::state::PendingPrompt,
) {
    schedule_pending_prompt_retry_attempt(state, session_name, pending_prompt, 1);
}

fn schedule_pending_prompt_retry_attempt(
    state: &SharedState,
    session_name: &str,
    pending_prompt: crate::state::PendingPrompt,
    attempt: u8,
) {
    let state = state.clone();
    let session_name = session_name.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(PENDING_PROMPT_RETRY_DELAY).await;
        let pending = reserve_pending_prompt_if_matches(
            &state,
            &session_name,
            &pending_prompt.pane_id,
            &pending_prompt.prompt,
            pending_prompt.backend_session_id.as_deref(),
        );
        let Some(pending) = pending else {
            return;
        };
        match deliver_pending_prompt_via_raw_tmux(
            &state,
            &session_name,
            &pending.pane_id,
            &pending.prompt,
            pending.backend_session_id.as_deref(),
        )
        .await
        {
            Ok(()) => {}
            Err(error) => {
                restore_pending_prompt_if_absent(&state, &session_name, pending.clone());
                if attempt < PENDING_PROMPT_MAX_RETRIES {
                    schedule_pending_prompt_retry_attempt(
                        &state,
                        &session_name,
                        pending,
                        attempt + 1,
                    );
                }
                tracing::warn!(
                    "readiness prompt retry fallback attempt {attempt}/{PENDING_PROMPT_MAX_RETRIES} failed for {session_name}: {error}"
                )
            }
        }
    });
}

fn reserve_pending_prompt_if_matches(
    state: &SharedState,
    session_name: &str,
    pane_id: &str,
    prompt: &str,
    backend_session_id: Option<&str>,
) -> Option<crate::state::PendingPrompt> {
    let mut pending = state.pending_prompts.lock().unwrap();
    if pending.get(session_name).is_some_and(|pending| {
        pending.pane_id == pane_id
            && pending.prompt == prompt
            && pending.backend_session_id.as_deref() == backend_session_id
    }) {
        return pending.remove(session_name);
    }
    None
}

async fn deliver_pending_prompt_via_raw_tmux(
    state: &SharedState,
    session_name: &str,
    pane_id: &str,
    prompt: &str,
    expected_backend_session_id: Option<&str>,
) -> anyhow::Result<()> {
    let (pane_still_registered, backend_session_matches) = {
        let proto = state.protocol.read().await;
        match proto.sessions.get(session_name) {
            Some(session) => (
                session.pane.as_deref() == Some(pane_id),
                expected_backend_session_id.is_none_or(|expected| {
                    session.metadata.backend_session_id.as_deref() == Some(expected)
                }),
            ),
            None => (false, false),
        }
    };
    if !pane_still_registered {
        anyhow::bail!(
            "readiness prompt fallback skipped: pane {pane_id} is no longer registered to session {session_name}"
        );
    }
    if !backend_session_matches {
        anyhow::bail!(
            "readiness prompt fallback skipped: queued OpenCode backend session is no longer current for session {session_name}"
        );
    }
    ensure_pending_prompt_pane_is_live(state, session_name, pane_id).await?;

    crate::tmux::locked_inject_raw_tmux(state, session_name, pane_id, prompt, false).await
}

async fn ensure_pending_prompt_pane_is_live(
    state: &SharedState,
    session_name: &str,
    pane_id: &str,
) -> anyhow::Result<()> {
    let pane_live = if cfg!(test) {
        state
            .list_assistant_panes()
            .await
            .iter()
            .any(|pane| pane.pane_id == pane_id)
    } else {
        let process_names: Vec<String> = state
            .backend_for_session(session_name)
            .await
            .process_names()
            .iter()
            .map(|name| name.to_string())
            .collect();
        let pane_id = pane_id.to_string();
        tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = process_names.iter().map(|name| name.as_str()).collect();
            crate::tmux::pane_alive(&pane_id, &refs)
        })
        .await
        .unwrap_or(false)
    };

    if !pane_live {
        anyhow::bail!(
            "readiness prompt fallback skipped: pane {pane_id} is not running the session backend"
        );
    }
    Ok(())
}

fn restore_pending_prompt_if_absent(
    state: &SharedState,
    session_name: &str,
    pending_prompt: crate::state::PendingPrompt,
) {
    let mut pending = state.pending_prompts.lock().unwrap();
    pending
        .entry(session_name.to_string())
        .or_insert(pending_prompt);
}

/// Handle a readiness signal from an HttpApi session's plugin.
pub async fn session_ready(
    State(state): State<SharedState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    let delivered = deliver_pending_prompt(&state, &session_id).await;
    Json(json!({"delivered": delivered}))
}

/// Handle a readiness signal keyed by opencode backend session ID.
/// Resolves the ouija session name internally, avoiding plugin-side race conditions.
///
/// Resolution order:
/// 1. Direct lookup by `backend_session_id` — hub-spawned sessions and any
///    previously-adopted session already have this bound.
/// 2. Adoption — query opencode serve for the session's directory, then
///    look for a pre-existing local ouija session in that directory whose
///    `backend_session_id` is still unset. This handles the case where the
///    daemon knew about the session before opencode attached a backend ID.
/// 3. Auto-provision (issue #35) — when the caller is a human/agent starting
///    opencode themselves in a fresh directory, there is no pre-existing
///    record to adopt. Scan tmux for the opencode pane in that directory and
///    create a fresh session record with the backend_session_id bound.
///    Gated by the `auto_register` setting so operators who opted out of
///    implicit registration keep the strict behaviour.
pub async fn backend_session_ready(
    State(state): State<SharedState>,
    axum::extract::Path(backend_sid): axum::extract::Path<String>,
    body_bytes: Bytes,
) -> Json<serde_json::Value> {
    if let Some(error) = validate_backend_session_id_boundary(&backend_sid) {
        return Json(json!({"delivered": false, "error": error}));
    }

    // Parse the body as optional hints. An empty body, `{}`, or malformed
    // JSON all degrade cleanly to "no hints" — older plugin builds POST an
    // empty body, and we must not 400 them.
    let hints = if body_bytes.is_empty() {
        BackendSessionReadyHints::default()
    } else {
        match serde_json::from_slice::<BackendSessionReadyHints>(&body_bytes) {
            Ok(h) => h,
            Err(e) => {
                // Log so operators can see the fallback — without this, a
                // plugin bug or a wire-format drift would silently route
                // every request through the slow scan path with no clue.
                tracing::debug!(
                    target: "ouija::api::backend_session_ready",
                    "failed to parse readiness hints ({e}); falling back to scan path"
                );
                BackendSessionReadyHints::default()
            }
        }
    };
    tracing::info!(
        target: "ouija::api::backend_session_ready",
        backend_session_id = %backend_sid,
        hint_pane = ?hints.pane,
        hint_cwd = ?hints.cwd,
        "backend session ready received"
    );
    Json(backend_session_ready_inner_with_hints(&state, backend_sid, hints).await)
}

/// Optional hints the opencode plugin may send in the readiness POST body.
/// Both fields present: skip the opencode-serve dir lookup AND the tmux
/// scan-by-dir, using the explicit values directly. Otherwise fall back to
/// the existing resolve-by-scan path (see the decision recorded on this
/// task — partial hints are an out-of-scope refactor).
///
/// The plugin-side body is a forward-compatible contract: future plugin
/// releases will add fields (plugin_version, tty_path, etc.) that older
/// daemons MUST be able to ignore without discarding the fields they do
/// know. That is why there is no `deny_unknown_fields` — Postel's law
/// applies at the plugin-to-daemon boundary.
#[derive(Debug, Default, Deserialize)]
struct BackendSessionReadyHints {
    #[serde(default)]
    pane: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[cfg(test)]
async fn backend_session_ready_inner(
    state: &std::sync::Arc<crate::state::AppState>,
    backend_sid: String,
) -> serde_json::Value {
    backend_session_ready_inner_with_hints(state, backend_sid, BackendSessionReadyHints::default())
        .await
}

async fn backend_session_ready_inner_with_hints(
    state: &std::sync::Arc<crate::state::AppState>,
    backend_sid: String,
    hints: BackendSessionReadyHints,
) -> serde_json::Value {
    if let Some(error) = validate_backend_session_id_boundary(&backend_sid) {
        return json!({"delivered": false, "error": error});
    }

    // Step 1: direct lookup. This runs FIRST regardless of hints — hub-
    // spawned and previously-adopted sessions must win over any hint-derived
    // id, or a stale plugin cwd could shadow the real session.
    let session_name = {
        let proto = state.protocol.read().await;
        find_local_session_by_backend_session_id(&proto, &backend_sid).map(|s| s.id.clone())
    };

    let name = if let Some(n) = session_name {
        tracing::info!(
            target: "ouija::api::backend_session_ready",
            backend_session_id = %backend_sid,
            session = %n,
            "ready direct lookup matched existing backend session"
        );
        n
    } else {
        // Step 2: adoption. Consumes one opencode-serve round-trip internally;
        // we redo it here in step 3 if adoption misses, since auto-provision
        // needs the dir too. The double call is intentionally kept to preserve
        // adoption's existing call signature (and fail-mode coverage) for this
        // surgical change — dir lookup is a cheap loopback GET.
        let adopted = adopt_backend_session_id(state, &backend_sid).await;

        if let Some(n) = adopted {
            tracing::info!(
                target: "ouija::api::backend_session_ready",
                backend_session_id = %backend_sid,
                session = %n,
                "ready adopted backend session into existing ouija session"
            );
            n
        } else {
            // Step 3: auto-provision for ad-hoc opencode sessions (issue #35).
            let auto_register = state.settings.read().await.auto_register;
            if !auto_register {
                tracing::warn!(
                    target: "ouija::api::backend_session_ready",
                    "auto_register disabled; declining to auto-provision for backend_session_id {backend_sid}"
                );
                return json!({"delivered": false, "error": "no session with this backend_session_id"});
            }

            // Fast path: plugin sent both pane + cwd. Skip the opencode-serve
            // round-trip AND the tmux pane scan and use the hints directly.
            if let (Some(pane), Some(cwd)) = (hints.pane.as_deref(), hints.cwd.as_deref()) {
                tracing::info!(
                    target: "ouija::api::backend_session_ready",
                    backend_session_id = %backend_sid,
                    pane,
                    cwd,
                    "ready trying explicit pane/cwd auto-provision"
                );
                if let Some(n) =
                    auto_provision_with_explicit_pane(state, &backend_sid, pane, cwd).await
                {
                    tracing::info!(
                        target: "ouija::api::backend_session_ready",
                        backend_session_id = %backend_sid,
                        session = %n,
                        "ready explicit auto-provision succeeded"
                    );
                    n
                } else {
                    tracing::warn!(
                        target: "ouija::api::backend_session_ready",
                        backend_session_id = %backend_sid,
                        pane,
                        cwd,
                        "ready explicit auto-provision failed"
                    );
                    return json!({"delivered": false, "error": "no session with this backend_session_id"});
                }
            } else {
                // Fallback: resolve dir from opencode serve, then scan tmux.
                let Some(dir) = lookup_opencode_session_dir(state, &backend_sid).await else {
                    tracing::warn!(
                        target: "ouija::api::backend_session_ready",
                        backend_session_id = %backend_sid,
                        "ready fallback could not resolve opencode session directory"
                    );
                    return json!({"delivered": false, "error": "no session with this backend_session_id"});
                };
                tracing::info!(
                    target: "ouija::api::backend_session_ready",
                    backend_session_id = %backend_sid,
                    dir,
                    "ready trying scan-based auto-provision"
                );

                let Some(n) = auto_provision_from_backend_session(state, &backend_sid, &dir).await
                else {
                    tracing::warn!(
                        target: "ouija::api::backend_session_ready",
                        backend_session_id = %backend_sid,
                        dir,
                        "ready scan-based auto-provision failed"
                    );
                    return json!({"delivered": false, "error": "no session with this backend_session_id"});
                };
                tracing::info!(
                    target: "ouija::api::backend_session_ready",
                    backend_session_id = %backend_sid,
                    session = %n,
                    "ready scan-based auto-provision succeeded"
                );
                n
            }
        }
    };

    let delivered = deliver_pending_prompt(state, &name).await;
    tracing::info!(
        target: "ouija::api::backend_session_ready",
        backend_session_id = %backend_sid,
        session = %name,
        delivered,
        "backend session ready complete"
    );
    json!({"delivered": delivered, "session": name})
}

fn validate_backend_session_id_boundary(backend_sid: &str) -> Option<String> {
    crate::daemon_protocol::validate_backend_session_id_boundary(backend_sid)
}

/// Auto-provision a fresh session record for an opencode backend session that
/// has no pre-existing ouija entry (issue #35).
///
/// Finds the tmux pane currently running opencode in `dir`. On exactly one
/// match, registers a new session with the backend_session_id bound atomically.
/// Fails closed (returns `None`) on zero or multiple matching panes — we
/// cannot map a backend_session_id to a pane in that case, same principle as
/// `disambiguate_adoption_candidates`.
///
/// The tmux pane scan is indirected through `AppState::list_assistant_panes`
/// so unit tests can seed `cached_assistant_panes` rather than shelling out
/// to a real tmux server.
async fn auto_provision_from_backend_session(
    state: &std::sync::Arc<crate::state::AppState>,
    backend_sid: &str,
    dir: &str,
) -> Option<String> {
    // Race guard 1: another concurrent ready callback may have already
    // bound this backend_session_id while we were doing the dir lookup.
    // Short-circuit here so we surface the concurrent winner's id (which
    // the caller returns to the plugin as `session`) instead of either
    // inventing a new id or failing closed because the pane is now filtered
    // out of the "unregistered panes" set. Must run BEFORE the pane filter:
    // the winner's session binds the pane, so the filter would drop it.
    {
        let proto = state.protocol.read().await;
        if let Some(existing) = find_local_session_by_backend_session_id(&proto, backend_sid) {
            return Some(existing.id.clone());
        }
    }

    // Snapshot the current pane layout and the registered pane → session map.
    let panes = state.list_assistant_panes().await;
    let registered_panes: std::collections::HashSet<String> = {
        let proto = state.protocol.read().await;
        proto
            .sessions
            .values()
            .filter(|s| matches!(s.origin, crate::daemon_protocol::Origin::Local))
            .filter_map(|s| s.pane.clone())
            .collect()
    };

    let opencode_process_names = state
        .backends
        .get("opencode")
        .map(|backend| {
            backend
                .process_names()
                .iter()
                .map(|name| (*name).to_string())
                .collect::<std::collections::HashSet<_>>()
        })
        .unwrap_or_default();

    // Filter to OpenCode panes in the target dir that are not already registered.
    let candidates: Vec<String> = panes
        .into_iter()
        .filter(|p| {
            !registered_panes.contains(&p.pane_id)
                && p.process_name
                    .as_deref()
                    .is_some_and(|name| opencode_process_names.contains(name))
                && p.pane_current_path
                    .as_deref()
                    .map(|path| crate::state::resolve_project_root(path) == dir)
                    .unwrap_or(false)
        })
        .map(|p| p.pane_id)
        .collect();

    let pane_id = match candidates.len() {
        1 => candidates.into_iter().next().unwrap(),
        0 => {
            tracing::warn!(
                "auto-provision declined: no tmux pane running opencode in dir {dir} for backend_session_id {backend_sid}"
            );
            return None;
        }
        n => {
            // Same fail-closed principle as disambiguate_adoption_candidates:
            // with multiple opencode panes in the same dir, the daemon has
            // no way to tell which one this backend_session_id belongs to.
            // Let the user disambiguate via `ouija register ...`.
            tracing::warn!(
                "auto-provision declined: {n} opencode panes in dir {dir}; cannot map backend_session_id {backend_sid} unambiguously"
            );
            return None;
        }
    };

    register_auto_provisioned_session(state, backend_sid, &pane_id, dir).await
}

/// Auto-provision using an explicit `(pane, dir)` pair supplied by the
/// opencode plugin in the readiness POST body. Skips the opencode-serve
/// dir lookup and the tmux pane scan lookup-by-dir, but still verifies the
/// pane against the same invariants the scan path enforces:
///
/// 1. The pane must appear in `list_assistant_panes`. This rejects stale
///    `TMUX_PANE` values (pane died between capture and POST), inherited
///    env vars from non-opencode callers, and any other caller who hands
///    us a pane id that isn't actually running an assistant process.
/// 2. The pane's current path must resolve to the hinted project root. This
///    rejects stale or inherited cwd values that point at a different pane's
///    project.
/// 3. The pane must not already be bound to another Local session. Without
///    this filter, `apply_register`'s pane-dedup silently evicts whoever
///    currently owns the pane — a concurrent claude-code SessionStart, a
///    prior auto-provision, or a manual `ouija register` would all be
///    vulnerable. Fail closed instead; the operator can disambiguate via
///    `ouija register` if they really want to reassign the pane.
///
/// Race guards and apply_register dedup still protect against concurrent
/// writers as before.
async fn auto_provision_with_explicit_pane(
    state: &std::sync::Arc<crate::state::AppState>,
    backend_sid: &str,
    pane: &str,
    cwd: &str,
) -> Option<String> {
    // Validate cwd BEFORE resolving / deriving anything from it. Every
    // other ouija code path treats project_dir as an absolute path with
    // at least one non-root segment, so reject degenerate input at the
    // boundary rather than letting it corrupt the session record.
    //
    // - Empty string: Path::file_name() returns None → basename falls
    //   through to "unnamed" in register_auto_provisioned_session; also
    //   project_dir would be persisted as "".
    // - Bare "/": same file_name() = None pathology; and no realistic
    //   caller has / as their project root.
    // - Relative (no leading "/"): would poison downstream comparisons
    //   (adoption, scan-by-dir, bulletin dedup) that string-compare
    //   project_dir against absolute paths.
    if cwd.is_empty() || cwd == "/" || !cwd.starts_with('/') {
        tracing::warn!(
            "auto-provision declined: invalid hint cwd {cwd:?} (must be absolute, non-empty, non-root) for backend_session_id {backend_sid}"
        );
        return None;
    }

    // Resolve worktree paths up to the repo root, matching the
    // scan-path's behaviour so the session id we derive is stable
    // across /repo and /repo/.claude/worktrees/<branch>.
    let dir = crate::state::resolve_project_root(cwd);

    // Race guard: the backend_session_id may already be bound. Surface
    // the concurrent winner rather than racing a second Register that
    // apply_register's pane-dedup would resolve by evicting them.
    {
        let proto = state.protocol.read().await;
        if let Some(existing) = find_local_session_by_backend_session_id(&proto, backend_sid) {
            return Some(existing.id.clone());
        }
    }

    // Defense 1: the supplied pane must be in list_assistant_panes. This is
    // the same liveness + is-an-assistant-pane check the scan path applies
    // implicitly when it iterates find_assistant_panes results.
    let panes = state.list_assistant_panes().await;
    let Some(hinted_pane) = panes.iter().find(|p| p.pane_id == pane) else {
        tracing::warn!(
            "auto-provision declined: hint pane {pane} is not among current assistant panes (backend_session_id {backend_sid})"
        );
        return None;
    };
    let opencode_process_names = state
        .backends
        .get("opencode")
        .map(|backend| {
            backend
                .process_names()
                .iter()
                .map(|name| (*name).to_string())
                .collect::<std::collections::HashSet<_>>()
        })
        .unwrap_or_default();
    let pane_matches_opencode = hinted_pane
        .process_name
        .as_deref()
        .is_some_and(|name| opencode_process_names.contains(name));
    if !pane_matches_opencode {
        tracing::warn!(
            "auto-provision declined: hint pane {pane} is not running opencode (backend_session_id {backend_sid})"
        );
        return None;
    }

    // Defense 2: the explicit cwd must match the pane's actual cwd after
    // applying the same project-root normalization as the scan path.
    let cwd_matches_pane = hinted_pane
        .pane_current_path
        .as_deref()
        .map(|path| crate::state::resolve_project_root(path) == dir)
        .unwrap_or(false);
    if !cwd_matches_pane {
        tracing::warn!(
            "auto-provision declined: hint pane {pane} is not in hinted cwd {dir} (backend_session_id {backend_sid})"
        );
        return None;
    }

    // Defense 3: the supplied pane must not already belong to another
    // Local session. Matches the `registered_panes` filter in the scan path
    // (auto_provision_from_backend_session). Without this, apply_register's
    // pane-dedup would silently evict the current owner.
    {
        let proto = state.protocol.read().await;
        let already_bound = proto.sessions.values().any(|s| {
            matches!(s.origin, crate::daemon_protocol::Origin::Local)
                && s.pane.as_deref() == Some(pane)
        });
        if already_bound {
            tracing::warn!(
                "auto-provision declined: hint pane {pane} is already bound to another local session (backend_session_id {backend_sid})"
            );
            return None;
        }
    }

    register_auto_provisioned_session(state, backend_sid, pane, dir).await
}

/// Inner helper: derive the session id, re-check the race, and apply the
/// Register. Shared by both the scan-path and the explicit-hint path so
/// the id-derivation + race-guard + metadata layout stay in one place.
async fn register_auto_provisioned_session(
    state: &std::sync::Arc<crate::state::AppState>,
    backend_sid: &str,
    pane_id: &str,
    dir: &str,
) -> Option<String> {
    // Derive a unique session id from the dir basename.
    let basename = std::path::Path::new(dir)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unnamed");
    let base_id = crate::state::sanitize_session_id(basename);
    if base_id.is_empty() {
        tracing::warn!(
            "auto-provision declined: could not derive a session id from dir {dir} (basename='{basename}')"
        );
        return None;
    }

    // Race guard: re-check the backend_session_id under the same lock we
    // use to build id_to_pane, so the winning concurrent writer is visible
    // to every subsequent branch. Without this, a writer that landed between
    // the top-of-function guard and this point would cause us to pick a
    // suffix-bumped id and run a redundant Register. apply_register's
    // pane-dedup would then evict the winner's session by pane (different
    // id, same pane) — stomping their atomic bind.
    let id = {
        let proto = state.protocol.read().await;
        if let Some(existing) = find_local_session_by_backend_session_id(&proto, backend_sid) {
            return Some(existing.id.clone());
        }
        let id_to_pane: std::collections::HashMap<String, Option<String>> = proto
            .sessions
            .iter()
            .map(|(id, s)| (id.clone(), s.pane.clone()))
            .collect();
        crate::state::resolve_unique_session_id(&id_to_pane, &base_id, Some(pane_id))
    };

    tracing::info!(
        "auto-provisioned session '{id}' for pane {pane_id} / backend_session_id {backend_sid} (dir: {dir})"
    );

    let metadata = crate::daemon_protocol::SessionMeta {
        project_dir: Some(dir.to_string()),
        role: Some(format!("working on {basename}")),
        backend: Some("opencode".into()),
        backend_session_id: Some(backend_sid.to_string()),
        ..Default::default()
    };
    let effects = state
        .apply_and_execute(crate::daemon_protocol::Event::RegisterIfPaneUnbound {
            id: id.clone(),
            pane: pane_id.to_string(),
            expected_backend_session_id: Some(backend_sid.to_string()),
            metadata,
        })
        .await;

    let proto = state.protocol.read().await;
    auto_provision_register_result(&proto, backend_sid, &id, &effects)
}

fn auto_provision_register_result(
    proto: &crate::daemon_protocol::DaemonState,
    backend_sid: &str,
    id: &str,
    effects: &[crate::daemon_protocol::Effect],
) -> Option<String> {
    if effects.iter().any(|effect| {
        matches!(
            effect,
            crate::daemon_protocol::Effect::RegisterOk { session_id, .. } if session_id == id
        )
    }) {
        Some(id.to_string())
    } else if effects.iter().any(|effect| {
        matches!(
            effect,
            crate::daemon_protocol::Effect::RegisterFailed { session_id, .. } if session_id == id
        )
    }) {
        find_local_session_by_backend_session_id(proto, backend_sid)
            .map(|session| session.id.clone())
    } else {
        None
    }
}

fn find_local_session_by_backend_session_id<'a>(
    proto: &'a crate::daemon_protocol::DaemonState,
    backend_sid: &str,
) -> Option<&'a crate::daemon_protocol::SessionEntry> {
    proto.sessions.values().find(|s| {
        matches!(s.origin, crate::daemon_protocol::Origin::Local)
            && s.metadata.backend_session_id.as_deref() == Some(backend_sid)
    })
}

/// Choose at most one candidate session ID for adoption (issue #15).
/// Returns None for zero or >1 candidates — ambiguity is fail-closed.
fn disambiguate_adoption_candidates(
    backend_sid: &str,
    dir: &str,
    candidates: Vec<String>,
) -> Option<String> {
    match candidates.len() {
        0 => None,
        1 => candidates.into_iter().next(),
        n => {
            tracing::warn!(
                "refusing to adopt backend_session_id {backend_sid}: {n} ambiguous candidates in dir {dir}: {candidates:?}"
            );
            None
        }
    }
}

/// Query the opencode serve for the project directory associated with a
/// `backend_session_id`. Returns `None` if the serve is unreachable, the
/// session is unknown to the serve, or the response lacks a `directory` field.
///
/// Extracted so the auto-provision path (issue #35) can reuse the resolved
/// directory without a second HTTP round-trip after adoption misses.
async fn lookup_opencode_session_dir(
    state: &std::sync::Arc<crate::state::AppState>,
    backend_sid: &str,
) -> Option<String> {
    let port = state.opencode_serve_port();
    let url = format!("http://127.0.0.1:{port}/session/{backend_sid}");
    let resp = state
        .http_client
        .get(&url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(
                target: "ouija::api::backend_session_ready",
                backend_session_id = %backend_sid,
                port,
                error = %e,
                "opencode session dir lookup request failed"
            );
            e
        })
        .ok()?;
    if !resp.status().is_success() {
        tracing::warn!(
            target: "ouija::api::backend_session_ready",
            backend_session_id = %backend_sid,
            port,
            status = %resp.status(),
            "opencode session dir lookup returned non-success"
        );
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    let dir = body["directory"].as_str().map(str::to_string);
    tracing::info!(
        target: "ouija::api::backend_session_ready",
        backend_session_id = %backend_sid,
        port,
        directory = ?dir,
        "opencode session dir lookup complete"
    );
    dir
}

/// Parse an opencode model string of the form `"providerID/modelID"` into its
/// two segments. Splits on the first `/` only (matching opencode's parser at
/// `packages/opencode/src/provider/provider.ts`), trims each segment, and
/// rejects empty segments on either side so callers don't send
/// `providerID: " "` or `modelID: ""` to opencode's summarize endpoint.
///
/// Kept separate from [`crate::nostr_transport::opencode_prompt_body`] (which
/// has the same parse embedded) so /summarize can reuse the tuple shape
/// directly. The two parsers must stay consistent: a model string that
/// `opencode_prompt_body` accepts for `prompt_async` must also parse here so
/// the same session isn't rejected for compaction.
fn parse_opencode_model(model: &str) -> Option<(String, String)> {
    let trimmed = model.trim();
    let (provider, model_id) = trimmed.split_once('/')?;
    let provider = provider.trim();
    let model_id = model_id.trim();
    if provider.is_empty() || model_id.is_empty() {
        return None;
    }
    Some((provider.to_string(), model_id.to_string()))
}

fn parse_opencode_config_model(config: &serde_json::Value) -> Option<(String, String)> {
    config.get("model")?.as_str().and_then(parse_opencode_model)
}

/// Resolve `(providerID, modelID)` for the opencode `/session/:id/summarize`
/// endpoint. First tries to parse the session's configured model (set via
/// `ouija spawn-session --model`); if absent or unparseable, falls back to
/// GET `/config` and parses the top-level OpenCode `model` key.
///
/// Returns `None` when neither source yields a pair — e.g. the session has no
/// `model` and the opencode serve is unreachable or has no configured default
/// model. Callers should surface this as a 400, since `/summarize` cannot be
/// called without a concrete provider+model pair.
async fn resolve_opencode_compact_model(
    state: &std::sync::Arc<crate::state::AppState>,
    session_model: Option<&str>,
    project_dir: Option<&str>,
) -> Option<(String, String)> {
    if let Some(m) = session_model
        && let Some(pair) = parse_opencode_model(m)
    {
        return Some(pair);
    }

    let port = state.opencode_serve_port();
    let url = format!("http://127.0.0.1:{port}/config");
    let mut req = state
        .http_client
        .get(&url)
        .timeout(std::time::Duration::from_secs(5));
    if let Some(dir) = project_dir {
        req = req.header("x-opencode-directory", dir);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    parse_opencode_config_model(&body)
}

/// Query the opencode serve for a session's directory, find the matching ouija
/// session, and set its `backend_session_id` + `backend`.
async fn adopt_backend_session_id(
    state: &std::sync::Arc<crate::state::AppState>,
    backend_sid: &str,
) -> Option<String> {
    let dir = lookup_opencode_session_dir(state, backend_sid).await?;
    tracing::info!(
        target: "ouija::api::backend_session_ready",
        backend_session_id = %backend_sid,
        dir,
        "adoption resolved opencode session directory"
    );

    // Collect ALL local ouija sessions matching this directory that lack a
    // backend_session_id (issue #15). Silently picking the first match — as
    // the original implementation did — lets an adopt for session A clobber
    // the metadata of unrelated session B when both live in the same dir
    // (hashbrown iteration order is effectively random). Fail closed on
    // ambiguity: adopt only when exactly one candidate exists.
    let candidates: Vec<String> = {
        let proto = state.protocol.read().await;
        proto
            .sessions
            .values()
            .filter(|s| {
                matches!(s.origin, crate::daemon_protocol::Origin::Local)
                    && s.metadata.project_dir.as_deref() == Some(dir.as_str())
                    && s.metadata.backend_session_id.is_none()
            })
            .map(|s| s.id.clone())
            .collect()
    };

    let session_id = disambiguate_adoption_candidates(backend_sid, &dir, candidates)?;
    tracing::info!(
        target: "ouija::api::backend_session_ready",
        backend_session_id = %backend_sid,
        session = %session_id,
        "adoption selected existing ouija session"
    );

    tracing::info!(
        "adopting backend_session_id {backend_sid} for session {session_id} (dir: {dir})"
    );

    // Update the session metadata with backend info
    state
        .apply_and_execute(crate::daemon_protocol::Event::AdoptBackend {
            id: session_id.clone(),
            backend: "opencode".into(),
            backend_session_id: backend_sid.to_string(),
            expected_backend_session_id: None,
        })
        .await;

    Some(session_id)
}

/// List indexed projects from the configured projects directory.
pub async fn list_projects(
    State(state): State<SharedState>,
) -> axum::Json<Vec<crate::project_index::ProjectInfo>> {
    let index = state.project_index.read().await;
    let mut projects: Vec<_> = index.values().cloned().collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name));
    axum::Json(projects)
}

// ── Clear reminder (REST equivalent of removed MCP tool) ─────────────

#[derive(Deserialize)]
pub struct ClearReminderBody {
    pub from: String,
    pub clearing_id: u64,
}

pub async fn clear_reminder(
    State(state): State<SharedState>,
    Json(body): Json<ClearReminderBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    state
        .notify_agent(
            &body.from,
            crate::session_agent::SessionMsg::ClearReminder {
                clearing_id: body.clearing_id,
            },
        )
        .await;
    (
        StatusCode::OK,
        Json(json!({
            "cleared": body.clearing_id,
            "session": body.from,
            "hint": "Reminder paused. It will resume after new activity (incoming message, hook fire, etc.)."
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_prompt(pane_id: &str, prompt: &str) -> crate::state::PendingPrompt {
        crate::state::PendingPrompt::new(pane_id.into(), prompt.into(), None)
    }

    fn pending_opencode_prompt(
        pane_id: &str,
        prompt: &str,
        backend_session_id: &str,
    ) -> crate::state::PendingPrompt {
        crate::state::PendingPrompt::new(
            pane_id.into(),
            prompt.into(),
            Some(backend_session_id.into()),
        )
    }

    #[test]
    fn normalize_optional_string_passthrough() {
        assert_eq!(
            normalize_optional_string(Some("sonnet".into())),
            Some("sonnet".into())
        );
        assert_eq!(normalize_optional_string(None), None);
    }

    #[test]
    fn normalize_optional_string_trims_and_drops_empty() {
        assert_eq!(normalize_optional_string(Some("".into())), None);
        assert_eq!(normalize_optional_string(Some("   ".into())), None);
        assert_eq!(normalize_optional_string(Some("\t\n ".into())), None);
        assert_eq!(
            normalize_optional_string(Some("  opus  ".into())),
            Some("opus".into())
        );
    }

    #[test]
    fn disambiguate_single_candidate_adopts() {
        let got = disambiguate_adoption_candidates("ses_x", "/repo", vec!["only".into()]);
        assert_eq!(got.as_deref(), Some("only"));
    }

    #[test]
    fn disambiguate_zero_candidates_fails() {
        let got = disambiguate_adoption_candidates("ses_x", "/repo", vec![]);
        assert!(got.is_none());
    }

    #[test]
    fn disambiguate_multiple_candidates_fails_closed() {
        // Two or more sessions in the same project_dir with no backend_session_id
        // must NOT be silently resolved — adopt_backend_session_id has no way
        // to know which one the backend SID actually belongs to (issue #15).
        let got = disambiguate_adoption_candidates(
            "ses_x",
            "/repo",
            vec!["hub".into(), "hub-skill-probe".into()],
        );
        assert!(got.is_none());
    }

    #[test]
    fn needle_for_127_0_0_1_encodes_little_endian_hex() {
        let peer: SocketAddr = "127.0.0.1:45084".parse().unwrap();
        // 127.0.0.1 little-endian = 01 00 00 7F → "0100007F"; port 45084 = 0xB01C
        assert_eq!(
            needle_for_loopback_peer(peer).as_deref(),
            Some("0100007F:B01C")
        );
    }

    #[test]
    fn parse_tcp_inode_finds_matching_local() {
        let table = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:B01C 0100007F:1EC8 01 00000000:00000000 00:00000000 00000000  1000        0 1234567 1 0000000000000000 20 4 20 10 -1\n\
   1: 0100007F:1EC8 0100007F:B01C 01 00000000:00000000 00:00000000 00000000  1000        0 7654321 1 0000000000000000 20 4 20 10 -1\n";
        assert_eq!(
            parse_tcp_inode_for_local(table, "0100007F:B01C"),
            Some(1234567)
        );
        assert_eq!(
            parse_tcp_inode_for_local(table, "0100007F:1EC8"),
            Some(7654321)
        );
        assert_eq!(parse_tcp_inode_for_local(table, "0100007F:FFFF"), None);
    }

    #[test]
    fn parse_tcp_inode_skips_header_and_short_lines() {
        let table = "sl  local_address rem_address   st\nshort line\n";
        assert!(parse_tcp_inode_for_local(table, "0100007F:0001").is_none());
    }

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

    // --- compact endpoint ---

    #[tokio::test]
    async fn inject_for_opencode_pane_reports_tmux_delivery() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-live".into(),
                pane: Some("%oc".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_oc".into()),
                    ..Default::default()
                },
            })
            .await;

        let (status, body) = inject(
            State(state),
            Json(InjectBody {
                pane: "%oc".into(),
                message: "hello raw tmux".into(),
                vim_mode: false,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "injected");
        assert_eq!(body["delivery"], "tmux");
    }

    #[tokio::test]
    async fn inject_unregistered_pane_uses_raw_tmux_delivery() {
        let state = crate::state::AppState::new_for_test();

        let (status, body) = inject(
            State(state),
            Json(InjectBody {
                pane: "%unregistered".into(),
                message: "hello raw pane".into(),
                vim_mode: false,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "injected");
        assert_eq!(body["delivery"], "tmux");
    }

    #[tokio::test]
    async fn send_to_adopted_opencode_live_pane_reports_tmux_delivery() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sender".into(),
                pane: Some("%sender".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-live".into(),
                pane: Some("%oc".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_oc".into()),
                    ..Default::default()
                },
            })
            .await;

        let (status, body) = send_msg(
            State(state),
            Json(SendBody {
                from: "sender".into(),
                to: "oc-live".into(),
                message: "hello live pane".into(),
                expects_reply: false,
                responds_to: None,
                done: false,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "delivered");
        assert_eq!(body["method"], "tmux");
    }

    async fn state_with_opencode_prompt_server() -> (SharedState, tokio::task::JoinHandle<()>) {
        use axum::Router;
        use axum::routing::post;
        use tokio::net::TcpListener;

        async fn prompt_async() -> Json<serde_json::Value> {
            Json(serde_json::json!({ "ok": true }))
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let serve_port = listener.local_addr().unwrap().port();
        assert!(serve_port >= 320, "test listener port must be >= 320");
        let app = Router::new().route("/session/{session_id}/prompt_async", post(prompt_async));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: serve_port - 320,
            data_dir: std::path::PathBuf::from("/tmp/ouija-test-agent"),
            config_dir: std::path::PathBuf::from("/tmp/ouija-test-agent"),
        });

        (state, server)
    }

    #[tokio::test]
    async fn send_to_strong_opencode_binding_reports_http_acceptance() {
        let (state, server) = state_with_opencode_prompt_server().await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sender".into(),
                pane: Some("%sender".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-managed".into(),
                pane: Some("%oc".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_oc".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;

        let (status, body) = send_msg(
            State(state),
            Json(SendBody {
                from: "sender".into(),
                to: "oc-managed".into(),
                message: "hello http".into(),
                expects_reply: false,
                responds_to: None,
                done: false,
            }),
        )
        .await;

        server.abort();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "accepted");
        assert_eq!(body["method"], "http");
    }

    #[tokio::test]
    async fn send_to_session_with_soft_restart_in_progress_returns_conflict() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sender".into(),
                pane: Some("%sender".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-managed".into(),
                pane: Some("%oc".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_old".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;
        let _guard = state
            .try_acquire_soft_restart_in_progress("oc-managed")
            .expect("soft restart guard should be acquired");

        let (status, body) = send_msg(
            State(state.clone()),
            Json(SendBody {
                from: "sender".into(),
                to: "oc-managed".into(),
                message: "hello during restart".into(),
                expects_reply: false,
                responds_to: None,
                done: false,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            body["error"]
                .as_str()
                .unwrap_or("")
                .contains("soft restart is in progress"),
            "expected in-flight restart error, got {body:?}"
        );
    }

    #[tokio::test]
    async fn send_to_strong_opencode_binding_reports_http_failure() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sender".into(),
                pane: Some("%sender".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-managed".into(),
                pane: Some("%oc".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_oc".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;

        let (status, body) = send_msg(
            State(state),
            Json(SendBody {
                from: "sender".into(),
                to: "oc-managed".into(),
                message: "hello unreachable http".into(),
                expects_reply: false,
                responds_to: None,
                done: false,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["method"], "http");
        assert!(body["error"].as_str().unwrap().contains("prompt_async"));
    }

    #[tokio::test]
    async fn send_effect_execution_uses_recorded_http_method_after_binding_changes() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sender".into(),
                pane: Some("%sender".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-managed".into(),
                pane: Some("%oc".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_oc".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;

        let effects = {
            let mut proto = state.protocol.write().await;
            proto.apply(crate::daemon_protocol::Event::Send {
                from: "sender".into(),
                to: "oc-managed".into(),
                message: "hello http".into(),
                expects_reply: false,
                responds_to: None,
                done: false,
            })
        };

        {
            let mut proto = state.protocol.write().await;
            let session = proto.sessions.get_mut("oc-managed").unwrap();
            session.metadata.opencode_binding =
                Some(crate::daemon_protocol::OpenCodeBinding::WeakAdopted);
        }

        let result = execute_send_effects_for_api(&state, &effects).await;

        assert!(matches!(
            result,
            Ok(crate::state::DeliveryOutcome::Rejected(reason)) if reason.contains("prompt_async")
        ));
    }

    #[tokio::test]
    async fn send_effect_execution_uses_recorded_http_session_after_metadata_changes() {
        use axum::Router;
        use axum::extract::Path;
        use axum::routing::post;
        use tokio::net::TcpListener;

        type Captures = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

        async fn prompt_async(
            Path(session_id): Path<String>,
            State(captures): State<Captures>,
        ) -> Json<serde_json::Value> {
            captures.lock().unwrap().push(session_id);
            Json(serde_json::json!({ "ok": true }))
        }

        let captures = Captures::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let serve_port = listener.local_addr().unwrap().port();
        assert!(serve_port >= 320, "test listener port must be >= 320");
        let app = Router::new()
            .route("/session/{session_id}/prompt_async", post(prompt_async))
            .with_state(captures.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: serve_port - 320,
            data_dir: std::path::PathBuf::from("/tmp/ouija-test-agent"),
            config_dir: std::path::PathBuf::from("/tmp/ouija-test-agent"),
        });
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sender".into(),
                pane: Some("%sender".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-managed".into(),
                pane: Some("%oc".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_old".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;

        let effects = {
            let mut proto = state.protocol.write().await;
            proto.apply(crate::daemon_protocol::Event::Send {
                from: "sender".into(),
                to: "oc-managed".into(),
                message: "hello old http session".into(),
                expects_reply: false,
                responds_to: None,
                done: false,
            })
        };

        {
            let mut proto = state.protocol.write().await;
            let session = proto.sessions.get_mut("oc-managed").unwrap();
            session.metadata.backend_session_id = Some("ses_new".into());
        }

        execute_send_effects_for_api(&state, &effects)
            .await
            .unwrap();

        server.abort();
        assert_eq!(captures.lock().unwrap().as_slice(), ["ses_old"]);
    }

    #[tokio::test]
    async fn send_http_failure_does_not_leave_pending_reply() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sender".into(),
                pane: Some("%sender".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-managed".into(),
                pane: Some("%oc".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_oc".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;

        let (status, _) = send_msg(
            State(state.clone()),
            Json(SendBody {
                from: "sender".into(),
                to: "oc-managed".into(),
                message: "hello unreachable http".into(),
                expects_reply: true,
                responds_to: None,
                done: false,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let proto = state.protocol.read().await;
        assert!(!proto.pending_replies.contains_key("oc-managed"));
    }

    #[tokio::test]
    async fn send_ambiguous_http_failure_keeps_pending_reply_and_reports_unknown() {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::post;
        use tokio::net::TcpListener;

        async fn prompt_async() -> StatusCode {
            StatusCode::BAD_GATEWAY
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route("/session/{session_id}/prompt_async", post(prompt_async));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: port.checked_sub(320).unwrap(),
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sender".into(),
                pane: Some("%sender".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-managed".into(),
                pane: Some("%oc".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_oc".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;

        let (status, body) = send_msg(
            State(state.clone()),
            Json(SendBody {
                from: "sender".into(),
                to: "oc-managed".into(),
                message: "hello maybe accepted".into(),
                expects_reply: true,
                responds_to: None,
                done: false,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "unknown");
        assert_eq!(body["method"], "http");
        let proto = state.protocol.read().await;
        assert!(proto.pending_replies.contains_key("oc-managed"));
        server.abort();
    }

    #[tokio::test]
    async fn failed_done_reply_preserves_sender_retry_state() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sender".into(),
                pane: Some("%sender".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    reminder: Some("keep working".into()),
                    ..Default::default()
                },
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-managed".into(),
                pane: Some("%oc".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_oc".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;
        {
            let mut proto = state.protocol.write().await;
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

        let (status, _) = send_msg(
            State(state.clone()),
            Json(SendBody {
                from: "sender".into(),
                to: "oc-managed".into(),
                message: "done, but unreachable".into(),
                expects_reply: false,
                responds_to: Some(7),
                done: true,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
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
    async fn failed_send_rollback_does_not_restore_concurrently_cleared_pending_reply() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sender".into(),
                pane: Some("%sender".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "target".into(),
                pane: Some("%target".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;

        let (effects, rollback) = {
            let mut proto = state.protocol.write().await;
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
            let mut rollback = FailedSendRollback::capture(&proto, "sender", Some(7), false);
            let effects = proto.apply(crate::daemon_protocol::Event::Send {
                from: "sender".into(),
                to: "target".into(),
                message: "progress that will fail".into(),
                expects_reply: false,
                responds_to: Some(7),
                done: false,
            });
            rollback.capture_after_send(&proto);
            (effects, rollback)
        };

        {
            let mut proto = state.protocol.write().await;
            proto.pending_replies.remove("sender");
        }

        rollback_failed_delivery(&state, &effects, rollback).await;

        let proto = state.protocol.read().await;
        assert!(!proto.pending_replies.contains_key("sender"));
    }

    #[tokio::test]
    async fn failed_done_reply_does_not_restore_concurrently_cleared_retry_state() {
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use tokio::net::TcpListener;
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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session/{session_id}/prompt_async", post(prompt_async))
            .with_state(gate.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: port.checked_sub(320).unwrap(),
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sender".into(),
                pane: Some("%sender".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    reminder: Some("keep working".into()),
                    ..Default::default()
                },
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-managed".into(),
                pane: Some("%oc".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_oc".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;
        {
            let mut proto = state.protocol.write().await;
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
                send_msg(
                    State(state),
                    Json(SendBody {
                        from: "sender".into(),
                        to: "oc-managed".into(),
                        message: "done, but unreachable".into(),
                        expects_reply: false,
                        responds_to: Some(7),
                        done: true,
                    }),
                )
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
        let (status, _) = delivery.await.unwrap();

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let proto = state.protocol.read().await;
        assert!(!proto.pending_replies.contains_key("sender"));
        assert_eq!(proto.sessions["sender"].metadata.reminder, None);
        server.abort();
    }

    #[tokio::test]
    async fn successful_done_reply_clears_mutated_reserved_retry_state() {
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use tokio::net::TcpListener;
        use tokio::sync::Notify;

        #[derive(Clone)]
        struct Gate {
            started: StdArc<Notify>,
            release: StdArc<Notify>,
        }

        async fn prompt_async(AxumState(gate): AxumState<Gate>) -> StatusCode {
            gate.started.notify_one();
            gate.release.notified().await;
            StatusCode::NO_CONTENT
        }

        let gate = Gate {
            started: StdArc::new(Notify::new()),
            release: StdArc::new(Notify::new()),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session/{session_id}/prompt_async", post(prompt_async))
            .with_state(gate.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: port.checked_sub(320).unwrap(),
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sender".into(),
                pane: Some("%sender".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    reminder: Some("keep working".into()),
                    ..Default::default()
                },
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-managed".into(),
                pane: Some("%oc".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_oc".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;
        {
            let mut proto = state.protocol.write().await;
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
                send_msg(
                    State(state),
                    Json(SendBody {
                        from: "sender".into(),
                        to: "oc-managed".into(),
                        message: "done successfully".into(),
                        expects_reply: false,
                        responds_to: Some(7),
                        done: true,
                    }),
                )
                .await
            }
        });
        gate.started.notified().await;
        {
            let mut proto = state.protocol.write().await;
            let pending = proto.pending_replies.get_mut("sender").unwrap();
            pending[0].in_progress = true;
            pending[0].last_activity = 200;
            proto.sessions.get_mut("sender").unwrap().metadata.reminder =
                Some("keep working (activity tick)".into());
        }

        gate.release.notify_one();
        let (status, _) = delivery.await.unwrap();

        assert_eq!(status, StatusCode::OK);
        let proto = state.protocol.read().await;
        assert!(
            !proto.pending_replies.contains_key("sender"),
            "successful done delivery must clear the reply slot by msg_id even if the reserved entry mutated"
        );
        assert_eq!(proto.sessions["sender"].metadata.reminder, None);
        server.abort();
    }

    #[tokio::test]
    async fn send_failed_restores_done_reply_retry_state() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sender".into(),
                pane: Some("%sender".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    reminder: Some("keep working".into()),
                    ..Default::default()
                },
            })
            .await;
        {
            let mut proto = state.protocol.write().await;
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

        let (status, body) = send_msg(
            State(state.clone()),
            Json(SendBody {
                from: "sender".into(),
                to: "missing-target".into(),
                message: "done, but target is gone".into(),
                expects_reply: false,
                responds_to: Some(7),
                done: true,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("not found"));
        let proto = state.protocol.read().await;
        let entries = proto
            .pending_replies
            .get("sender")
            .expect("failed send must restore sender pending reply");
        assert!(entries.iter().any(|entry| entry.msg_id == 7));
        assert_eq!(
            proto.sessions["sender"].metadata.reminder.as_deref(),
            Some("keep working")
        );
    }

    #[tokio::test]
    async fn send_failed_restores_progress_reply_retry_state() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sender".into(),
                pane: Some("%sender".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        {
            let mut proto = state.protocol.write().await;
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

        let (status, body) = send_msg(
            State(state.clone()),
            Json(SendBody {
                from: "sender".into(),
                to: "missing-target".into(),
                message: "still working, but target is gone".into(),
                expects_reply: false,
                responds_to: Some(7),
                done: false,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("not found"));
        let proto = state.protocol.read().await;
        let entry = proto
            .pending_replies
            .get("sender")
            .and_then(|entries| entries.iter().find(|entry| entry.msg_id == 7))
            .expect("failed send must preserve sender pending reply");
        assert!(!entry.in_progress);
        assert_eq!(entry.last_activity, 100);
    }

    #[tokio::test]
    async fn send_to_headless_opencode_session_reports_http_acceptance() {
        let (state, server) = state_with_opencode_prompt_server().await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sender".into(),
                pane: Some("%sender".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-headless".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_oc".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;

        let (status, body) = send_msg(
            State(state),
            Json(SendBody {
                from: "sender".into(),
                to: "oc-headless".into(),
                message: "hello headless".into(),
                expects_reply: false,
                responds_to: None,
                done: false,
            }),
        )
        .await;

        server.abort();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "accepted");
        assert_eq!(body["method"], "http");
    }

    #[tokio::test]
    async fn compact_session_not_found_returns_404() {
        let state = crate::state::AppState::new_for_test();
        let (status, body) = compact_inner(
            &state,
            "ghost".into(),
            CompactBody {
                continuation: Some("go".into()),
            },
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn compact_cc_without_pane_returns_400() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "cc-no-pane".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    ..Default::default()
                },
            })
            .await;

        let (status, body) = compact_inner(
            &state,
            "cc-no-pane".into(),
            CompactBody {
                continuation: Some("go".into()),
            },
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("pane"));
    }

    #[tokio::test]
    async fn compact_oc_without_backend_session_id_returns_400() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-no-sid".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: None,
                    ..Default::default()
                },
            })
            .await;

        let (status, body) = compact_inner(
            &state,
            "oc-no-sid".into(),
            CompactBody {
                continuation: Some("go".into()),
            },
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("backend_session_id"),
            "expected error to mention backend_session_id, got: {}",
            body["error"]
        );
    }

    #[tokio::test]
    async fn compact_oc_weak_binding_returns_400_before_summarize() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-weak".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_probe".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::WeakAdopted),
                    model: Some("anthropic/claude-sonnet-4-6".into()),
                    ..Default::default()
                },
            })
            .await;

        let (status, body) = compact_inner(
            &state,
            "oc-weak".into(),
            CompactBody {
                continuation: Some("keep going".into()),
            },
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        let err = body["error"].as_str().unwrap_or_default();
        assert!(
            err.contains("strong") || err.contains("binding"),
            "expected error to mention weak binding, got: {err}"
        );
    }

    #[tokio::test]
    async fn compact_oc_summarize_failure_returns_502() {
        // In the test env, opencode_serve_port() == 320 (privileged, unbound)
        // so the POST connection is refused. The HTTP branch now calls
        // /summarize before delivery, so the failure surfaces at that step.
        // A 502 (rather than a silent 200) is required so the caller can
        // distinguish "compact didn't happen" from "compact done".
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-fail".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_probe".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    // A parseable model short-circuits /config, so
                    // the 502 is specifically the /summarize failure rather
                    // than model-resolution failure (covered in a separate
                    // test below).
                    model: Some("anthropic/claude-sonnet-4-6".into()),
                    ..Default::default()
                },
            })
            .await;

        let (status, body) = compact_inner(
            &state,
            "oc-fail".into(),
            CompactBody {
                continuation: Some("keep going".into()),
            },
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let err = body["error"].as_str().unwrap_or_default();
        assert!(
            err.contains("summarize"),
            "expected error to mention /summarize, got: {err}"
        );
    }

    #[tokio::test]
    async fn compact_oc_bare_compact_is_no_longer_rejected_at_api_boundary() {
        // Before this change, a bare /compact (no continuation) on an HTTP
        // backend was rejected with 400 at the API boundary because phase-1
        // had no context-shrink path. With real summarize wired up, a bare
        // /compact is a legitimate request (just shrink context, deliver
        // nothing). The test env has no opencode serve, so the call reaches
        // /summarize and fails there with 502 — which is the proof that the
        // request was accepted for processing rather than rejected upfront.
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-no-cont".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_probe".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    model: Some("anthropic/claude-sonnet-4-6".into()),
                    ..Default::default()
                },
            })
            .await;

        let (status, body) = compact_inner(
            &state,
            "oc-no-cont".into(),
            CompactBody { continuation: None },
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_GATEWAY,
            "bare /compact must now progress to the summarize call (which 502s in the test env), \
             not get rejected at the API boundary"
        );
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("summarize"),
            "expected error to mention /summarize, got: {}",
            body["error"]
        );
    }

    #[tokio::test]
    async fn compact_oc_empty_continuation_normalizes_to_bare_compact() {
        // Whitespace-only continuation is normalized to None upstream, so
        // this is equivalent to the bare-compact case — progresses to
        // summarize and 502s in the test env. The assertion catches a
        // regression where whitespace would be treated as a meaningful
        // continuation and sent through prompt_async.
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-empty-cont".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_probe".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    model: Some("anthropic/claude-sonnet-4-6".into()),
                    ..Default::default()
                },
            })
            .await;

        let (status, body) = compact_inner(
            &state,
            "oc-empty-cont".into(),
            CompactBody {
                continuation: Some("   ".into()),
            },
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("summarize"),
            "expected error to mention /summarize, got: {}",
            body["error"]
        );
    }

    #[tokio::test]
    async fn compact_oc_reentrant_request_returns_409_while_summarize_in_flight() {
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::net::TcpListener;
        use tokio::sync::Notify;

        struct SummarizeState {
            calls: AtomicUsize,
            first_started: Notify,
            release_first: Notify,
        }

        async fn summarize(AxumState(state): AxumState<StdArc<SummarizeState>>) -> StatusCode {
            let call = state.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call == 1 {
                state.first_started.notify_waiters();
                state.release_first.notified().await;
            }
            StatusCode::OK
        }

        let summarize_state = StdArc::new(SummarizeState {
            calls: AtomicUsize::new(0),
            first_started: Notify::new(),
            release_first: Notify::new(),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session/{session_id}/summarize", post(summarize))
            .with_state(summarize_state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: port.checked_sub(320).unwrap(),
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        };
        let state = crate::state::AppState::new(config);
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-busy".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_probe".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    model: Some("anthropic/claude-sonnet-4-6".into()),
                    ..Default::default()
                },
            })
            .await;

        let first_state = state.clone();
        let first = tokio::spawn(async move {
            compact_inner(
                &first_state,
                "oc-busy".into(),
                CompactBody { continuation: None },
            )
            .await
        });
        summarize_state.first_started.notified().await;

        let (status, body) =
            compact_inner(&state, "oc-busy".into(), CompactBody { continuation: None }).await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("compact"),
            "expected compact conflict error, got: {}",
            body["error"]
        );
        assert_eq!(summarize_state.calls.load(Ordering::SeqCst), 1);

        summarize_state.release_first.notify_waiters();
        let (first_status, _) = first.await.unwrap();
        assert_eq!(first_status, StatusCode::OK);
        server.abort();
    }

    #[tokio::test]
    async fn compact_oc_reentrant_request_after_rename_returns_409() {
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::net::TcpListener;
        use tokio::sync::Notify;

        struct SummarizeState {
            calls: AtomicUsize,
            first_started: Notify,
            release_first: Notify,
        }

        async fn summarize(AxumState(state): AxumState<StdArc<SummarizeState>>) -> StatusCode {
            let call = state.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call == 1 {
                state.first_started.notify_waiters();
                state.release_first.notified().await;
            }
            StatusCode::OK
        }

        let summarize_state = StdArc::new(SummarizeState {
            calls: AtomicUsize::new(0),
            first_started: Notify::new(),
            release_first: Notify::new(),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session/{session_id}/summarize", post(summarize))
            .with_state(summarize_state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: port.checked_sub(320).unwrap(),
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        };
        let state = crate::state::AppState::new(config);
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-busy".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_probe".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    model: Some("anthropic/claude-sonnet-4-6".into()),
                    ..Default::default()
                },
            })
            .await;

        let first_state = state.clone();
        let first = tokio::spawn(async move {
            compact_inner(
                &first_state,
                "oc-busy".into(),
                CompactBody { continuation: None },
            )
            .await
        });
        summarize_state.first_started.notified().await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Rename {
                old_id: "oc-busy".into(),
                new_id: "oc-renamed".into(),
            })
            .await;

        let (status, _) = compact_inner(
            &state,
            "oc-renamed".into(),
            CompactBody { continuation: None },
        )
        .await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(summarize_state.calls.load(Ordering::SeqCst), 1);

        summarize_state.release_first.notify_waiters();
        let (first_status, _) = first.await.unwrap();
        assert_eq!(first_status, StatusCode::OK);
        server.abort();
    }

    #[tokio::test]
    async fn compact_oc_without_model_and_serve_unreachable_returns_400() {
        // If the session has no `model` AND opencode serve is unreachable
        // (so /config also can't supply a default), the endpoint
        // cannot build a valid {providerID, modelID} body for /summarize.
        // It must reject with 400 up-front rather than reach a 502 — a 400
        // is the actionable signal for the operator ("configure a model").
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc-no-model".into(),
                pane: None,
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_probe".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    model: None,
                    ..Default::default()
                },
            })
            .await;

        let (status, body) = compact_inner(
            &state,
            "oc-no-model".into(),
            CompactBody {
                continuation: Some("keep going".into()),
            },
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let err = body["error"].as_str().unwrap_or_default();
        assert!(
            err.contains("provider") || err.contains("model"),
            "expected error to mention provider/model resolution, got: {err}"
        );
    }

    #[test]
    fn compact_body_rejects_unknown_fields() {
        // Guard against silently-swallowed typos like {"continuatino": "..."}.
        let bad = serde_json::json!({"continuatino": "oops"});
        let err = serde_json::from_value::<CompactBody>(bad).unwrap_err();
        assert!(
            err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn compact_success_body_matches_docstring_envelope() {
        // compact()'s docstring advertises {status, compacted, continuation_delivered}
        // as the success envelope across BOTH backends. A typed deserializer that
        // requires all three fields must work against TUI responses too, so the
        // TUI and HTTP full-success sites must route through the same builder.
        let body = compact_success_body(false, None);
        let obj = body.as_object().expect("success body is a JSON object");
        assert_eq!(
            obj.len(),
            3,
            "success body must have exactly 3 keys; got {:?}",
            obj.keys().collect::<Vec<_>>()
        );
        assert_eq!(body["status"], "ok");
        assert_eq!(body["compacted"], true);
        assert_eq!(body["continuation_delivered"], false);
    }

    #[test]
    fn compact_success_body_propagates_continuation_delivered_flag() {
        assert_eq!(
            compact_success_body(true, None)["continuation_delivered"],
            true
        );
        assert_eq!(
            compact_success_body(false, None)["continuation_delivered"],
            false
        );
    }

    #[test]
    fn compact_success_body_with_error_preserves_envelope() {
        // The HTTP partial-success path (summarize OK, delivery failed) adds an
        // `error` field on top of the same envelope so callers can read a
        // human-readable reason without losing the shape a typed deserializer
        // expects. All four fields must be present and the base envelope must
        // not be mutated.
        let body = compact_success_body(false, Some("boom".into()));
        let obj = body.as_object().expect("success body is a JSON object");
        assert_eq!(obj.len(), 4);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["compacted"], true);
        assert_eq!(body["continuation_delivered"], false);
        assert_eq!(body["error"], "boom");
    }

    #[tokio::test]
    async fn compact_cc_inject_failure_drains_parked_continuation() {
        // When locked_inject fails AFTER the compact slot has been reserved, the
        // rollback must drain what we parked so the next /compact on this session
        // doesn't 409 forever and the post-compact hook doesn't splice a stale
        // continuation into an unrelated turn. The inject path shells out to tmux,
        // so in the test env it fails after the inject-queue's 3 retries — takes
        // ~1.5s of real time but exercises the exact failure mode.
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "cc-inject-fail".into(),
                pane: Some("%999999999".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    ..Default::default()
                },
            })
            .await;

        let (status, _body) = compact_inner(
            &state,
            "cc-inject-fail".into(),
            CompactBody {
                continuation: Some("rollback me".into()),
            },
        )
        .await;
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "expected inject failure to surface as 500"
        );

        // Critical: the slot must be free so a follow-up compact can proceed.
        let pending = state
            .drain_agent_compact_continuation("cc-inject-fail")
            .await;
        assert_eq!(
            pending, None,
            "rollback must drain the parked continuation on inject failure — slot was not released"
        );
    }

    #[tokio::test]
    async fn compact_cc_with_pending_continuation_returns_409() {
        // Simulates a second compact arriving while the first is still in flight.
        // The second caller must NOT overwrite the first caller's continuation;
        // the endpoint returns 409 Conflict instead.
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "cc-busy".into(),
                pane: Some("%1".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("claude-code".into()),
                    ..Default::default()
                },
            })
            .await;

        // Pre-park a continuation to simulate an in-flight compact
        let acquired = state
            .try_set_pending_compact_continuation("cc-busy", "first".into())
            .await;
        assert!(acquired, "slot should be empty for a fresh session");

        let (status, body) = compact_inner(
            &state,
            "cc-busy".into(),
            CompactBody {
                continuation: Some("second".into()),
            },
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let err = body["error"].as_str().unwrap();
        assert!(
            err.contains("pending") || err.contains("in progress"),
            "expected error to mention pending/in-progress, got: {err}"
        );

        // Critically: the first continuation must still be parked, not overwritten
        let pending = state.drain_agent_compact_continuation("cc-busy").await;
        assert_eq!(
            pending.as_deref(),
            Some("first"),
            "first caller's continuation must not be overwritten by the rejected second attempt"
        );
    }

    // --- lookup_opencode_session_dir ---

    #[tokio::test]
    async fn lookup_opencode_session_dir_returns_none_when_serve_unreachable() {
        // The test env binds opencode_serve_port() to config.port + 320.
        // For new_for_test(), config.port = 0 → port 320 (privileged, unbound),
        // so the GET will fail with connection refused. The helper must not
        // panic and must surface the failure as None so callers can fall back
        // gracefully (e.g. to the auto-provision path or the strict error).
        let state = crate::state::AppState::new_for_test();
        let dir = lookup_opencode_session_dir(&state, "ses_does_not_exist").await;
        assert!(
            dir.is_none(),
            "unreachable opencode serve must produce None, got Some({dir:?})"
        );
    }

    // --- parse_opencode_model / resolve_opencode_compact_model ---

    #[test]
    fn parse_opencode_model_two_segments() {
        assert_eq!(
            parse_opencode_model("anthropic/claude-sonnet-4-6"),
            Some(("anthropic".into(), "claude-sonnet-4-6".into()))
        );
    }

    #[test]
    fn parse_opencode_model_splits_on_first_slash_only() {
        // Opencode's parser keeps trailing slashes in the modelID segment;
        // mirror that so `openrouter/openai/gpt-5.4` maps to the same
        // providerID our `prompt_async` delivery uses.
        assert_eq!(
            parse_opencode_model("openrouter/openai/gpt-5.4"),
            Some(("openrouter".into(), "openai/gpt-5.4".into()))
        );
    }

    #[test]
    fn parse_opencode_model_trims_whitespace_per_segment() {
        assert_eq!(
            parse_opencode_model("  openrouter / gpt-5.4  "),
            Some(("openrouter".into(), "gpt-5.4".into()))
        );
    }

    #[test]
    fn parse_opencode_model_rejects_no_slash() {
        assert_eq!(parse_opencode_model("sonnet"), None);
        assert_eq!(parse_opencode_model(""), None);
        assert_eq!(parse_opencode_model("   "), None);
    }

    #[test]
    fn parse_opencode_model_rejects_empty_segment() {
        assert_eq!(parse_opencode_model("/"), None);
        assert_eq!(parse_opencode_model("anthropic/"), None);
        assert_eq!(parse_opencode_model("/sonnet"), None);
        assert_eq!(parse_opencode_model("  /  "), None);
        assert_eq!(parse_opencode_model("anthropic/   "), None);
    }

    #[test]
    fn parse_opencode_config_model_prefers_top_level_model_over_provider_default() {
        let config = serde_json::json!({
            "model": "openrouter/openai/gpt-5.5",
            "providers": {
                "default": {
                    "openai": "gpt-5.5-pro"
                }
            }
        });

        assert_eq!(
            parse_opencode_config_model(&config),
            Some(("openrouter".into(), "openai/gpt-5.5".into()))
        );
    }

    #[tokio::test]
    async fn resolve_opencode_compact_model_uses_session_model_when_parseable() {
        // Session model is the primary source — when set and parseable, the
        // helper must not even attempt the /config HTTP call.
        let state = crate::state::AppState::new_for_test();
        let result =
            resolve_opencode_compact_model(&state, Some("anthropic/claude-sonnet-4-6"), None).await;
        assert_eq!(
            result,
            Some(("anthropic".into(), "claude-sonnet-4-6".into()))
        );
    }

    #[tokio::test]
    async fn readiness_prompt_delivery_skips_tmux_fallback_for_ambiguous_http_status() {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::post;
        use tokio::net::TcpListener;

        async fn prompt_async() -> StatusCode {
            StatusCode::BAD_GATEWAY
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route("/session/{session_id}/prompt_async", post(prompt_async));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: port.checked_sub(320).unwrap(),
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_ready".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/project", "%17")];
        state.pending_prompts.lock().unwrap().insert(
            "oc".into(),
            pending_opencode_prompt("%17", "queued prompt", "ses_ready"),
        );

        let delivered = deliver_pending_prompt(&state, "oc").await;

        assert!(!delivered);
        assert_eq!(
            state.pending_prompts.lock().unwrap().get("oc"),
            Some(&pending_opencode_prompt(
                "%17",
                "queued prompt",
                "ses_ready"
            ))
        );
        server.abort();
    }

    #[tokio::test]
    async fn readiness_prompt_delivery_reserves_pending_prompt_while_http_is_in_flight() {
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use tokio::net::TcpListener;
        use tokio::sync::Notify;

        #[derive(Clone)]
        struct Gate {
            started: StdArc<Notify>,
            release: StdArc<Notify>,
        }

        async fn prompt_async(AxumState(gate): AxumState<Gate>) -> StatusCode {
            gate.started.notify_one();
            gate.release.notified().await;
            StatusCode::NO_CONTENT
        }

        let gate = Gate {
            started: StdArc::new(Notify::new()),
            release: StdArc::new(Notify::new()),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session/{session_id}/prompt_async", post(prompt_async))
            .with_state(gate.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: port.checked_sub(320).unwrap(),
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_ready".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;
        state.pending_prompts.lock().unwrap().insert(
            "oc".into(),
            pending_opencode_prompt("%17", "queued prompt", "ses_ready"),
        );

        let delivery = tokio::spawn({
            let state = state.clone();
            async move { deliver_pending_prompt(&state, "oc").await }
        });
        gate.started.notified().await;

        assert!(!state.pending_prompts.lock().unwrap().contains_key("oc"));

        gate.release.notify_one();
        assert!(delivery.await.unwrap());
        server.abort();
    }

    #[tokio::test]
    async fn readiness_prompt_delivery_retries_after_http_and_fallback_fail() {
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use tokio::net::TcpListener;
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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session/{session_id}/prompt_async", post(prompt_async))
            .with_state(gate.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: port.checked_sub(320).unwrap(),
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_ready".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;
        state.pending_prompts.lock().unwrap().insert(
            "oc".into(),
            pending_opencode_prompt("%17", "queued prompt", "ses_ready"),
        );

        let delivery = tokio::spawn({
            let state = state.clone();
            async move { deliver_pending_prompt(&state, "oc").await }
        });
        gate.started.notified().await;

        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%18".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_ready".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;
        gate.release.notify_one();

        assert!(!delivery.await.unwrap());
        assert_eq!(
            state.pending_prompts.lock().unwrap().get("oc"),
            Some(&pending_opencode_prompt(
                "%17",
                "queued prompt",
                "ses_ready"
            ))
        );

        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/project", "%17")];

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            state.pending_prompts.lock().unwrap().get("oc"),
            Some(&pending_opencode_prompt(
                "%17",
                "queued prompt",
                "ses_ready"
            ))
        );
        server.abort();
    }

    #[test]
    fn pending_prompt_retry_reserves_prompt_before_raw_fallback() {
        let state = crate::state::AppState::new_for_test();
        state
            .pending_prompts
            .lock()
            .unwrap()
            .insert("oc".into(), pending_prompt("%17", "queued prompt"));

        let reserved =
            reserve_pending_prompt_if_matches(&state, "oc", "%17", "queued prompt", None);

        assert_eq!(reserved, Some(pending_prompt("%17", "queued prompt")));
        assert!(!state.pending_prompts.lock().unwrap().contains_key("oc"));
    }

    #[tokio::test]
    async fn pending_prompt_retry_rearms_after_transient_pane_failure() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_ready".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;
        let pending = pending_opencode_prompt("%17", "queued prompt", "ses_ready");
        state
            .pending_prompts
            .lock()
            .unwrap()
            .insert("oc".into(), pending.clone());

        schedule_pending_prompt_retry(&state, "oc", pending.clone());
        tokio::time::sleep(PENDING_PROMPT_RETRY_DELAY * 2).await;
        assert_eq!(
            state.pending_prompts.lock().unwrap().get("oc"),
            Some(&pending),
            "first retry should restore the prompt while the pane is not live"
        );

        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/project", "%17")];
        tokio::time::sleep(PENDING_PROMPT_RETRY_DELAY * 2).await;

        assert!(
            !state.pending_prompts.lock().unwrap().contains_key("oc"),
            "a follow-up retry should consume the restored prompt once the pane becomes live"
        );
    }

    #[tokio::test]
    async fn readiness_prompt_delivery_uses_raw_tmux_for_weak_opencode_binding() {
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::net::TcpListener;

        async fn prompt_async(AxumState(called): AxumState<StdArc<AtomicBool>>) -> StatusCode {
            called.store(true, Ordering::SeqCst);
            StatusCode::NO_CONTENT
        }

        let called = StdArc::new(AtomicBool::new(false));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session/{session_id}/prompt_async", post(prompt_async))
            .with_state(called.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: port.checked_sub(320).unwrap(),
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_ready".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::WeakAdopted),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/project", "%17")];
        state
            .pending_prompts
            .lock()
            .unwrap()
            .insert("oc".into(), pending_prompt("%17", "queued prompt"));

        let delivered = deliver_pending_prompt(&state, "oc").await;

        assert!(delivered);
        assert!(!called.load(Ordering::SeqCst));
        assert!(!state.pending_prompts.lock().unwrap().contains_key("oc"));
        server.abort();
    }

    #[tokio::test]
    async fn readiness_prompt_delivery_rejects_stale_weak_opencode_pane() {
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::net::TcpListener;

        async fn prompt_async(AxumState(called): AxumState<StdArc<AtomicBool>>) -> StatusCode {
            called.store(true, Ordering::SeqCst);
            StatusCode::NO_CONTENT
        }

        let called = StdArc::new(AtomicBool::new(false));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session/{session_id}/prompt_async", post(prompt_async))
            .with_state(called.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: port.checked_sub(320).unwrap(),
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_ready".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::WeakAdopted),
                    ..Default::default()
                },
            })
            .await;
        state
            .pending_prompts
            .lock()
            .unwrap()
            .insert("oc".into(), pending_prompt("%17", "queued prompt"));

        let delivered = deliver_pending_prompt(&state, "oc").await;

        assert!(!delivered);
        assert!(!called.load(Ordering::SeqCst));
        assert_eq!(
            state.pending_prompts.lock().unwrap().get("oc"),
            Some(&pending_prompt("%17", "queued prompt"))
        );
        server.abort();
    }

    #[tokio::test]
    async fn readiness_prompt_delivery_rejects_stale_strong_opencode_pane() {
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

        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: port.checked_sub(320).unwrap(),
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%new".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_new".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;
        state.pending_prompts.lock().unwrap().insert(
            "oc".into(),
            pending_opencode_prompt("%old", "queued prompt", "ses_old"),
        );

        let delivered = deliver_pending_prompt(&state, "oc").await;

        assert!(!delivered);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            state.pending_prompts.lock().unwrap().get("oc"),
            Some(&pending_opencode_prompt("%old", "queued prompt", "ses_old"))
        );
        server.abort();
    }

    #[tokio::test]
    async fn readiness_prompt_delivery_rejects_stale_opencode_backend_session() {
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

        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: port.checked_sub(320).unwrap(),
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_old".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;
        state.pending_prompts.lock().unwrap().insert(
            "oc".into(),
            pending_opencode_prompt("%17", "queued prompt", "ses_old"),
        );
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_new".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;

        let delivered = deliver_pending_prompt(&state, "oc").await;

        assert!(!delivered);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            state.pending_prompts.lock().unwrap().get("oc"),
            Some(&pending_opencode_prompt("%17", "queued prompt", "ses_old"))
        );
        server.abort();
    }

    #[tokio::test]
    async fn readiness_prompt_http_fallback_rejects_stale_opencode_backend_session() {
        use axum::Router;
        use axum::extract::State as AxumState;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc as StdArc;
        use tokio::net::TcpListener;
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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/session/{session_id}/prompt_async", post(prompt_async))
            .with_state(gate.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: port.checked_sub(320).unwrap(),
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        });
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_old".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/project", "%17")];
        state.pending_prompts.lock().unwrap().insert(
            "oc".into(),
            pending_opencode_prompt("%17", "queued prompt", "ses_old"),
        );

        let delivery = tokio::spawn({
            let state = state.clone();
            async move { deliver_pending_prompt(&state, "oc").await }
        });
        gate.started.notified().await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "oc".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_new".into()),
                    opencode_binding: Some(crate::daemon_protocol::OpenCodeBinding::StrongManaged),
                    ..Default::default()
                },
            })
            .await;
        gate.release.notify_one();

        assert!(!delivery.await.unwrap());
        assert_eq!(
            state.pending_prompts.lock().unwrap().get("oc"),
            Some(&pending_opencode_prompt("%17", "queued prompt", "ses_old"))
        );
        server.abort();
    }

    #[tokio::test]
    async fn resolve_opencode_compact_model_returns_none_when_no_model_and_serve_unreachable() {
        // No session model, opencode serve unreachable in test env
        // (opencode_serve_port() == 320 for config.port=0) → both sources
        // fail → None. The compact endpoint must surface this as a 400 so
        // the caller knows summarize cannot proceed.
        let state = crate::state::AppState::new_for_test();
        let result = resolve_opencode_compact_model(&state, None, None).await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn resolve_opencode_compact_model_falls_through_on_unparseable_session_model() {
        // An unparseable session model (e.g. bare "sonnet" with no provider
        // segment) must not be used as-is; the helper falls through to the
        // /config fallback rather than invent a providerID.
        let state = crate::state::AppState::new_for_test();
        let result = resolve_opencode_compact_model(&state, Some("sonnet"), None).await;
        assert_eq!(result, None, "bare 'sonnet' must not be accepted as a pair");
    }

    #[tokio::test]
    async fn resolve_opencode_compact_model_scopes_config_lookup_to_session_directory() {
        use axum::Router;
        use axum::http::HeaderMap;
        use axum::routing::get;
        use tokio::net::TcpListener;

        async fn config(headers: HeaderMap) -> Json<serde_json::Value> {
            let model = match headers
                .get("x-opencode-directory")
                .and_then(|v| v.to_str().ok())
            {
                Some("/tmp/project-a") => "openrouter/openai/gpt-5.5",
                _ => "openai/gpt-5.5-pro",
            };
            Json(serde_json::json!({ "model": model }))
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let serve_port = listener.local_addr().unwrap().port();
        assert!(serve_port >= 320, "test listener port must be >= 320");
        let app = Router::new().route("/config", get(config));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let state = crate::state::AppState::new(crate::config::OuijaConfig {
            name: "test".into(),
            npub: "npub1test".into(),
            port: serve_port - 320,
            data_dir: std::path::PathBuf::from("/tmp/ouija-test-agent"),
            config_dir: std::path::PathBuf::from("/tmp/ouija-test-agent"),
        });

        let result = resolve_opencode_compact_model(&state, None, Some("/tmp/project-a")).await;

        server.abort();
        assert_eq!(
            result,
            Some(("openrouter".into(), "openai/gpt-5.5".into())),
            "resolver must use the target session directory's OpenCode config model"
        );
    }

    // --- auto_provision_from_backend_session (issue #35) ---

    fn pane_in(dir: &str, pane_id: &str) -> crate::tmux::TmuxPane {
        crate::tmux::TmuxPane {
            pane_id: pane_id.into(),
            session_name: "test".into(),
            pane_current_path: Some(dir.into()),
            process_name: Some("opencode".into()),
        }
    }

    fn pane_for_backend(dir: &str, pane_id: &str, process_name: &str) -> crate::tmux::TmuxPane {
        crate::tmux::TmuxPane {
            pane_id: pane_id.into(),
            session_name: "test".into(),
            pane_current_path: Some(dir.into()),
            process_name: Some(process_name.into()),
        }
    }

    #[tokio::test]
    async fn auto_provision_creates_session_for_single_matching_pane() {
        // Issue #35: opencode TUI started in a fresh dir with no pre-existing
        // ouija record. After direct lookup and adoption both miss, the
        // daemon must auto-provision a session record so the user's first
        // `ouija` CLI call resolves the pane.
        let state = crate::state::AppState::new_for_test();
        // Seed the cached pane snapshot — in tests, list_assistant_panes
        // reads from this rather than shelling out to tmux.
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/freshproject", "%17")];

        let result =
            auto_provision_from_backend_session(&state, "ses_brand_new", "/tmp/freshproject").await;

        let session_id = result.expect("auto-provision must succeed for exactly one matching pane");
        assert_eq!(session_id, "freshproject");

        // Verify state mutations end-to-end.
        let proto = state.protocol.read().await;
        let session = proto
            .sessions
            .get(&session_id)
            .expect("session must exist in protocol state");
        assert_eq!(session.pane.as_deref(), Some("%17"), "pane must be bound");
        assert_eq!(
            session.metadata.backend.as_deref(),
            Some("opencode"),
            "backend must be opencode"
        );
        assert_eq!(
            session.metadata.backend_session_id.as_deref(),
            Some("ses_brand_new"),
            "backend_session_id must be bound atomically with the Register"
        );
        assert_eq!(
            session.metadata.project_dir.as_deref(),
            Some("/tmp/freshproject"),
            "project_dir must be set"
        );
        drop(proto);

        // The pane must now resolve back to the new session, which is the
        // end-state that unblocks the ouija CLI's `@ouija_session` / pane
        // lookup inside the user's terminal.
        let resolved = state.find_session_by_pane("%17").await;
        assert_eq!(
            resolved.as_deref(),
            Some("freshproject"),
            "find_session_by_pane must resolve the auto-provisioned pane"
        );
    }

    #[tokio::test]
    async fn auto_provision_declines_when_no_pane_matches_dir() {
        // Zero tmux panes running opencode in the target directory. The daemon
        // cannot invent a pane, so it must fail closed and leave state empty.
        // The user's workaround (explicit `ouija register ...`) still applies.
        let state = crate::state::AppState::new_for_test();
        // Seed a pane in a DIFFERENT dir so list_assistant_panes is non-empty
        // but no candidate matches the target. Catches regressions where an
        // empty vs. non-empty cache path diverges.
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/someother", "%11")];

        let result =
            auto_provision_from_backend_session(&state, "ses_unmatched", "/tmp/freshproject").await;

        assert!(
            result.is_none(),
            "auto-provision must decline with zero matching panes, got Some({result:?})"
        );
        let proto = state.protocol.read().await;
        assert!(
            proto.sessions.is_empty(),
            "no session must be created on zero-match decline, got: {:?}",
            proto.sessions.keys().collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn auto_provision_declines_when_matching_dir_pane_is_not_opencode() {
        let state = crate::state::AppState::new_for_test();
        *state.cached_assistant_panes.write().await =
            vec![pane_for_backend("/tmp/freshproject", "%17", "claude")];

        let result =
            auto_provision_from_backend_session(&state, "ses_claude", "/tmp/freshproject").await;

        assert!(
            result.is_none(),
            "auto-provision must reject non-opencode panes, got Some({result:?})"
        );
        let proto = state.protocol.read().await;
        assert!(
            proto.sessions.is_empty(),
            "no session must be created for a non-opencode pane"
        );
    }

    #[tokio::test]
    async fn auto_provision_declines_on_ambiguous_multiple_panes() {
        // Two opencode panes in the same project dir — very common when a
        // user iterates on a feature in split panes. The daemon has no way
        // to know which pane this backend_session_id belongs to, so it must
        // fail closed for the same reason disambiguate_adoption_candidates
        // does (issue #15). User disambiguates via `ouija register ...`.
        let state = crate::state::AppState::new_for_test();
        *state.cached_assistant_panes.write().await = vec![
            pane_in("/tmp/freshproject", "%17"),
            pane_in("/tmp/freshproject", "%23"),
        ];

        let result =
            auto_provision_from_backend_session(&state, "ses_ambiguous", "/tmp/freshproject").await;

        assert!(
            result.is_none(),
            "auto-provision must decline on >=2 matching panes, got Some({result:?})"
        );
        let proto = state.protocol.read().await;
        assert!(
            proto.sessions.is_empty(),
            "no session must be created on ambiguity decline"
        );
    }

    #[tokio::test]
    async fn auto_provision_skips_panes_already_registered() {
        // A pane that already owns a session (different backend_session_id,
        // or none at all) must not be re-registered by auto-provision. This
        // protects sessions created via the claude-code SessionStart hook or
        // the periodic scan_and_autoregister_panes loop from being stomped
        // when an unrelated opencode readiness signal arrives.
        let state = crate::state::AppState::new_for_test();
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/freshproject", "%17")];
        // Pre-register the pane to a different session.
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "preexisting".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/tmp/freshproject".into()),
                    ..Default::default()
                },
            })
            .await;

        let result =
            auto_provision_from_backend_session(&state, "ses_intruder", "/tmp/freshproject").await;

        assert!(
            result.is_none(),
            "auto-provision must skip already-registered panes, got Some({result:?})"
        );
        let proto = state.protocol.read().await;
        assert_eq!(
            proto.sessions.len(),
            1,
            "no additional session must be created, got: {:?}",
            proto.sessions.keys().collect::<Vec<_>>()
        );
        let preexisting = proto.sessions.get("preexisting").unwrap();
        assert!(
            preexisting.metadata.backend_session_id.is_none(),
            "pre-existing session must NOT be clobbered with the intruder's backend_session_id"
        );
    }

    #[tokio::test]
    async fn auto_provision_short_circuits_when_concurrent_call_won_the_race() {
        // Simulate the case where a concurrent backend-session/ready callback
        // has just completed auto-provision by the time this one reaches the
        // recheck point. The recheck must return the concurrent result instead
        // of racing a second Register (which would churn @ouija_session under
        // the user's feet with a different suffix).
        let state = crate::state::AppState::new_for_test();
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/freshproject", "%17")];
        // Seed a session that already owns the backend_session_id — the
        // state the "winning" concurrent call would have left behind.
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "already-bound".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/tmp/freshproject".into()),
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_raced".into()),
                    ..Default::default()
                },
            })
            .await;

        let result =
            auto_provision_from_backend_session(&state, "ses_raced", "/tmp/freshproject").await;

        assert_eq!(
            result.as_deref(),
            Some("already-bound"),
            "recheck must surface the concurrent winner's id, not invent a new one"
        );
        let proto = state.protocol.read().await;
        assert_eq!(
            proto.sessions.len(),
            1,
            "no extra session must be created on the lost race"
        );
    }

    #[test]
    fn auto_provision_register_result_returns_backend_winner_after_register_failed() {
        let mut proto = crate::daemon_protocol::DaemonState::new_for_model("d".into(), "h".into());
        proto.apply(crate::daemon_protocol::Event::Register {
            id: "winner".into(),
            pane: Some("%17".into()),
            metadata: crate::daemon_protocol::SessionMeta {
                backend: Some("opencode".into()),
                backend_session_id: Some("ses_raced".into()),
                ..Default::default()
            },
        });
        let effects = vec![crate::daemon_protocol::Effect::RegisterFailed {
            session_id: "freshproject".into(),
            reason: "backend_session_id ses_raced is already bound to session 'winner'".into(),
        }];

        let result = auto_provision_register_result(&proto, "ses_raced", "freshproject", &effects);

        assert_eq!(
            result.as_deref(),
            Some("winner"),
            "lost guarded-register races must surface the concurrent backend owner"
        );
    }

    // --- backend_session_ready_inner (end-to-end through the outer handler) ---

    #[tokio::test]
    async fn backend_session_ready_respects_auto_register_disabled() {
        // auto_register=false is the operator opt-out. Even with a matching
        // tmux pane in the target dir, the daemon must not invent a session
        // record behind the operator's back. The request carries the strict
        // error the pre-#35 daemon returned.
        let state = crate::state::AppState::new_for_test();
        state.settings.write().await.auto_register = false;
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/freshproject", "%17")];

        let response = backend_session_ready_inner(&state, "ses_gated".into()).await;

        assert_eq!(response["delivered"], false);
        assert!(
            response["error"]
                .as_str()
                .unwrap_or("")
                .contains("no session with this backend_session_id"),
            "expected strict error, got: {response}"
        );
        assert!(response.get("session").is_none());

        let proto = state.protocol.read().await;
        assert!(
            proto.sessions.is_empty(),
            "no session must be created when auto_register is disabled"
        );
    }

    #[tokio::test]
    async fn backend_session_ready_returns_strict_error_when_serve_unreachable() {
        // opencode serve binds at config.port + 320 = 320 in new_for_test(),
        // so the GET connection is refused. lookup_opencode_session_dir
        // surfaces this as None, the outer handler must keep the historical
        // strict error rather than creating a session with an invented dir.
        let state = crate::state::AppState::new_for_test();
        // Even with a pane cached, serve unreachable -> no dir -> no
        // auto-provision. This guards against a future refactor that tries
        // to synthesize a dir from the pane's pane_current_path.
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/freshproject", "%17")];

        let response = backend_session_ready_inner(&state, "ses_no_serve".into()).await;

        assert_eq!(response["delivered"], false);
        assert!(
            response["error"]
                .as_str()
                .unwrap_or("")
                .contains("no session with this backend_session_id"),
            "expected strict error, got: {response}"
        );
        let proto = state.protocol.read().await;
        assert!(
            proto.sessions.is_empty(),
            "no session must be created when opencode serve is unreachable"
        );
    }

    #[tokio::test]
    async fn backend_session_ready_direct_lookup_hits_when_session_already_bound() {
        // Fast path: the backend_session_id is already attached to a session
        // (hub-spawned, or a prior auto-provision). The handler returns the
        // session name without touching the opencode serve or the pane scan.
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "prebound".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/tmp/freshproject".into()),
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_known".into()),
                    ..Default::default()
                },
            })
            .await;

        let response = backend_session_ready_inner(&state, "ses_known".into()).await;

        assert_eq!(
            response["session"].as_str(),
            Some("prebound"),
            "direct lookup must surface the session id, got: {response}"
        );
    }

    #[tokio::test]
    async fn backend_session_ready_rejects_backend_session_id_path_separators() {
        let state = crate::state::AppState::new_for_test();
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/local-project", "%31")];
        let hints = BackendSessionReadyHints {
            pane: Some("%31".into()),
            cwd: Some("/tmp/local-project".into()),
        };

        let response =
            backend_session_ready_inner_with_hints(&state, "ses_bad/../../x".into(), hints).await;

        assert_eq!(response["delivered"], false);
        assert!(
            response["error"]
                .as_str()
                .unwrap_or("")
                .contains("invalid backend_session_id"),
            "expected validation error, got: {response}"
        );
        assert!(
            state.protocol.read().await.sessions.is_empty(),
            "invalid backend_session_id must be rejected before auto-provision"
        );
    }

    #[test]
    fn backend_session_id_boundary_rejects_url_delimiters_and_whitespace() {
        for backend_sid in ["ses/bad", "ses?bad", "ses#bad", "ses bad", "ses\tbad"] {
            assert!(
                validate_backend_session_id_boundary(backend_sid).is_some(),
                "{backend_sid:?} must be rejected"
            );
        }
        assert!(validate_backend_session_id_boundary("ses_good-123").is_none());
    }

    #[tokio::test]
    async fn backend_session_ready_ignores_remote_backend_session_id_collision() {
        let state = crate::state::AppState::new_for_test();
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/local-project", "%31")];
        {
            let mut proto = state.protocol.write().await;
            proto.sessions.insert(
                "remote-host/local-project".into(),
                crate::daemon_protocol::SessionEntry {
                    id: "remote-host/local-project".into(),
                    pane: None,
                    origin: crate::daemon_protocol::Origin::Remote("npub1remote".into()),
                    metadata: crate::daemon_protocol::SessionMeta {
                        project_dir: Some("/tmp/remote-project".into()),
                        backend: Some("opencode".into()),
                        backend_session_id: Some("ses_collision".into()),
                        ..Default::default()
                    },
                    registered_at: 0,
                },
            );
        }

        let hints = BackendSessionReadyHints {
            pane: Some("%31".into()),
            cwd: Some("/tmp/local-project".into()),
        };

        let response =
            backend_session_ready_inner_with_hints(&state, "ses_collision".into(), hints).await;

        assert_eq!(
            response["session"].as_str(),
            Some("local-project"),
            "remote backend_session_id collision must not satisfy readiness: {response}"
        );
        let proto = state.protocol.read().await;
        assert!(
            proto.sessions.contains_key("local-project"),
            "local auto-provision must still create a local session"
        );
    }

    // --- Plugin-sent pane + cwd hints (fast-path) ---

    #[tokio::test]
    async fn backend_session_ready_uses_explicit_pane_and_cwd_hints() {
        // Happy path for the enriched opencode plugin: when both pane and
        // cwd arrive in the body, skip the opencode-serve round-trip AND the
        // scan-path's opencode-serve dir lookup. The pane still has to appear
        // in list_assistant_panes and its current path must match the hinted
        // cwd after project-root normalization.
        let state = crate::state::AppState::new_for_test();
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/explicit-project", "%31")];

        let hints = BackendSessionReadyHints {
            pane: Some("%31".into()),
            cwd: Some("/tmp/explicit-project".into()),
        };

        let response =
            backend_session_ready_inner_with_hints(&state, "ses_explicit".into(), hints).await;

        assert_eq!(
            response["session"].as_str(),
            Some("explicit-project"),
            "hint path must auto-provision with id derived from cwd basename, got: {response}"
        );

        // State mutations end-to-end: project_dir follows the explicit cwd hint.
        let proto = state.protocol.read().await;
        let session = proto
            .sessions
            .get("explicit-project")
            .expect("session exists");
        assert_eq!(session.pane.as_deref(), Some("%31"));
        assert_eq!(
            session.metadata.backend_session_id.as_deref(),
            Some("ses_explicit"),
        );
        assert_eq!(
            session.metadata.project_dir.as_deref(),
            Some("/tmp/explicit-project"),
        );
    }

    #[tokio::test]
    async fn backend_session_ready_explicit_hints_respect_auto_register_disabled() {
        // auto_register=false opts out of implicit session creation
        // regardless of whether the plugin sent explicit hints. The hint
        // shortcut must not bypass the operator guardrail.
        let state = crate::state::AppState::new_for_test();
        state.settings.write().await.auto_register = false;

        let hints = BackendSessionReadyHints {
            pane: Some("%31".into()),
            cwd: Some("/tmp/explicit-project".into()),
        };

        let response =
            backend_session_ready_inner_with_hints(&state, "ses_gated_with_hints".into(), hints)
                .await;

        assert_eq!(response["delivered"], false);
        assert!(
            response["error"]
                .as_str()
                .unwrap_or("")
                .contains("no session with this backend_session_id"),
            "expected strict error, got: {response}"
        );
        let proto = state.protocol.read().await;
        assert!(
            proto.sessions.is_empty(),
            "hints must not bypass the auto_register opt-out"
        );
    }

    #[tokio::test]
    async fn backend_session_ready_partial_hints_fall_back_to_scan_path() {
        // Only pane, no cwd: do NOT take the fast-path. Fall through to the
        // existing opencode-serve dir lookup. In the test env the serve is
        // unreachable, so the expected end-state is the strict error — this
        // test pins the fallback behaviour rather than relying on a
        // future half-hint shortcut that is explicitly out of scope
        // (see the "partial hints fall back entirely" decision on this task).
        let state = crate::state::AppState::new_for_test();
        // Seed a matching pane anyway. Hint path must NOT fire (cwd missing),
        // so the scan path will run and miss because opencode serve is down.
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/half-hinted", "%31")];

        let hints = BackendSessionReadyHints {
            pane: Some("%31".into()),
            cwd: None,
        };

        let response =
            backend_session_ready_inner_with_hints(&state, "ses_half".into(), hints).await;

        // Fallback path: no dir -> strict error, no session.
        assert_eq!(response["delivered"], false);
        assert!(response.get("session").is_none());
        let proto = state.protocol.read().await;
        assert!(proto.sessions.is_empty());
    }

    #[tokio::test]
    async fn backend_session_ready_explicit_hints_direct_lookup_still_wins() {
        // When the backend_session_id is already bound to a session, the
        // direct-lookup fast path in step 1 must short-circuit BEFORE the
        // hint path runs. Otherwise an existing session with a different
        // id could be shadowed by a newly-invented one derived from the
        // hint cwd.
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "prebound".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/tmp/some-real-project".into()),
                    backend: Some("opencode".into()),
                    backend_session_id: Some("ses_known".into()),
                    ..Default::default()
                },
            })
            .await;

        let hints = BackendSessionReadyHints {
            pane: Some("%99".into()),
            cwd: Some("/tmp/unrelated".into()),
        };

        let response =
            backend_session_ready_inner_with_hints(&state, "ses_known".into(), hints).await;

        assert_eq!(
            response["session"].as_str(),
            Some("prebound"),
            "direct lookup must win even when hints point elsewhere, got: {response}"
        );
        let proto = state.protocol.read().await;
        assert_eq!(
            proto.sessions.len(),
            1,
            "no session must be created from the hints when direct lookup hits"
        );
    }

    // --- Hint-path pane validation (review item: defense parity with scan path) ---

    #[tokio::test]
    async fn hint_path_rejects_pane_not_in_assistant_panes() {
        // Defense parity with scan path: a caller that POSTs an arbitrary
        // pane id that isn't actually running opencode must NOT be able to
        // create a ghost session bound to a dead pane. The scan path enforces
        // this implicitly by only considering panes from list_assistant_panes;
        // the hint path must match that contract explicitly.
        let state = crate::state::AppState::new_for_test();
        // Seed panes that do NOT include %99.
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/unrelated", "%11")];

        let result = auto_provision_with_explicit_pane(
            &state,
            "ses_ghost",
            "%99", // not in list_assistant_panes
            "/tmp/freshproject",
        )
        .await;

        assert!(
            result.is_none(),
            "hint path must reject a pane that is not in list_assistant_panes, got Some({result:?})"
        );
        let proto = state.protocol.read().await;
        assert!(
            proto.sessions.is_empty(),
            "no session must be created for an unverified pane"
        );
    }

    #[tokio::test]
    async fn hint_path_rejects_pane_whose_actual_cwd_differs_from_hint() {
        let state = crate::state::AppState::new_for_test();
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/actual-project", "%17")];

        let result =
            auto_provision_with_explicit_pane(&state, "ses_mismatch", "%17", "/tmp/hinted-project")
                .await;

        assert!(
            result.is_none(),
            "hint path must reject pane/cwd mismatches, got Some({result:?})"
        );
        assert!(state.protocol.read().await.sessions.is_empty());
    }

    #[tokio::test]
    async fn hint_path_rejects_non_opencode_pane_even_when_cwd_matches() {
        let state = crate::state::AppState::new_for_test();
        *state.cached_assistant_panes.write().await =
            vec![pane_for_backend("/tmp/freshproject", "%17", "claude")];

        let result = auto_provision_with_explicit_pane(
            &state,
            "ses_not_opencode",
            "%17",
            "/tmp/freshproject",
        )
        .await;

        assert!(
            result.is_none(),
            "hint path must reject a non-opencode assistant pane, got Some({result:?})"
        );
        assert!(state.protocol.read().await.sessions.is_empty());
    }

    #[tokio::test]
    async fn hint_path_rejects_empty_cwd() {
        // Degenerate cwd = "" makes Path::file_name() return None, which
        // register_auto_provisioned_session turns into the literal "unnamed".
        // And project_dir would be persisted as the empty string. Reject
        // this at the hint-path entry rather than letting it corrupt state.
        let state = crate::state::AppState::new_for_test();
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/ignored", "%17")];

        let result = auto_provision_with_explicit_pane(
            &state,
            "ses_bad_cwd",
            "%17",
            "", // degenerate cwd
        )
        .await;

        assert!(
            result.is_none(),
            "empty cwd must be rejected, got Some({result:?})"
        );
        assert!(state.protocol.read().await.sessions.is_empty());
    }

    #[tokio::test]
    async fn hint_path_rejects_bare_root_cwd() {
        // cwd = "/" has the same file_name() = None pathology: basename
        // falls through to "unnamed" and project_dir is persisted as "/".
        // No realistic caller has / as their project root.
        let state = crate::state::AppState::new_for_test();
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/ignored", "%17")];

        let result = auto_provision_with_explicit_pane(&state, "ses_root_cwd", "%17", "/").await;

        assert!(
            result.is_none(),
            "bare `/` cwd must be rejected, got Some({result:?})"
        );
        assert!(state.protocol.read().await.sessions.is_empty());
    }

    #[tokio::test]
    async fn hint_path_rejects_relative_cwd() {
        // Every other ouija code path treats project_dir as absolute.
        // Accepting relative paths here would poison downstream comparisons
        // (adoption, scan-by-dir, bulletin dedup, etc.) that string-compare
        // project_dir. Reject at the boundary instead.
        let state = crate::state::AppState::new_for_test();
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/ignored", "%17")];

        let result =
            auto_provision_with_explicit_pane(&state, "ses_rel_cwd", "%17", "relative/path").await;

        assert!(
            result.is_none(),
            "relative cwd must be rejected, got Some({result:?})"
        );
        assert!(state.protocol.read().await.sessions.is_empty());
    }

    // --- Hint body forward-compat (review item: drop deny_unknown_fields) ---

    #[test]
    fn hints_tolerate_unknown_fields_for_forward_compat() {
        // The readiness body is a plugin-side contract that will grow over
        // time (plugin_version, tty_path, etc.). With deny_unknown_fields,
        // a newer plugin talking to an older daemon would fail parsing; the
        // unwrap_or_default() at the handler entry then silently discards
        // BOTH pane and cwd, triggering a slow scan-path fallback with no
        // diagnostic. Postel's law: accept unknown fields, use the known
        // ones. This test pins that contract.
        let body =
            br#"{"pane":"%17","cwd":"/tmp/foo","tty_path":"/dev/pts/3","plugin_version":"2.0"}"#;
        let hints: BackendSessionReadyHints =
            serde_json::from_slice(body).expect("unknown fields must not fail parsing");
        assert_eq!(hints.pane.as_deref(), Some("%17"));
        assert_eq!(hints.cwd.as_deref(), Some("/tmp/foo"));
    }

    #[tokio::test]
    async fn hint_path_rejects_pane_already_registered_to_another_session() {
        // Silent-hijack guard: the scan path filters out panes already owned
        // by another Local session (api.rs:2154-2175). Without the same filter
        // on the hint path, apply_register's pane-dedup silently evicts
        // whoever currently owns the pane. A concurrent claude-code
        // SessionStart, a prior auto-provision, or a manual `ouija register`
        // would all be vulnerable. The hint path must fail closed instead.
        let state = crate::state::AppState::new_for_test();
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/freshproject", "%17")];
        // Pre-bind the pane to another session (e.g. the claude-code hook
        // got there first).
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "prebound".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/tmp/freshproject".into()),
                    ..Default::default()
                },
            })
            .await;

        let result = auto_provision_with_explicit_pane(
            &state,
            "ses_hijacker",
            "%17", // already owned by `prebound`
            "/tmp/freshproject",
        )
        .await;

        assert!(
            result.is_none(),
            "hint path must refuse to hijack an already-registered pane, got Some({result:?})"
        );
        let proto = state.protocol.read().await;
        assert_eq!(
            proto.sessions.len(),
            1,
            "the victim session must survive intact"
        );
        let survivor = proto.sessions.get("prebound").unwrap();
        assert!(
            survivor.metadata.backend_session_id.is_none(),
            "victim's metadata must not be rewritten with the hijacker's backend_session_id"
        );
    }

    #[tokio::test]
    async fn auto_provision_register_declines_if_pane_becomes_bound_before_apply() {
        // The outer auto-provision paths validate pane ownership before this
        // helper runs, but that snapshot can become stale before Register is
        // applied. The final apply-time guard must fail closed instead of
        // letting apply_register's pane-dedup evict the incumbent session.
        let state = crate::state::AppState::new_for_test();
        *state.cached_assistant_panes.write().await = vec![pane_in("/tmp/freshproject", "%17")];
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "incumbent".into(),
                pane: Some("%17".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/tmp/freshproject".into()),
                    ..Default::default()
                },
            })
            .await;

        let result =
            register_auto_provisioned_session(&state, "ses_late_race", "%17", "/tmp/freshproject")
                .await;

        assert!(
            result.is_none(),
            "late pane ownership race must fail closed, got Some({result:?})"
        );
        let proto = state.protocol.read().await;
        assert_eq!(
            proto.sessions.len(),
            1,
            "auto-provision must not create a second session or evict the incumbent"
        );
        let incumbent = proto
            .sessions
            .get("incumbent")
            .expect("incumbent session must survive");
        assert_eq!(incumbent.pane.as_deref(), Some("%17"));
        assert!(
            incumbent.metadata.backend_session_id.is_none(),
            "incumbent metadata must not be overwritten by late auto-provision"
        );
    }

    // --- force_reset plumbing (hub#528 guard) ---

    #[test]
    fn session_name_body_parses_force_reset_when_present() {
        // Hub opts in to the data-destructive reset by passing force_reset=true.
        let body: SessionNameBody = serde_json::from_str(
            r#"{"name":"s","worktree":true,"base_branch":"main","force_reset":true}"#,
        )
        .expect("body parses");
        assert_eq!(
            body.force_reset,
            Some(true),
            "force_reset=true must deserialize to Some(true)"
        );
    }

    #[test]
    fn session_name_body_force_reset_defaults_to_none() {
        // When the caller omits the field, it must default to None —
        // which start_session treats as force_reset=false (the safe default).
        let body: SessionNameBody = serde_json::from_str(r#"{"name":"s"}"#).expect("body parses");
        assert_eq!(
            body.force_reset, None,
            "omitted force_reset must deserialize to None (safe default)"
        );
    }

    // --- Dropped-intent predicate on the restart path (hub#528 review) ---
    //
    // `/api/sessions/start` routes to `restart_session` when the named
    // session is already registered. `restart_session` does not plumb
    // `base_branch` or `force_reset` into `create_ouija_worktree` — it
    // reuses the existing worktree dir as-is. Without a warning, a
    // caller that explicitly opted in with `force_reset=true` on a
    // registered session would see a 202 Accepted indistinguishable from
    // the reset being honored.
    //
    // The `restart_drops_destructive_intent` predicate is the single
    // source of truth for when that warning should fire. The API handler
    // calls it inside the `exists` branch; tests lock in the predicate's
    // behavior so the warning never silently regresses.

    #[test]
    fn restart_drops_destructive_intent_fires_for_force_reset_true() {
        let body: SessionNameBody =
            serde_json::from_str(r#"{"name":"s","force_reset":true}"#).unwrap();
        let warn = restart_drops_destructive_intent(&body);
        assert!(
            warn.is_some(),
            "force_reset=true on the restart path must produce a warn message"
        );
        let msg = warn.unwrap();
        assert!(
            msg.contains("force_reset"),
            "warn message must mention force_reset, got: {msg}"
        );
    }

    #[test]
    fn restart_drops_destructive_intent_fires_for_base_branch() {
        let body: SessionNameBody =
            serde_json::from_str(r#"{"name":"s","base_branch":"main"}"#).unwrap();
        let warn = restart_drops_destructive_intent(&body);
        assert!(
            warn.is_some(),
            "base_branch on the restart path must produce a warn message — \
             restart_session cannot act on it"
        );
        assert!(
            warn.unwrap().contains("base_branch"),
            "warn message must mention base_branch"
        );
    }

    #[test]
    fn restart_drops_destructive_intent_silent_when_no_opt_in() {
        let body: SessionNameBody = serde_json::from_str(r#"{"name":"s"}"#).unwrap();
        assert!(
            restart_drops_destructive_intent(&body).is_none(),
            "no opt-in supplied, no warn"
        );
    }

    #[test]
    fn restart_drops_destructive_intent_silent_when_force_reset_false() {
        // Explicit force_reset=false is not an opt-in; nothing is dropped.
        let body: SessionNameBody =
            serde_json::from_str(r#"{"name":"s","force_reset":false}"#).unwrap();
        assert!(
            restart_drops_destructive_intent(&body).is_none(),
            "force_reset=false is not an opt-in; no warn"
        );
    }

    // --- /api/pane/{pane}/... routing and %-prefix tolerance (issue #646) ---
    //
    // Regression harness for "silent 404 on %-prefixed pane ids". Axum
    // percent-decodes path segments, so a literal `%74` on the wire arrives
    // at the handler as `t`. Callers now send the pane *suffix* (without the
    // leading `%`), and the handler tolerates both forms defensively.

    #[tokio::test]
    async fn resolve_pane_to_session_accepts_bare_suffix() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sess-a".into(),
                pane: Some("%74".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;

        let proto = state.protocol.read().await;
        assert_eq!(
            resolve_pane_to_session(&proto, "74").as_deref(),
            Some("sess-a"),
            "bare numeric suffix must resolve"
        );
    }

    #[tokio::test]
    async fn resolve_pane_to_session_accepts_percent_prefix() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sess-a".into(),
                pane: Some("%74".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;

        let proto = state.protocol.read().await;
        assert_eq!(
            resolve_pane_to_session(&proto, "%74").as_deref(),
            Some("sess-a"),
            "%-prefixed form must also resolve (future %25-encoded callers)"
        );
    }

    #[tokio::test]
    async fn resolve_pane_to_session_none_for_unknown_pane() {
        let state = crate::state::AppState::new_for_test();
        let proto = state.protocol.read().await;
        assert!(resolve_pane_to_session(&proto, "999").is_none());
        assert!(resolve_pane_to_session(&proto, "%999").is_none());
    }

    #[tokio::test]
    async fn get_pending_replies_returns_404_for_unknown_pane() {
        // Fail-closed: the old code returned 200 + empty list for an unknown
        // pane, which masked the %-prefix silent-404 bug for read callers.
        let state = crate::state::AppState::new_for_test();
        let (status, _) = get_pending_replies_inner(&state, "999".into()).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "unknown pane must 404, never 200 + empty"
        );
    }

    #[tokio::test]
    async fn get_pending_replies_returns_200_for_known_pane() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sess-a".into(),
                pane: Some("%74".into()),
                metadata: crate::daemon_protocol::SessionMeta::default(),
            })
            .await;

        let (status, body) = get_pending_replies_inner(&state, "74".into()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["count"].as_u64(), Some(0));
    }

    #[tokio::test]
    async fn delete_pending_reply_returns_404_for_unknown_pane() {
        let state = crate::state::AppState::new_for_test();
        let (status, body) =
            delete_pending_reply_inner(&state, "999".into(), "sender".into()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(
            body["error"].as_str().is_some(),
            "404 response must include a JSON error field, got: {body}"
        );
    }

    #[tokio::test]
    async fn delete_pending_reply_returns_cleared_count_when_slot_existed() {
        // Acceptance criterion: callers must be able to distinguish
        // "actually cleared something" from "nothing to clear". The body
        // returns `cleared: N` where N > 0 when a real slot was removed.
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sender-a".into(),
                pane: Some("%74".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    networked: true,
                    ..Default::default()
                },
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "receiver-b".into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    networked: true,
                    ..Default::default()
                },
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Send {
                from: "sender-a".into(),
                to: "receiver-b".into(),
                message: "do a thing".into(),
                expects_reply: true,
                responds_to: None,
                done: false,
            })
            .await;

        let (status, body) =
            delete_pending_reply_inner(&state, "99".into(), "sender-a".into()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["cleared"].as_u64(),
            Some(1),
            "one slot existed → cleared must be 1, got body: {body}"
        );
    }

    #[tokio::test]
    async fn delete_pending_reply_reports_cleared_zero_when_nothing_to_clear() {
        // The pane is registered but the named sender has no pending slot.
        // This must not 404 (the pane exists) and must not lie about
        // clearing something — cleared is 0 so the caller can distinguish
        // "slot was cleared" from "nothing to clear; possibly already gone".
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "receiver-b".into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    networked: true,
                    ..Default::default()
                },
            })
            .await;

        let (status, body) =
            delete_pending_reply_inner(&state, "99".into(), "ghost-sender".into()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["cleared"].as_u64(),
            Some(0),
            "no matching slot → cleared must be 0, got body: {body}"
        );
    }

    #[test]
    fn clear_pending_reply_from_returns_removed_count() {
        // daemon_protocol helper returns the number of entries actually
        // removed. 0 when nothing matches (no-op) so callers don't lie.
        use crate::daemon_protocol::{DaemonState, Event, SessionMeta};
        let mut state = DaemonState::new_for_model("d".into(), "h".into());
        state.apply(Event::Register {
            id: "sender".into(),
            pane: Some("%1".into()),
            metadata: SessionMeta {
                networked: true,
                ..Default::default()
            },
        });
        state.apply(Event::Register {
            id: "target".into(),
            pane: Some("%2".into()),
            metadata: SessionMeta {
                networked: true,
                ..Default::default()
            },
        });
        state.apply(Event::Send {
            from: "sender".into(),
            to: "target".into(),
            message: "x".into(),
            expects_reply: true,
            responds_to: None,
            done: false,
        });

        assert_eq!(state.clear_pending_reply_from("target", "sender"), 1);
        // Second call: nothing left to clear.
        assert_eq!(state.clear_pending_reply_from("target", "sender"), 0);
        // Unknown session: also 0.
        assert_eq!(state.clear_pending_reply_from("ghost", "sender"), 0);
    }

    #[tokio::test]
    async fn failed_api_delivery_clears_only_matching_sender_pending_reply() {
        let state = crate::state::AppState::new_for_test();
        {
            let mut proto = state.protocol.write().await;
            proto.pending_replies.insert(
                "target".into(),
                vec![
                    crate::daemon_protocol::PendingReplyEntry {
                        msg_id: 42,
                        from: "sender-a".into(),
                        message: "first".into(),
                        received_at: 0,
                        last_activity: 0,
                        in_progress: false,
                    },
                    crate::daemon_protocol::PendingReplyEntry {
                        msg_id: 42,
                        from: "sender-b".into(),
                        message: "second".into(),
                        received_at: 0,
                        last_activity: 0,
                        in_progress: false,
                    },
                ],
            );
        }

        clear_pending_reply_for_failed_delivery(
            &state,
            &[crate::daemon_protocol::Effect::SendDelivered {
                from: "sender-a".into(),
                to: "target".into(),
                method: "tmux".into(),
                msg_id: 42,
                http_delivery: None,
            }],
        )
        .await;

        let proto = state.protocol.read().await;
        let pending = proto
            .pending_replies
            .get("target")
            .expect("sender-b pending reply should remain");
        assert_eq!(
            pending.len(),
            1,
            "only matching sender/msg_id must be removed"
        );
        assert_eq!(pending[0].from, "sender-b");
        assert_eq!(pending[0].msg_id, 42);
    }

    /// End-to-end regression through a real axum Router and TCP listener.
    ///
    /// The core bug in #646 was a percent-decoding mismatch: axum decodes
    /// `%74` to `t` in path segments. The `resolve_pane_to_session` unit
    /// tests cover the handler's side of that; this test additionally proves
    /// that the CLI's `pane_wire_suffix` convention (send the suffix only)
    /// actually round-trips through axum's real Path extractor.
    ///
    /// What this adds over the inner-fn tests: if axum ever changes its
    /// percent-decoding behaviour (or if a refactor accidentally reroutes
    /// through a different extractor), the inner-fn tests keep passing but
    /// this test would fail. It is the ultimate guard for the bug chain.
    #[tokio::test]
    async fn delete_pending_reply_end_to_end_through_axum_router() {
        use axum::Router;
        use axum::routing::delete;
        use tokio::net::TcpListener;

        let state = crate::state::AppState::new_for_test();

        // Register sender (%74) and recipient (%99).
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sender-a".into(),
                pane: Some("%74".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    networked: true,
                    ..Default::default()
                },
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "receiver-b".into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    networked: true,
                    ..Default::default()
                },
            })
            .await;

        // Stage a pending-reply slot on the recipient.
        state
            .apply_and_execute(crate::daemon_protocol::Event::Send {
                from: "sender-a".into(),
                to: "receiver-b".into(),
                message: "do a thing".into(),
                expects_reply: true,
                responds_to: None,
                done: false,
            })
            .await;

        // Build a minimal router that mounts the real production route.
        let app = Router::new()
            .route(
                "/api/pane/{pane}/pending-replies/{from}",
                delete(delete_pending_reply),
            )
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Send the request the way the CLI now does: suffix, no leading `%`.
        let client = reqwest::Client::new();
        let url = format!("http://{addr}/api/pane/99/pending-replies/sender-a");
        let resp = client.delete(&url).send().await.unwrap();
        assert_eq!(
            resp.status().as_u16(),
            200,
            "DELETE on real pane must return 200, not silent 404"
        );

        // Slot must be gone.
        {
            let proto = state.protocol.read().await;
            let still_there = proto
                .pending_replies
                .get("receiver-b")
                .map(|v| v.iter().any(|e| e.from == "sender-a"))
                .unwrap_or(false);
            assert!(
                !still_there,
                "pending-reply slot must be cleared after the DELETE"
            );
        }

        // Bonus: a correctly-URL-encoded `%` (sent as `%2599`, extracted by
        // axum as literal `%99`) must also route to the right pane. This is
        // the defensive tolerance that `resolve_pane_to_session` provides
        // for future callers that percent-escape the `%` properly. The
        // clear is idempotent on the DaemonState side so we still get 200
        // even though the slot is already empty.
        let url2 = format!("http://{addr}/api/pane/%2599/pending-replies/sender-a");
        let resp2 = client.delete(&url2).send().await.unwrap();
        assert_eq!(
            resp2.status().as_u16(),
            200,
            "%25-encoded `%` form must also route to the right pane"
        );

        // And the *buggy* pre-fix URL form — raw `%74` in the path — must
        // not spuriously succeed. Axum decodes `%74` to `t`, the helper
        // gets a non-matching pane id, and the correct answer is 404. This
        // is the exact failure the CLI used to silently swallow.
        let url3 = format!("http://{addr}/api/pane/%74/pending-replies/sender-a");
        let resp3 = client.delete(&url3).send().await.unwrap();
        assert_eq!(
            resp3.status().as_u16(),
            404,
            "raw `%74` URL (the pre-fix CLI's bug) must 404, not silently match"
        );

        server.abort();
    }

    /// End-to-end regression for the sender_id = `feat/646-...` case raised in
    /// code review. ouija session ids can contain `/` (branch-name-style ids
    /// passed to `/api/sessions/start` without validation), so `sender_id`
    /// in the DELETE URL must be percent-encoded or axum's two-segment
    /// route matcher fails and we hit the same silent-404 class.
    ///
    /// This test proves the full chain works end-to-end: register a session
    /// whose id contains `/`, stage a pending-reply slot for it, DELETE with
    /// the id percent-encoded, assert 200 and the slot is cleared.
    #[tokio::test]
    async fn delete_pending_reply_handles_slash_containing_sender_id() {
        use axum::Router;
        use axum::routing::delete;
        use tokio::net::TcpListener;

        let state = crate::state::AppState::new_for_test();

        // Register a sender with a branch-name-style id (contains `/`).
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "feat/646-test".into(),
                pane: Some("%74".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    networked: true,
                    ..Default::default()
                },
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "receiver-b".into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    networked: true,
                    ..Default::default()
                },
            })
            .await;

        // Stage a pending-reply slot from the slash-id sender.
        state
            .apply_and_execute(crate::daemon_protocol::Event::Send {
                from: "feat/646-test".into(),
                to: "receiver-b".into(),
                message: "do a thing".into(),
                expects_reply: true,
                responds_to: None,
                done: false,
            })
            .await;

        let app = Router::new()
            .route(
                "/api/pane/{pane}/pending-replies/{from}",
                delete(delete_pending_reply),
            )
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();

        // First, prove the broken form — raw `/` in the path — does NOT
        // match the two-segment route. Axum must not accept it; we get 404
        // (route not found) rather than the DELETE handler being called.
        // This is the exact failure the review flagged.
        let buggy_url = format!("http://{addr}/api/pane/99/pending-replies/feat/646-test");
        let buggy_resp = client.delete(&buggy_url).send().await.unwrap();
        assert_eq!(
            buggy_resp.status().as_u16(),
            404,
            "raw `/` in sender_id must break route matching (not silently \
             succeed); the CLI fix is to percent-encode it"
        );

        // Slot must still be there — the buggy URL was a no-op.
        {
            let proto = state.protocol.read().await;
            let entries = proto
                .pending_replies
                .get("receiver-b")
                .expect("slot should still exist after a 404");
            assert!(entries.iter().any(|e| e.from == "feat/646-test"));
        }

        // Now the correctly-encoded form: `feat%2F646-test`. axum decodes it
        // back to `feat/646-test` on the handler side, the lookup matches,
        // and the slot is cleared.
        let encoded_url = format!("http://{addr}/api/pane/99/pending-replies/feat%2F646-test");
        let resp = client.delete(&encoded_url).send().await.unwrap();
        assert_eq!(
            resp.status().as_u16(),
            200,
            "percent-encoded sender_id must route to the handler and clear the slot"
        );

        let proto = state.protocol.read().await;
        let still_there = proto
            .pending_replies
            .get("receiver-b")
            .map(|v| v.iter().any(|e| e.from == "feat/646-test"))
            .unwrap_or(false);
        assert!(
            !still_there,
            "slot from sender `feat/646-test` must be cleared"
        );

        server.abort();
    }

    #[tokio::test]
    async fn delete_pending_reply_clears_stuck_slot_after_sender_renamed() {
        // Full regression for the hub2 symptom: sender was *renamed*, so the
        // recipient's pending_replies bucket still has an entry whose `from`
        // points at a session id that no longer exists in the registry. The
        // daemon's cascade-on-remove does NOT trigger (no Remove event ran),
        // so the slot stays stuck and the reminder loop keeps firing on it.
        // The recipient must be able to clear it by pane id without
        // restarting the daemon — that is exactly what the hub2 operator
        // couldn't do before this PR.
        let state = crate::state::AppState::new_for_test();

        // Register sender (A) and recipient (B).
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "sender-a".into(),
                pane: Some("%74".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    networked: true,
                    ..Default::default()
                },
            })
            .await;
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "receiver-b".into(),
                pane: Some("%99".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    networked: true,
                    ..Default::default()
                },
            })
            .await;

        // A sends B a message that expects a reply → B's pending_replies[B]
        // has an entry keyed by `from = "sender-a"`.
        state
            .apply_and_execute(crate::daemon_protocol::Event::Send {
                from: "sender-a".into(),
                to: "receiver-b".into(),
                message: "do a thing".into(),
                expects_reply: true,
                responds_to: None,
                done: false,
            })
            .await;

        // Sanity: slot exists.
        {
            let proto = state.protocol.read().await;
            let entries = proto
                .pending_replies
                .get("receiver-b")
                .expect("receiver should have a pending-reply bucket");
            assert!(
                entries.iter().any(|e| e.from == "sender-a"),
                "sender-a slot should exist before clear"
            );
        }

        // Model the real hub2 shape: the sender was *renamed*, not removed.
        // apply_rename migrates the sender's own pending_replies bucket key
        // but does NOT rewrite `from` values in other sessions' pending
        // buckets — so B's pending_replies[B] still has an entry whose
        // `from = "sender-a"` pointing at a name that no longer exists in
        // the registry. No Event::Remove has fired, so the auto-clean
        // cascade does NOT kick in. This is exactly the stuck slot the
        // recipient used to be unable to clear without restarting the
        // daemon.
        state
            .apply_and_execute(crate::daemon_protocol::Event::Rename {
                old_id: "sender-a".into(),
                new_id: "sender-renamed".into(),
            })
            .await;

        // Sanity: the stuck slot is still there post-rename.
        {
            let proto = state.protocol.read().await;
            let entries = proto
                .pending_replies
                .get("receiver-b")
                .expect("receiver bucket must survive rename of sender");
            assert!(
                entries.iter().any(|e| e.from == "sender-a"),
                "sender-a slot must still be there after rename — this is \
                 the bug shape we're proving we can clear"
            );
        }

        // B clears the stuck slot by hitting the pane route with the numeric
        // suffix (what the CLI now sends). This must succeed, report the
        // cleared count, and the slot must be gone.
        let (status, body) =
            delete_pending_reply_inner(&state, "99".into(), "sender-a".into()).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "clear-reply on real pane + real pending slot must return 200"
        );
        assert_eq!(
            body["cleared"].as_u64(),
            Some(1),
            "cleared must report the removed slot count so the CLI is not lied to"
        );

        let proto = state.protocol.read().await;
        let still_there = proto
            .pending_replies
            .get("receiver-b")
            .map(|v| v.iter().any(|e| e.from == "sender-a"))
            .unwrap_or(false);
        assert!(
            !still_there,
            "sender-a slot must be cleared after DELETE /api/pane/99/pending-replies/sender-a"
        );
    }

    #[tokio::test]
    async fn prune_stale_sessions_dry_run_lists_stale() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "stale-s1".into(),
                pane: Some("%1".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/tmp/nonexistent".into()),
                    worktree_present: Some(false),
                    ..Default::default()
                },
            })
            .await;
        // Call handler directly
        let (status, body) = prune_stale_sessions(
            State(state.clone()),
            Json(PruneStaleBody { confirm: false }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let value = body.0;
        assert_eq!(value["dry_run"], true);
        assert_eq!(value["would_prune"], serde_json::json!(["stale-s1"]));
        // Session should still exist
        let proto = state.protocol.read().await;
        assert!(proto.sessions.contains_key("stale-s1"));
    }

    #[tokio::test]
    async fn prune_stale_sessions_confirm_removes_stale() {
        let state = crate::state::AppState::new_for_test();
        state
            .apply_and_execute(crate::daemon_protocol::Event::Register {
                id: "stale-s1".into(),
                pane: Some("%1".into()),
                metadata: crate::daemon_protocol::SessionMeta {
                    project_dir: Some("/tmp/nonexistent".into()),
                    worktree_present: Some(false),
                    ..Default::default()
                },
            })
            .await;
        let (status, body) =
            prune_stale_sessions(State(state.clone()), Json(PruneStaleBody { confirm: true }))
                .await;
        assert_eq!(status, StatusCode::OK);
        let value = body.0;
        assert_eq!(value["dry_run"], false);
        assert_eq!(value["pruned"], serde_json::json!(["stale-s1"]));
        // Session should be removed
        let proto = state.protocol.read().await;
        assert!(!proto.sessions.contains_key("stale-s1"));
    }
}
