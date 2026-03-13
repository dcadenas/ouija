use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nostr_sdk::prelude::*;
use tokio::sync::RwLock;

use crate::protocol::WireMessage;
use crate::state::AppState;
use crate::transport::Transport;

/// Nostr-based transport using NIP-17 private direct messages.
///
/// Each daemon is a Nostr identity. Messages are sent as gift-wrapped
/// DMs (NIP-59) through standard Nostr relays.
pub struct NostrTransport {
    client: Client,
    keys: Keys,
    relay_urls: RwLock<Vec<String>>,
    peer_pubkeys: RwLock<HashSet<PublicKey>>,
    connect_secret: String,
    data_dir: PathBuf,
    ready: AtomicBool,
}

impl NostrTransport {
    /// Create a new Nostr transport and connect to relays.
    pub async fn new(
        keys: Keys,
        relay_urls: Vec<String>,
        connect_secret: String,
        data_dir: PathBuf,
    ) -> anyhow::Result<Self> {
        let client = Client::builder().signer(keys.clone()).build();

        // NIP-42: auto-authenticate with relays that require AUTH
        // to serve kind:1059 (gift-wrapped DMs per NIP-17).
        client.automatic_authentication(true);

        for url in &relay_urls {
            if let Err(e) = client.add_relay(url.as_str()).await {
                tracing::warn!("failed to add relay {url}: {e}");
            }
        }

        client.connect().await;

        if !relay_urls.is_empty() {
            client
                .wait_for_connection(std::time::Duration::from_secs(5))
                .await;
        }

        let ready = !relay_urls.is_empty();

        let peer_pubkeys = load_peer_pubkeys(&data_dir);

        Ok(Self {
            client,
            keys,
            relay_urls: RwLock::new(relay_urls),
            peer_pubkeys: RwLock::new(peer_pubkeys),
            connect_secret,
            data_dir,
            ready: AtomicBool::new(ready),
        })
    }

    /// Authorize a peer pubkey and persist the updated set.
    async fn authorize_peer(&self, pubkey: PublicKey) {
        let mut pubkeys = self.peer_pubkeys.write().await;
        pubkeys.insert(pubkey);
        save_peer_pubkeys(&self.data_dir, &pubkeys);
    }

    /// Remove a peer pubkey and persist the updated set.
    async fn remove_peer(&self, pubkey: &PublicKey) {
        let mut pubkeys = self.peer_pubkeys.write().await;
        pubkeys.remove(pubkey);
        save_peer_pubkeys(&self.data_dir, &pubkeys);
    }

    /// Merge new relay URLs into our set, connect to them, and persist.
    async fn merge_relays(&self, new_relays: &[String]) {
        let mut urls = self.relay_urls.write().await;
        let mut changed = false;
        for url in new_relays {
            if !urls.contains(url) {
                // Add to the nostr client and connect
                match self.client.add_relay(url.as_str()).await {
                    Ok(_) => {
                        if let Err(e) = self.client.connect_relay(url.as_str()).await {
                            tracing::warn!("failed to connect new relay {url}: {e}");
                        }
                    }
                    Err(e) => {
                        tracing::warn!("failed to add relay {url}: {e}");
                        continue;
                    }
                }
                urls.push(url.clone());
                changed = true;
                tracing::info!("added relay from peer: {url}");
            }
        }
        if changed {
            if let Err(e) = save_relays(&self.data_dir, &urls) {
                tracing::warn!("failed to persist merged relays: {e}");
            }
        }
    }

