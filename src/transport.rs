use std::path::Path;
use std::sync::Arc;

use crate::protocol::WireMessage;
use crate::state::{AppState, SessionOrigin};
use crate::tmux;

/// P2P transport abstraction.
///
/// Implementations handle connection setup, message broadcasting, and
/// receiving. The receive side is an implementation detail: the transport
/// spawns its own receive loop and calls [`handle_incoming`] when a
/// `WireMessage` arrives.
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// Broadcast a wire message to all connected peers.
    /// Returns `true` if at least one peer was available.
    async fn broadcast(&self, msg: &WireMessage) -> bool;

    /// Connect to a peer using an opaque ticket string.
    /// When `wait` is true, blocks until the peer is reachable.
    async fn connect(
        &self,
        ticket: &str,
        state: Arc<AppState>,
        wait: bool,
    ) -> anyhow::Result<()>;

    /// Generate a ticket string for others to connect to us.
    fn ticket_string(&self) -> Option<String>;

    /// Regenerate identity/topic, invalidating old tickets.
    async fn regenerate(&self, data_dir: &Path) -> anyhow::Result<String>;

    /// Human-readable endpoint identifier for status display.
    fn endpoint_id(&self) -> Option<String>;

    /// Whether the transport is initialized and ready.
    fn is_ready(&self) -> bool;

    /// Short name identifying the transport backend (e.g. "nostr").
    fn transport_name(&self) -> &'static str;
}

