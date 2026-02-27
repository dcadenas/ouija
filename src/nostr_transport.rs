use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
    relay_urls: Vec<String>,
    peer_pubkeys: RwLock<HashSet<PublicKey>>,
    connect_secret: String,
    ready: AtomicBool,
}

impl NostrTransport {
    /// Create a new Nostr transport and connect to relays.
    pub async fn new(
        keys: Keys,
        relay_urls: Vec<String>,
        connect_secret: String,
    ) -> anyhow::Result<Self> {
        let client = Client::builder().signer(keys.clone()).build();

        for url in &relay_urls {
            if let Err(e) = client.add_relay(url.as_str()).await {
                tracing::warn!("failed to add relay {url}: {e}");
            }
        }

        client.connect().await;

        let ready = !relay_urls.is_empty();

        Ok(Self {
            client,
            keys,
            relay_urls,
            peer_pubkeys: RwLock::new(HashSet::new()),
            connect_secret,
            ready: AtomicBool::new(ready),
        })
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
            let result = client
                .handle_notifications(|notification| {
                    let transport = Arc::clone(&transport);
                    let state = Arc::clone(&state);
                    async move {
                        if let RelayPoolNotification::Event { event, .. } = notification
                            && event.kind == Kind::GiftWrap
                        {
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
                                        let wire_msg: Result<WireMessage, _> =
                                            serde_json::from_str(&rumor.content);
                                        match wire_msg {
                                            Ok(WireMessage::ConnectRequest { secret })
                                                if !is_authorized =>
                                            {
                                                if secret == transport.connect_secret {
                                                    transport
                                                        .peer_pubkeys
                                                        .write()
                                                        .await
                                                        .insert(sender);
                                                    tracing::info!(
                                                        "peer authorized via connect secret: {npub}"
                                                    );
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
            tracing::info!("nostr broadcast: no peer pubkeys, skipping");
            return false;
        }

        let relay_urls: Vec<&str> = self.relay_urls.iter().map(|s| s.as_str()).collect();
        let mut sent = false;

        for pubkey in pubkeys.iter() {
            let npub = pubkey.to_bech32().unwrap_or_default();
            tracing::info!("nostr: sending DM to {npub} via {} relays", relay_urls.len());
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

    async fn connect(
        &self,
        ticket: &str,
        state: Arc<AppState>,
        wait: bool,
    ) -> anyhow::Result<()> {
        // Split ticket on '#' — left side is nprofile, right side is connect secret
        let (nprofile_str, secret) = match ticket.split_once('#') {
            Some((left, right)) => (left, Some(right.to_string())),
            None => (ticket, None),
        };

        let profile = Nip19Profile::from_bech32(nprofile_str)?;

        // Add relay hints from the nprofile and connect them
        for relay_url in &profile.relays {
            match self.client.add_relay(relay_url.as_str()).await {
                Ok(_) => {
                    if let Err(e) = self.client.connect_relay(relay_url.as_str()).await {
                        tracing::warn!("failed to connect relay {relay_url}: {e}");
                    }
                }
                Err(e) => {
                    tracing::warn!("failed to add relay hint {relay_url}: {e}");
                }
            }
        }

        // Don't add peer pubkey yet — the remote side will authorize us
        // after we send the ConnectRequest with the correct secret.

        if wait {
            self.client
                .wait_for_connection(std::time::Duration::from_secs(5))
                .await;
        }

        // Send ConnectRequest with secret so the peer authorizes us
        if let Some(secret) = secret {
            let connect_msg = WireMessage::ConnectRequest { secret };
            let json = serde_json::to_string(&connect_msg)?;
            let relay_urls: Vec<&str> = self.relay_urls.iter().map(|s| s.as_str()).collect();
            self.client
                .send_private_msg_to(relay_urls, profile.public_key, json, [])
                .await?;
            tracing::info!(
                "sent connect request to {}",
                profile.public_key.to_bech32().unwrap_or_default()
            );
        }

        // Add peer pubkey so we can send messages to them
        self.peer_pubkeys.write().await.insert(profile.public_key);

        // Broadcast our sessions so the new peer discovers us
        crate::transport::broadcast_local_sessions(&state).await;

        tracing::info!(
            "connected to nostr peer {}",
            profile.public_key.to_bech32().unwrap_or_default()
        );
        Ok(())
    }

    fn ticket_string(&self) -> Option<String> {
        let relay_urls: Vec<RelayUrl> = self
            .relay_urls
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

        // Clear known peers
        self.peer_pubkeys.write().await.clear();

        // Generate new ticket with secret
        let relay_urls: Vec<RelayUrl> = self
            .relay_urls
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
    let transport = Arc::new(NostrTransport::new(keys, relay_urls, connect_secret).await?);

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