    /// Start the receive loop that listens for incoming gift-wrapped DMs.
    pub async fn start_receive_loop(self: &Arc<Self>, state: Arc<AppState>) -> anyhow::Result<()> {
        let filter = Filter::new()
            .pubkey(self.keys.public_key())
            .kind(Kind::GiftWrap)
            .limit(0); // only new events (timestamps are tweaked for gift wraps)

        self.client.subscribe(filter, None).await?;

        let transport = Arc::clone(self);
        let client = self.client.clone();
        tokio::spawn(async move {
            // Dedup gift-wrap events that arrive from multiple relays.
            // nostr-sdk's relay pool has a race in check_id/save_event that
            // allows duplicate RelayPoolNotification::Event for the same event
            // when multiple relays deliver it near-simultaneously.
            // See: https://github.com/rust-nostr/nostr/issues/909
            // TODO: remove once fixed upstream in nostr-relay-pool
            let seen_events: Arc<Mutex<HashSet<EventId>>> = Arc::new(Mutex::new(HashSet::new()));

            let result = client
                .handle_notifications(|notification| {
                    let transport = Arc::clone(&transport);
                    let state = Arc::clone(&state);
                    let seen_events = Arc::clone(&seen_events);
                    async move {
                        if let RelayPoolNotification::Event { event, .. } = notification
                            && event.kind == Kind::GiftWrap
                        {
                            {
                                let mut seen = seen_events.lock().unwrap();
                                if !seen.insert(event.id) {
                                    tracing::debug!(
                                        "skipping duplicate gift-wrap event {}",
                                        event.id
                                    );
                                    return Ok(false);
                                }
                                // Prevent unbounded growth — duplicates only
                                // arrive within seconds, so purging is safe.
                                if seen.len() > 2048 {
                                    seen.clear();
                                }
                            }
                            match transport.client.unwrap_gift_wrap(&event).await {
                                Ok(UnwrappedGift { rumor, sender }) => {
                                    let npub = sender
                                        .to_bech32()
                                        .unwrap_or_else(|_| "unknown".into());
                                    let is_authorized = transport
                                        .peer_pubkeys
                                        .read()
                                        .await
                                        .contains(&sender);

                                    if rumor.kind == Kind::PrivateDirectMessage {
                                        // Check if sender is a configured human
                                        let human_name = find_human_by_npub(&state, &npub).await;

                                        if let Some(name) = human_name {
                                            // Human message path — plain text, not JSON
                                            handle_human_message(
                                                &state,
                                                &name,
                                                &npub,
                                                &rumor.content,
                                            )
                                            .await;
                                        } else {
                                            // Wire protocol path (peer daemons)
                                            let wire_msg: Result<WireMessage, _> =
                                                serde_json::from_str(&rumor.content);
                                            match wire_msg {
                                                Ok(WireMessage::ConnectRequest {
                                                    secret,
                                                    relays,
                                                }) if !is_authorized => {
                                                    if secret == transport.connect_secret {
                                                        transport.authorize_peer(sender).await;
                                                        tracing::info!(
                                                        "peer authorized via connect secret: {npub}"
                                                    );
                                                        if !relays.is_empty() {
                                                            transport
                                                                .merge_relays(&relays)
                                                                .await;
                                                        }
                                                        crate::transport::broadcast_local_sessions(
                                                            &state,
                                                        )
                                                        .await;
                                                    } else {
                                                        tracing::warn!(
                                                        "rejected connect with invalid secret from {npub}"
                                                    );
                                                    }
                                                }
                                                Ok(_) if is_authorized => {
                                                    crate::transport::handle_incoming(
                                                        &state,
                                                        rumor.content.as_bytes(),
                                                    )
                                                    .await;
                                                }
                                                _ => {
                                                    tracing::warn!(
                                                    "rejected message from unauthorized sender: {npub}"
                                                );
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("failed to unwrap gift wrap: {e}");
                                }
                            }
                        }
                        Ok(false) // keep listening
                    }
                })
                .await;

            if let Err(e) = result {
                tracing::error!("nostr notification loop ended: {e}");
            }
        });

        Ok(())
    }
}

#[async_trait::async_trait]
impl Transport for NostrTransport {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn broadcast(&self, msg: &WireMessage) -> bool {
        let json = match serde_json::to_string(msg) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("failed to serialize WireMessage: {e}");
                return false;
            }
        };

        let pubkeys = self.peer_pubkeys.read().await;
        if pubkeys.is_empty() {
            tracing::debug!("nostr broadcast: no peer pubkeys, skipping");
            return false;
        }

        let urls = self.relay_urls.read().await;
        let relay_urls: Vec<&str> = urls.iter().map(|s| s.as_str()).collect();
        let mut sent = false;

        for pubkey in pubkeys.iter() {
            let npub = pubkey.to_bech32().unwrap_or_default();
            tracing::info!(
                "nostr: sending DM to {npub} via {} relays",
                relay_urls.len()
            );
            let result = self
                .client
                .send_private_msg_to(relay_urls.clone(), *pubkey, json.clone(), [])
                .await;
            match result {
                Ok(_) => {
                    tracing::info!("nostr: DM sent to {npub}");
                    sent = true;
                }
                Err(e) => tracing::warn!("failed to send DM to {npub}: {e}"),
            }
        }

        sent
    }

    async fn connect(&self, ticket: &str, _state: Arc<AppState>, wait: bool) -> anyhow::Result<()> {
        // Split ticket on '#' — left side is nprofile, right side is connect secret
        let (nprofile_str, secret) = match ticket.split_once('#') {
            Some((left, right)) => (left, Some(right.to_string())),
            None => (ticket, None),
        };

        let profile = Nip19Profile::from_bech32(nprofile_str)?;

        // Merge the peer's relays (from nprofile) into ours
        let peer_relays: Vec<String> = profile.relays.iter().map(|u| u.to_string()).collect();
        self.merge_relays(&peer_relays).await;

        // Don't add peer pubkey yet — the remote side will authorize us
        // after we send the ConnectRequest with the correct secret.

        if wait {
            self.client
                .wait_for_connection(std::time::Duration::from_secs(5))
                .await;
        }

        // Send ConnectRequest with secret and our relay list so the peer can reach us
        if let Some(secret) = secret {
            let our_relays = self.relay_urls.read().await.clone();
            let connect_msg = WireMessage::ConnectRequest {
                secret,
                relays: our_relays,
            };
            let json = serde_json::to_string(&connect_msg)?;
            let urls = self.relay_urls.read().await;
            let relay_urls: Vec<&str> = urls.iter().map(|s| s.as_str()).collect();
            self.client
                .send_private_msg_to(relay_urls, profile.public_key, json, [])
                .await?;
            tracing::info!(
                "sent connect request to {}",
                profile.public_key.to_bech32().unwrap_or_default()
            );
        }

        // Add peer pubkey so we can send messages to them
        self.authorize_peer(profile.public_key).await;

        // Don't broadcast sessions here — the peer hasn't authorized us yet.
        // Session exchange happens via the is_new_node trigger in handle_incoming
        // when we receive the peer's SessionList response, plus the periodic
        // broadcast in the main loop provides additional resilience.

        tracing::info!(
            "connected to nostr peer {}",
            profile.public_key.to_bech32().unwrap_or_default()
        );
        Ok(())
    }

    async fn ticket_string(&self) -> Option<String> {
        let urls = self.relay_urls.read().await;
        let relay_urls: Vec<RelayUrl> = urls
            .iter()
            .filter_map(|u| RelayUrl::parse(u).ok())
            .collect();

        let profile = Nip19Profile::new(self.keys.public_key(), relay_urls);
        profile
            .to_bech32()
            .ok()
            .map(|bech32| format!("{bech32}#{}", self.connect_secret))
    }

    async fn regenerate(&self, data_dir: &Path) -> anyhow::Result<String> {
        // For nostr, regenerating means generating new keys + new secret
        let new_keys = Keys::generate();

        // Persist the new nsec
        save_nsec(data_dir, &new_keys)?;

        // Generate and persist new connect secret
        let new_secret = generate_secret();
        save_secret(data_dir, &new_secret)?;

        // Clear persisted connections
        if let Err(e) = crate::persistence::clear_connections(data_dir) {
            tracing::warn!("failed to clear connections: {e}");
        }

        // Clear known peers (memory + disk)
        self.peer_pubkeys.write().await.clear();
        save_peer_pubkeys(data_dir, &HashSet::new());

        // Generate new ticket with secret
        let urls = self.relay_urls.read().await;
        let relay_urls: Vec<RelayUrl> = urls
            .iter()
            .filter_map(|u| RelayUrl::parse(u).ok())
            .collect();

        let profile = Nip19Profile::new(new_keys.public_key(), relay_urls);
        let bech32 = profile.to_bech32()?;
        let ticket = format!("{bech32}#{new_secret}");

        tracing::info!("nostr identity regenerated (new keys + secret)");
        tracing::warn!("restart required for new nostr identity to take effect");

        Ok(ticket)
    }

    async fn deauthorize_peer(&self, peer_id: &str) {
        if let Ok(pubkey) = PublicKey::from_bech32(peer_id) {
            self.remove_peer(&pubkey).await;
            tracing::info!("deauthorized peer: {peer_id}");
        } else {
            tracing::warn!("deauthorize_peer: invalid npub '{peer_id}'");
        }
    }

    fn endpoint_id(&self) -> Option<String> {
        self.keys.public_key().to_bech32().ok().map(|npub| {
            if npub.len() > 16 {
                format!("{}...", &npub[..16])
            } else {
                npub
            }
        })
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    fn transport_name(&self) -> &'static str {
        "nostr"
    }
}

/// Look up a configured human session by npub.
async fn find_human_by_npub(state: &AppState, npub: &str) -> Option<String> {
    let settings = state.settings.read().await;
    settings
        .human_sessions
        .iter()
        .find(|h| h.npub == npub)
        .map(|h| h.name.clone())
}

/// Handle an incoming plain-text message from a human.
async fn handle_human_message(
    state: &std::sync::Arc<AppState>,
    human_name: &str,
    npub: &str,
    content: &str,
) {
    let text = content.trim();
    tracing::info!("human message from {human_name}: {text}");

    // Check if this is first contact — send welcome
    {
        let mut settings = state.settings.write().await;
        if let Some(h) = settings
            .human_sessions
            .iter_mut()
            .find(|h| h.name == human_name)
        {
            if !h.welcomed {
                h.welcomed = true;
                let settings_snapshot = settings.clone();
                drop(settings);
                if let Err(e) =
                    crate::persistence::save_settings(&state.config.config_dir, &settings_snapshot)
                {
                    tracing::warn!("failed to save welcomed flag: {e}");
                }
                let welcome = format_help_message(state, human_name).await;
                if let Err(e) = send_plain_dm(state, npub, &welcome).await {
                    tracing::warn!("failed to send welcome to {human_name}: {e}");
                }
                // If the message is just a greeting or empty, don't route further
                if text.is_empty() {
                    return;
                }
            }
        }
    }

    match parse_human_command(text) {
        HumanCommand::Help => {
            let help = format_help_message(state, human_name).await;
            if let Err(e) = send_plain_dm(state, npub, &help).await {
                tracing::warn!("failed to send help to {human_name}: {e}");
            }
        }
        HumanCommand::List => {
            let list = format_session_list(state, human_name).await;
            if let Err(e) = send_plain_dm(state, npub, &list).await {
                tracing::warn!("failed to send list to {human_name}: {e}");
            }
        }
        HumanCommand::SetDefault(session_id) => {
            let reply = set_default_session(state, human_name, &session_id).await;
            if let Err(e) = send_plain_dm(state, npub, &reply).await {
                tracing::warn!("failed to send default reply to {human_name}: {e}");
            }
        }
        HumanCommand::Status => {
            let status = format_status(state).await;
            if let Err(e) = send_plain_dm(state, npub, &status).await {
                tracing::warn!("failed to send status to {human_name}: {e}");
            }
        }
        HumanCommand::Admin(cmd) => {
            let is_admin = state
                .settings
                .read()
                .await
                .human_sessions
                .iter()
                .any(|h| h.name == human_name && h.admin);
            if !is_admin {
                let _ = send_plain_dm(state, npub, "admin access required").await;
                return;
            }
            let reply = handle_admin_command(state, &cmd).await;
            if let Err(e) = send_plain_dm(state, npub, &reply).await {
                tracing::warn!("failed to send admin reply to {human_name}: {e}");
            }
        }
        HumanCommand::SendTo(target, message) => {
            route_human_message(state, human_name, &target, &message).await;
        }
        HumanCommand::SendDefault(message) => {
            // Try LLM router: explicit config, or env var fallback
            let router_config = state.settings.read().await.router.clone().or_else(|| {
                // No explicit config — check if env var provides a key
                if std::env::var("ROUTER_API_KEY").is_ok()
                    || std::env::var("GEMINI_API_KEY").is_ok()
                {
                    Some(crate::persistence::RouterConfig {
                        api_key: None, // resolved at call time from env
                        model: "gemini-2.5-flash".to_string(),
                        base_url: "https://generativelanguage.googleapis.com/v1beta/openai"
                            .to_string(),
                    })
                } else {
                    None
                }
            });
            if let Some(ref config) = router_config {
                // Log the inbound human message so future router calls have context
                state
                    .log_message(
                        human_name.to_string(),
                        "router".to_string(),
                        message.clone(),
                        true,
                        "human-dm",
                    )
                    .await;

                let (sessions, messages) = crate::router::gather_context(state, human_name).await;
                let is_admin = state
                    .settings
                    .read()
                    .await
                    .human_sessions
                    .iter()
                    .any(|h| h.name == human_name && h.admin);
                match crate::router::classify(
                    config, &message, &sessions, &messages, human_name, is_admin,
                )
                .await
                {
                    Ok(Some(crate::router::RouterDecision::Route { targets })) => {
                        let valid_targets: Vec<String> = {
                            let known = state.sessions.read().await;
                            targets
                                .into_iter()
                                .filter(|t| known.contains_key(t))
                                .collect()
                        };
                        if !valid_targets.is_empty() {
                            tracing::info!(
                                "router: dispatching to {} target(s): {}",
                                valid_targets.len(),
                                valid_targets.join(", ")
                            );
                            for target in &valid_targets {
                                route_human_message(state, human_name, target, &message).await;
                            }
                            return;
                        }
                        tracing::warn!("router: no valid targets found, falling back to default");
                    }
                    Ok(Some(crate::router::RouterDecision::Command(cmd))) => {
                        tracing::info!("router: classified as command: {cmd}");
                        match parse_human_command(&cmd) {
                            HumanCommand::Help => {
                                let help = format_help_message(state, human_name).await;
                                let _ = send_plain_dm(state, npub, &help).await;
                                state
                                    .log_message(
                                        "router".into(),
                                        human_name.into(),
                                        help,
                                        true,
                                        "human-dm",
                                    )
                                    .await;
                                return;
                            }
                            HumanCommand::List => {
                                let list = format_session_list(state, human_name).await;
                                let _ = send_plain_dm(state, npub, &list).await;
                                state
                                    .log_message(
                                        "router".into(),
                                        human_name.into(),
                                        list,
                                        true,
                                        "human-dm",
                                    )
                                    .await;
                                return;
                            }
                            HumanCommand::Status => {
                                let status = format_status(state).await;
                                let _ = send_plain_dm(state, npub, &status).await;
                                state
                                    .log_message(
                                        "router".into(),
                                        human_name.into(),
                                        status,
                                        true,
                                        "human-dm",
                                    )
                                    .await;
                                return;
                            }
                            _ => {
                                tracing::warn!("router: ignoring unrecognized command: {cmd}");
                            }
                        }
                    }
                    Ok(Some(crate::router::RouterDecision::DirectAnswer(answer))) => {
                        tracing::info!("router: direct answer");
                        let _ = send_plain_dm(state, npub, &answer).await;
                        state
                            .log_message(
                                "router".into(),
                                human_name.into(),
                                answer,
                                true,
                                "human-dm",
                            )
                            .await;
                        return;
                    }
                    Ok(None) => {
                        tracing::warn!("router: unparseable LLM response, falling back to default");
                    }
                    Err(e) => {
                        tracing::warn!("router API error: {e}");
                        let _ = send_plain_dm(
                            state,
                            npub,
                            &format!("router error: {e}\nfalling back to default session"),
                        )
                        .await;
                        // fall through to default
                    }
                }
            }

            // Fallback: existing default_session behavior
            let default = {
                state
                    .settings
                    .read()
                    .await
                    .human_sessions
                    .iter()
                    .find(|h| h.name == human_name)
                    .and_then(|h| h.default_session.clone())
            };
            match default {
                Some(target) => {
                    route_human_message(state, human_name, &target, &message).await;
                }
                None => {
                    let _ = send_plain_dm(
                        state,
                        npub,
                        "no default session set. use /default <id> or @<id> <message>",
                    )
                    .await;
                }
            }
        }
    }
}

#[derive(Debug)]
enum HumanCommand {
    Help,
    List,
    SetDefault(String),
    Status,
    Admin(String),
    SendTo(String, String),
    SendDefault(String),
}

fn parse_human_command(text: &str) -> HumanCommand {
    if text.eq_ignore_ascii_case("/help") {
        return HumanCommand::Help;
    }
    if text.eq_ignore_ascii_case("/list") {
        return HumanCommand::List;
    }
    if text.eq_ignore_ascii_case("/status") {
        return HumanCommand::Status;
    }
    if let Some(rest) = text.strip_prefix("/default ") {
        let id = rest.trim();
        if !id.is_empty() {
            return HumanCommand::SetDefault(id.to_string());
        }
    }
    // Admin commands
    if text.starts_with("/connect ")
        || text.starts_with("/disconnect ")
        || text.starts_with("/nodes")
        || text.starts_with("/task ")
        || text.starts_with("/kill ")
        || text.starts_with("/start ")
        || text.starts_with("/restart ")
    {
        return HumanCommand::Admin(text.to_string());
    }
    // @target message — tolerates optional space after @, trailing punctuation on target
    if let Some(rest) = text.strip_prefix('@') {
        let rest = rest.trim_start();
        if let Some((raw_target, msg)) = rest.split_once(|c: char| c.is_whitespace()) {
            let target = raw_target.trim_end_matches(|c: char| c.is_ascii_punctuation());
            let msg = msg.trim();
            if !target.is_empty() && !msg.is_empty() {
                return HumanCommand::SendTo(target.to_string(), msg.to_string());
            }
        }
        // Handle @target,message (no space, comma-separated)
        if let Some((raw_target, msg)) = rest.split_once(',') {
            let target = raw_target.trim_end_matches(|c: char| c.is_ascii_punctuation());
            let msg = msg.trim();
            if !target.is_empty() && !msg.is_empty() {
                return HumanCommand::SendTo(target.to_string(), msg.to_string());
            }
        }
    }
    // Bare text → default session
    HumanCommand::SendDefault(text.to_string())
}

async fn format_help_message(state: &AppState, human_name: &str) -> String {
    let default = state
        .settings
        .read()
        .await
        .human_sessions
        .iter()
        .find(|h| h.name == human_name)
        .and_then(|h| h.default_session.clone());

    let mut lines = Vec::new();
    lines.push(format!("ouija ({})\n", state.config.name));
    lines.push("Commands:".to_string());
    lines.push("  /help              — this message".to_string());
    lines.push("  /list              — show sessions".to_string());
    lines.push("  /default <id>      — set default session".to_string());
    lines.push("  /status            — daemon status".to_string());
    lines.push(String::new());
    lines.push("Usage:".to_string());
    if let Some(ref d) = default {
        lines.push(format!(
            "  <message>          — send to default session ({d})"
        ));
    } else {
        lines.push("  <message>          — send to default session (none set)".to_string());
    }
    lines.push("  @<id> <message>    — send to specific session".to_string());

    let is_admin = state
        .settings
        .read()
        .await
        .human_sessions
        .iter()
        .any(|h| h.name == human_name && h.admin);
    if is_admin {
        lines.push(String::new());
        lines.push("Admin:".to_string());
        lines.push("  /kill <session>    — kill a session".to_string());
        lines.push("  /start <name>      — start new session".to_string());
        lines.push(
            "  /restart <name> [--fresh]  — restart a session (--fresh: no prior context)"
                .to_string(),
        );
        lines.push("  /connect <ticket>  — connect to peer".to_string());
        lines.push("  /nodes             — list connected nodes".to_string());
        lines.push("  /task list|trigger — manage tasks".to_string());
    }

    lines.join("\n")
}

async fn format_session_list(state: &AppState, human_name: &str) -> String {
    let sessions = state.sessions.read().await;
    let default = state
        .settings
        .read()
        .await
        .human_sessions
        .iter()
        .find(|h| h.name == human_name)
        .and_then(|h| h.default_session.clone());

    let mut lines = Vec::new();
    for s in sessions.values() {
        // Don't show the asking human their own session
        if s.id == human_name {
            continue;
        }
        let origin = s.origin.label();
        let marker = if default.as_deref() == Some(&s.id) {
            " [default]"
        } else {
            ""
        };
        let role = s
            .metadata
            .role
            .as_deref()
            .map(|r| format!(" — {r}"))
            .unwrap_or_default();
        lines.push(format!("  {} ({origin}){role}{marker}", s.id));
    }
    if lines.is_empty() {
        "no sessions".to_string()
    } else {
        lines.push(String::new());
        lines.push("Send @<id> <message> to talk to a session.".to_string());
        lines.join("\n")
    }
}

async fn set_default_session(state: &AppState, human_name: &str, session_id: &str) -> String {
    // Verify session exists
    let exists = state.sessions.read().await.contains_key(session_id);
    if !exists {
        return format!("session '{session_id}' not found");
    }

    let mut settings = state.settings.write().await;
    if let Some(h) = settings
        .human_sessions
        .iter_mut()
        .find(|h| h.name == human_name)
    {
        h.default_session = Some(session_id.to_string());
        let snapshot = settings.clone();
        drop(settings);
        if let Err(e) = crate::persistence::save_settings(&state.config.config_dir, &snapshot) {
            tracing::warn!("failed to save default session: {e}");
            return "failed to save setting".to_string();
        }
        format!("default session set to '{session_id}'")
    } else {
        "human session not found".to_string()
    }
}

async fn format_status(state: &AppState) -> String {
    let sessions = state.sessions.read().await;
    let nodes = state.nodes.read().await;
    let transports = state.transports().await;

    let local = sessions
        .values()
        .filter(|s| matches!(s.origin, crate::state::SessionOrigin::Local))
        .count();
    let remote = sessions
        .values()
        .filter(|s| matches!(s.origin, crate::state::SessionOrigin::Remote(_)))
        .count();
    let human = sessions
        .values()
        .filter(|s| matches!(s.origin, crate::state::SessionOrigin::Human(_)))
        .count();

    let p2p = if transports.values().any(|t| t.is_ready()) {
        "ready"
    } else {
        "initializing"
    };

    format!(
        "daemon: {}\nsessions: {local} local, {remote} remote, {human} human\nnodes: {}\np2p: {p2p}",
        state.config.name,
        nodes.len(),
    )
}

async fn route_human_message(state: &AppState, from: &str, to: &str, message: &str) {
    // Use the same send path as the API
    let sessions = state.sessions.read().await;
    let target = sessions.get(to).cloned();
    drop(sessions);

    match target {
        Some(session) => match &session.origin {
            crate::state::SessionOrigin::Local => {
                if let Some(pane) = &session.pane {
                    // Human messages always expect a reply
                    let formatted = crate::tmux::format_session_message(from, message, true);
                    let pane = pane.clone();
                    let vim_mode = session.metadata.vim_mode;
                    let lock = state.pane_lock(&pane);
                    let _guard = lock.lock().await;
                    let result = tokio::task::spawn_blocking(move || {
                        crate::tmux::inject(&pane, &formatted, vim_mode)
                    })
                    .await;
                    let delivered = matches!(result, Ok(Ok(())));
                    if delivered {
                        let mut sessions = state.sessions.write().await;
                        if let Some(s) = sessions.get_mut(to) {
                            s.block_interactive = true;
                        }
                    }
                    state
                        .log_message(
                            from.to_string(),
                            to.to_string(),
                            message.to_string(),
                            delivered,
                            "human-dm",
                        )
                        .await;
                }
            }
            crate::state::SessionOrigin::Remote(_) => {
                let wire_to = crate::state::strip_remote_prefix(to).to_string();
                let wire_msg = crate::protocol::WireMessage::SessionSend {
                    from: from.to_string(),
                    to: wire_to,
                    message: message.to_string(),
                    expects_reply: true,
                };
                let sent = crate::transport::broadcast(state, &wire_msg).await;
                state
                    .log_message(
                        from.to_string(),
                        to.to_string(),
                        message.to_string(),
                        sent,
                        "nostr",
                    )
                    .await;
            }
            crate::state::SessionOrigin::Human(npub) => {
                // Human-to-human relay
                let formatted = format!("[from {from}]: {message}");
                let delivered = send_plain_dm(state, npub, &formatted).await.is_ok();
                state
                    .log_message(
                        from.to_string(),
                        to.to_string(),
                        message.to_string(),
                        delivered,
                        "nostr-dm",
                    )
                    .await;
            }
        },
        None => {
            tracing::warn!("human message target '{to}' not found");
        }
    }
}

pub async fn handle_admin_command(state: &std::sync::Arc<AppState>, cmd: &str) -> String {
    if let Some(ticket) = cmd.strip_prefix("/connect ") {
        let ticket = ticket.trim();
        let transport = match state.transport_by_name("nostr").await {
            Some(t) => t,
            None => return "nostr transport not active".to_string(),
        };
        match transport.connect(ticket, state.clone(), true).await {
            Ok(()) => "connected".to_string(),
            Err(e) => format!("connect failed: {e}"),
        }
    } else if let Some(name) = cmd.strip_prefix("/disconnect ") {
        let name = name.trim();
        // Find daemon_id by node name
        let daemon_id = {
            let nodes = state.nodes.read().await;
            nodes
                .values()
                .find(|n| n.name == name)
                .map(|n| n.daemon_id.clone())
        };
        match daemon_id {
            Some(id) => {
                let removed = state.disconnect_node(&id).await;
                format!("disconnected '{name}', {removed} sessions removed")
            }
            None => format!("node '{name}' not found"),
        }
    } else if cmd.starts_with("/nodes") {
        let npub_short = |s: &str| -> String {
            if s.len() > 20 {
                format!("{}…{}", &s[..10], &s[s.len() - 6..])
            } else {
                s.to_string()
            }
        };
        let mut lines = vec![format!(
            "  {} (self) {}",
            state.config.name,
            npub_short(&state.config.npub)
        )];
        let nodes = state.nodes.read().await;
        for n in nodes.values() {
            lines.push(format!(
                "  {} ({}) {}",
                n.name,
                n.connected_at.format("%H:%M"),
                npub_short(&n.daemon_id)
            ));
        }
        lines.join("\n")
    } else if cmd.starts_with("/task ") {
        let rest = cmd.strip_prefix("/task ").unwrap().trim();
        if rest == "list" {
            let tasks = state.scheduled_tasks.read().await;
            if tasks.is_empty() {
                "no scheduled tasks".to_string()
            } else {
                let lines: Vec<String> = tasks
                    .values()
                    .map(|t| {
                        format!(
                            "  {} — {} [{}] {}",
                            t.id,
                            t.name,
                            t.cron,
                            if t.enabled { "on" } else { "off" }
                        )
                    })
                    .collect();
                lines.join("\n")
            }
        } else if let Some(id) = rest.strip_prefix("trigger ") {
            let id = id.trim();
            let exists = state.scheduled_tasks.read().await.contains_key(id);
            if exists {
                crate::scheduler::execute_task(state, id).await;
                format!("task '{id}' triggered")
            } else {
                format!("task '{id}' not found")
            }
        } else {
            "usage: /task list, /task trigger <id>".to_string()
        }
    } else if let Some(name) = cmd.strip_prefix("/kill ") {
        let name = name.trim();
        admin_kill_session(state, name).await
    } else if let Some(rest) = cmd.strip_prefix("/start ") {
        let name = rest.trim();
        admin_start_session(state, name, None, None).await
    } else if let Some(rest) = cmd.strip_prefix("/restart ") {
        let rest = rest.trim();
        let (name, fresh) = if let Some(name) = rest.strip_suffix(" --fresh") {
            (name.trim(), true)
        } else if let Some(name) = rest.strip_prefix("--fresh ") {
            (name.trim(), true)
        } else {
            (rest, false)
        };
        admin_restart_session(state, name, fresh).await
    } else {
        "unknown admin command".to_string()
    }
}

pub async fn admin_kill_session(state: &std::sync::Arc<AppState>, name: &str) -> String {
    let session = state.sessions.read().await.get(name).cloned();
    let Some(session) = session else {
        return format!("session '{name}' not found");
    };
    if !matches!(session.origin, crate::state::SessionOrigin::Local) {
        return format!("'{name}' is not a local session");
    }
    let Some(pane) = &session.pane else {
        return format!("'{name}' has no pane");
    };

    // Get the pane's PID and find the claude process
    let pane = pane.clone();
    let kill_result = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        use std::process::Command;

        // Get pane PID
        let output = Command::new("tmux")
            .args(["display-message", "-t", &pane, "-p", "#{pane_pid}"])
            .output()?;
        if !output.status.success() {
            anyhow::bail!("could not get pane PID");
        }
        let pane_pid: u32 = String::from_utf8_lossy(&output.stdout).trim().parse()?;

        // Find claude process in the tree
        let output = Command::new("ps").args(["-eo", "pid,ppid,comm"]).output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut children: std::collections::HashMap<u32, Vec<u32>> =
            std::collections::HashMap::new();
        let mut names: std::collections::HashMap<u32, String> = std::collections::HashMap::new();

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

        // BFS to find claude PID
        let mut stack = vec![pane_pid];
        let mut claude_pid = None;
        while let Some(pid) = stack.pop() {
            if names.get(&pid).is_some_and(|n| n == "claude") {
                claude_pid = Some(pid);
                break;
            }
            if let Some(kids) = children.get(&pid) {
                stack.extend(kids);
            }
        }

        match claude_pid {
            Some(pid) => {
                // Graceful: send /exit first, give Claude time to shut down cleanly
                let _ = Command::new("tmux")
                    .args(["send-keys", "-t", &pane, "/exit", "Enter"])
                    .status();

                // Poll up to 10s for claude process to exit
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                let mut exited = false;
                while std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    // Check if process still exists
                    let status = Command::new("kill").args(["-0", &pid.to_string()]).status();
                    if status.is_err() || !status.unwrap().success() {
                        exited = true;
                        break;
                    }
                }

                if !exited {
                    // Fallback: SIGTERM
                    let _ = Command::new("kill").arg(pid.to_string()).status();
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }

                let _ = Command::new("tmux")
                    .args(["kill-pane", "-t", &pane])
                    .status();
                let method = if exited {
                    "exited gracefully"
                } else {
                    "SIGTERM"
                };
                Ok(format!("killed claude (pid {pid}, {method})"))
            }
            None => {
                let _ = Command::new("tmux")
                    .args(["kill-pane", "-t", &pane])
                    .status();
                Ok("no claude process found".to_string())
            }
        }
    })
    .await;

    let msg = match kill_result {
        Ok(Ok(msg)) => msg,
        Ok(Err(e)) => return format!("kill failed: {e}"),
        Err(e) => return format!("kill failed: {e}"),
    };

    // Also kill any tmux session that matches the ouija session name
    let session_name = name.to_string();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &session_name])
            .status();
    })
    .await;

    state.remove_session(name).await;
    format!("{msg}, session '{name}' removed")
}

