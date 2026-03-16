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
    /// Downcast to concrete type.
    fn as_any(&self) -> &dyn std::any::Any;

    /// Broadcast a wire message to all connected peers.
    /// Returns `true` if at least one peer was available.
    async fn broadcast(&self, msg: &WireMessage) -> bool;

    /// Connect to a peer using an opaque ticket string.
    /// When `wait` is true, blocks until the peer is reachable.
    async fn connect(&self, ticket: &str, state: Arc<AppState>, wait: bool) -> anyhow::Result<()>;

    /// Generate a ticket string for others to connect to us.
    async fn ticket_string(&self) -> Option<String>;

    /// Regenerate identity/topic, invalidating old tickets.
    async fn regenerate(&self, config_dir: &Path, data_dir: &Path) -> anyhow::Result<String>;

    /// Remove a peer so future messages from it are rejected.
    ///
    /// The `peer_id` is transport-specific (e.g. an npub for Nostr).
    /// Default implementation is a no-op for transports without peer auth.
    async fn deauthorize_peer(&self, _peer_id: &str) {}

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
/// When `sender_npub` is provided, the daemon_id field in the message is
/// verified against the actual Nostr sender pubkey. Mismatches are logged
/// and the message is dropped.
pub async fn handle_incoming(state: &Arc<AppState>, content: &[u8], sender_npub: Option<&str>) {
    let msg: WireMessage = match serde_json::from_slice(content) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("failed to decode incoming message: {e}");
            return;
        }
    };

    // Verify daemon_id matches the actual Nostr sender when available.
    if let Some(expected) = sender_npub {
        let claimed = msg.daemon_id();
        if let Some(claimed) = claimed {
            if claimed != expected {
                tracing::warn!(
                    "daemon_id mismatch: message claims {claimed} but sender is {expected}, dropping"
                );
                return;
            }
        }
    }

    // Drop stale wire messages using the monotonic sequence counter.
    if let (Some(daemon_id), Some(seq)) = (msg.daemon_id(), msg.seq()) {
        if !state.accept_seq(daemon_id, seq) {
            tracing::debug!("dropping stale message from {daemon_id} (seq={seq} < last_seen)");
            return;
        }
    }

    match msg {
        WireMessage::SessionSend {
            from,
            to,
            message,
            expects_reply,
        } => {
            enum DeliveryTarget {
                Local { pane: String, vim_mode: bool },
                Human { npub: String },
            }

            // Resolve bare `from` to daemon-prefixed key so replies route
            // back across machines (e.g. "ouija" -> "locota/ouija").
            let (target, display_from, sender_known) = {
                let sessions = state.sessions.read().await;
                let target = sessions.get(&to).and_then(|session| match &session.origin {
                    SessionOrigin::Local if session.metadata.networked => {
                        session.pane.as_ref().map(|p| DeliveryTarget::Local {
                            pane: p.clone(),
                            vim_mode: session.metadata.vim_mode,
                        })
                    }
                    SessionOrigin::Human(npub) => {
                        Some(DeliveryTarget::Human { npub: npub.clone() })
                    }
                    _ => None,
                });
                let remote_match = sessions
                    .iter()
                    .find(|(_, s)| {
                        matches!(&s.origin, SessionOrigin::Remote(_))
                            && crate::state::strip_remote_prefix(&s.id) == from.as_str()
                    })
                    .map(|(key, _)| key.clone());
                let sender_known = remote_match.is_some();
                let display_from = remote_match.unwrap_or_else(|| from.clone());
                (target, display_from, sender_known)
            };

            match target {
                Some(DeliveryTarget::Local { pane, vim_mode }) => {
                    let formatted =
                        tmux::format_session_message(&display_from, &message, expects_reply);
                    let delivered = tmux::locked_inject(state, &pane, &formatted, vim_mode)
                        .await
                        .is_ok();
                    if delivered {
                        let mut sessions = state.sessions.write().await;
                        if let Some(s) = sessions.get_mut(&to) {
                            s.block_interactive = true;
                        }
                    }
                    if delivered && expects_reply && sender_known {
                        state
                            .notify_agent(
                                &to,
                                crate::session_agent::SessionMsg::MessageDelivered {
                                    from: from.clone(),
                                    message: message.clone(),
                                    expects_reply: true,
                                },
                            )
                            .await;
                    }
                    if delivered {
                        state
                            .notify_agent(
                                &from,
                                crate::session_agent::SessionMsg::ReplySent { to: to.clone() },
                            )
                            .await;
                    }
                    state
                        .log_message(from.clone(), to.clone(), message, delivered, "nostr")
                        .await;

                    let ack = WireMessage::SessionSendAck {
                        from: from.clone(),
                        to: to.clone(),
                        delivered,
                        daemon_id: state.config.npub.clone(),
                    };
                    broadcast(state, &ack).await;
                }
                Some(DeliveryTarget::Human { npub }) => {
                    let formatted = format!("[from {display_from}]: {message}");
                    let delivered = crate::nostr_transport::send_plain_dm(state, &npub, &formatted)
                        .await
                        .is_ok();
                    state
                        .log_message(from.clone(), to.clone(), message, delivered, "nostr-dm")
                        .await;
                }
                None => {
                    tracing::warn!("SessionSend target '{to}' not found or not local");
                }
            }
        }
        WireMessage::SessionSendAck {
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
            ..
        } => {
            let display_name = display_name(&daemon_name, &daemon_id);
            let key = crate::state::remote_session_key(display_name, &id);
            tracing::info!("remote session announced: {key} from daemon {daemon_id}");
            let mut sessions = state.sessions.write().await;
            let entry = sessions
                .entry(key.clone())
                .or_insert_with(|| crate::state::Session {
                    id: key,
                    pane: None,
                    origin: SessionOrigin::Remote(daemon_id),
                    registered_at: chrono::Utc::now(),
                    last_activity_at: chrono::Utc::now(),
                    metadata: metadata.clone().unwrap_or_default(),
                    block_interactive: false,
                });
            if let Some(m) = metadata {
                entry.metadata = m;
            }
        }
        WireMessage::SessionList {
            sessions: session_infos,
            daemon_id,
            daemon_name,
            ..
        } => {
            let ids: Vec<&str> = session_infos.iter().map(|i| i.id.as_str()).collect();
            tracing::info!("received session list from {daemon_name} ({daemon_id}): {ids:?}",);
            let expected_keys: std::collections::HashSet<String> = session_infos
                .iter()
                .map(|info| crate::state::remote_session_key(&daemon_name, &info.id))
                .collect();

            let mut sessions = state.sessions.write().await;

            // Remove stale entries from a prior SessionAnnounce that used a
            // different prefix (e.g. daemon_id when daemon_name was empty).
            let raw_ids: std::collections::HashSet<&str> =
                session_infos.iter().map(|i| i.id.as_str()).collect();
            let announce_dupes: Vec<String> = sessions
                .iter()
                .filter(|(_, s)| matches!(&s.origin, SessionOrigin::Remote(d) if d == &daemon_id))
                .filter(|(key, _)| {
                    let suffix = crate::state::strip_remote_prefix(key);
                    let canonical = crate::state::remote_session_key(&daemon_name, suffix);
                    raw_ids.contains(suffix) && **key != canonical
                })
                .map(|(key, _)| key.clone())
                .collect();
            for key in &announce_dupes {
                sessions.remove(key);
                tracing::info!("removed announce-race duplicate: {key}");
            }

            for info in &session_infos {
                let key = crate::state::remote_session_key(&daemon_name, &info.id);
                let entry = sessions
                    .entry(key.clone())
                    .or_insert_with(|| crate::state::Session {
                        id: key,
                        pane: None,
                        origin: SessionOrigin::Remote(daemon_id.clone()),
                        registered_at: chrono::Utc::now(),
                        last_activity_at: chrono::Utc::now(),
                        metadata: info.metadata.clone().unwrap_or_default(),
                        block_interactive: false,
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

            // Clear orphaned pending replies for all removed remote sessions.
            // Pending replies track bare session names (from wire messages),
            // so strip the daemon prefix from removed keys.
            let mut removed_bare: Vec<String> = stale
                .iter()
                .chain(announce_dupes.iter())
                .map(|key| crate::state::strip_remote_prefix(key).to_string())
                .collect();
            removed_bare.dedup();
            if !removed_bare.is_empty() {
                state.clear_orphaned_pending_replies(&removed_bare).await;
            }

            state.nodes.write().await.insert(
                daemon_id.clone(),
                crate::state::NodeInfo {
                    name: daemon_name,
                    daemon_id: daemon_id.clone(),
                    connected_at: chrono::Utc::now(),
                },
            );
            // Reciprocate so the sender gets our sessions (e.g. after they
            // restart).  Debounced to prevent infinite ping-pong over Nostr.
            if state.should_reciprocate(&daemon_id) {
                tracing::info!("reciprocating session list to {daemon_id}");
                broadcast_local_sessions(state).await;
            } else {
                tracing::debug!("skipping reciprocation to {daemon_id} (debounced)");
            }
        }
        WireMessage::ConnectRequest { .. } => {
            // Handled directly in the nostr receive loop, not here.
        }
        WireMessage::SessionRemove {
            id,
            daemon_id,
            daemon_name,
            ..
        } => {
            let display_name = display_name(&daemon_name, &daemon_id);
            let key = crate::state::remote_session_key(display_name, &id);
            tracing::info!("remote session removed: {key} from daemon {daemon_id}");
            let removed = {
                let mut sessions = state.sessions.write().await;
                if sessions.get(&key).is_some_and(
                    |s| matches!(&s.origin, SessionOrigin::Remote(d) if d == &daemon_id),
                ) {
                    sessions.remove(&key).is_some()
                } else {
                    false
                }
            };
            if removed {
                state.clear_orphaned_pending_replies(&[id]).await;
            }
        }
        WireMessage::SessionRenamed {
            old_id,
            new_id,
            daemon_id,
            daemon_name,
            metadata,
            ..
        } => {
            let display = display_name(&daemon_name, &daemon_id);
            let old_key = crate::state::remote_session_key(display, &old_id);
            let new_key = crate::state::remote_session_key(display, &new_id);
            tracing::info!("remote session renamed: {old_key} -> {new_key}");

            let mut sessions = state.sessions.write().await;
            let old_session = sessions.remove(&old_key);
            let new_session = crate::state::Session {
                id: new_key.clone(),
                pane: None,
                origin: SessionOrigin::Remote(daemon_id),
                registered_at: old_session
                    .as_ref()
                    .map(|s| s.registered_at)
                    .unwrap_or_else(chrono::Utc::now),
                last_activity_at: chrono::Utc::now(),
                metadata: metadata
                    .unwrap_or_else(|| old_session.map(|s| s.metadata).unwrap_or_default()),
                block_interactive: false,
            };
            sessions.insert(new_key.clone(), new_session);
            drop(sessions);

            state.add_alias(&old_key, &new_key).await;
            state.add_alias(&old_id, &new_id).await;
        }
        WireMessage::Command { command, daemon_id } => {
            tracing::info!("received command from {daemon_id}: {command}");
            let result = crate::nostr_transport::handle_admin_command(state, &command).await;
            let reply = WireMessage::CommandResult {
                command,
                result,
                daemon_id: state.config.npub.clone(),
            };
            broadcast(state, &reply).await;
        }
        WireMessage::CommandResult {
            command,
            result,
            daemon_id,
        } => {
            tracing::info!("command result from {daemon_id}: {command} -> {result}");
            state
                .deliver_command_result(&daemon_id, &command, &result)
                .await;
        }
    }
}

/// Broadcast all local networked sessions to peers for discovery.
pub async fn broadcast_local_sessions(state: &AppState) {
    let sessions = state.sessions.read().await;
    let local_infos: Vec<crate::protocol::SessionInfo> = sessions
        .values()
        .filter(|s| matches!(s.origin, SessionOrigin::Local) && s.metadata.networked)
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
        seq: state.current_seq(),
    };
    broadcast(state, &msg).await;
}

/// Pick `daemon_name` if non-empty, otherwise fall back to `daemon_id`.
fn display_name<'a>(daemon_name: &'a str, daemon_id: &'a str) -> &'a str {
    if daemon_name.is_empty() {
        daemon_id
    } else {
        daemon_name
    }
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