/// Route an incoming wire message to the appropriate handler.
///
/// Called by transport implementations when bytes arrive from a peer.
pub async fn handle_incoming(state: &AppState, content: &[u8]) {
    let msg: WireMessage = match serde_json::from_slice(content) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("failed to decode incoming message: {e}");
            return;
        }
    };

    match msg {
        WireMessage::PeerSend { from, to, message } => {
            let session_info = {
                let sessions = state.sessions.read().await;
                sessions.get(&to).and_then(|session| {
                    if matches!(session.origin, SessionOrigin::Local) {
                        session
                            .pane
                            .as_ref()
                            .map(|p| (p.clone(), session.metadata.vim_mode))
                    } else {
                        None
                    }
                })
            };
            if let Some((pane, vim_mode)) = session_info {
                let formatted = tmux::format_peer_message(&from, &message);
                let lock = state.pane_lock(&pane);
                let _guard = lock.lock().await;
                let result =
                    tokio::task::spawn_blocking(move || tmux::inject(&pane, &formatted, vim_mode))
                        .await;
                drop(_guard);
                let delivered = matches!(result, Ok(Ok(())));
                state
                    .log_message(from.clone(), to.clone(), message, delivered, "gossip")
                    .await;

                let ack = WireMessage::PeerSendAck {
                    from: from.clone(),
                    to: to.clone(),
                    delivered,
                    daemon_id: state.config.npub.clone(),
                };
                broadcast(state, &ack).await;
            } else {
                tracing::warn!("PeerSend target '{to}' not found or not local");
            }
        }
        WireMessage::PeerSendAck {
            from,
            to,
            delivered,
            daemon_id,
        } => {
            if delivered {
                tracing::info!("ack: message {from}->{to} delivered by {daemon_id}");
            } else {
                tracing::warn!("ack: message {from}->{to} FAILED delivery at {daemon_id}");
            }
        }
        WireMessage::SessionAnnounce {
            id,
            daemon_id,
            daemon_name,
            metadata,
        } => {
            let display_name = if daemon_name.is_empty() { &daemon_id } else { &daemon_name };
            let key = crate::state::remote_session_key(display_name, &id);
            tracing::info!("remote session announced: {key} from daemon {daemon_id}");
            let mut sessions = state.sessions.write().await;
            let entry =
                sessions
                    .entry(key.clone())
                    .or_insert_with(|| crate::state::Session {
                        id: key,
                        pane: None,
                        origin: SessionOrigin::Remote(daemon_id),
                        registered_at: chrono::Utc::now(),
                        metadata: metadata.clone().unwrap_or_default(),
                    });
            if let Some(m) = metadata {
                entry.metadata = m;
            }
        }
        WireMessage::SessionList {
            sessions: session_infos,
            daemon_id,
            daemon_name,
        } => {
            let ids: Vec<&str> = session_infos.iter().map(|i| i.id.as_str()).collect();
            tracing::info!(
                "received session list from {daemon_name} ({daemon_id}): {ids:?}",
            );
            let expected_keys: std::collections::HashSet<String> = session_infos
                .iter()
                .map(|info| crate::state::remote_session_key(&daemon_name, &info.id))
                .collect();

            let mut sessions = state.sessions.write().await;

            for info in &session_infos {
                let key = crate::state::remote_session_key(&daemon_name, &info.id);
                let entry =
                    sessions
                        .entry(key.clone())
                        .or_insert_with(|| crate::state::Session {
                            id: key,
                            pane: None,
                            origin: SessionOrigin::Remote(daemon_id.clone()),
                            registered_at: chrono::Utc::now(),
                            metadata: info.metadata.clone().unwrap_or_default(),
                        });
                if let Some(m) = &info.metadata {
                    entry.metadata = m.clone();
                }
            }

            let stale: Vec<String> = sessions
                .iter()
                .filter(|(_, s)| matches!(&s.origin, SessionOrigin::Remote(d) if d == &daemon_id))
                .map(|(key, _)| key.clone())
                .filter(|key| !expected_keys.contains(key))
                .collect();
            for key in &stale {
                sessions.remove(key);
                tracing::info!("reconciled stale remote session: {key}");
            }

            drop(sessions);

            let is_new_peer = !state.peers.read().await.contains_key(&daemon_id);
            state.peers.write().await.insert(
                daemon_id.clone(),
                crate::state::PeerInfo {
                    name: daemon_name,
                    daemon_id,
                    connected_at: chrono::Utc::now(),
                },
            );
            if is_new_peer {
                broadcast_local_sessions(state).await;
            }
        }
        WireMessage::ConnectRequest { .. } => {
            // Handled directly in the nostr receive loop, not here.
        }
        WireMessage::SessionRemove { id, daemon_id, daemon_name } => {
            let display_name = if daemon_name.is_empty() { &daemon_id } else { &daemon_name };
            let key = crate::state::remote_session_key(display_name, &id);
            tracing::info!("remote session removed: {key} from daemon {daemon_id}");
            let mut sessions = state.sessions.write().await;
            if sessions
                .get(&key)
                .is_some_and(|s| matches!(&s.origin, SessionOrigin::Remote(d) if d == &daemon_id))
            {
                sessions.remove(&key);
            }
        }
    }
}

/// Broadcast all local sessions to peers for discovery.
pub async fn broadcast_local_sessions(state: &AppState) {
    let sessions = state.sessions.read().await;
    let local_infos: Vec<crate::protocol::SessionInfo> = sessions
        .values()
        .filter(|s| matches!(s.origin, SessionOrigin::Local))
        .map(|s| crate::protocol::SessionInfo {
            id: s.id.clone(),
            metadata: Some(s.metadata.clone()),
        })
        .collect();
    drop(sessions);

    if local_infos.is_empty() {
        return;
    }

    let msg = WireMessage::SessionList {
        sessions: local_infos,
        daemon_id: state.config.npub.clone(),
        daemon_name: state.config.name.clone(),
    };
    broadcast(state, &msg).await;
}

/// Broadcast a wire message via all active transports.
///
/// Returns `true` if at least one transport successfully sent.
pub async fn broadcast(state: &AppState, msg: &WireMessage) -> bool {
    let transports = state.transports().await;
    let mut any_sent = false;
    for t in transports.values() {
        if t.broadcast(msg).await {
            any_sent = true;
        }
    }
    any_sent
}