pub async fn admin_start_session(
    state: &std::sync::Arc<AppState>,
    name: &str,
    worktree: Option<bool>,
    project_dir: Option<&str>,
) -> String {
    // Check if already exists
    if state.sessions.read().await.contains_key(name) {
        return format!("session '{name}' already exists");
    }

    let dir = if let Some(pd) = project_dir {
        pd.to_string()
    } else {
        let projects_dir = state.settings.read().await.projects_dir.clone();
        let base = match projects_dir {
            Some(dir) => {
                // Expand ~ to HOME
                if let Some(rest) = dir.strip_prefix("~/") {
                    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                    format!("{home}/{rest}")
                } else {
                    dir
                }
            }
            None => {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                format!("{home}/code")
            }
        };
        format!("{base}/{name}")
    };

    // Auto-enable worktree if another session shares this directory
    let (worktree, auto_worktree) = match worktree {
        Some(wt) => (wt, false),
        None => {
            let sessions = state.sessions.read().await;
            let conflict = sessions.values().any(|s| {
                matches!(s.origin, crate::state::SessionOrigin::Local)
                    && s.metadata.project_dir.as_deref() == Some(dir.as_str())
            });
            (conflict, conflict)
        }
    };

    // Create directory if it doesn't exist
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return format!("failed to create {dir}: {e}");
    }

    let session_name = name.to_string();
    let start_result = tokio::task::spawn_blocking({
        let dir = dir.clone();
        let session_name = session_name.clone();
        move || -> anyhow::Result<String> {
            use std::process::Command;

            // If a tmux session with this name exists, add a window to it;
            // otherwise create a new tmux session. Window name matches ouija
            // session name so the user gets visual identification.
            let tmux_session_exists = Command::new("tmux")
                .args(["has-session", "-t", &session_name])
                .output()
                .is_ok_and(|o| o.status.success());

            let pane_id = if tmux_session_exists {
                let output = Command::new("tmux")
                    .args([
                        "new-window",
                        "-d",
                        "-t",
                        &session_name,
                        "-n",
                        &session_name,
                        "-P",
                        "-F",
                        "#{pane_id}",
                    ])
                    .output()?;
                if !output.status.success() {
                    anyhow::bail!(
                        "tmux new-window failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            } else {
                let output = Command::new("tmux")
                    .args([
                        "new-session",
                        "-d",
                        "-s",
                        &session_name,
                        "-n",
                        &session_name,
                        "-P",
                        "-F",
                        "#{pane_id}",
                    ])
                    .output()?;
                if !output.status.success() {
                    anyhow::bail!(
                        "tmux new-session failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            };

            // Prevent tmux from overriding the window name
            let _ = Command::new("tmux")
                .args([
                    "set-window-option",
                    "-t",
                    &pane_id,
                    "automatic-rename",
                    "off",
                ])
                .status();

            // Launch claude
            let cmd = if worktree {
                format!(
                    "cd {} && claude --dangerously-skip-permissions --worktree {}",
                    crate::scheduler::shell_escape(&dir),
                    crate::scheduler::shell_escape(&session_name),
                )
            } else {
                format!(
                    "cd {} && claude --dangerously-skip-permissions",
                    crate::scheduler::shell_escape(&dir),
                )
            };
            Command::new("tmux")
                .args(["send-keys", "-t", &pane_id, &cmd, "Enter"])
                .status()?;

            Ok(pane_id)
        }
    })
    .await;

    match start_result {
        Ok(Ok(pane_id)) => {
            // Register immediately so the session is visible right away
            let metadata = crate::state::SessionMetadata {
                project_dir: Some(dir.clone()),
                worktree,
                ..Default::default()
            };
            state
                .register_session(name.to_string(), Some(pane_id.clone()), metadata)
                .await;
            if auto_worktree {
                let conflict_name = {
                    let sessions = state.sessions.read().await;
                    sessions
                        .values()
                        .find(|s| {
                            s.id != name && s.metadata.project_dir.as_deref() == Some(dir.as_str())
                        })
                        .map(|s| s.id.clone())
                        .unwrap_or_default()
                };
                format!(
                    "started '{name}' in {dir} (pane {pane_id}, worktree: auto-enabled — session '{conflict_name}' shares this directory)"
                )
            } else {
                format!("started '{name}' in {dir} (pane {pane_id})")
            }
        }
        Ok(Err(e)) => format!("start failed: {e}"),
        Err(e) => format!("start failed: {e}"),
    }
}

pub async fn admin_restart_session(
    state: &std::sync::Arc<AppState>,
    name: &str,
    fresh: bool,
) -> String {
    // Snapshot full metadata before killing so we can carry it forward
    let session = state.sessions.read().await.get(name).cloned();
    let prev_metadata = session.as_ref().map(|s| s.metadata.clone());

    // Capture existing pane before killing
    let existing_pane = session.as_ref().and_then(|s| s.pane.clone());

    // Remove the ouija session record (agent cleanup etc) but don't touch tmux
    if session.is_some() {
        state.remove_session(name).await;
    }

    let projects_dir = state.settings.read().await.projects_dir.clone();
    let base = match projects_dir {
        Some(dir) => {
            if let Some(rest) = dir.strip_prefix("~/") {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                format!("{home}/{rest}")
            } else {
                dir
            }
        }
        None => {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            format!("{home}/code")
        }
    };

    // Use previous project_dir if available, otherwise derive from name
    let dir = prev_metadata
        .as_ref()
        .and_then(|m| m.project_dir.clone())
        .unwrap_or_else(|| format!("{base}/{name}"));

    if let Err(e) = std::fs::create_dir_all(&dir) {
        return format!("failed to create {dir}: {e}");
    }

    let resume_id = if fresh {
        None
    } else {
        prev_metadata
            .as_ref()
            .and_then(|m| m.claude_session_id.clone())
            .or_else(|| detect_claude_session_id(&dir))
    };
    if let Some(ref sid) = resume_id {
        tracing::info!("restart '{name}': using --resume {sid}");
    }

    let worktree_flag = prev_metadata.as_ref().is_some_and(|m| m.worktree);

    let session_name = name.to_string();
    let start_result = tokio::task::spawn_blocking({
        let dir = dir.clone();
        let session_name = session_name.clone();
        let existing_pane = existing_pane.clone();
        move || -> anyhow::Result<String> {
            use std::process::Command;

            let resume_flag = match (&resume_id, fresh) {
                (_, true) => String::new(),
                (Some(sid), false) => format!("--resume {}", crate::scheduler::shell_escape(sid)),
                (None, false) => "--continue".to_string(),
            };
            let claude_cmd = if worktree_flag {
                format!(
                    "cd {} && claude --dangerously-skip-permissions {resume_flag} --worktree {}",
                    crate::scheduler::shell_escape(&dir),
                    crate::scheduler::shell_escape(&session_name),
                )
            } else {
                format!(
                    "cd {} && claude --dangerously-skip-permissions {resume_flag}",
                    crate::scheduler::shell_escape(&dir),
                )
            };

            // Try respawn-pane on existing pane — kills the process and restarts
            // in-place, keeping the same pane ID and tmux session intact
            if let Some(ref pane) = existing_pane {
                let output = Command::new("tmux")
                    .args(["respawn-pane", "-k", "-t", pane, &claude_cmd])
                    .output();
                match output {
                    Ok(o) if o.status.success() => {
                        tracing::info!("restart: respawn-pane {pane} succeeded");
                        return Ok(pane.clone());
                    }
                    Ok(o) => {
                        tracing::info!(
                            "restart: respawn-pane {pane} failed: {}",
                            String::from_utf8_lossy(&o.stderr).trim()
                        );
                    }
                    Err(e) => {
                        tracing::info!("restart: respawn-pane {pane} error: {e}");
                    }
                }
            }

            // Fallback: add window to existing tmux session, or create new one
            let tmux_session_exists = Command::new("tmux")
                .args(["has-session", "-t", &session_name])
                .output()
                .is_ok_and(|o| o.status.success());

            let output = if tmux_session_exists {
                Command::new("tmux")
                    .args([
                        "new-window",
                        "-d",
                        "-t",
                        &session_name,
                        "-n",
                        &session_name,
                        "-P",
                        "-F",
                        "#{pane_id}",
                    ])
                    .output()?
            } else {
                Command::new("tmux")
                    .args([
                        "new-session",
                        "-d",
                        "-s",
                        &session_name,
                        "-n",
                        &session_name,
                        "-P",
                        "-F",
                        "#{pane_id}",
                    ])
                    .output()?
            };
            if !output.status.success() {
                anyhow::bail!(
                    "tmux session/window creation failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            let pane_id = String::from_utf8_lossy(&output.stdout).trim().to_string();

            // Prevent tmux from overriding the window name
            let _ = Command::new("tmux")
                .args([
                    "set-window-option",
                    "-t",
                    &pane_id,
                    "automatic-rename",
                    "off",
                ])
                .status();

            Command::new("tmux")
                .args(["send-keys", "-t", &pane_id, &claude_cmd, "Enter"])
                .status()?;

            Ok(pane_id)
        }
    })
    .await;

    match start_result {
        Ok(Ok(pane_id)) => {
            let metadata = match prev_metadata {
                Some(mut m) => {
                    m.project_dir = Some(dir.clone());
                    m.last_metadata_update = None;
                    if fresh {
                        m.claude_session_id = None;
                    }
                    m
                }
                None => crate::state::SessionMetadata {
                    project_dir: Some(dir.clone()),
                    ..Default::default()
                },
            };
            state
                .register_session(name.to_string(), Some(pane_id.clone()), metadata)
                .await;
            format!("restarted '{name}' in {dir} (pane {pane_id})")
        }
        Ok(Err(e)) => format!("restart failed: {e}"),
        Err(e) => format!("restart failed: {e}"),
    }
}

/// Auto-detect the Claude session ID from the most recently modified JSONL
/// in `~/.claude/projects/<project_slug>/`. The project slug is the absolute
/// path with `/` replaced by `-`.
pub fn detect_claude_session_id(project_dir: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    // Claude encodes project dirs as: absolute path with / replaced by -
    // e.g. /home/daniel/code/ouija -> -home-daniel-code-ouija
    let slug = project_dir.replace('/', "-");
    let sessions_dir = std::path::PathBuf::from(&home)
        .join(".claude")
        .join("projects")
        .join(&slug);
    if !sessions_dir.is_dir() {
        return None;
    }

    // Find the most recently modified .jsonl file
    let mut newest: Option<(std::time::SystemTime, String)> = None;
    let entries = std::fs::read_dir(&sessions_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let modified = entry.metadata().ok()?.modified().ok()?;
        let stem = path.file_stem()?.to_str()?.to_string();
        if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
            newest = Some((modified, stem));
        }
    }

    let (_, session_id) = newest?;
    tracing::debug!(
        "auto-detected claude session {session_id} from {}",
        sessions_dir.display()
    );
    Some(session_id)
}

/// Send a plain-text NIP-17 DM to a human's npub.
///
/// Uses the nostr transport's client to send a gift-wrapped DM with plain text
/// content (not JSON wire protocol).
pub async fn send_plain_dm(
    state: &crate::state::AppState,
    npub: &str,
    text: &str,
) -> anyhow::Result<()> {
    let transport = state
        .transport_by_name("nostr")
        .await
        .ok_or_else(|| anyhow::anyhow!("nostr transport not active"))?;

    let nostr = transport
        .as_ref()
        .as_any()
        .downcast_ref::<NostrTransport>()
        .ok_or_else(|| anyhow::anyhow!("transport is not NostrTransport"))?;

    let pubkey = PublicKey::from_bech32(npub)?;
    let urls = nostr.relay_urls.read().await;
    let relay_urls: Vec<&str> = urls.iter().map(|s| s.as_str()).collect();

    nostr
        .client
        .send_private_msg_to(relay_urls, pubkey, text.to_string(), [])
        .await?;

    tracing::info!("sent plain DM to {npub}");
    Ok(())
}

// --- Lazy activation ---

const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://nos.lol",
];

/// Ensure the nostr transport is active, starting it if needed.
///
/// If already running, returns the existing transport. Otherwise loads/creates
/// keys, merges `extra_relays` with persisted relays, spins up the transport,
/// starts the receive loop, and registers it.
pub async fn ensure_active(
    state: &crate::state::SharedState,
    extra_relays: Vec<String>,
) -> anyhow::Result<Arc<dyn Transport>> {
    // Already running? Return it.
    if let Some(t) = state.transport_by_name("nostr").await {
        return Ok(t);
    }

    let keys = load_or_create_keys(&state.config.data_dir)?;

    let npub = keys
        .public_key()
        .to_bech32()
        .unwrap_or_else(|_| "unknown".into());
    tracing::info!("nostr identity: {npub}");

    // Merge persisted relays with extra relays
    let mut relay_urls = load_relays(&state.config.data_dir);
    for r in &extra_relays {
        if !relay_urls.contains(r) {
            relay_urls.push(r.clone());
        }
    }

    // Fall back to default relays if none configured
    if relay_urls.is_empty() {
        relay_urls.extend(DEFAULT_RELAYS.iter().map(|s| s.to_string()));
    }

    // Persist merged relay list
    if let Err(e) = save_relays(&state.config.data_dir, &relay_urls) {
        tracing::warn!("failed to save relay URLs: {e}");
    }

    let connect_secret = load_or_create_secret(&state.config.data_dir)?;
    let transport = Arc::new(
        NostrTransport::new(
            keys,
            relay_urls,
            connect_secret,
            state.config.data_dir.clone(),
        )
        .await?,
    );

    transport.start_receive_loop(state.clone()).await?;
    state.add_transport(transport.clone()).await;
    tracing::info!("P2P networking ready (nostr)");

    Ok(transport)
}

// --- Key persistence ---

/// Load nostr keys from nsec file, or generate new ones.
pub fn load_or_create_keys(data_dir: &Path) -> anyhow::Result<Keys> {
    let path = data_dir.join("nostr_nsec");
    if path.exists() {
        let nsec = std::fs::read_to_string(&path)?;
        let keys = Keys::parse(nsec.trim())?;
        tracing::info!("loaded nostr identity from {}", path.display());
        Ok(keys)
    } else {
        let keys = Keys::generate();
        save_nsec(data_dir, &keys)?;
        tracing::info!("generated new nostr identity at {}", path.display());
        Ok(keys)
    }
}

fn save_nsec(data_dir: &Path, keys: &Keys) -> anyhow::Result<()> {
    let nsec = keys.secret_key().to_bech32()?;
    let path = data_dir.join("nostr_nsec");
    std::fs::write(&path, &nsec)?;
    Ok(())
}

// --- Connect secret persistence ---

/// Generate a random 32-char hex string for use as a connect secret.
fn generate_secret() -> String {
    use std::fmt::Write;
    let bytes: [u8; 16] = ::rand::random();
    let mut s = String::with_capacity(32);
    for b in bytes {
        write!(s, "{b:02x}").unwrap();
    }
    s
}

/// Load connect secret from disk, or generate and persist a new one.
fn load_or_create_secret(data_dir: &Path) -> anyhow::Result<String> {
    let path = data_dir.join("connect_secret");
    if path.exists() {
        let secret = std::fs::read_to_string(&path)?.trim().to_string();
        if !secret.is_empty() {
            return Ok(secret);
        }
    }
    let secret = generate_secret();
    save_secret(data_dir, &secret)?;
    Ok(secret)
}

fn save_secret(data_dir: &Path, secret: &str) -> anyhow::Result<()> {
    let path = data_dir.join("connect_secret");
    std::fs::write(&path, secret)?;
    Ok(())
}

// --- Relay persistence ---

/// Load persisted relay URLs from disk.
pub fn load_relays(data_dir: &Path) -> Vec<String> {
    let path = data_dir.join("nostr_relays.json");
    if !path.exists() {
        return Vec::new();
    }
    match std::fs::read_to_string(&path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
        Err(e) => {
            tracing::warn!("failed to load nostr relays: {e}");
            Vec::new()
        }
    }
}

/// Save relay URLs to disk.
pub fn save_relays(data_dir: &Path, relays: &[String]) -> anyhow::Result<()> {
    let data = serde_json::to_string(relays)?;
    let path = data_dir.join("nostr_relays.json");
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data.as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

// --- Peer pubkey persistence ---

/// Load authorized peer pubkeys from disk.
fn load_peer_pubkeys(data_dir: &Path) -> HashSet<PublicKey> {
    let path = data_dir.join("peer_pubkeys.json");
    if !path.exists() {
        return HashSet::new();
    }
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("failed to load peer pubkeys: {e}");
            return HashSet::new();
        }
    };
    let npubs: Vec<String> = serde_json::from_str(&data).unwrap_or_default();
    npubs
        .iter()
        .filter_map(|s| PublicKey::from_bech32(s).ok())
        .collect()
}

/// Save authorized peer pubkeys to disk.
fn save_peer_pubkeys(data_dir: &Path, pubkeys: &HashSet<PublicKey>) {
    let npubs: Vec<String> = pubkeys
        .iter()
        .filter_map(|pk| pk.to_bech32().ok())
        .collect();
    let data = match serde_json::to_string(&npubs) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("failed to serialize peer pubkeys: {e}");
            return;
        }
    };
    let path = data_dir.join("peer_pubkeys.json");
    let tmp = path.with_extension("tmp");
    if let Err(e) =
        std::fs::write(&tmp, data.as_bytes()).and_then(|()| std::fs::rename(&tmp, &path))
    {
        tracing::warn!("failed to persist peer pubkeys: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_or_create_keys_generates_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let keys = load_or_create_keys(dir.path()).unwrap();

        // File should exist now
        assert!(dir.path().join("nostr_nsec").exists());

        // Loading again should return the same keys
        let keys2 = load_or_create_keys(dir.path()).unwrap();
        assert_eq!(keys.public_key(), keys2.public_key());
    }

    #[test]
    fn load_or_create_keys_loads_existing() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::generate();
        save_nsec(dir.path(), &keys).unwrap();

        let loaded = load_or_create_keys(dir.path()).unwrap();
        assert_eq!(keys.public_key(), loaded.public_key());
    }

    #[test]
    fn relay_persistence_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let relays = vec![
            "wss://relay.damus.io".to_string(),
            "wss://nos.lol".to_string(),
        ];
        save_relays(dir.path(), &relays).unwrap();
        let loaded = load_relays(dir.path());
        assert_eq!(loaded, relays);
    }

    #[test]
    fn load_relays_missing_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_relays(dir.path()).is_empty());
    }

    #[test]
    fn nprofile_ticket_round_trip() {
        let keys = Keys::generate();
        let relay_urls: Vec<RelayUrl> = vec![RelayUrl::parse("wss://relay.damus.io").unwrap()];
        let profile = Nip19Profile::new(keys.public_key(), relay_urls);
        let bech32 = profile.to_bech32().unwrap();

        assert!(bech32.starts_with("nprofile1"));

        let parsed = Nip19Profile::from_bech32(&bech32).unwrap();
        assert_eq!(parsed.public_key, keys.public_key());
        assert_eq!(parsed.relays.len(), 1);
    }

    #[test]
    fn secret_persistence_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let secret = load_or_create_secret(dir.path()).unwrap();
        assert_eq!(secret.len(), 32);

        // Loading again returns the same secret
        let secret2 = load_or_create_secret(dir.path()).unwrap();
        assert_eq!(secret, secret2);
    }

    #[test]
    fn secret_is_32_char_hex() {
        let secret = generate_secret();
        assert_eq!(secret.len(), 32);
        assert!(secret.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // --- Human command parsing tests ---

    #[test]
    fn parse_help() {
        assert!(matches!(parse_human_command("/help"), HumanCommand::Help));
        assert!(matches!(parse_human_command("/HELP"), HumanCommand::Help));
    }

    #[test]
    fn parse_list() {
        assert!(matches!(parse_human_command("/list"), HumanCommand::List));
    }

    #[test]
    fn parse_status() {
        assert!(matches!(
            parse_human_command("/status"),
            HumanCommand::Status
        ));
    }

    #[test]
    fn parse_default() {
        match parse_human_command("/default ouija") {
            HumanCommand::SetDefault(id) => assert_eq!(id, "ouija"),
            other => panic!("expected SetDefault, got {other:?}"),
        }
    }

    #[test]
    fn parse_admin_connect() {
        match parse_human_command("/connect nprofile1abc") {
            HumanCommand::Admin(cmd) => assert_eq!(cmd, "/connect nprofile1abc"),
            other => panic!("expected Admin, got {other:?}"),
        }
    }

    #[test]
    fn parse_admin_nodes() {
        assert!(matches!(
            parse_human_command("/nodes"),
            HumanCommand::Admin(_)
        ));
    }

    #[test]
    fn parse_admin_task() {
        assert!(matches!(
            parse_human_command("/task list"),
            HumanCommand::Admin(_)
        ));
    }

    #[test]
    fn parse_at_target() {
        match parse_human_command("@ouija hello world") {
            HumanCommand::SendTo(target, msg) => {
                assert_eq!(target, "ouija");
                assert_eq!(msg, "hello world");
            }
            other => panic!("expected SendTo, got {other:?}"),
        }
    }

    #[test]
    fn parse_at_target_with_space_after_at() {
        match parse_human_command("@ loca.local/rust-nostr do you see me?") {
            HumanCommand::SendTo(target, msg) => {
                assert_eq!(target, "loca.local/rust-nostr");
                assert_eq!(msg, "do you see me?");
            }
            other => panic!("expected SendTo, got {other:?}"),
        }
    }

    #[test]
    fn parse_at_target_with_trailing_comma() {
        match parse_human_command("@ouija, that was great") {
            HumanCommand::SendTo(target, msg) => {
                assert_eq!(target, "ouija");
                assert_eq!(msg, "that was great");
            }
            other => panic!("expected SendTo, got {other:?}"),
        }
    }

    #[test]
    fn parse_at_target_with_trailing_punctuation() {
        match parse_human_command("@ouija: what's up?") {
            HumanCommand::SendTo(target, msg) => {
                assert_eq!(target, "ouija");
                assert_eq!(msg, "what's up?");
            }
            other => panic!("expected SendTo, got {other:?}"),
        }
    }

    #[test]
    fn parse_at_target_comma_no_space() {
        match parse_human_command("@ouija,hello") {
            HumanCommand::SendTo(target, msg) => {
                assert_eq!(target, "ouija");
                assert_eq!(msg, "hello");
            }
            other => panic!("expected SendTo, got {other:?}"),
        }
    }

    #[test]
    fn parse_bare_text() {
        match parse_human_command("just a message") {
            HumanCommand::SendDefault(msg) => assert_eq!(msg, "just a message"),
            other => panic!("expected SendDefault, got {other:?}"),
        }
    }

    #[test]
    fn parse_at_without_message_is_default() {
        // "@ouija" with no message body falls through to SendDefault
        assert!(matches!(
            parse_human_command("@ouija"),
            HumanCommand::SendDefault(_)
        ));
    }

    #[test]
    fn ticket_contains_secret_after_hash() {
        let keys = Keys::generate();
        let relay_urls: Vec<RelayUrl> = vec![RelayUrl::parse("wss://relay.damus.io").unwrap()];
        let profile = Nip19Profile::new(keys.public_key(), relay_urls);
        let bech32 = profile.to_bech32().unwrap();

        let secret = "abcdef0123456789abcdef0123456789";
        let ticket = format!("{bech32}#{secret}");

        let (nprofile_part, secret_part) = ticket.split_once('#').unwrap();
        assert!(nprofile_part.starts_with("nprofile1"));
        assert_eq!(secret_part, secret);

        // nprofile part still parses correctly
        let parsed = Nip19Profile::from_bech32(nprofile_part).unwrap();
        assert_eq!(parsed.public_key, keys.public_key());
    }
}
